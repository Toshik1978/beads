//! Inbound adoption: an issue a human created in the web UI becomes a bead.
//!
//! This is the only remote→local direction in `br remote` beyond the three
//! returnable fields. An issue no bead claims is imported **whole, once**, and
//! is thereafter an ordinary mirrored bead like any other.
//!
//! **Refusing beats defaulting.** beads is authoritative from the moment of
//! pairing, so a classification silently defaulted at adoption is a
//! classification *destroyed one push later*. A `Type` or `State` with no
//! beads preimage therefore refuses that one issue by name — see
//! [`classify_adoption`] — rather than importing it as `task`/`open`. The
//! refusal costs a line of output and a map edit and loses nothing: the issue
//! is still sitting in YouTrack, and the next run offers it again.
//!
//! **One refusal does not stop the run.** Each candidate is classified
//! independently; a refusal removes that issue from the adoption set and
//! every other adoption in the same run still proceeds.
//!
//! **Order exists to avoid a rename.** beads mints hierarchical ids from
//! parentage. Creating an adoptee flat and reparenting it afterwards *is* a
//! rename — a tombstone, `former_ids` churn and a forwarding pointer — for
//! every subtask anyone ever types into the web UI. [`topological_order`]
//! emits parents before children so [`adopt_one`] can mint the child id
//! directly and leave no tombstone at all.
//!
//! **Adoption is a local-only transaction.** Nothing in this module opens a
//! socket. The bead is created with `external_ref` already set, in the single
//! `create_issue` transaction that also writes its parent link, its labels
//! and its prose, so an adoption either happened or it did not: a crash
//! leaves either no bead at all — the next run re-derives the same answer
//! from the same fetch and offers the issue again — or a fully paired bead,
//! which the next run pairs normally and never re-adopts. There is no state
//! in which an issue is adopted twice. Writing the bead id back onto the
//! remote issue's `Beads ID` field is a separate, ignorable courtesy; see
//! [`stamp_adoption`].

use crate::error::{BeadsError, Result};
use crate::model::{Dependency, DependencyType, Issue};
use crate::remote::comments::{RemoteComment, YOUTRACK_AUTHOR, is_br_echo};
use crate::remote::config::RemoteConfig;
use crate::remote::error::RemoteError;
use crate::remote::link_diff::{direction_tag, mirrored_direction};
use crate::remote::model::RemoteIssue;
use crate::remote::youtrack::links::{LinkKind, RemoteLink};
use crate::remote::youtrack::mapping::{RemoteIssueFields, reverse_fields_structured};
use crate::storage::SqliteStorage;
use crate::util::id::{IdConfig, IdGenerator, child_id};
use chrono::Utc;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;

/// An unpaired remote issue br can read, and the fields it resolved to.
///
/// `fields` is resolved here rather than taken from `remote.fields` because
/// adoption must not trust a resolution performed elsewhere — see the module
/// docs on why a defaulted classification is worse than a refused one.
#[derive(Debug, Clone)]
pub struct AdoptionCandidate {
    pub remote: RemoteIssue,
    pub fields: RemoteIssueFields,
}

/// An unpaired remote issue br cannot read, and everything needed to fix it.
///
/// Distinct from [`crate::remote::plan::RefusedAdoption`], which carries the
/// already-formatted `reason` the *fetch* produced for `br remote status`.
/// This one is the structured form the adoption path produces for itself; the
/// `From` impl below converts it for rendering alongside the others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedAdoption {
    pub id_readable: String,
    /// The YouTrack field name, e.g. `Type`.
    pub field: String,
    /// The offending value, e.g. `User Story`.
    pub value: String,
    /// The `remote.yaml` key that would cover it, e.g. `type_map`.
    pub config_key: String,
}

impl RefusedAdoption {
    /// One line naming the issue, the field, the value, and the remedy.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{}: {} '{}' has no beads mapping — add an entry to {} in remote.yaml and the next \
             run will offer this issue again",
            self.id_readable, self.field, self.value, self.config_key
        )
    }
}

impl From<RefusedAdoption> for crate::remote::plan::RefusedAdoption {
    fn from(refusal: RefusedAdoption) -> Self {
        Self {
            remote_id: refusal.id_readable.clone(),
            reason: refusal.render(),
        }
    }
}

/// Decide whether one unpaired remote issue can be adopted.
///
/// Every bundle value is re-resolved against `cfg` from the raw
/// `customFields` the fetch retained. A value that is *absent* resolves to the
/// beads default; a value that is *present but unmapped* refuses the
/// adoption, because beads is authoritative from the moment of pairing and
/// the next push would overwrite whatever default was invented here.
///
/// # Errors
/// Returns a [`RefusedAdoption`] naming the issue, the field, the offending
/// value and the config key that would cover it. Nothing else can fail: the
/// resolution is pure and issues no request.
pub fn classify_adoption(
    cfg: &RemoteConfig,
    issue: &RemoteIssue,
) -> std::result::Result<AdoptionCandidate, RefusedAdoption> {
    // `reverse_fields_structured` reads a whole issue body; the fetch keeps
    // only the part that can refuse, so the rest is rebuilt from the parsed
    // issue. Both halves come from the same response.
    let raw = json!({
        "summary": issue.summary,
        "description": issue.description,
        "customFields": issue.raw_custom_fields,
    });
    match reverse_fields_structured(cfg, &raw) {
        Ok(fields) => Ok(AdoptionCandidate {
            remote: issue.clone(),
            fields,
        }),
        Err(unmapped) => Err(RefusedAdoption {
            id_readable: issue.id_readable.clone(),
            field: unmapped.field,
            value: unmapped.value,
            config_key: unmapped.config_key,
        }),
    }
}

/// The remote id of this issue's parent, if it declares one.
///
/// A parent link appears on the *child* as the `Subtask` link written through
/// `LinkTypes::link_id(Subtask, mirrored_direction(Subtask))`, which YouTrack
/// reports as `INWARD`. That correspondence is stated once, in
/// [`crate::remote::link_diff`], and pinned by a test there; this reads it
/// from that single definition rather than spelling `"INWARD"` again.
fn remote_parent(links: &[RemoteLink]) -> Option<&str> {
    let tag = direction_tag(mirrored_direction(LinkKind::Subtask));
    links
        .iter()
        .filter(|link| link.kind == LinkKind::Subtask && link.direction == tag)
        .flat_map(|link| link.issues.iter())
        .map(|issue| issue.id_readable.as_str())
        .next()
}

/// Why an adoption candidate did not make it into this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferralReason {
    /// The parent is neither an existing bead nor a candidate in this run —
    /// most often because it was refused by [`classify_adoption`], but also
    /// when it lies outside the configured project.
    ParentNotAdoptable,
    /// The parent *is* a candidate, but was itself deferred. This is also the
    /// shape a parentage cycle takes: every member's parent is deferred, and
    /// no member is ever the one that breaks it.
    ParentDeferred,
}

/// One candidate this run will not adopt, and why.
///
/// Deferral is refusal's sibling, and this feature's rule for refusal is that
/// it is named and reported, never silently skipped. Without this the user who
/// filed EM-11 under an unreadable EM-99 sees nothing at all on stdout, run
/// after run, and has no way to learn that fixing EM-99's mapping would bring
/// two issues in rather than one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredAdoption {
    pub id_readable: String,
    /// The remote id of the parent that blocked it.
    pub parent_id: String,
    pub reason: DeferralReason,
}

impl DeferredAdoption {
    /// One line naming the issue, the parent that blocked it, and the remedy.
    #[must_use]
    pub fn render(&self) -> String {
        match self.reason {
            DeferralReason::ParentNotAdoptable => format!(
                "{}: deferred because its parent {} is neither mirrored nor adoptable in this \
                 run — make {} adoptable and the next run takes both",
                self.id_readable, self.parent_id, self.parent_id
            ),
            DeferralReason::ParentDeferred => format!(
                "{}: deferred because its parent {} was itself deferred",
                self.id_readable, self.parent_id
            ),
        }
    }
}

/// Candidates in creation order, and the ones this run will not touch.
#[derive(Debug, Clone, Default)]
pub struct AdoptionOrder {
    /// Ready to adopt, parents before children.
    pub ordered: Vec<AdoptionCandidate>,
    /// Left for a later run, each named and explained.
    pub deferred: Vec<DeferredAdoption>,
}

/// Order candidates so that every parent is created before its children, and
/// name the ones that cannot be adopted at all.
///
/// `paired` maps a remote `idReadable` onto the bead id already mirroring it.
/// A candidate is ready when it declares no parent, when its parent is
/// already a bead, or when its parent appears earlier in this ordering. Ready
/// candidates keep their input order, so a printed plan is stable run to run.
///
/// Whatever never becomes ready is **deferred**, not flattened. An adoptee
/// whose parent was refused would otherwise be created flat and reparented on
/// a later run, and a reparent is a rename: a tombstone and `former_ids`
/// churn, permanently, for an issue whose only fault was being typed in
/// before its parent could be read. Deferring costs one run — and it is
/// returned rather than dropped, so a caller can say so out loud.
///
/// A cycle — which YouTrack's subtask tree does not admit, but which a
/// hand-built input does — is the same case rather than a special one: no
/// member ever becomes ready, the wave loop stops making progress, and every
/// member is deferred as [`DeferralReason::ParentDeferred`]. It cannot spin
/// and it cannot panic.
#[must_use]
pub fn topological_order<S: BuildHasher>(
    candidates: Vec<AdoptionCandidate>,
    paired: &HashMap<String, String, S>,
) -> AdoptionOrder {
    // Not consulted by the readiness test below — `done` only ever holds ids
    // that were emitted, so it is already a subset of this. It exists solely
    // to tell the two deferral reasons apart once the waves stop.
    let adoptable: HashSet<String> = candidates
        .iter()
        .map(|c| c.remote.id_readable.clone())
        .collect();

    let mut pending = candidates;
    let mut ordered: Vec<AdoptionCandidate> = Vec::with_capacity(pending.len());
    let mut done: HashSet<String> = HashSet::new();
    while !pending.is_empty() {
        let mut waiting = Vec::with_capacity(pending.len());
        let mut progressed = false;
        for candidate in pending {
            let ready = match remote_parent(&candidate.remote.links) {
                None => true,
                Some(parent) => paired.contains_key(parent) || done.contains(parent),
            };
            if ready {
                done.insert(candidate.remote.id_readable.clone());
                ordered.push(candidate);
                progressed = true;
            } else {
                waiting.push(candidate);
            }
        }
        pending = waiting;
        if !progressed {
            break;
        }
    }

    let deferred = pending
        .iter()
        .map(|candidate| {
            // Unwrap-free: a candidate only reaches here by failing the
            // readiness test, which a parentless candidate cannot do.
            let parent = remote_parent(&candidate.remote.links).unwrap_or_default();
            DeferredAdoption {
                id_readable: candidate.remote.id_readable.clone(),
                parent_id: parent.to_string(),
                reason: if adoptable.contains(parent) {
                    DeferralReason::ParentDeferred
                } else {
                    DeferralReason::ParentNotAdoptable
                },
            }
        })
        .collect();

    AdoptionOrder { ordered, deferred }
}

/// The bead id an adoptee must be created *under*, if it has one.
///
/// This is the second argument [`adopt_one`] takes, and the reason
/// [`topological_order`] exists: by the time a candidate comes up in that
/// order its parent is either an older bead or one minted earlier in the same
/// run, so `id_map` can answer. A candidate whose parent is in neither is not
/// in the ordering at all.
///
/// `id_map` maps remote `idReadable` onto bead id and must hold both the
/// pre-existing pairings and the ids minted so far by this run.
#[must_use]
pub fn creation_parent<S: BuildHasher>(
    candidate: &AdoptionCandidate,
    id_map: &HashMap<String, String, S>,
) -> Option<String> {
    remote_parent(&candidate.remote.links).and_then(|parent| id_map.get(parent).cloned())
}

/// Everything [`adopt_one`] needs that is not the candidate itself.
///
/// Deliberately not a `RemoteConfig`: minting a bead id and naming an actor
/// are workspace facts, and nothing here is backend-shaped.
#[derive(Debug, Clone, Copy)]
pub struct AdoptContext<'a> {
    /// How a parentless adoptee's flat bead id is minted. A child's id is
    /// derived from its parent's instead and never touches this.
    pub id_config: &'a IdConfig,
    /// Recorded as the local write's actor and the bead's creator.
    pub actor: &'a str,
}

/// What one adoption produced.
///
/// `stamp_failed` is never a reason to retry or roll back: the bead is the
/// transaction and the stamp is a courtesy. See [`stamp_adoption`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptionOutcome {
    pub bead_id: String,
    pub stamp_failed: bool,
}

/// Create one bead from one adoption candidate, locally and atomically.
///
/// When `parent_bead_id` is `Some`, the bead is minted **as that parent's
/// child** — `bds-4r2.11`, not a flat id followed by a reparent. The parent
/// dependency, the labels, the prose and `external_ref` all land in the same
/// `create_issue` transaction, so a crash leaves either nothing or a fully
/// paired bead. Nothing is written to the remote.
///
/// # Errors
/// Returns whatever storage returns: an id collision, a parent that does not
/// exist, a validation failure, or a write failure. On any error no bead
/// exists and the next run offers the issue again.
pub fn adopt_one(
    storage: &mut SqliteStorage,
    cfg: &AdoptContext<'_>,
    candidate: &AdoptionCandidate,
    parent_bead_id: Option<&str>,
) -> Result<AdoptionOutcome> {
    let bead_id = mint_bead_id(storage, cfg, candidate, parent_bead_id)?;
    let now = Utc::now();

    let mut issue = Issue {
        id: bead_id.clone(),
        title: adopted_title(candidate),
        description: candidate.fields.description.clone(),
        design: candidate.fields.design.clone(),
        acceptance_criteria: candidate.fields.acceptance_criteria.clone(),
        notes: candidate.fields.notes.clone(),
        status: candidate.fields.status.clone(),
        priority: candidate.fields.priority,
        issue_type: candidate.fields.issue_type.clone(),
        created_at: now,
        created_by: Some(cfg.actor.to_string()),
        updated_at: now,
        close_reason: candidate.fields.close_reason.clone(),
        // The whole point: the bead is paired the instant it exists, so the
        // next run pairs it normally and never re-adopts it.
        external_ref: Some(candidate.remote.id_readable.clone()),
        labels: candidate.remote.labels.clone(),
        ..Issue::default()
    };
    if let Some(parent_bead_id) = parent_bead_id {
        issue.dependencies.push(Dependency {
            issue_id: bead_id.clone(),
            depends_on_id: parent_bead_id.to_string(),
            dep_type: DependencyType::ParentChild,
            created_at: now,
            created_by: Some(cfg.actor.to_string()),
            metadata: None,
            thread_id: None,
        });
    }

    storage.create_issue(&issue, cfg.actor)?;
    Ok(AdoptionOutcome {
        bead_id,
        stamp_failed: false,
    })
}

/// Attempt the `Beads ID` stamp, and give up on it permanently if it fails.
///
/// The stamp writes the new bead id back onto the remote issue so a human
/// reading the web UI can find it. It is not part of the adoption: the bead
/// already exists and is already paired, and re-running would find nothing to
/// do. So a failure is recorded on the outcome and logged, never retried and
/// never rolled back — rolling back would delete a correct bead over a
/// cosmetic field, and retrying would let a courtesy fail a run.
///
/// `stamp` is a closure rather than an `HttpClient` because this module is
/// backend-neutral; the YouTrack shape of that write lives under
/// `crate::remote::youtrack`. It is also what keeps every test of adoption
/// itself socket-free.
pub fn stamp_adoption<F>(outcome: &mut AdoptionOutcome, remote_id: &str, stamp: F)
where
    F: FnOnce() -> std::result::Result<(), RemoteError>,
{
    if let Err(error) = stamp() {
        outcome.stamp_failed = true;
        eprintln!(
            "note: adopted {remote_id} as {} but could not stamp its Beads ID field ({error}); \
             the bead is correct and nothing will retry this",
            outcome.bead_id
        );
    }
}

/// The title to create the bead under.
///
/// A YouTrack issue cannot exist without a summary, but a bead title has a
/// minimum length, so an empty one is named after the issue rather than
/// refused by the validator three layers down.
fn adopted_title(candidate: &AdoptionCandidate) -> String {
    let title = if candidate.fields.title.trim().is_empty() {
        candidate.remote.summary.trim()
    } else {
        candidate.fields.title.trim()
    };
    if title.is_empty() {
        format!("(untitled {})", candidate.remote.id_readable)
    } else {
        title.to_string()
    }
}

/// A child's id comes from its parent; a root's comes from the generator.
fn mint_bead_id(
    storage: &SqliteStorage,
    cfg: &AdoptContext<'_>,
    candidate: &AdoptionCandidate,
    parent_bead_id: Option<&str>,
) -> Result<String> {
    let Some(parent_bead_id) = parent_bead_id else {
        let generator = IdGenerator::new(cfg.id_config.clone());
        let count = storage.count_issues()?;
        return generator.generate(
            &adopted_title(candidate),
            candidate.fields.description.as_deref(),
            Some(cfg.actor),
            Utc::now(),
            count,
            |id| storage.id_exists(id),
        );
    };

    if !storage.id_exists(parent_bead_id)? {
        return Err(BeadsError::IssueNotFound {
            id: parent_bead_id.to_string(),
        });
    }
    let first = storage.next_child_number(parent_bead_id)?;
    let ceiling = first.saturating_add(100);
    let mut number = first;
    loop {
        let candidate_id = child_id(parent_bead_id, number);
        if !storage.id_exists(&candidate_id)? {
            return Ok(candidate_id);
        }
        number += 1;
        if number > ceiling {
            return Err(BeadsError::IdCollision { id: candidate_id });
        }
    }
}

/// Import an adoptee's `Depend` and `Relates` links, and any `Subtask` link
/// its creation did not already establish.
///
/// **This is the function whose absence is a data-loss bug, not a missing
/// feature.** The link differ is local-wins. An adopted bead that arrives
/// without the links its mirror carries looks exactly like a bead whose links
/// a human deleted locally, so the very next push *deletes the parent link
/// that was just created* — the mirror destroying its own inbound edit, one
/// sync after importing it, silently. See
/// `tests/repro/remote_adoption_link_deletion.rs`.
///
/// Run this as a **second pass**, after every adoptee in the run has an id:
/// `blocks` and `relates` do not affect id minting, and either end of one may
/// be an adoptee that does not exist yet when the other is created.
///
/// `id_map` maps remote `idReadable` onto bead id, and must hold both the
/// pre-existing pairings and the ids minted by this run. A link whose other
/// end is in neither is **skipped, not guessed**: it lands on a later run,
/// once both ends exist. That is exactly the rule `diff_links` applies in the
/// other direction.
///
/// **A `Depend` link is read from *both* ends, unlike everywhere else.** The
/// differ reads only the mirrored direction of each kind, because a link
/// mirrored from both ends would have each end remove what the other just
/// added. That reasoning does not carry over here, and following it cost a
/// whole class of edge: a `Depend` is owned by the *blocker*, so "EM-10 is
/// required for EM-11" — an already-mirrored issue blocking a brand-new one,
/// which is an ordinary way to file work — is `OUTWARD` on EM-10 and
/// `INWARD` on the adoptee EM-11. Reading only `OUTWARD` skips it entirely
/// (EM-10 is paired, not a candidate, so nothing walks its links), the
/// adoptee lands with no `blocks` row, and the local-wins differ deletes the
/// link from YouTrack on the next push. Reading the `INWARD` half as well
/// picks it up from the end this function *does* walk, and costs nothing when
/// both ends are adoptees: the two directions produce the identical row and
/// `add_dependency` reports an existing row as `Ok(false)` rather than
/// failing.
///
/// **A `Subtask` link is read from both ends too, and for the same reason.**
/// Its `OUTWARD` half means an adoptee has become the *parent* of an
/// already-paired bead — someone dragged an existing issue under a new one in
/// the web UI. That is not a case this can decline: the differ is local-wins,
/// so the already-paired bead reports no parent, and the next push removes the
/// link the human just made. The identical bug the `INWARD` half exists to
/// prevent, in the other direction.
///
/// What it does about it is deliberately narrow. It writes the `parent-child`
/// dependency row and **nothing else** — it does not rename the child bead to
/// sit under its new parent's id. A rename is a tombstone, `former_ids` churn
/// and a forwarding pointer, permanently, and inflicting that on an existing
/// bead because somebody rearranged a board is exactly what
/// [`topological_order`]'s deferral was written to avoid for new ones. The
/// result is a bead whose id no longer describes its parentage, which beads
/// already tolerates (`br dep add … parent-child` produces the same shape) and
/// which a human can resolve with `br reparent` if they want the id to match.
/// The alternative was letting the mirror delete an inbound edit, and that is
/// not a trade.
///
/// # Errors
/// Returns whatever storage returns on a dependency write.
pub fn import_links<S: BuildHasher>(
    storage: &mut SqliteStorage,
    candidate: &AdoptionCandidate,
    id_map: &HashMap<String, String, S>,
) -> Result<()> {
    let Some(bead_id) = id_map.get(&candidate.remote.id_readable) else {
        // Not adopted (deferred, or refused): there is nothing to hang links
        // on, and the next run will offer it again.
        return Ok(());
    };

    for link in &candidate.remote.links {
        let mirrored = link.direction == direction_tag(mirrored_direction(link.kind));
        for other in &link.issues {
            let Some(other_bead) = id_map.get(&other.id_readable) else {
                continue;
            };
            if other_bead == bead_id {
                continue;
            }
            match (link.kind, mirrored) {
                // "subtask of": the linked issue is this one's parent.
                // `adopt_one` already wrote this when it was the creation
                // parent, and `add_dependency` reports an existing row as
                // `false` rather than failing, so this is then a no-op.
                (LinkKind::Subtask, true) => {
                    storage.add_dependency(bead_id, other_bead, "parent-child", YOUTRACK_AUTHOR)?;
                }
                // "is required for": this issue blocks the linked one, and
                // beads records that as the linked bead depending on this one.
                (LinkKind::Depend, true) => {
                    storage.add_dependency(other_bead, bead_id, "blocks", YOUTRACK_AUTHOR)?;
                }
                // "depends on": the linked issue blocks this one. See the doc
                // comment — this is the half that would otherwise escape.
                (LinkKind::Depend, false) => {
                    storage.add_dependency(bead_id, other_bead, "blocks", YOUTRACK_AUTHOR)?;
                }
                // "parent for": this issue is the linked one's parent. The
                // linked bead already exists, so this writes the row and does
                // not rename it — see the doc comment.
                (LinkKind::Subtask, false) => {
                    storage.add_dependency(other_bead, bead_id, "parent-child", YOUTRACK_AUTHOR)?;
                }
                // Undirected, and the differ builds the local set
                // symmetrically, so one row is enough.
                (LinkKind::Relates, true) => {
                    storage.add_dependency(bead_id, other_bead, "related", YOUTRACK_AUTHOR)?;
                }
                // `Relates` has no non-mirrored direction to reach: YouTrack
                // reports it as `BOTH` on both ends.
                (LinkKind::Relates, false) => {}
            }
        }
    }
    Ok(())
}

/// Import an adoptee's comments, authored by the integration user.
///
/// `author = "youtrack"` is not decoration: it is exactly the marker the
/// symmetric echo suppression in [`crate::remote::comments`] already reads,
/// so a comment imported here is never pushed back out. Adoption needs no new
/// mechanism for that — only the existing one.
///
/// A `[br]`-marked comment is br's own echo of a bead comment, and on a
/// brand-new bead there is no such comment to echo. Importing it would turn
/// br's own output into human content and then push it back as a duplicate,
/// so it is skipped — the same test [`crate::remote::comments::is_br_echo`]
/// applies on every other fetch.
///
/// # Errors
/// Returns whatever storage returns on a comment write.
pub fn import_comments(
    storage: &mut SqliteStorage,
    bead_id: &str,
    comments: &[RemoteComment],
) -> Result<()> {
    for comment in comments {
        if is_br_echo(&comment.text) || comment.text.trim().is_empty() {
            continue;
        }
        storage.add_comment(bead_id, YOUTRACK_AUTHOR, &comment.text)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::youtrack::links::LinkedIssue;

    fn config() -> RemoteConfig {
        RemoteConfig::from_yaml_str(include_str!("../../tests/fixtures/remote_em.yaml"))
            .expect("valid config")
    }

    fn issue_with_type(id: &str, type_value: Option<&str>) -> RemoteIssue {
        let mut issue = RemoteIssue::for_test(id);
        issue.raw_custom_fields = serde_json::json!([
            {"name":"Type","value": type_value.map(|v| serde_json::json!({"name": v}))},
            {"name":"State","value":{"name":"Open"}},
            {"name":"Priority","value":{"name":"Major"}}
        ]);
        issue
    }

    #[test]
    fn an_unmapped_type_is_refused_and_fully_named() {
        let refusal = classify_adoption(&config(), &issue_with_type("EM-7", Some("User Story")))
            .expect_err("must refuse");

        assert_eq!(refusal.id_readable, "EM-7");
        assert_eq!(refusal.field, "Type");
        assert_eq!(refusal.value, "User Story");
        assert_eq!(refusal.config_key, "type_map");

        let rendered = refusal.render();
        assert!(rendered.contains("EM-7"), "{rendered}");
        assert!(rendered.contains("User Story"), "{rendered}");
        assert!(
            rendered.contains("type_map"),
            "must say how to fix it: {rendered}"
        );
    }

    #[test]
    fn a_mapped_issue_is_accepted() {
        let candidate = classify_adoption(&config(), &issue_with_type("EM-8", Some("Task")))
            .expect("must accept");
        assert_eq!(candidate.fields.issue_type, crate::model::IssueType::Task);
    }

    #[test]
    fn an_unset_field_takes_the_beads_default() {
        // Only reachable where canBeEmpty is true — not the stock config,
        // but retained for projects configured otherwise.
        let candidate =
            classify_adoption(&config(), &issue_with_type("EM-9", None)).expect("must accept");
        assert_eq!(candidate.fields.issue_type, crate::model::IssueType::Task);
        assert_eq!(candidate.fields.status, crate::model::Status::Open);
        // `Priority::MEDIUM` is the constant; `P2` is how it prints.
        assert_eq!(candidate.fields.priority, crate::model::Priority::MEDIUM);
        assert_eq!(candidate.fields.priority.to_string(), "P2");
    }

    #[test]
    fn one_refusal_does_not_stop_the_others() {
        let issues = vec![
            issue_with_type("EM-1", Some("Task")),
            issue_with_type("EM-2", Some("User Story")),
            issue_with_type("EM-3", Some("Bug")),
        ];
        let (accepted, refused): (Vec<_>, Vec<_>) = issues
            .iter()
            .map(|i| classify_adoption(&config(), i))
            .partition(std::result::Result::is_ok);

        assert_eq!(
            accepted.len(),
            2,
            "a refusal removes one issue, not the run"
        );
        assert_eq!(refused.len(), 1);
    }

    #[test]
    fn a_refusal_renders_into_the_plan_shape_too() {
        let refusal = classify_adoption(&config(), &issue_with_type("EM-7", Some("User Story")))
            .expect_err("must refuse");
        let planned: crate::remote::plan::RefusedAdoption = refusal.into();
        assert_eq!(planned.remote_id, "EM-7");
        assert!(planned.reason.contains("type_map"), "{}", planned.reason);
    }

    /// `classify_adoption` rebuilds an issue body from three keys of the
    /// parsed `RemoteIssue`, because the fetched body is gone by then. This
    /// pins the reconstruction against resolving the *whole* body: if
    /// `reverse_fields` ever reads a key the reconstruction does not carry,
    /// the two answers diverge here rather than silently in production.
    #[test]
    fn adoption_resolves_the_same_fields_as_the_whole_body() {
        let body = serde_json::json!({
            "id": "3-11",
            "idReadable": "EM-11",
            "summary": "Typed in the web UI",
            "description": "body **markdown**",
            "updated": 1_786_881_829_147_i64,
            "commentsCount": 2,
            "tags": [{"id":"6-1","name":"mirror"}],
            "links": [],
            "customFields": [
                {"name":"Type","value":{"name":"Bug"}},
                {"name":"State","value":{"name":"In Progress"}},
                {"name":"Priority","value":{"name":"Critical"}},
                {"name":"Design","value":{"text":"d"}},
                {"name":"Acceptance Criteria","value":{"text":"ac"}},
                {"name":"Notes","value":{"text":"n"}},
                {"name":"Close Reason","value":{"text":"cr"}},
                {"name":"Beads ID","value":"em-1"},
                {"name":"Assignee","value":null}
            ]
        });
        let whole =
            crate::remote::youtrack::mapping::reverse_fields(&config(), &body).expect("whole body");

        let mut issue = RemoteIssue::for_test("EM-11");
        issue.summary = "Typed in the web UI".into();
        issue.description = Some("body **markdown**".into());
        issue.raw_custom_fields = body["customFields"].clone();
        let candidate = classify_adoption(&config(), &issue).expect("mapped");

        assert_eq!(
            candidate.fields, whole,
            "adoption's reconstruction must resolve to exactly what the fetched body does"
        );
    }

    fn candidate_with_parent(id: &str, parent: Option<&str>) -> AdoptionCandidate {
        let mut remote = issue_with_type(id, Some("Task"));
        remote.summary = format!("web-ui {id}");
        if let Some(parent) = parent {
            remote.links = vec![RemoteLink {
                // The reference instance's Subtask id, target→source. Ids
                // live in tests, never in `src/` — see `links::tests`.
                link_id: "173-3t".into(),
                kind: LinkKind::Subtask,
                direction: "INWARD".into(),
                issues: vec![LinkedIssue {
                    id: format!("3-{}", parent.rsplit('-').next().unwrap_or("0")),
                    id_readable: parent.to_string(),
                }],
            }];
        }
        classify_adoption(&config(), &remote).expect("mapped")
    }

    fn ids(order: &AdoptionOrder) -> Vec<&str> {
        order
            .ordered
            .iter()
            .map(|c| c.remote.id_readable.as_str())
            .collect()
    }

    #[test]
    fn a_child_is_ordered_after_its_parent() {
        let parent = candidate_with_parent("EM-10", None);
        let child = candidate_with_parent("EM-11", Some("EM-10"));
        // Deliberately supplied child-first.
        let order = topological_order(vec![child, parent], &HashMap::new());
        assert_eq!(ids(&order), ["EM-10", "EM-11"]);
        assert!(order.deferred.is_empty());
    }

    #[test]
    fn a_three_deep_chain_is_ordered_root_first() {
        let a = candidate_with_parent("EM-1", None);
        let b = candidate_with_parent("EM-2", Some("EM-1"));
        let c = candidate_with_parent("EM-3", Some("EM-2"));
        let order = topological_order(vec![c, b, a], &HashMap::new());
        assert_eq!(ids(&order), ["EM-1", "EM-2", "EM-3"]);
        assert!(order.deferred.is_empty());
    }

    #[test]
    fn an_adoptee_under_an_already_paired_parent_is_ordered_first() {
        // Its parent is already a bead, so it can be adopted immediately.
        let child = candidate_with_parent("EM-11", Some("EM-10"));
        let mut paired = HashMap::new();
        paired.insert("EM-10".to_string(), "bds-4r2".to_string());
        let order = topological_order(vec![child], &paired);
        assert_eq!(order.ordered.len(), 1, "a paired parent is not a blocker");
        assert!(order.deferred.is_empty());
    }

    #[test]
    fn an_adoptee_whose_parent_is_unadoptable_is_deferred_not_flattened() {
        // Its parent was refused (bds-4r2.10.1), so adopting this one flat
        // would need a reparent later — a rename, with a tombstone.
        let child = candidate_with_parent("EM-11", Some("EM-99"));
        let order = topological_order(vec![child], &HashMap::new());
        assert!(
            order.ordered.is_empty(),
            "deferring costs one run; flattening costs a tombstone and former_ids churn forever"
        );

        // And it is *named*. Deferral is refusal's sibling: silently dropping
        // it leaves the user who filed EM-11 under an unreadable EM-99 with
        // nothing on stdout, run after run.
        assert_eq!(
            order.deferred,
            vec![DeferredAdoption {
                id_readable: "EM-11".into(),
                parent_id: "EM-99".into(),
                reason: DeferralReason::ParentNotAdoptable,
            }]
        );
        let rendered = order.deferred[0].render();
        assert!(rendered.contains("EM-11"), "{rendered}");
        assert!(
            rendered.contains("EM-99"),
            "must name the blocker: {rendered}"
        );
    }

    #[test]
    fn a_deferred_parent_defers_its_whole_subtree() {
        // EM-99 is unreadable, so EM-11 defers — and EM-12 under EM-11 must
        // defer with it rather than be flattened onto a root id.
        let child = candidate_with_parent("EM-11", Some("EM-99"));
        let grandchild = candidate_with_parent("EM-12", Some("EM-11"));
        let order = topological_order(vec![child, grandchild], &HashMap::new());
        assert!(order.ordered.is_empty(), "got {:?}", ids(&order));

        // Both are reported, and each says which parent stopped it: EM-11 is
        // blocked by an issue outside the run, EM-12 by EM-11 itself.
        let named: Vec<(&str, &str, DeferralReason)> = order
            .deferred
            .iter()
            .map(|d| (d.id_readable.as_str(), d.parent_id.as_str(), d.reason))
            .collect();
        assert_eq!(
            named,
            [
                ("EM-11", "EM-99", DeferralReason::ParentNotAdoptable),
                ("EM-12", "EM-11", DeferralReason::ParentDeferred),
            ]
        );
    }

    #[test]
    fn a_parent_cycle_is_deferred_rather_than_spun_on() {
        // YouTrack's subtask graph is a tree, so this is unreachable through
        // the web UI — but a wave loop that assumes a DAG spins forever on it,
        // and "unreachable" is not a thing to bet a run's liveness on.
        let a = candidate_with_parent("EM-1", Some("EM-2"));
        let b = candidate_with_parent("EM-2", Some("EM-1"));
        let order = topological_order(vec![a, b], &HashMap::new());
        assert!(
            order.ordered.is_empty(),
            "neither member can ever become ready"
        );
        assert_eq!(order.deferred.len(), 2, "and both are reported by name");
        assert!(
            order
                .deferred
                .iter()
                .all(|d| d.reason == DeferralReason::ParentDeferred),
            "a cycle is not a special case: each member's parent is deferred: {:?}",
            order.deferred
        );
    }

    #[test]
    fn independent_candidates_keep_their_input_order() {
        let a = candidate_with_parent("EM-1", None);
        let b = candidate_with_parent("EM-2", None);
        let c = candidate_with_parent("EM-3", None);
        let order = topological_order(vec![c, a, b], &HashMap::new());
        assert_eq!(
            ids(&order),
            ["EM-3", "EM-1", "EM-2"],
            "a stable order keeps a printed plan stable run to run"
        );
    }

    #[test]
    fn a_failed_stamp_is_recorded_and_never_retried() {
        let mut outcome = AdoptionOutcome {
            bead_id: "em-1.1".into(),
            stamp_failed: false,
        };
        let mut attempts = 0_u32;
        stamp_adoption(&mut outcome, "EM-11", || {
            attempts += 1;
            Err(RemoteError::Config("boom".into()))
        });
        assert!(outcome.stamp_failed);
        assert_eq!(attempts, 1, "the stamp is a courtesy, not a retried write");
        assert_eq!(outcome.bead_id, "em-1.1", "the bead is never rolled back");
    }
}
