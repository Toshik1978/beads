//! The reconciliation plan: everything one run would do, computed without
//! doing any of it.
//!
//! `br remote status` prints this and stops; `push`, `pull` and `sync`
//! execute it. Building it is pure — it takes the workspace's issues and the
//! fetched mirror and returns a value — so every decision the engine makes is
//! testable without a socket.

use crate::model::{DependencyType, Issue, Status};
use crate::remote::config::RemoteConfig;
use crate::remote::diff::{Direction, FieldChange, diff_pair};
use crate::remote::link_diff::{BeadLinks, LinkChange, diff_links};
use crate::remote::model::RemoteSnapshot;
use crate::remote::reconcile::pair_workspace;
use crate::remote::youtrack::links::LinkTypes;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// Printed when a plan has **no** comment work, and only then.
///
/// It used to say comments were not reconciled at all, unconditionally, which
/// was true when nothing reconciled them. Now that something does, saying it
/// on a plan that carries comment changes would be a plain lie — and the
/// caveat that survives is a narrower and more useful one. Comment
/// reconciliation is gated on the two sides' comment *counts* differing
/// (`comments::comment_counts_agree`), because inspecting every pair would
/// cost one request per mirrored issue per run to learn nothing. A comment
/// edited in place on either side leaves the counts equal, so it is never
/// looked at; that is the limitation this note exists to state, and it is the
/// one a reader of "nothing to do" needs to know about.
const COMMENT_COUNT_GATE: &str = "note: comments are compared only where the local and remote comment counts \
     differ, so an edit to a comment that has already crossed is not detected.";

/// One paired issue's differing fields.
#[derive(Debug, Clone, Serialize)]
pub struct IssueFieldPlan {
    pub bead_id: String,
    pub remote_id: String,
    pub changes: Vec<FieldChange>,
}

/// One paired issue's link additions and removals.
#[derive(Debug, Clone, Serialize)]
pub struct IssueLinkPlan {
    pub bead_id: String,
    pub remote_id: String,
    pub changes: Vec<LinkChange>,
}

/// A remote issue no bead claims — an adoption candidate.
///
/// `beads_id` is the issue's own `Beads ID` field, and it is what tells a
/// genuine web-UI adoption apart from **a create this mirror already made and
/// then lost the answer to**. `issue_create_body` stamps the bead id into that
/// field on every create, so an issue that was created and whose response
/// never arrived comes back naming the bead it belongs to. Pairing it is a
/// local repair; adopting it would mint a *second* bead for one issue, and
/// creating it again would mint a second issue for one bead. See
/// [`crate::remote::execute::orphaned_creates`].
#[derive(Debug, Clone, Serialize)]
pub struct Adoption {
    pub remote_id: String,
    pub summary: String,
    /// The issue's `Beads ID` field, when it carries one.
    pub beads_id: Option<String>,
}

/// A remote issue br cannot read, and therefore will not adopt.
///
/// `reason` comes from the mapping layer and already names the field, the
/// offending value and the config key that would cover it; `parse_issue`
/// prefixes it with the issue's own id.
#[derive(Debug, Clone, Serialize)]
pub struct RefusedAdoption {
    pub remote_id: String,
    pub reason: String,
}

/// A dependency row that has no mirror, and never will.
///
/// Only `parent-child`, `blocks` and `related`/`relates-to` map onto the
/// three YouTrack link types. Collapsing `waits-for` or `duplicates` onto
/// `Depend` would be lossy in a way nothing could undo. Dropping them
/// silently, though, leaves a user who sees `waits-for` in `br dep list` and
/// no link change in `br remote status` with no way to find out why — so they
/// are printed.
#[derive(Debug, Clone, Serialize)]
pub struct UnmirroredLink {
    pub bead_id: String,
    pub dep_type: String,
    pub target_id: String,
}

/// A tombstoned bead that is still paired with a live remote issue.
///
/// Its field diff is deliberately not computed: `tombstone` has no
/// `status_map` entry (the mirrored-status list excludes it on purpose), so a
/// diff would emit `state: tombstone → open [push]` and a push would then
/// fail on a value that cannot be mapped. Deletion semantics belong to
/// [`crate::remote::tombstone`], which classifies each of these bead ids as a
/// rename forward or a genuine deletion by `former_ids` membership — see
/// [`crate::remote::tombstone::plan_tombstones`].
#[derive(Debug, Clone, Serialize)]
pub struct TombstonedPair {
    pub bead_id: String,
    pub remote_id: String,
}

/// A bead whose `external_ref` names an issue that is gone.
#[derive(Debug, Clone, Serialize)]
pub struct DanglingRef {
    pub bead_id: String,
    pub external_ref: String,
}

/// A local value no map covers, which would refuse a push.
#[derive(Debug, Clone, Serialize)]
pub struct UnmappedLocal {
    pub bead_id: String,
    pub field: String,
    pub value: String,
    pub map_key: String,
}

/// Comments to move for one paired issue.
///
/// Populated by the comment reconciliation; the plan carries the section so
/// the rendering and the JSON shape are settled in one place.
#[derive(Debug, Clone, Serialize)]
pub struct CommentPlan {
    pub bead_id: String,
    pub remote_id: String,
    pub direction: Direction,
    pub count: usize,
}

/// One paired issue's comment reconciliation, **already computed**.
///
/// [`build_plan`] is pure and issues no request, and every one of its tests
/// depends on that. Deciding what to do about comments needs the comments
/// themselves, which needs a socket — so the fetch happens in the caller
/// (`crate::remote::execute::fetch_comment_work`) and arrives here as data.
/// The plan reduces it to counts for printing; the executor keeps the texts
/// and writes them. One fetch, two consumers.
#[derive(Debug, Clone, Default)]
pub struct CommentWork {
    pub bead_id: String,
    pub remote_id: String,
    /// Comment texts to post, `[br]` marker line included.
    pub to_push: Vec<String>,
    /// Remote comments to import as bead comments.
    pub to_pull: Vec<crate::remote::comments::RemoteComment>,
}

/// Which half of a bidirectional plan a rendering covers.
///
/// One `ReconcilePlan` describes both directions, and `br remote status`
/// prints all of it. `pull` and `push` each execute one direction, and
/// rendering the whole plan under either is worse than noise: a bare
/// `br remote pull` opened with `create 171 issue(s)` — naming work only a
/// push performs — and `br remote sync`, being `pull` then `push`, printed
/// that same 171-line list twice before writing anything.
///
/// The sections partition cleanly. Creates, link changes, tombstones and
/// unmapped locals are push work; adoptions and refused adoptions are pull
/// work; field and comment changes carry a per-entry [`Direction`] and split
/// along it. Dangling refs, out-of-scope beads, unmirrored relations and the
/// standing notes belong to neither direction and are shown under both,
/// because each is a standing fact about the pairing rather than pending
/// work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanScope {
    /// Everything, in both directions. What `br remote status` prints.
    Both,
    /// Only what `br remote push` would do.
    Push,
    /// Only what `br remote pull` would do.
    Pull,
}

impl PlanScope {
    /// Whether a change resolving this way belongs in this rendering.
    const fn covers(self, direction: Direction) -> bool {
        matches!(
            (self, direction),
            (Self::Both, _) | (Self::Push, Direction::Push) | (Self::Pull, Direction::Pull)
        )
    }

    /// Whether push-only sections belong in this rendering.
    const fn shows_push(self) -> bool {
        matches!(self, Self::Both | Self::Push)
    }

    /// Whether pull-only sections belong in this rendering.
    const fn shows_pull(self) -> bool {
        matches!(self, Self::Both | Self::Pull)
    }
}

/// Everything a run would do.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReconcilePlan {
    pub field_changes: Vec<IssueFieldPlan>,
    pub link_changes: Vec<IssueLinkPlan>,
    /// Live beads with no `external_ref` — issues to create.
    pub creates: Vec<String>,
    pub adoptions: Vec<Adoption>,
    pub refused_adoptions: Vec<RefusedAdoption>,
    pub dangling: Vec<DanglingRef>,
    pub out_of_scope: Vec<String>,
    pub comment_changes: Vec<CommentPlan>,
    pub unmapped_local: Vec<UnmappedLocal>,
    /// Paired beads that have been deleted locally. Named, never diffed; see
    /// [`crate::remote::tombstone::plan_tombstones`] for what to do about
    /// each one — join its `Delete` entries against this field by `bead_id`
    /// to find the `remote_id` to act on.
    pub tombstoned: Vec<TombstonedPair>,
    /// Dependency rows with no YouTrack equivalent. Informational, permanent.
    pub unmirrored_links: Vec<UnmirroredLink>,
    /// Standing caveats about what this plan does *not* cover. Rendered last
    /// and carried in `--json` so a consumer sees them too.
    pub notes: Vec<String>,
}

impl ReconcilePlan {
    /// Whether the two sides already agree and nothing at all is pending.
    ///
    /// `out_of_scope` and `unmirrored_links` are excluded on purpose: both are
    /// permanent, correct states rather than pending work, and counting them
    /// would mean a workspace with one `waits-for` row could never report
    /// itself reconciled. `tombstoned` *is* counted — a deleted bead whose
    /// mirror is still live is real outstanding work, even though nothing
    /// executes it yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.is_empty_for(PlanScope::Both)
    }

    /// [`ReconcilePlan::is_empty`], restricted to one direction's work.
    ///
    /// This is what decides whether a scoped rendering ends with "nothing to
    /// do", so it must count exactly the sections that rendering shows —
    /// otherwise a `pull` with only push work pending would print no sections
    /// and no closing line either, which reads as output that got truncated.
    #[must_use]
    pub fn is_empty_for(&self, scope: PlanScope) -> bool {
        let push_work = scope.shows_push()
            && !(self.creates.is_empty()
                && self.link_changes.is_empty()
                && self.tombstoned.is_empty()
                && self.unmapped_local.is_empty());
        let pull_work =
            scope.shows_pull() && !(self.adoptions.is_empty() && self.refused_adoptions.is_empty());
        let directed = self
            .field_changes
            .iter()
            .flat_map(|issue| &issue.changes)
            .any(|change| scope.covers(change.direction))
            || self
                .comment_changes
                .iter()
                .any(|entry| scope.covers(entry.direction));
        !(push_work || pull_work || directed || !self.dangling.is_empty())
    }

    /// The human rendering, one section per kind of work.
    ///
    /// Every YouTrack-wins case is marked explicitly: it is the only place
    /// where a remote edit overwrites a local value, so it is never allowed
    /// to be silent.
    #[must_use]
    pub fn render(&self) -> String {
        self.render_scoped(PlanScope::Both)
    }

    /// [`ReconcilePlan::render`], restricted to one direction's work.
    ///
    /// See [`PlanScope`] for which section belongs to which direction.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn render_scoped(&self, scope: PlanScope) -> String {
        let mut out = String::new();

        if scope.shows_push() && !self.creates.is_empty() {
            let _ = writeln!(out, "create {} issue(s):", self.creates.len());
            for bead_id in &self.creates {
                let _ = writeln!(out, "  {bead_id}");
            }
        }

        // Filtered before the header is written: the count names the issues
        // this rendering is about to list, so computing it from the unfiltered
        // vector would announce a number the lines below do not add up to.
        let field_changes: Vec<(&IssueFieldPlan, Vec<&FieldChange>)> = self
            .field_changes
            .iter()
            .filter_map(|issue| {
                let changes: Vec<&FieldChange> = issue
                    .changes
                    .iter()
                    .filter(|change| scope.covers(change.direction))
                    .collect();
                (!changes.is_empty()).then_some((issue, changes))
            })
            .collect();
        if !field_changes.is_empty() {
            let _ = writeln!(out, "update {} issue(s):", field_changes.len());
            for (issue, changes) in field_changes {
                let _ = writeln!(out, "  {} → {}", issue.bead_id, issue.remote_id);
                for change in changes {
                    let marker = match change.direction {
                        Direction::Push => "push",
                        Direction::Pull => "YouTrack wins",
                    };
                    let _ = writeln!(
                        out,
                        "    {}: {} → {} [{marker}]",
                        change.field.as_str(),
                        one_line(&change.local),
                        one_line(&change.remote),
                    );
                }
            }
        }

        if scope.shows_push() && !self.link_changes.is_empty() {
            let _ = writeln!(out, "link changes:");
            for issue in &self.link_changes {
                let _ = writeln!(out, "  {} → {}", issue.bead_id, issue.remote_id);
                for change in &issue.changes {
                    let _ = writeln!(out, "    {}", render_link_change(change));
                }
            }
        }

        let comment_changes: Vec<&CommentPlan> = self
            .comment_changes
            .iter()
            .filter(|entry| scope.covers(entry.direction))
            .collect();
        if !comment_changes.is_empty() {
            let _ = writeln!(out, "comments:");
            for entry in comment_changes {
                let marker = match entry.direction {
                    Direction::Push => "push",
                    Direction::Pull => "YouTrack wins",
                };
                let _ = writeln!(
                    out,
                    "  {} → {}: {} comment(s) [{marker}]",
                    entry.bead_id, entry.remote_id, entry.count
                );
            }
        }

        if scope.shows_pull() && !self.adoptions.is_empty() {
            let _ = writeln!(
                out,
                "adoption candidates ({} issue(s) no bead claims):",
                self.adoptions.len()
            );
            for adoption in &self.adoptions {
                let _ = writeln!(
                    out,
                    "  {}  {}",
                    adoption.remote_id,
                    one_line(&adoption.summary)
                );
            }
        }

        if scope.shows_pull() && !self.refused_adoptions.is_empty() {
            let _ = writeln!(
                out,
                "refused adoptions (br cannot read these issues with this remote.yaml):"
            );
            for refusal in &self.refused_adoptions {
                let _ = writeln!(out, "  {}  {}", refusal.remote_id, refusal.reason);
            }
        }

        if scope.shows_push() && !self.tombstoned.is_empty() {
            let _ = writeln!(
                out,
                "deleted beads still mirrored (the tombstone rule owns these; no field change is planned):"
            );
            for entry in &self.tombstoned {
                let _ = writeln!(out, "  {} → {}", entry.bead_id, entry.remote_id);
            }
        }

        if !self.dangling.is_empty() {
            let _ = writeln!(
                out,
                "dangling local refs (the issue named is gone; nothing was re-created):"
            );
            for entry in &self.dangling {
                let _ = writeln!(out, "  {} → {}", entry.bead_id, entry.external_ref);
            }
        }

        if scope.shows_push() && !self.unmapped_local.is_empty() {
            let _ = writeln!(
                out,
                "unmapped local values (a push would refuse until remote.yaml covers them):"
            );
            for entry in &self.unmapped_local {
                let _ = writeln!(
                    out,
                    "  {}: {} '{}' — add it to {}",
                    entry.bead_id, entry.field, entry.value, entry.map_key
                );
            }
        }

        if !self.out_of_scope.is_empty() {
            let _ = writeln!(
                out,
                "out of scope ({} bead(s) whose external_ref names another tracker; untouched):",
                self.out_of_scope.len()
            );
            for bead_id in &self.out_of_scope {
                let _ = writeln!(out, "  {bead_id}");
            }
        }

        if !self.unmirrored_links.is_empty() {
            let _ = writeln!(
                out,
                "unmirrored relations ({} row(s) whose dependency type has no YouTrack link type; \
                 never mirrored):",
                self.unmirrored_links.len()
            );
            for entry in &self.unmirrored_links {
                let _ = writeln!(
                    out,
                    "  {} {} {}",
                    entry.bead_id, entry.dep_type, entry.target_id
                );
            }
        }

        if self.is_empty_for(scope) {
            let _ = writeln!(
                out,
                "the mirror already matches this workspace on everything br reconciles today; \
                 nothing to do."
            );
        }

        for note in &self.notes {
            let _ = writeln!(out, "{note}");
        }

        out
    }
}

fn render_link_change(change: &LinkChange) -> String {
    use crate::remote::link_diff::kind_name;
    match change {
        LinkChange::Add {
            kind,
            target_readable,
        } => format!("+ {} {target_readable}", kind_name(*kind)),
        LinkChange::Remove {
            kind,
            target_readable,
            ..
        } => format!("- {} {target_readable}", kind_name(*kind)),
    }
}

/// Prose fields are multi-line; the plan is a list, so each value is shown as
/// one line with a length cap.
fn one_line(value: &str) -> String {
    const LIMIT: usize = 60;
    let flattened: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = flattened.trim();
    if trimmed.is_empty() {
        return "(none)".to_string();
    }
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(LIMIT).collect();
    format!("{head}…")
}

/// Compute the whole plan from the workspace and one fetched snapshot.
///
/// `snapshot` is the non-fatal fetch: issues br could read, plus the ones it
/// could not. The unreadable ones become `refused_adoptions` rather than
/// aborting, because `br remote status` is the tool a user reaches for
/// precisely when the mirror holds a value their config does not cover.
///
/// `types` reaches the link differ, which selects a remote link by the same
/// `{linkID}` an executor would write through.
///
/// `comments` is the comment reconciliation the caller already performed — see
/// [`CommentWork`]. Passing an empty slice is legitimate and means exactly
/// what it says: no comment work, for a caller that did not do any.
#[must_use]
pub fn build_plan(
    cfg: &RemoteConfig,
    beads: &[Issue],
    snapshot: RemoteSnapshot,
    types: &LinkTypes,
    comments: &[CommentWork],
) -> ReconcilePlan {
    let by_id: HashMap<&str, &Issue> = beads.iter().map(|b| (b.id.as_str(), b)).collect();
    let (links, unmirrored_links) = bead_links_index(beads);
    let RemoteSnapshot { issues, unmappable } = snapshot;
    let unreadable: HashSet<String> = unmappable
        .iter()
        .map(|issue| issue.id_readable.clone())
        .collect();
    let pairing = pair_workspace(cfg, beads, issues);

    let readable: HashMap<&str, String> = pairing
        .paired
        .iter()
        .map(|pair| (pair.bead_id.as_str(), pair.remote.id_readable.clone()))
        .collect();
    let resolve = |bead_id: &str| readable.get(bead_id).cloned();

    let mut plan = ReconcilePlan {
        creates: pairing.unpaired_local.clone(),
        out_of_scope: pairing.out_of_scope.clone(),
        unmirrored_links,
        comment_changes: comment_sections(comments),
        ..ReconcilePlan::default()
    };
    if plan.comment_changes.is_empty() {
        plan.notes.push(COMMENT_COUNT_GATE.to_string());
    }

    for pair in &pairing.paired {
        let Some(bead) = by_id.get(pair.bead_id.as_str()) else {
            continue;
        };
        if bead.status == Status::Tombstone {
            // `tombstone` has no status_map entry by design, so diffing this
            // pair would plan `state: tombstone → open` and a push would then
            // refuse on it. See `TombstonedPair`.
            plan.tombstoned.push(TombstonedPair {
                bead_id: pair.bead_id.clone(),
                remote_id: pair.remote.id_readable.clone(),
            });
            continue;
        }
        let changes = diff_pair(cfg, bead, &pair.remote);
        if !changes.is_empty() {
            plan.field_changes.push(IssueFieldPlan {
                bead_id: pair.bead_id.clone(),
                remote_id: pair.remote.id_readable.clone(),
                changes,
            });
        }
        let empty = BeadLinks::default();
        let bead_links = links.get(pair.bead_id.as_str()).unwrap_or(&empty);
        let link_changes = diff_links(bead_links, &pair.remote, types, &resolve);
        if !link_changes.is_empty() {
            plan.link_changes.push(IssueLinkPlan {
                bead_id: pair.bead_id.clone(),
                remote_id: pair.remote.id_readable.clone(),
                changes: link_changes,
            });
        }
    }

    plan.adoptions = pairing
        .unpaired_remote
        .iter()
        .map(|issue| Adoption {
            remote_id: issue.id_readable.clone(),
            summary: issue.summary.clone(),
            beads_id: issue.fields.beads_id.clone(),
        })
        .collect();

    plan.refused_adoptions = unmappable
        .into_iter()
        .map(|issue| RefusedAdoption {
            remote_id: issue.id_readable,
            reason: issue.reason,
        })
        .collect();

    plan.dangling = pairing
        .dangling_local
        .iter()
        .filter_map(|bead_id| {
            let external_ref = by_id
                .get(bead_id.as_str())
                .and_then(|bead| bead.external_ref.clone())
                .unwrap_or_default();
            // An issue br could not read is absent from the pairing input, so
            // a bead pointing at it lands in `dangling_local`. It is not gone
            // — it is unreadable, and the refusal above already says so.
            if unreadable.contains(&external_ref) {
                return None;
            }
            Some(DanglingRef {
                bead_id: bead_id.clone(),
                external_ref,
            })
        })
        .collect();

    // Scoped to beads a push would actually touch: an out-of-scope bead
    // belongs to another tracker and is never written here, so an unmapped
    // value on one is not a problem this config has.
    let ignored: HashSet<&str> = pairing.out_of_scope.iter().map(String::as_str).collect();
    plan.unmapped_local = unmapped_local(cfg, beads, &ignored);
    plan
}

/// Reduce the caller's comment reconciliation to the plan's printable form.
///
/// One entry per direction per issue, and none at all for an issue with
/// nothing to move — a `0 comment(s) [push]` line is noise, and it would also
/// make `is_empty` report outstanding work where there is none.
fn comment_sections(comments: &[CommentWork]) -> Vec<CommentPlan> {
    let mut sections = Vec::new();
    for work in comments {
        for (direction, count) in [
            (Direction::Push, work.to_push.len()),
            (Direction::Pull, work.to_pull.len()),
        ] {
            if count > 0 {
                sections.push(CommentPlan {
                    bead_id: work.bead_id.clone(),
                    remote_id: work.remote_id.clone(),
                    direction,
                    count,
                });
            }
        }
    }
    sections
}

/// Local values no map covers. A push would refuse on each of these, so
/// `status` names them rather than letting a first run discover them partway
/// through.
///
/// `ignored` holds bead ids no verb will ever write — out-of-scope beads,
/// whose `external_ref` belongs to a different tracker. Reporting one of
/// those would be a pure false positive: nothing here is going to push it.
fn unmapped_local(
    cfg: &RemoteConfig,
    beads: &[Issue],
    ignored: &HashSet<&str>,
) -> Vec<UnmappedLocal> {
    let mut out = Vec::new();
    for bead in beads {
        if ignored.contains(bead.id.as_str()) {
            continue;
        }
        if bead.status == Status::Tombstone {
            // The tombstone rule owns tombstones, and `tombstone` is
            // deliberately absent from status_map.
            continue;
        }
        let issue_type = bead.issue_type.as_str();
        if !cfg.type_map.contains_key(issue_type) {
            out.push(UnmappedLocal {
                bead_id: bead.id.clone(),
                field: "issue_type".to_string(),
                value: issue_type.to_string(),
                map_key: "type_map".to_string(),
            });
        }
        let status = bead.status.as_str();
        if !cfg.status_map.contains_key(status) {
            out.push(UnmappedLocal {
                bead_id: bead.id.clone(),
                field: "status".to_string(),
                value: status.to_string(),
                map_key: "status_map".to_string(),
            });
        }
    }
    out
}

/// Turn dependency rows into per-bead relations.
///
/// Two of the three are asymmetric on purpose. A parent link is owned by the
/// child and a `Depend` link by the blocker, so exactly one end of each emits
/// it. `Relates` is undirected in YouTrack and shows up on both ends as
/// `BOTH`, so the local set is built symmetrically — otherwise the end that
/// does not hold the row would see an unexplained remote link and try to
/// remove what the other end just added.
/// Rows whose type has no YouTrack equivalent are returned alongside, not
/// dropped: a user who sees `waits-for` in `br dep list` and no link change in
/// `br remote status` would otherwise have no way to learn why.
fn bead_links_index(beads: &[Issue]) -> (HashMap<String, BeadLinks>, Vec<UnmirroredLink>) {
    let mut index: HashMap<String, BeadLinks> = HashMap::with_capacity(beads.len());
    let mut unmirrored = Vec::new();
    for bead in beads {
        index.entry(bead.id.clone()).or_default();
    }
    for bead in beads {
        for dep in &bead.dependencies {
            match dep.dep_type {
                DependencyType::ParentChild => {
                    index.entry(dep.issue_id.clone()).or_default().parent =
                        Some(dep.depends_on_id.clone());
                }
                // `A depends_on B` means B blocks A, and the blocker owns the
                // link: `is required for` runs from B to A.
                DependencyType::Blocks => {
                    index
                        .entry(dep.depends_on_id.clone())
                        .or_default()
                        .blocks
                        .push(dep.issue_id.clone());
                }
                DependencyType::Related | DependencyType::RelatesTo => {
                    index
                        .entry(dep.issue_id.clone())
                        .or_default()
                        .related
                        .push(dep.depends_on_id.clone());
                    index
                        .entry(dep.depends_on_id.clone())
                        .or_default()
                        .related
                        .push(dep.issue_id.clone());
                }
                ref other => unmirrored.push(UnmirroredLink {
                    bead_id: dep.issue_id.clone(),
                    dep_type: other.to_string(),
                    target_id: dep.depends_on_id.clone(),
                }),
            }
        }
    }
    for links in index.values_mut() {
        links.blocks.sort_unstable();
        links.blocks.dedup();
        links.related.sort_unstable();
        links.related.dedup();
    }
    unmirrored.sort_by(|a, b| (&a.bead_id, &a.target_id).cmp(&(&b.bead_id, &b.target_id)));
    (index, unmirrored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Dependency;
    use crate::remote::model::{RemoteIssue, UnmappableIssue};
    use chrono::{TimeZone, Utc};

    fn config() -> RemoteConfig {
        RemoteConfig::from_yaml_str(include_str!("../../tests/fixtures/remote_em.yaml"))
            .expect("valid config")
    }

    fn at(millis: i64) -> chrono::DateTime<Utc> {
        Utc.timestamp_millis_opt(millis)
            .single()
            .expect("valid instant")
    }

    fn bead(id: &str, external_ref: Option<&str>) -> Issue {
        Issue {
            id: id.to_string(),
            title: "t".to_string(),
            external_ref: external_ref.map(str::to_string),
            updated_at: at(1_000),
            ..Issue::default()
        }
    }

    fn dependency(issue_id: &str, depends_on: &str, dep_type: DependencyType) -> Dependency {
        Dependency {
            issue_id: issue_id.to_string(),
            depends_on_id: depends_on.to_string(),
            dep_type,
            created_at: at(0),
            created_by: None,
            metadata: None,
            thread_id: None,
        }
    }

    fn mirror(id_readable: &str, summary: &str) -> RemoteIssue {
        let mut issue = RemoteIssue::for_test(id_readable);
        issue.summary = summary.to_string();
        issue.fields.title = summary.to_string();
        issue
    }

    fn snapshot(issues: Vec<RemoteIssue>) -> RemoteSnapshot {
        RemoteSnapshot {
            issues,
            unmappable: Vec::new(),
        }
    }

    fn types() -> LinkTypes {
        LinkTypes {
            subtask: "173-3".into(),
            depend: "173-1".into(),
            relates: "173-0".into(),
        }
    }

    #[test]
    fn a_reconciled_workspace_plans_nothing_and_says_so() {
        let beads = vec![bead("bds-1", Some("EM-1"))];
        let plan = build_plan(
            &config(),
            &beads,
            snapshot(vec![mirror("EM-1", "t")]),
            &types(),
            &[],
        );
        assert!(plan.is_empty(), "{plan:?}");
        assert!(plan.render().contains("nothing to do"), "{}", plan.render());
    }

    #[test]
    fn the_render_names_every_section_it_has_work_for() {
        let mut plan = ReconcilePlan {
            creates: vec!["bds-9".into()],
            adoptions: vec![Adoption {
                remote_id: "EM-7".into(),
                summary: "unclaimed".into(),
                beads_id: None,
            }],
            refused_adoptions: vec![RefusedAdoption {
                remote_id: "EM-8".into(),
                reason: "EM-8: issue Type 'User Story' has no beads mapping; \
                         add an entry to type_map in remote.yaml"
                    .into(),
            }],
            dangling: vec![DanglingRef {
                bead_id: "bds-3".into(),
                external_ref: "EM-99".into(),
            }],
            out_of_scope: vec!["bds-4".into()],
            tombstoned: vec![TombstonedPair {
                bead_id: "bds-6".into(),
                remote_id: "EM-6".into(),
            }],
            unmirrored_links: vec![UnmirroredLink {
                bead_id: "bds-7".into(),
                dep_type: "waits-for".into(),
                target_id: "bds-8".into(),
            }],
            ..ReconcilePlan::default()
        };
        plan.unmapped_local.push(UnmappedLocal {
            bead_id: "bds-5".into(),
            field: "issue_type".into(),
            value: "spike".into(),
            map_key: "type_map".into(),
        });

        let text = plan.render();
        for expected in [
            "create 1 issue(s)",
            "bds-9",
            "adoption candidates",
            "EM-7",
            "refused adoptions",
            "Type 'User Story'",
            "dangling local refs",
            "EM-99",
            "out of scope",
            "bds-4",
            "unmapped local values",
            "spike",
            "deleted beads still mirrored",
            "EM-6",
            "unmirrored relations",
            "waits-for",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        assert!(!text.contains("nothing to do"));
    }

    #[test]
    fn a_youtrack_wins_field_is_marked_explicitly() {
        let mut bead = bead("bds-1", Some("EM-1"));
        bead.updated_at = at(1_000);
        let mut remote = mirror("EM-1", "t");
        remote.updated = at(2_000);
        remote.fields.status = Status::Closed;

        let plan = build_plan(&config(), &[bead], snapshot(vec![remote]), &types(), &[]);
        let text = plan.render();
        assert!(
            text.contains("YouTrack wins"),
            "a pull must never be silent:\n{text}"
        );
    }

    #[test]
    fn a_parent_row_becomes_a_subtask_addition_on_the_child() {
        let mut child = bead("bds-2", Some("EM-2"));
        child
            .dependencies
            .push(dependency("bds-2", "bds-1", DependencyType::ParentChild));
        let parent = bead("bds-1", Some("EM-1"));

        let plan = build_plan(
            &config(),
            &[parent, child],
            snapshot(vec![mirror("EM-1", "t"), mirror("EM-2", "t")]),
            &types(),
            &[],
        );

        assert_eq!(plan.link_changes.len(), 1, "{:?}", plan.link_changes);
        assert_eq!(plan.link_changes[0].bead_id, "bds-2");
        assert!(plan.render().contains("+ parent EM-1"), "{}", plan.render());
    }

    #[test]
    fn a_related_row_is_mirrored_from_both_ends() {
        let mut left = bead("bds-1", Some("EM-1"));
        left.dependencies
            .push(dependency("bds-1", "bds-2", DependencyType::Related));
        let right = bead("bds-2", Some("EM-2"));

        let plan = build_plan(
            &config(),
            &[left, right],
            snapshot(vec![mirror("EM-1", "t"), mirror("EM-2", "t")]),
            &types(),
            &[],
        );

        assert_eq!(
            plan.link_changes.len(),
            2,
            "an undirected link is present on both ends, so both must want it: {:?}",
            plan.link_changes
        );
    }

    #[test]
    fn an_unmapped_local_type_is_reported_rather_than_discovered_mid_push() {
        let mut spiky = bead("bds-1", Some("EM-1"));
        spiky.issue_type = crate::model::IssueType::Custom("spike".into());
        let plan = build_plan(
            &config(),
            &[spiky],
            snapshot(vec![mirror("EM-1", "t")]),
            &types(),
            &[],
        );
        assert_eq!(plan.unmapped_local.len(), 1);
        assert_eq!(plan.unmapped_local[0].value, "spike");
    }

    #[test]
    fn an_out_of_scope_bead_is_not_reported_as_an_unmapped_local_value() {
        // Nothing here will ever push it, so naming its type is a pure false
        // positive.
        let mut spiky = bead("bds-1", Some("JIRA-9"));
        spiky.issue_type = crate::model::IssueType::Custom("spike".into());
        let plan = build_plan(&config(), &[spiky], snapshot(vec![]), &types(), &[]);
        assert_eq!(plan.out_of_scope, ["bds-1"]);
        assert!(plan.unmapped_local.is_empty(), "{:?}", plan.unmapped_local);
    }

    #[test]
    fn an_unreadable_remote_issue_is_refused_and_printed_not_fatal() {
        let beads = vec![bead("bds-1", Some("EM-1"))];
        let snapshot = RemoteSnapshot {
            issues: vec![],
            unmappable: vec![UnmappableIssue {
                id_readable: "EM-1".into(),
                reason: "EM-1: issue State 'In Review' has no beads mapping; \
                         add an entry to status_map in remote.yaml"
                    .into(),
            }],
        };
        let plan = build_plan(&config(), &beads, snapshot, &types(), &[]);

        assert_eq!(plan.refused_adoptions.len(), 1);
        assert!(
            plan.dangling.is_empty(),
            "the issue is unreadable, not gone; saying 'gone' would be false: {:?}",
            plan.dangling
        );
        let text = plan.render();
        assert!(text.contains("refused adoptions"), "{text}");
        assert!(text.contains("In Review"), "{text}");
    }

    #[test]
    fn a_tombstoned_pair_is_named_and_never_diffed() {
        let mut deleted = bead("bds-1", Some("EM-1"));
        deleted.status = Status::Tombstone;
        deleted.title = "renamed since".into();

        let plan = build_plan(
            &config(),
            &[deleted],
            snapshot(vec![mirror("EM-1", "original")]),
            &types(),
            &[],
        );

        assert!(
            plan.field_changes.is_empty(),
            "a tombstone has no status_map entry; planning a push would plan a failure: {:?}",
            plan.field_changes
        );
        assert_eq!(plan.tombstoned.len(), 1);
        assert_eq!(plan.tombstoned[0].remote_id, "EM-1");
        assert!(!plan.is_empty(), "it is still outstanding work");
        assert!(
            plan.render().contains("deleted beads still mirrored"),
            "{}",
            plan.render()
        );
    }

    #[test]
    fn a_dependency_type_with_no_link_type_is_named_rather_than_dropped() {
        let mut waiting = bead("bds-1", Some("EM-1"));
        waiting
            .dependencies
            .push(dependency("bds-1", "bds-2", DependencyType::WaitsFor));

        let plan = build_plan(
            &config(),
            &[waiting, bead("bds-2", Some("EM-2"))],
            snapshot(vec![mirror("EM-1", "t"), mirror("EM-2", "t")]),
            &types(),
            &[],
        );

        assert_eq!(plan.unmirrored_links.len(), 1);
        assert_eq!(plan.unmirrored_links[0].dep_type, "waits-for");
        assert_eq!(plan.unmirrored_links[0].target_id, "bds-2");
        assert!(
            plan.is_empty(),
            "a permanently unmirrored relation is not pending work"
        );
        let text = plan.render();
        assert!(text.contains("unmirrored relations"), "{text}");
        assert!(
            text.contains("nothing to do"),
            "it must still read as reconciled: {text}"
        );
    }

    #[test]
    fn a_plan_with_comment_work_never_claims_comments_are_unreconciled() {
        // The inverse of the note this replaced. That note said, on every
        // plan, that comments were not reconciled at all; leaving it in place
        // once they are would be a lie the suite actively protected. A plan
        // that carries comment changes must say nothing of the kind.
        let comments = vec![CommentWork {
            bead_id: "bds-1".into(),
            remote_id: "EM-1".into(),
            to_push: vec!["[br]\nfrom beads".into()],
            to_pull: vec![crate::remote::comments::RemoteComment::for_test(
                "7-2",
                "typed in the web UI",
                "kate",
            )],
        }];
        let beads = vec![bead("bds-1", Some("EM-1"))];
        let plan = build_plan(
            &config(),
            &beads,
            snapshot(vec![mirror("EM-1", "t")]),
            &types(),
            &comments,
        );

        assert!(!plan.is_empty(), "comment work is outstanding work");
        assert_eq!(plan.comment_changes.len(), 2, "{:?}", plan.comment_changes);
        let text = plan.render();
        assert!(
            !text.contains("not reconciled"),
            "the retired caveat must not survive alongside real comment work: {text}"
        );
        assert!(text.contains("1 comment(s) [push]"), "{text}");
        assert!(text.contains("1 comment(s) [YouTrack wins]"), "{text}");
    }

    #[test]
    fn a_plan_with_no_comment_work_still_names_the_count_gate() {
        // "nothing to do" is only true up to the gate: two sides whose comment
        // counts agree are never inspected, so an in-place edit is invisible.
        let plan = build_plan(&config(), &[], snapshot(vec![]), &types(), &[]);
        assert!(plan.is_empty());
        let text = plan.render();
        assert!(
            text.contains("comment counts"),
            "the caveat that survives must reach the rendering: {text}"
        );
        assert!(text.contains("everything br reconciles today"), "{text}");
    }

    #[test]
    fn an_issue_with_nothing_to_move_produces_no_comment_section() {
        // A `0 comment(s)` line is noise, and it would also make `is_empty`
        // report outstanding work where there is none.
        let comments = vec![CommentWork {
            bead_id: "bds-1".into(),
            remote_id: "EM-1".into(),
            to_push: Vec::new(),
            to_pull: Vec::new(),
        }];
        let beads = vec![bead("bds-1", Some("EM-1"))];
        let plan = build_plan(
            &config(),
            &beads,
            snapshot(vec![mirror("EM-1", "t")]),
            &types(),
            &comments,
        );
        assert!(plan.comment_changes.is_empty());
        assert!(plan.is_empty());
    }

    /// A plan carrying work in both directions at once: a create only a push
    /// performs, an adoption candidate only a pull performs, and one paired
    /// issue whose two field changes resolve opposite ways.
    fn two_directional_plan() -> ReconcilePlan {
        ReconcilePlan {
            creates: vec!["bds-new".into()],
            adoptions: vec![Adoption {
                remote_id: "EM-9".into(),
                summary: "Unclaimed".into(),
                beads_id: None,
            }],
            field_changes: vec![IssueFieldPlan {
                bead_id: "bds-1".into(),
                remote_id: "EM-1".into(),
                changes: vec![
                    FieldChange {
                        field: crate::remote::diff::Field::Title,
                        direction: Direction::Push,
                        local: "local title".into(),
                        remote: "remote title".into(),
                    },
                    FieldChange {
                        field: crate::remote::diff::Field::State,
                        direction: Direction::Pull,
                        local: "open".into(),
                        remote: "in_progress".into(),
                    },
                ],
            }],
            ..ReconcilePlan::default()
        }
    }

    #[test]
    fn a_push_rendering_omits_the_work_only_a_pull_performs() {
        let text = two_directional_plan().render_scoped(PlanScope::Push);
        assert!(text.contains("create 1 issue(s)"), "{text}");
        assert!(
            text.contains("title: local title → remote title [push]"),
            "{text}"
        );
        assert!(
            !text.contains("adoption candidates"),
            "a push adopts nothing: {text}"
        );
        assert!(
            !text.contains("YouTrack wins"),
            "a push never resolves a field the remote's way: {text}"
        );
    }

    #[test]
    fn a_pull_rendering_omits_the_work_only_a_push_performs() {
        let text = two_directional_plan().render_scoped(PlanScope::Pull);
        assert!(text.contains("adoption candidates"), "{text}");
        assert!(
            text.contains("state: open → in_progress [YouTrack wins]"),
            "{text}"
        );
        assert!(
            !text.contains("create 1 issue(s)"),
            "a pull creates no remote issue: {text}"
        );
        assert!(
            !text.contains("[push]"),
            "a pull writes nothing to the remote: {text}"
        );
    }

    #[test]
    fn a_scoped_rendering_counts_only_the_issues_it_goes_on_to_list() {
        // The header is written from the filtered set, not the whole vector.
        // Announcing "update 2 issue(s)" and then listing one is the failure
        // this pins: a reader cannot tell a filtered rendering from a
        // truncated one.
        let mut plan = two_directional_plan();
        plan.field_changes.push(IssueFieldPlan {
            bead_id: "bds-2".into(),
            remote_id: "EM-2".into(),
            changes: vec![FieldChange {
                field: crate::remote::diff::Field::State,
                direction: Direction::Pull,
                local: "open".into(),
                remote: "closed".into(),
            }],
        });
        let text = plan.render_scoped(PlanScope::Push);
        assert!(text.contains("update 1 issue(s)"), "{text}");
        assert!(!text.contains("bds-2"), "{text}");
        assert!(
            plan.render_scoped(PlanScope::Pull)
                .contains("update 2 issue(s)"),
            "both pull-direction issues belong to the pull half"
        );
    }

    #[test]
    fn a_direction_with_no_work_still_closes_with_nothing_to_do() {
        // Push-only work must not leave `br remote pull` printing no sections
        // and no closing line either, which reads as truncated output.
        let plan = ReconcilePlan {
            creates: vec!["bds-new".into()],
            ..ReconcilePlan::default()
        };
        assert!(!plan.is_empty(), "the plan as a whole has work");
        assert!(plan.is_empty_for(PlanScope::Pull));
        assert!(!plan.is_empty_for(PlanScope::Push));
        assert!(
            plan.render_scoped(PlanScope::Pull)
                .contains("nothing to do"),
            "{}",
            plan.render_scoped(PlanScope::Pull)
        );
    }

    #[test]
    fn the_unscoped_rendering_is_unchanged_and_still_shows_both_halves() {
        // `br remote status` reports on the whole plan and must keep doing so.
        let plan = two_directional_plan();
        assert_eq!(plan.render(), plan.render_scoped(PlanScope::Both));
        let text = plan.render();
        assert!(text.contains("create 1 issue(s)"), "{text}");
        assert!(text.contains("adoption candidates"), "{text}");
        assert!(
            text.contains("[push]") && text.contains("YouTrack wins"),
            "{text}"
        );
    }
}
