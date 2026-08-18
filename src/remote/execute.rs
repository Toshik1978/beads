//! Turning a [`ReconcilePlan`] into requests: batching, backoff, the
//! first-run gate, and a resume that cannot duplicate.
//!
//! Everything above this module decides *what* a run would do without doing
//! any of it. This is the layer that does it, and the one ordering rule below
//! is the whole reason it can be interrupted safely.
//!
//! ## `external_ref` is written the instant its issue exists
//!
//! For each bead: POST the issue, **persist `external_ref` at once**, and only
//! then move to the next bead. Batch the writes at the end, or hold the refs
//! in memory until the run completes, and an interruption loses every pairing
//! made since the last flush — so the next run re-creates all of them and the
//! mirror silently doubles. With the write immediately after the POST the
//! worst case is one issue created whose ref was not recorded: one duplicate,
//! not N.
//!
//! `external_ref` is not a `content_hash` input, so this write moves no digest
//! and produces no churn in the exported record.
//!
//! ## And that worst case is closed too
//!
//! One duplicate is still one duplicate, and the window is real: a 503 arriving
//! after the server applied the create leaves an issue whose id this process
//! never learned. `HttpClient` deliberately does not retry that (see
//! `crate::remote::http::may_retry`) — repeating a create is how a mirror
//! doubles — so the recovery is here instead, and it is a check for the
//! write's own effect rather than a retry: `issue_create_body` stamps the bead
//! id into the issue's `Beads ID` field, so the orphan comes back on the next
//! fetch naming the bead it belongs to. [`orphaned_creates`] finds it and the
//! bead is paired with no POST at all. That is what makes the create
//! idempotent — not the transport.
//!
//! ## The same check, for a link write
//!
//! A link `POST` is in exactly the same position, and was not covered for
//! longer than it should have been. The symptom is quieter than a duplicate
//! and reads worse: the run names `EM-27 + blocks EM-29` as failed, exits
//! non-zero, and says re-running does the remainder — and the re-run then
//! reports nothing to do, because the write had landed and the socket died
//! carrying the answer back. Nothing is wrong with the mirror at that point;
//! the report was.
//!
//! [`link_change_landed`] closes it the same way `orphaned_creates` does, by
//! reading the effect rather than repeating the write: one GET of that
//! issue's links, on the failure path only, selecting by the same `link_id`
//! the differ read by. If the end state the change asked for holds, the
//! change is reported as done — annotated, so a lost response stays visible
//! — and if it does not, or the check itself cannot reach the server, the
//! failure stands. An unproven write is never assumed.
//!
//! ## And a comment write's, by counting
//!
//! A comment is the third write in the same position and the quietest of the
//! three, because the thing that hides it is the optimisation that makes
//! comments affordable at all: `comment_counts_agree` compares counts, a
//! landed comment makes them agree, and the next run never looks. The
//! reported failure is then the only trace, and it is wrong.
//!
//! [`crate::remote::comments::comment_landed`] reads the effect the same
//! way, with one wrinkle the link case does not have. `plan_comment_sync`
//! puts a body in `to_push` only when the mirror carries no `[br]` comment
//! with it, so the mirror's count for that body starts the run at zero — but
//! a bead that says the same thing twice pushes two identical bodies, and
//! "is it there?" for the second would be answered by the first one's copy.
//! So the check counts: the ordinal of the copy being pushed is passed in,
//! and the mirror must show at least that many.

use crate::error::Result;
use crate::model::{Issue, Status};
use crate::remote::adopt::{
    AdoptContext, AdoptionCandidate, DeferredAdoption, RefusedAdoption, adopt_one,
    classify_adoption, creation_parent, import_comments, import_links, stamp_adoption,
    topological_order,
};
use crate::remote::comments::{
    LocalComment, YOUTRACK_AUTHOR, comment_counts_agree, comment_landed, fetch_comments,
    plan_comment_sync, push_comment,
};
use crate::remote::config::RemoteConfig;
use crate::remote::diff::{Direction, Field};
use crate::remote::error::RemoteError;
use crate::remote::http::HttpClient;
use crate::remote::link_diff::{LinkChange, kind_name, mirrored_direction};
use crate::remote::model::RemoteIssue;
use crate::remote::plan::{CommentWork, ReconcilePlan, build_plan};
use crate::remote::reconcile::{is_in_scope, pair_workspace};
use crate::remote::tombstone::{TombstoneAction, deleted_comment_text, plan_tombstones};
use crate::remote::youtrack::fetch::fetch_snapshot;
use crate::remote::youtrack::links::{LinkTypes, fetch_issue_links, link_add, link_remove};
use crate::remote::youtrack::mapping::{
    FieldSet, deleted_state_body, issue_create_body, issue_update_body,
};
use crate::remote::youtrack::tags::{TagCache, tags_body};
use crate::storage::{IssueUpdate, SqliteStorage};
use crate::util::id::IdConfig;
use serde_json::{Value, json};
use std::collections::HashMap;

/// The path an issue create posts to. `idReadable` is the field that matters:
/// it is what a bead's `external_ref` holds.
pub const CREATE_PATH: &str = "/api/issues?fields=id,idReadable";

/// How many creates go by between progress reports.
///
/// This is a reporting boundary and nothing more. It is deliberately *not* a
/// transaction boundary: the `external_ref` write happens per create, not per
/// batch, which is what makes an interrupted run resumable at all.
pub const BATCH_SIZE: usize = 25;

/// Recorded as the actor on every local write this module makes.
pub const REMOTE_ACTOR: &str = "br-remote";

/// How far through the creates a run has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchProgress {
    pub done: usize,
    pub total: usize,
}

/// What one pass of [`execute_creates`] produced.
///
/// All three lists are what the CLI prints, and all three are meaningful even
/// when the run stopped early: the refs are already durable, so a partial
/// report describes real, permanent progress rather than work to redo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateReport {
    /// `(bead id, remote idReadable)` for each issue this pass created.
    pub created: Vec<(String, String)>,
    /// `(bead id, reason)` for the create that stopped the pass.
    pub failed: Vec<(String, String)>,
    /// `(bead id, remote idReadable)` for each bead paired with an issue an
    /// **earlier** pass created but never recorded — see [`orphaned_creates`].
    /// No request is issued for these.
    pub recovered: Vec<(String, String)>,
    /// Beads more than one unpaired issue claims. Skipped, never guessed at.
    pub ambiguous: Vec<AmbiguousStamp>,
}

impl CreateReport {
    /// Whether this pass created or recovered anything, and therefore changed
    /// which beads are paired.
    #[must_use]
    pub fn changed_pairings(&self) -> bool {
        !self.created.is_empty() || !self.recovered.is_empty()
    }
}

/// More than one unpaired remote issue claiming the same bead.
///
/// Only reachable by a human copying a `Beads ID` value between issues, or by
/// two issues surviving a create that was somehow attempted twice. Either way
/// br cannot choose between them, and guessing would pair one and leave the
/// other to be adopted as a second bead — silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousStamp {
    pub bead_id: String,
    /// Every remote issue whose `Beads ID` names that bead, in fetch order.
    pub remote_ids: Vec<String>,
}

impl AmbiguousStamp {
    /// One line naming the bead, the issues that claim it, and the remedy.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{} is claimed by {} — br cannot tell which issue belongs to it, so it created \
             none of them and paired nothing. Clear the Beads ID field on all but one of \
             them and re-run.",
            self.bead_id,
            self.remote_ids.join(", ")
        )
    }
}

/// Which of this run's creates already exist, and which are unresolvable.
#[derive(Debug, Clone, Default)]
pub struct OrphanScan {
    /// Bead id → the remote `idReadable` to pair it with.
    pub recoverable: HashMap<String, String>,
    /// Beads more than one issue claims. Never recovered, always reported.
    pub ambiguous: Vec<AmbiguousStamp>,
}

impl OrphanScan {
    /// Whether `bead_id` is one this run must leave alone.
    #[must_use]
    pub fn is_ambiguous(&self, bead_id: &str) -> bool {
        self.ambiguous.iter().any(|entry| entry.bead_id == bead_id)
    }

    /// Whether `remote_id` is one of the issues in an ambiguous claim.
    ///
    /// Adoption consults this: turning a disputed issue into a fresh bead is
    /// the one move that makes an already-wrong state worse.
    #[must_use]
    pub fn claims(&self, remote_id: &str) -> bool {
        self.ambiguous
            .iter()
            .any(|entry| entry.remote_ids.iter().any(|id| id == remote_id))
    }
}

/// Beads whose issue already exists because a previous run created it and lost
/// the response.
///
/// An entry appears only when all three of the following hold, which together
/// admit no other explanation:
///
/// - the remote issue is **unpaired** — no bead claims it, so it is sitting in
///   `plan.adoptions`;
/// - its `Beads ID` field names a bead that this run intends to **create** —
///   so that bead has no `external_ref` of its own;
/// - and that field is only ever written by `issue_create_body`.
///
/// A human typing an issue into the web UI does not fill in `Beads ID` with
/// the id of an unpaired local bead, so this cannot misfire on a genuine
/// adoption candidate.
///
/// **Two issues naming one bead is not resolved by picking one.** An earlier
/// revision collected straight into a map, so the last issue in fetch order
/// won and the other was adopted as a second bead with nothing said. Both are
/// now returned in [`OrphanScan::ambiguous`] instead: the bead is not created,
/// neither issue is adopted, and the run says so.
#[must_use]
pub fn orphaned_creates(plan: &ReconcilePlan) -> OrphanScan {
    let pending: std::collections::HashSet<&str> =
        plan.creates.iter().map(String::as_str).collect();
    // Claim order is fetch order, which `pair_workspace` preserves, so the
    // report is stable run to run.
    let mut claims: Vec<(String, Vec<String>)> = Vec::new();
    for adoption in &plan.adoptions {
        let Some(beads_id) = adoption.beads_id.as_deref() else {
            continue;
        };
        if !pending.contains(beads_id) {
            continue;
        }
        match claims.iter_mut().find(|(bead, _)| bead == beads_id) {
            Some((_, remote_ids)) => remote_ids.push(adoption.remote_id.clone()),
            None => claims.push((beads_id.to_string(), vec![adoption.remote_id.clone()])),
        }
    }

    let mut scan = OrphanScan::default();
    for (bead_id, remote_ids) in claims {
        match <[String; 1]>::try_from(remote_ids) {
            Ok([only]) => {
                scan.recoverable.insert(bead_id, only);
            }
            Err(remote_ids) => scan.ambiguous.push(AmbiguousStamp {
                bead_id,
                remote_ids,
            }),
        }
    }
    scan
}

/// Create every issue `plan` calls for, pairing each bead the instant its
/// issue exists.
///
/// Stops at the first create that fails rather than spending the rest of the
/// run discovering the same failure nine more times — a rate limit or a
/// severed connection does not become truer with repetition, and everything
/// already created is already durable. What succeeded is returned, not
/// discarded.
///
/// `progress` is called every [`BATCH_SIZE`] creates and once more at the end.
///
/// # Errors
/// Returns whatever storage returns when persisting a pairing. A *remote*
/// failure is not an error here: it lands in [`CreateReport::failed`], because
/// the refs written before it are real and the report is what the CLI prints.
pub fn execute_creates(
    storage: &mut SqliteStorage,
    http: &HttpClient,
    cfg: &RemoteConfig,
    plan: &ReconcilePlan,
    progress: &mut dyn FnMut(BatchProgress),
) -> Result<CreateReport> {
    let mut report = CreateReport::default();
    if plan.creates.is_empty() {
        return Ok(report);
    }

    let orphans = orphaned_creates(plan);
    report.ambiguous.clone_from(&orphans.ambiguous);
    let total = plan.creates.len();
    // Resolved once, and only when a create might actually need to POST a
    // new issue. `plan.creates` being non-empty is not enough on its own: an
    // entry that turns out recoverable is paired against an issue that
    // already exists, and an ambiguous one is skipped outright — neither
    // ever reaches `issue_create_body`, the only place `project_id` is used.
    // A run where every pending create lands in one of those two buckets
    // must not pay for a project lookup it will never spend.
    let needs_project_id = plan.creates.iter().any(|bead_id| {
        !orphans.recoverable.contains_key(bead_id) && !orphans.is_ambiguous(bead_id)
    });
    let project_id = if needs_project_id {
        resolve_project_id(http, cfg)?
    } else {
        String::new()
    };
    let mut done = 0_usize;
    let mut reported = 0_usize;

    for bead_id in &plan.creates {
        match create_one(storage, http, cfg, &project_id, bead_id, &orphans)? {
            CreateOutcome::Created(remote_id) => report.created.push((bead_id.clone(), remote_id)),
            CreateOutcome::Recovered(remote_id) => {
                report.recovered.push((bead_id.clone(), remote_id));
            }
            CreateOutcome::Skipped => {}
            CreateOutcome::Failed(reason) => {
                report.failed.push((bead_id.clone(), reason));
                break;
            }
        }
        done += 1;
        if done.is_multiple_of(BATCH_SIZE) {
            reported = done;
            progress(BatchProgress { done, total });
        }
    }
    if reported != done {
        progress(BatchProgress { done, total });
    }
    Ok(report)
}

/// The four ways one create can end.
enum CreateOutcome {
    Created(String),
    Recovered(String),
    /// Two issues claim this bead; see [`AmbiguousStamp`]. Reported by
    /// `execute_creates` from the scan, not from here.
    Skipped,
    Failed(String),
}

fn create_one(
    storage: &mut SqliteStorage,
    http: &HttpClient,
    cfg: &RemoteConfig,
    project_id: &str,
    bead_id: &str,
    orphans: &OrphanScan,
) -> Result<CreateOutcome> {
    if let Some(remote_id) = orphans.recoverable.get(bead_id) {
        // The issue already exists — an earlier run created it and lost the
        // answer. Pair it; do not create a second one.
        pair(storage, bead_id, remote_id)?;
        return Ok(CreateOutcome::Recovered(remote_id.clone()));
    }
    if orphans.is_ambiguous(bead_id) {
        // Creating it would add a *third* issue to a bead two already claim.
        return Ok(CreateOutcome::Skipped);
    }

    let Some(bead) = storage.get_issue(bead_id)? else {
        // The plan was built from a snapshot of the workspace; a bead that has
        // gone since is not this run's problem.
        return Ok(CreateOutcome::Failed(format!(
            "{bead_id} is no longer in this workspace"
        )));
    };
    let body = match issue_create_body(cfg, project_id, &bead) {
        Ok(body) => body,
        Err(err) => return Ok(CreateOutcome::Failed(err.to_string())),
    };
    let response = match http.post_json(CREATE_PATH, &body, "issue") {
        Ok(response) => response,
        Err(err) => return Ok(CreateOutcome::Failed(err.to_string())),
    };
    let Some(remote_id) = response
        .get("idReadable")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        // The issue may well have been created. Saying so is the only honest
        // answer: the next run finds it by its `Beads ID` stamp and pairs it.
        return Ok(CreateOutcome::Failed(
            "the create response carried no idReadable, so the issue could not be paired; \
             the next run will recover it by its Beads ID stamp"
                .to_string(),
        ));
    };
    let remote_id = remote_id.to_string();

    // THE ordering rule. Not after the batch, not at the end of the run.
    pair(storage, bead_id, &remote_id)?;
    Ok(CreateOutcome::Created(remote_id))
}

/// Write `remote_id` onto `bead_id`'s `external_ref`.
///
/// # Errors
/// Returns whatever storage returns.
pub fn pair(storage: &mut SqliteStorage, bead_id: &str, remote_id: &str) -> Result<()> {
    let updates = IssueUpdate {
        external_ref: Some(Some(remote_id.to_string())),
        ..IssueUpdate::default()
    };
    storage.update_issue(bead_id, &updates, REMOTE_ACTOR)?;
    Ok(())
}

/// The internal database id of `cfg.project`.
///
/// A create body addresses its project by database id; `remote.yaml` names it
/// by short name, because that is what a human reads off a YouTrack URL.
///
/// Delegates to `admin::resolve_project_by_short_name` rather than carrying
/// its own read of `/api/admin/projects`: the two were previously separate,
/// and the copy here stayed on a single unpaginated `$top=500` request after
/// the other was paged, so a project past the first page resolved for
/// `init` and refused every writing verb. Sharing one paged implementation
/// closes that gap for good rather than requiring it be paged twice.
///
/// # Errors
/// Returns `RemoteError::Config` when no visible project has that short name,
/// and whatever `http` returns on transport or HTTP failure.
pub fn resolve_project_id(
    http: &HttpClient,
    cfg: &RemoteConfig,
) -> std::result::Result<String, RemoteError> {
    crate::remote::youtrack::admin::resolve_project_by_short_name(http, &cfg.project)
        .map(|project| project.id)
}

/// A first run refused for want of `--confirm-initial`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateRefusal {
    /// How many issues the run would have created.
    pub would_create: usize,
}

impl GateRefusal {
    /// The refusal, naming the count and the flag that lifts it.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "refusing a first run: no bead in this workspace is paired with an issue in this \
             project yet, so this would create {} issue(s) in it. br has no code path that \
             deletes a YouTrack issue, so a first run against the wrong project leaves every \
             one of them to be deleted by hand. Check `project` and `url` in \
             .beads/remote.yaml, run `br remote status` to see exactly what would be created, \
             and re-run with --confirm-initial when it is right.",
            self.would_create
        )
    }
}

/// Refuse a first run — one where no bead is paired with this project — unless
/// it has been confirmed.
///
/// **The gate is about irreversibility, not size.** `br remote` never deletes a
/// remote issue (`crate::remote::tombstone`), so a first run against the wrong
/// project leaves a few hundred issues a human has to delete by hand. A
/// workspace where even one bead is already paired is not a first run: the
/// blast radius of a wrong project is already visible to the operator.
///
/// Only an **in-scope** pairing counts. A bead whose `external_ref` names
/// another tracker is invisible to every verb (`reconcile::is_in_scope`), so
/// it is not evidence that *this* project has ever been mirrored.
///
/// # Errors
/// Returns [`GateRefusal`] carrying the number of issues the run would create.
pub fn first_run_gate(
    beads: &[Issue],
    cfg: &RemoteConfig,
    confirmed: bool,
) -> std::result::Result<(), GateRefusal> {
    if confirmed {
        return Ok(());
    }
    let mirrored = beads.iter().any(|bead| {
        bead.external_ref
            .as_deref()
            .is_some_and(|reference| is_in_scope(cfg, reference))
    });
    if mirrored {
        return Ok(());
    }
    Err(GateRefusal {
        would_create: beads
            .iter()
            .filter(|bead| bead.external_ref.is_none() && bead.status != Status::Tombstone)
            .count(),
    })
}

// ---------------------------------------------------------------------------
// Comment reconciliation, which needs a socket and therefore cannot live in
// `plan.rs`.
// ---------------------------------------------------------------------------

/// Work out what to do about every paired issue's comments.
///
/// **The count gate is the whole performance story, and it is enforced here.**
/// `RemoteIssue::comments_count` already arrived with the issue list, so a pair
/// whose count matches the bead's costs a plain integer comparison and
/// **zero** requests. Without that, a workspace of 134 issues would pay 134
/// comment requests on every `status` run to learn nothing. The gate is only
/// as good as `bead.comments` being populated — see `hydrated_issues`.
///
/// Tombstoned pairs are skipped: the tombstone rule owns them, and the
/// `[br] deleted in beads` comment it leaves has no local counterpart by
/// design, so their counts never agree again.
///
/// # Errors
/// Returns whatever `http` returns on transport or HTTP failure.
pub fn fetch_comment_work(
    http: &HttpClient,
    cfg: &RemoteConfig,
    beads: &[Issue],
    remote: &[RemoteIssue],
) -> std::result::Result<Vec<CommentWork>, RemoteError> {
    let by_id: HashMap<&str, &Issue> = beads.iter().map(|bead| (bead.id.as_str(), bead)).collect();
    let pairing = pair_workspace(cfg, beads, remote.to_vec());
    let mut work = Vec::new();
    for pair in &pairing.paired {
        let Some(bead) = by_id.get(pair.bead_id.as_str()) else {
            continue;
        };
        if bead.status == Status::Tombstone {
            continue;
        }
        let local_count = u32::try_from(bead.comments.len()).unwrap_or(u32::MAX);
        if comment_counts_agree(&pair.remote, local_count) {
            continue;
        }
        let remote_comments = fetch_comments(http, &pair.remote.id_readable)?;
        let local: Vec<LocalComment> = bead
            .comments
            .iter()
            .map(|comment| LocalComment {
                author: comment.author.clone(),
                text: comment.body.clone(),
            })
            .collect();
        let sync = plan_comment_sync(&local, &remote_comments);
        if sync.to_push.is_empty() && sync.to_pull.is_empty() {
            continue;
        }
        work.push(CommentWork {
            bead_id: pair.bead_id.clone(),
            remote_id: pair.remote.id_readable.clone(),
            to_push: sync.to_push,
            to_pull: sync.to_pull,
        });
    }
    Ok(work)
}

// ---------------------------------------------------------------------------
// One fetched picture of both sides
// ---------------------------------------------------------------------------

/// Every issue with its relations, labels **and comments** attached.
///
/// `get_all_issues_for_export` returns bare rows: no `dependencies`, no
/// `labels`, no `comments`, and each consumer is only as good as what it is
/// handed. The link differ needs the relations; the comment reconciliation
/// needs the comments, and needs them for a sharper reason than convenience —
/// `comment_counts_agree` compares the remote's count against
/// `bead.comments.len()`, so leaving that at 0 makes **every** mirrored issue
/// look like it has comments to pull, and the count gate that exists to keep a
/// `status` run from issuing 134 comment requests would issue all 134 of them
/// and then plan a phantom pull for each.
///
/// Four queries rather than 4N.
///
/// # Errors
/// Returns whatever storage returns from any of the four reads.
pub fn hydrated_issues(storage: &SqliteStorage) -> Result<Vec<Issue>> {
    let mut issues = storage.get_all_issues_for_export()?;
    let mut dependencies = storage.get_all_dependency_records()?;
    let mut labels = storage.get_labels_for_export()?;
    let ids: Vec<String> = issues.iter().map(|issue| issue.id.clone()).collect();
    let mut comments = storage.get_comments_for_issues(&ids)?;
    for issue in &mut issues {
        if let Some(rows) = dependencies.remove(&issue.id) {
            issue.dependencies = rows;
        }
        if let Some(names) = labels.remove(&issue.id) {
            issue.labels = names;
        }
        if let Some(rows) = comments.remove(&issue.id) {
            issue.comments = rows;
        }
    }
    Ok(issues)
}

/// One fetched picture of both sides, and the plan derived from it.
///
/// The plan is deliberately a *report* — ids, rendered values and counts — so
/// an executor needs the things behind it too, which is why this owns them all
/// together: `remote` for the typed value a field pull writes and the raw
/// custom fields an adoption re-resolves, `comments` for the texts the plan
/// reduced to counts, and `beads` for the local side of every write.
pub struct Reconciliation {
    pub beads: Vec<Issue>,
    pub remote: Vec<RemoteIssue>,
    pub comments: Vec<CommentWork>,
    pub plan: ReconcilePlan,
}

impl Reconciliation {
    /// The borrowed view both executors take.
    #[must_use]
    pub fn inputs(&self) -> RunInputs<'_> {
        RunInputs {
            plan: &self.plan,
            comments: &self.comments,
            beads: &self.beads,
            remote: &self.remote,
        }
    }
}

/// Read both sides and compute the plan. Issues no write.
///
/// The **non-fatal** fetch, for every verb and not only `status`. An issue this
/// `remote.yaml` cannot read is reported as a refused adoption and the run
/// continues: refusing one issue must not disable the command that would tell
/// you which issue to fix, and an unreadable issue is skipped by every write
/// path anyway — it never pairs, so nothing pushes to it, and `build_plan`
/// deliberately keeps the bead pointing at it out of `dangling`.
///
/// # Errors
/// Returns whatever storage returns from the hydration, and whatever `http`
/// returns on transport or HTTP failure.
pub fn reconcile(
    storage: &SqliteStorage,
    http: &HttpClient,
    cfg: &RemoteConfig,
    types: &LinkTypes,
) -> Result<Reconciliation> {
    let beads = hydrated_issues(storage)?;
    let snapshot = fetch_snapshot(http, cfg, types)?;
    let remote = snapshot.issues.clone();
    // `build_plan` is pure and socket-free; the comment fetch happens here and
    // reaches it as data. The count gate means a reconciled workspace issues
    // no comment request at all.
    let comments = fetch_comment_work(http, cfg, &beads, &remote)?;
    let plan = build_plan(cfg, &beads, snapshot, types, &comments);
    Ok(Reconciliation {
        beads,
        remote,
        comments,
        plan,
    })
}

// ---------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------

/// One reconciliation's fetched inputs, passed as a unit to both executors.
pub struct RunInputs<'a> {
    pub plan: &'a ReconcilePlan,
    pub comments: &'a [CommentWork],
    pub beads: &'a [Issue],
    pub remote: &'a [RemoteIssue],
}

/// Everything one push did, and everything it could not do.
///
/// A failure on one issue does not abort the rest: each issue's update, each
/// link change and each comment is independent, and stopping the run over one
/// of them would leave the remainder undone for no reason. They are collected
/// and reported, and the command exits non-zero.
#[derive(Debug, Clone, Default)]
pub struct PushReport {
    pub creates: CreateReport,
    /// `"bds-1 → EM-1"` per issue whose fields were written.
    pub fields_updated: Vec<String>,
    /// `"EM-1 + parent EM-2"` per link written.
    pub links_added: Vec<String>,
    /// `"EM-1 - blocks EM-3"` per link removed.
    pub links_removed: Vec<String>,
    pub comments_pushed: usize,
    /// How many of `comments_pushed` were already on the mirror when this run
    /// looked: the write landed and its answer was lost. Counted apart so a
    /// dropped response stays visible instead of reading as an ordinary
    /// success — see this module's doc.
    pub comments_recovered: usize,
    /// Bead ids whose mirror was marked with `deleted_state`.
    pub tombstones_marked: Vec<String>,
    /// Tag names br could not create or attach. Never fatal: a label is not
    /// worth losing a mirrored issue over.
    pub tags_skipped: Vec<String>,
    /// Set when the creates changed which beads are paired and the push
    /// therefore re-planned before mirroring the rest. See [`execute_push`].
    pub replanned_after: usize,
    /// Field changes this push deliberately left alone because the remote won
    /// them. `br remote pull` is what applies those.
    pub left_to_pull: usize,
    /// Comments this push deliberately left alone because the remote won
    /// them — the plan's `N comment(s) [YouTrack wins]` lines, which a bare
    /// push renders (via [`crate::remote::plan::ReconcilePlan::render`]) but
    /// never acts on, exactly like a field pull. Counted separately from
    /// `left_to_pull` because the two come from different plan sections and
    /// the CLI names them separately.
    pub comments_left_to_pull: usize,
    pub failures: Vec<String>,
}

impl PushReport {
    /// Whether everything the plan called for actually happened.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty() && self.creates.failed.is_empty()
    }
}

/// Execute the push half of `plan`.
///
/// **Nothing in here checks `--dry-run`.** The check is one branch at the
/// single point where execution begins (`crate::cli::commands::remote`), so a
/// step added below cannot forget it.
///
/// ## Why a push that creates anything plans twice
///
/// The plan handed in was built before a single issue existed, so every one of
/// its sections covers **paired** beads only: `field_changes` and
/// `link_changes` come from `pairing.paired`, and `fetch_comment_work` walks
/// the same list. A bead created by this very run is in none of them, and
/// `issue_create_body` carries no `tags` either. Left there, a first
/// `br remote push --confirm-initial` would mirror every issue's fields
/// correctly and its **links, comments and labels not at all** — and exit 0
/// saying nothing about the second run needed to finish the job. A
/// half-mirrored project that reports success is the one outcome worth paying a
/// request for.
///
/// So when the create pass changed any pairing, both sides are read again and
/// the plan rebuilt; the beads created a moment ago are paired by then, so the
/// second plan sees their links, comments and labels like any other pair.
///
/// **It cannot loop.** The re-plan happens at most once, it is not in a loop,
/// and the second pass runs [`apply_push`] — which has no create step at all —
/// rather than re-entering this function. A third pass is unrepresentable.
///
/// **The cost is one extra reconciliation, and only on a run that created
/// something.** The link types are already resolved and passed in; the extra
/// spend is one paged issue fetch plus the comment fetches the count gate lets
/// through, which for freshly created issues is one per issue that has local
/// comments. A steady-state push — the common case, where nothing is created —
/// pays nothing.
///
/// # Errors
/// Returns whatever storage returns when persisting a pairing, and whatever
/// the re-plan's fetch returns. Remote failures from the individual writes are
/// collected into [`PushReport::failures`] rather than raised.
pub fn execute_push(
    storage: &mut SqliteStorage,
    http: &HttpClient,
    cfg: &RemoteConfig,
    types: &LinkTypes,
    inputs: &RunInputs<'_>,
    progress: &mut dyn FnMut(BatchProgress),
) -> Result<PushReport> {
    let mut report = PushReport {
        creates: execute_creates(storage, http, cfg, inputs.plan, progress)?,
        ..PushReport::default()
    };
    if !report.creates.failed.is_empty() {
        // The creates stopped for a reason that will stop everything else too
        // — a rate limit, a severed connection, a refused value. What was
        // paired is durable; the rest is the next run's work.
        return Ok(report);
    }

    if report.creates.changed_pairings() {
        report.replanned_after = report.creates.created.len() + report.creates.recovered.len();
        let refreshed = reconcile(storage, http, cfg, types)?;
        apply_push(http, cfg, types, &refreshed.inputs(), &mut report);
    } else {
        apply_push(http, cfg, types, inputs, &mut report);
    }
    Ok(report)
}

/// Everything a push writes except the creates.
///
/// Split out so that the re-plan above can run it against a second plan
/// without any possibility of creating again: there is no create step in here
/// to re-enter.
fn apply_push(
    http: &HttpClient,
    cfg: &RemoteConfig,
    types: &LinkTypes,
    inputs: &RunInputs<'_>,
    report: &mut PushReport,
) {
    let plan = inputs.plan;
    report.left_to_pull = plan
        .field_changes
        .iter()
        .flat_map(|issue| issue.changes.iter())
        .filter(|change| change.direction == Direction::Pull)
        .count();
    // The plan's render also marks comment pulls `[YouTrack wins]`
    // (`ReconcilePlan::render`), and a bare push does not act on those
    // either — `apply_push` never reads `CommentWork::to_pull`, only
    // `to_push` (see `push_comments`). Counted here so the CLI can name that
    // line too instead of leaving it the one `[YouTrack wins]` marker
    // nothing in the report explains.
    report.comments_left_to_pull = plan
        .comment_changes
        .iter()
        .filter(|entry| entry.direction == Direction::Pull)
        .map(|entry| entry.count)
        .sum();

    let by_id: HashMap<&str, &Issue> = inputs
        .beads
        .iter()
        .map(|bead| (bead.id.as_str(), bead))
        .collect();
    let mut tags: Option<TagCache> = None;
    push_fields(http, cfg, plan, &by_id, &mut tags, report);
    if let Some(cache) = &tags {
        report.tags_skipped = cache.skipped().to_vec();
    }
    push_links(http, types, plan, report);
    push_comments(http, inputs.comments, report);
    push_tombstones(http, cfg, plan, inputs.beads, report);
}

/// The update path for one issue. `idReadable` addresses it; the response is
/// only read for its shape.
fn update_path(remote_id: &str) -> String {
    format!("/api/issues/{remote_id}?fields=idReadable")
}

fn push_fields(
    http: &HttpClient,
    cfg: &RemoteConfig,
    plan: &ReconcilePlan,
    by_id: &HashMap<&str, &Issue>,
    tags: &mut Option<TagCache>,
    report: &mut PushReport,
) {
    for issue_plan in &plan.field_changes {
        let Some(bead) = by_id.get(issue_plan.bead_id.as_str()) else {
            continue;
        };
        let (fields, labels) = push_field_set(&issue_plan.changes);
        if !labels && !any_field(fields) {
            // Every difference on this issue resolves the other way; that is
            // `pull`'s work, not this one's.
            continue;
        }
        let mut body = match issue_update_body(cfg, bead, fields) {
            Ok(body) => body,
            Err(err) => {
                report
                    .failures
                    .push(format!("{}: {err}", issue_plan.bead_id));
                continue;
            }
        };
        if labels {
            match resolve_tags(http, tags, &bead.labels) {
                Ok(ids) => body["tags"] = tags_body(&ids),
                Err(err) => {
                    report
                        .failures
                        .push(format!("{}: {err}", issue_plan.bead_id));
                    continue;
                }
            }
        }
        match http.post_json(&update_path(&issue_plan.remote_id), &body, "issue") {
            Ok(_) => report
                .fields_updated
                .push(format!("{} → {}", issue_plan.bead_id, issue_plan.remote_id)),
            Err(err) => report.failures.push(format!(
                "{} → {}: {err}",
                issue_plan.bead_id, issue_plan.remote_id
            )),
        }
    }
}

/// Which fields of an update body this issue's push-direction changes call
/// for, and whether its labels are among them.
///
/// `Labels` is returned separately because it is not a custom field: it rides
/// the issue's `tags` array and needs an id lookup first.
fn push_field_set(changes: &[crate::remote::diff::FieldChange]) -> (FieldSet, bool) {
    let mut fields = FieldSet::default();
    let mut labels = false;
    for change in changes.iter().filter(|c| c.direction == Direction::Push) {
        match change.field {
            Field::Title => fields.title = true,
            Field::Description => fields.description = true,
            Field::State => fields.status = true,
            Field::Priority => fields.priority = true,
            Field::Type => fields.issue_type = true,
            Field::Design => fields.design = true,
            Field::AcceptanceCriteria => fields.acceptance_criteria = true,
            Field::Notes => fields.notes = true,
            Field::CloseReason => fields.close_reason = true,
            Field::Labels => labels = true,
        }
    }
    (fields, labels)
}

const fn any_field(fields: FieldSet) -> bool {
    fields.title
        || fields.description
        || fields.status
        || fields.issue_type
        || fields.priority
        || fields.design
        || fields.acceptance_criteria
        || fields.notes
        || fields.close_reason
        || fields.beads_id
}

/// Tag ids for `labels`, loading the cache on first use.
///
/// Lazy on purpose: the tag list is a request, and a run with no label change
/// must not pay for it.
fn resolve_tags(
    http: &HttpClient,
    cache: &mut Option<TagCache>,
    labels: &[String],
) -> std::result::Result<Vec<String>, RemoteError> {
    if cache.is_none() {
        *cache = Some(TagCache::load(http)?);
    }
    let cache = cache.as_mut().expect("loaded just above");
    let mut ids = Vec::with_capacity(labels.len());
    for label in labels {
        if let Some(id) = cache.resolve_or_create(http, label)? {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn push_links(http: &HttpClient, types: &LinkTypes, plan: &ReconcilePlan, report: &mut PushReport) {
    for issue_plan in &plan.link_changes {
        for change in &issue_plan.changes {
            // The differ selected this link by exactly this id; read and write
            // share one identifier by construction (`link_diff`'s module doc).
            let (outcome, rendered, added) = match change {
                LinkChange::Add {
                    kind,
                    target_readable,
                } => (
                    link_add(
                        http,
                        &issue_plan.remote_id,
                        &types.link_id(*kind, mirrored_direction(*kind)),
                        target_readable,
                    ),
                    format!(
                        "{} + {} {target_readable}",
                        issue_plan.remote_id,
                        kind_name(*kind)
                    ),
                    true,
                ),
                LinkChange::Remove {
                    kind,
                    target_internal_id,
                    target_readable,
                } => (
                    link_remove(
                        http,
                        &issue_plan.remote_id,
                        &types.link_id(*kind, mirrored_direction(*kind)),
                        target_internal_id,
                    ),
                    format!(
                        "{} - {} {target_readable}",
                        issue_plan.remote_id,
                        kind_name(*kind)
                    ),
                    false,
                ),
            };
            match outcome {
                Ok(()) if added => report.links_added.push(rendered),
                Ok(()) => report.links_removed.push(rendered),
                // A failed link write is not the same as an unapplied one.
                // `RemoteError::Transport` flattens a request that never left
                // and a response that died on the way back into one variant,
                // and `http::may_retry` cannot tell them apart either, so it
                // refuses to repeat the `POST`. That leaves this layer to do
                // what the create path already does: check for the write's
                // own effect. Reporting a failure without checking is how a
                // run names work it actually completed — and the re-run it
                // tells you to do then finds nothing to do, because there is
                // nothing left.
                Err(err) => {
                    let recovered = format!("{rendered} {LOST_ANSWER}");
                    match link_change_landed(http, types, &issue_plan.remote_id, change) {
                        Ok(true) if added => report.links_added.push(recovered),
                        Ok(true) => report.links_removed.push(recovered),
                        // Either the write genuinely did not land, or the
                        // check itself could not reach the server — and an
                        // unproven write is reported as a failure, never
                        // assumed.
                        Ok(false) | Err(_) => report.failures.push(format!("{rendered}: {err}")),
                    }
                }
            }
        }
    }
}

/// What a recovered link write is annotated with, so a lost response is
/// visible in the report rather than silently swallowed.
const LOST_ANSWER: &str = "(the answer was lost; the mirror already has it)";

/// Whether the end state `change` asked for holds on the mirror now.
///
/// Selects the link by `LinkTypes::link_id(kind, mirrored_direction(kind))`
/// — the identifier the differ read by and the executor wrote with, so this
/// check cannot disagree with either (see `link_diff`'s module doc).
fn link_change_landed(
    http: &HttpClient,
    types: &LinkTypes,
    remote_id: &str,
    change: &LinkChange,
) -> std::result::Result<bool, RemoteError> {
    let (kind, target_readable) = match change {
        LinkChange::Add {
            kind,
            target_readable,
        }
        | LinkChange::Remove {
            kind,
            target_readable,
            ..
        } => (*kind, target_readable.as_str()),
    };
    let link_id = types.link_id(kind, mirrored_direction(kind));
    let present = fetch_issue_links(http, remote_id, types)?
        .iter()
        .filter(|link| link.link_id == link_id)
        .flat_map(|link| link.issues.iter())
        .any(|issue| issue.id_readable == target_readable);
    Ok(match change {
        LinkChange::Add { .. } => present,
        LinkChange::Remove { .. } => !present,
    })
}

fn push_comments(http: &HttpClient, comments: &[CommentWork], report: &mut PushReport) {
    for work in comments {
        // How many copies of each body this run has already put on this
        // issue. `comment_landed` needs the ordinal of the copy being pushed,
        // not a yes/no — see its doc for why a repeated comment would
        // otherwise recover itself.
        let mut landed: HashMap<&str, usize> = HashMap::new();
        for text in &work.to_push {
            let ordinal = landed.get(text.as_str()).copied().unwrap_or(0) + 1;
            let recovered = if let Err(err) = push_comment(http, &work.remote_id, text) {
                // `unwrap_or(false)` folds the two ways this can fail to
                // prove anything into one: the comment is genuinely not on
                // the mirror, or the check itself could not reach the
                // server. An unproven write is reported as a failure.
                if !comment_landed(http, &work.remote_id, text, ordinal).unwrap_or(false) {
                    report
                        .failures
                        .push(format!("comment on {}: {err}", work.remote_id));
                    continue;
                }
                true
            } else {
                false
            };
            report.comments_pushed += 1;
            report.comments_recovered += usize::from(recovered);
            landed.insert(text.as_str(), ordinal);
        }
    }
}

/// Record a genuine local deletion on the mirror, without deleting anything.
///
/// [`plan_tombstones`] classifies every tombstone in the workspace; this joins
/// its `Delete` entries against [`ReconcilePlan::tombstoned`] by `bead_id` to
/// learn which remote issue to act on. A `Delete` with no counterpart there was
/// never mirrored and needs no remote write at all. A `Forward` is a pure local
/// repair and *cannot* appear in `tombstoned` — a rename's tombstone has its
/// `external_ref` unconditionally cleared, so the pairing already followed the
/// identity on its own (see `crate::remote::tombstone`'s module doc).
///
/// The write is idempotent for a reason worth stating, because it is not
/// obvious: `deleted_state` is deliberately absent from `status_map`, so once
/// it is set the mirrored issue can no longer be read by `reverse_fields`. The
/// next fetch diverts it into `unmappable`, it never reaches the pairing, and
/// this tombstone stops appearing in `plan.tombstoned`. A second run therefore
/// posts neither the state change nor a second `[br] deleted in beads`
/// comment.
fn push_tombstones(
    http: &HttpClient,
    cfg: &RemoteConfig,
    plan: &ReconcilePlan,
    beads: &[Issue],
    report: &mut PushReport,
) {
    let mirrored: HashMap<&str, &str> = plan
        .tombstoned
        .iter()
        .map(|pair| (pair.bead_id.as_str(), pair.remote_id.as_str()))
        .collect();
    for (bead_id, action) in plan_tombstones(beads) {
        let TombstoneAction::Delete { reason } = action else {
            continue;
        };
        let Some(remote_id) = mirrored.get(bead_id.as_str()) else {
            continue;
        };
        let body = deleted_state_body(cfg);
        if let Err(err) = http.post_json(&update_path(remote_id), &body, "issue") {
            report
                .failures
                .push(format!("{bead_id} → {remote_id}: {err}"));
            continue;
        }
        if let Err(err) = push_comment(http, remote_id, &deleted_comment_text(reason.as_deref())) {
            report
                .failures
                .push(format!("deletion comment on {remote_id}: {err}"));
            continue;
        }
        report
            .tombstones_marked
            .push(format!("{bead_id} → {remote_id}"));
    }
}

// ---------------------------------------------------------------------------
// Pull
// ---------------------------------------------------------------------------

/// Everything one pull did, and everything it declined to do.
#[derive(Debug, Clone, Default)]
pub struct PullReport {
    /// `"bds-1: state open → closed"` per field a remote edit won.
    pub fields_applied: Vec<String>,
    /// `(remote id, bead id)` per issue adopted.
    pub adopted: Vec<(String, String)>,
    /// `(bead id, remote id)` per bead paired with an issue an earlier push
    /// created but could not record.
    pub recovered: Vec<(String, String)>,
    /// Rendered refusals from the adoption classifier that the fetch had not
    /// already reported.
    pub refused: Vec<String>,
    /// Rendered deferrals — candidates this run will not touch, and why.
    pub deferred: Vec<String>,
    /// Beads more than one unpaired issue claims. Neither recovered nor
    /// adopted; always reported.
    pub ambiguous: Vec<AmbiguousStamp>,
    pub comments_imported: usize,
    pub failures: Vec<String>,
}

impl PullReport {
    /// Whether everything the plan called for actually happened.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Execute the pull half of `plan`.
///
/// The order matters twice over. Orphaned creates are recovered **first**, so
/// an issue a previous push created but could not record is paired with the
/// bead it belongs to rather than adopted as a second one. Adoption then runs
/// before comment pulls, so an adoptee's comments arrive through
/// `import_comments` — with the `youtrack` author that stops them being pushed
/// straight back — rather than through a comment plan built before the bead
/// existed.
///
/// **Nothing in here checks `--dry-run`**; see [`execute_push`].
///
/// # Errors
/// Returns whatever storage returns from a local write.
pub fn execute_pull(
    storage: &mut SqliteStorage,
    http: &HttpClient,
    cfg: &RemoteConfig,
    id_config: &IdConfig,
    inputs: &RunInputs<'_>,
) -> Result<PullReport> {
    let RunInputs {
        plan,
        comments,
        beads,
        remote,
    } = *inputs;
    let mut report = PullReport::default();

    // First, so an issue an earlier push created but could not record is
    // paired with the bead it belongs to rather than adopted as a second one.
    let orphans = orphaned_creates(plan);
    report.ambiguous.clone_from(&orphans.ambiguous);
    for (bead_id, remote_id) in &orphans.recoverable {
        pair(storage, bead_id, remote_id)?;
        report.recovered.push((bead_id.clone(), remote_id.clone()));
    }

    apply_field_pulls(storage, plan, remote, &mut report)?;
    adopt_everything(
        storage,
        http,
        cfg,
        id_config,
        plan,
        beads,
        remote,
        &orphans,
        &mut report,
    )?;

    for work in comments {
        for comment in &work.to_pull {
            storage.add_comment(&work.bead_id, YOUTRACK_AUTHOR, &comment.text)?;
            report.comments_imported += 1;
        }
    }
    Ok(report)
}

/// Apply the field changes a remote edit won.
///
/// Only `State` and `Priority` can ever resolve this way
/// (`diff::RETURNABLE_FIELDS`), and the value written is taken from the fetched
/// issue rather than parsed back out of the plan's rendered strings — the
/// rendering is for humans and must not become a wire format.
fn apply_field_pulls(
    storage: &mut SqliteStorage,
    plan: &ReconcilePlan,
    remote: &[RemoteIssue],
    report: &mut PullReport,
) -> Result<()> {
    let by_readable: HashMap<&str, &RemoteIssue> = remote
        .iter()
        .map(|issue| (issue.id_readable.as_str(), issue))
        .collect();
    for issue_plan in &plan.field_changes {
        let pulls: Vec<&crate::remote::diff::FieldChange> = issue_plan
            .changes
            .iter()
            .filter(|change| change.direction == Direction::Pull)
            .collect();
        if pulls.is_empty() {
            continue;
        }
        let Some(mirror) = by_readable.get(issue_plan.remote_id.as_str()) else {
            continue;
        };
        let mut updates = IssueUpdate::default();
        for change in &pulls {
            match change.field {
                Field::State => updates.status = Some(mirror.fields.status.clone()),
                Field::Priority => updates.priority = Some(mirror.fields.priority),
                // Unreachable by `direction_for`, and silently writing a
                // local-wins field from the remote would be the one thing this
                // whole direction rule exists to prevent.
                _ => continue,
            }
            report.fields_applied.push(format!(
                "{}: {} {} → {}",
                issue_plan.bead_id,
                change.field.as_str(),
                change.local,
                change.remote
            ));
        }
        storage.update_issue(&issue_plan.bead_id, &updates, REMOTE_ACTOR)?;
    }
    Ok(())
}

/// Classify, order and adopt every unpaired remote issue.
///
/// The call order is the contract: classify each candidate independently (one
/// refusal removes one issue, never the run), order parents before children so
/// a child's bead id can be minted under its parent instead of renamed onto it
/// later, adopt in that order while feeding each new id back into the map, and
/// only then import links — a `Depend` or `Relates` may point at an adoptee
/// that did not exist when the other end was created.
#[allow(clippy::too_many_arguments)]
fn adopt_everything(
    storage: &mut SqliteStorage,
    http: &HttpClient,
    cfg: &RemoteConfig,
    id_config: &IdConfig,
    plan: &ReconcilePlan,
    beads: &[Issue],
    remote: &[RemoteIssue],
    orphans: &OrphanScan,
    report: &mut PullReport,
) -> Result<()> {
    let pairing = pair_workspace(cfg, beads, remote.to_vec());
    // Seeded with the pairings that already exist, plus the ones the orphan
    // recovery just made — an issue recovered above is no longer adoptable.
    let mut id_map: HashMap<String, String> = pairing
        .paired
        .iter()
        .map(|pair| (pair.remote.id_readable.clone(), pair.bead_id.clone()))
        .collect();
    for (bead_id, remote_id) in &report.recovered {
        id_map.insert(remote_id.clone(), bead_id.clone());
    }

    let mut candidates: Vec<AdoptionCandidate> = Vec::new();
    for issue in &pairing.unpaired_remote {
        if id_map.contains_key(&issue.id_readable) {
            continue;
        }
        if orphans.claims(&issue.id_readable) {
            // One of two issues claiming the same bead. Adopting it would turn
            // a disputed pairing into a duplicate bead — the one move that
            // makes an already-wrong state worse. `report.ambiguous` names it.
            continue;
        }
        match classify_adoption(cfg, issue) {
            Ok(candidate) => candidates.push(candidate),
            Err(refusal) => push_unreported_refusal(plan, &refusal, report),
        }
    }

    let order = topological_order(candidates, &id_map);
    report
        .deferred
        .extend(order.deferred.iter().map(DeferredAdoption::render));

    let ctx = AdoptContext {
        id_config,
        actor: YOUTRACK_AUTHOR,
    };
    for candidate in &order.ordered {
        let parent = creation_parent(candidate, &id_map);
        let mut outcome = adopt_one(storage, &ctx, candidate, parent.as_deref())?;
        let remote_id = candidate.remote.id_readable.clone();
        id_map.insert(remote_id.clone(), outcome.bead_id.clone());

        if candidate.remote.comments_count > 0 {
            match fetch_comments(http, &remote_id) {
                Ok(fetched) => {
                    import_comments(storage, &outcome.bead_id, &fetched)?;
                    report.comments_imported += fetched.len();
                }
                Err(err) => report
                    .failures
                    .push(format!("comments of {remote_id}: {err}")),
            }
        }

        // The one remote write an adoption makes, and the one it is allowed to
        // lose: the bead already exists and is already paired.
        let body = json!({
            "customFields": [{
                "name": "Beads ID",
                "$type": "SimpleIssueCustomField",
                "value": outcome.bead_id,
            }]
        });
        stamp_adoption(&mut outcome, &remote_id, || {
            http.post_json(&update_path(&remote_id), &body, "issue")
                .map(|_| ())
        });
        report.adopted.push((remote_id, outcome.bead_id));
    }

    // Second pass, once every adoptee has an id.
    for candidate in &order.ordered {
        import_links(storage, candidate, &id_map)?;
    }
    Ok(())
}

/// Report a classifier refusal only if the fetch has not already reported it.
///
/// `fetch_snapshot` diverts an issue it cannot read into `snapshot.unmappable`,
/// which the plan renders as `refused_adoptions` with the id prefixed, so that
/// is the live path and this arm is defence in depth against a future caller
/// that skips the fetch. Two `RefusedAdoption` types exist for the two shapes;
/// printing both for one issue would tell the user twice, in two wordings, to
/// make one edit.
fn push_unreported_refusal(
    plan: &ReconcilePlan,
    refusal: &RefusedAdoption,
    report: &mut PullReport,
) {
    let already = plan
        .refused_adoptions
        .iter()
        .any(|reported| reported.remote_id == refusal.id_readable);
    if !already {
        report.refused.push(refusal.render());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::plan::Adoption;

    fn config() -> RemoteConfig {
        RemoteConfig::from_yaml_str(include_str!("../../tests/fixtures/remote_em.yaml"))
            .expect("valid config")
    }

    fn unpaired(id: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: "t".to_string(),
            ..Issue::default()
        }
    }

    fn paired(id: &str, external_ref: &str) -> Issue {
        Issue {
            external_ref: Some(external_ref.to_string()),
            ..unpaired(id)
        }
    }

    #[test]
    fn a_workspace_with_no_pairings_is_gated() {
        let beads = vec![unpaired("bds-1"), unpaired("bds-2")];
        let refusal = first_run_gate(&beads, &config(), false).expect_err("must gate");
        assert_eq!(refusal.would_create, 2);
        let rendered = refusal.render();
        assert!(
            rendered.contains("--confirm-initial"),
            "must name the flag: {rendered}"
        );
        assert!(rendered.contains('2'), "must say how many: {rendered}");
    }

    #[test]
    fn confirming_lifts_the_gate() {
        let beads = vec![unpaired("bds-1")];
        assert!(first_run_gate(&beads, &config(), true).is_ok());
    }

    #[test]
    fn one_existing_pairing_lifts_the_gate() {
        // Not a first run: something is already mirrored, so the blast radius
        // of a wrong project is already visible to the operator.
        let beads = vec![unpaired("bds-1"), paired("bds-2", "EM-1")];
        assert!(first_run_gate(&beads, &config(), false).is_ok());
    }

    #[test]
    fn an_out_of_scope_ref_does_not_lift_the_gate() {
        // A JIRA ref is invisible to every verb, so it is not evidence that
        // this project has ever been mirrored.
        let beads = vec![paired("bds-1", "JIRA-123")];
        assert!(
            first_run_gate(&beads, &config(), false).is_err(),
            "only an in-scope pairing counts"
        );
    }

    #[test]
    fn a_tombstone_is_not_counted_as_an_issue_the_run_would_create() {
        let mut gone = unpaired("bds-2");
        gone.status = Status::Tombstone;
        let beads = vec![unpaired("bds-1"), gone];
        let refusal = first_run_gate(&beads, &config(), false).expect_err("must gate");
        assert_eq!(
            refusal.would_create, 1,
            "the tombstone rule owns tombstones; none of them is ever created"
        );
    }

    fn adoption(remote_id: &str, beads_id: Option<&str>) -> Adoption {
        Adoption {
            remote_id: remote_id.to_string(),
            summary: "s".to_string(),
            beads_id: beads_id.map(str::to_string),
        }
    }

    #[test]
    fn an_issue_stamped_with_a_pending_bead_id_is_a_lost_create_not_an_adoption() {
        let plan = ReconcilePlan {
            creates: vec!["bds-1".into(), "bds-2".into()],
            adoptions: vec![adoption("EM-9", Some("bds-2"))],
            ..ReconcilePlan::default()
        };
        let orphans = orphaned_creates(&plan);
        assert_eq!(
            orphans.recoverable.get("bds-2").map(String::as_str),
            Some("EM-9")
        );
        assert_eq!(orphans.recoverable.len(), 1, "{orphans:?}");
        assert!(orphans.ambiguous.is_empty());
    }

    #[test]
    fn a_web_ui_issue_is_never_mistaken_for_a_lost_create() {
        // No `Beads ID` at all, and a stamp naming a bead this run is not
        // creating. Neither is evidence of a create that already happened.
        let plan = ReconcilePlan {
            creates: vec!["bds-1".into()],
            adoptions: vec![adoption("EM-7", None), adoption("EM-8", Some("bds-other"))],
            ..ReconcilePlan::default()
        };
        let orphans = orphaned_creates(&plan);
        assert!(orphans.recoverable.is_empty());
        assert!(orphans.ambiguous.is_empty());
    }

    #[test]
    fn two_issues_claiming_one_bead_are_reported_rather_than_silently_collapsed() {
        // An earlier revision collected straight into a map, so the last issue
        // in fetch order won, the other was adopted as a second bead, and
        // nothing was said about either.
        let plan = ReconcilePlan {
            creates: vec!["bds-1".into()],
            adoptions: vec![
                adoption("EM-8", Some("bds-1")),
                adoption("EM-9", Some("bds-1")),
            ],
            ..ReconcilePlan::default()
        };
        let orphans = orphaned_creates(&plan);

        assert!(
            orphans.recoverable.is_empty(),
            "br cannot choose between them, so it chooses neither: {orphans:?}"
        );
        assert_eq!(
            orphans.ambiguous,
            vec![AmbiguousStamp {
                bead_id: "bds-1".into(),
                remote_ids: vec!["EM-8".into(), "EM-9".into()],
            }]
        );
        assert!(orphans.is_ambiguous("bds-1"), "the create is skipped");
        assert!(orphans.claims("EM-8"), "and neither issue is adopted");
        assert!(orphans.claims("EM-9"));

        let rendered = orphans.ambiguous[0].render();
        assert!(rendered.contains("bds-1"), "{rendered}");
        assert!(
            rendered.contains("EM-8") && rendered.contains("EM-9"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Beads ID"),
            "must say how to fix it: {rendered}"
        );
    }

    /// A comment write whose response never arrived. Same shape as the link
    /// case below, and the same reason: the count gate makes a landed
    /// comment invisible to the next run, so a failure reported here names
    /// work that is already done and nothing ever revisits it.
    mod comment_writes_whose_response_was_lost {
        use super::*;
        use crate::remote::http::{RetryPolicy, Token};
        use test_support::mock_http::MockServer;

        const POST_PATH: &str = "/api/issues/EM-27/comments?fields=id";
        const READ_PATH: &str =
            "/api/issues/EM-27/comments?fields=id,text,author(login),created&$skip=0&$top=500";

        fn client(server: &MockServer) -> HttpClient {
            HttpClient::new(&server.base_url(), Token::new("t"), RetryPolicy::none())
        }

        fn work(texts: &[&str]) -> Vec<CommentWork> {
            vec![CommentWork {
                bead_id: "bds-1".into(),
                remote_id: "EM-27".into(),
                to_push: texts.iter().map(|t| (*t).to_string()).collect(),
                to_pull: Vec::new(),
            }]
        }

        /// One `[br]` comment carrying `body`, as the mirror reports it.
        fn echo(id: &str, body: &str) -> String {
            format!(
                r#"{{"id":"{id}","text":"[br]\n{body}","author":{{"login":"anton"}},"created":1786881856457}}"#
            )
        }

        #[test]
        fn a_landed_comment_is_reported_as_done_not_failed() {
            let server = MockServer::start();
            server.on_drop("POST", POST_PATH);
            server.on(
                "GET",
                READ_PATH,
                200,
                &format!("[{}]", echo("7-1", "hello")),
            );

            let mut report = PushReport::default();
            push_comments(&client(&server), &work(&["[br]\nhello"]), &mut report);

            assert!(report.failures.is_empty(), "{report:?}");
            assert_eq!(report.comments_pushed, 1, "{report:?}");
            assert_eq!(report.comments_recovered, 1, "{report:?}");
        }

        #[test]
        fn a_comment_that_never_landed_is_still_a_failure() {
            let server = MockServer::start();
            server.on_drop("POST", POST_PATH);
            server.on("GET", READ_PATH, 200, "[]");

            let mut report = PushReport::default();
            push_comments(&client(&server), &work(&["[br]\nhello"]), &mut report);

            assert_eq!(report.comments_pushed, 0, "{report:?}");
            assert_eq!(report.comments_recovered, 0, "{report:?}");
            assert_eq!(report.failures.len(), 1, "{report:?}");
            assert!(
                report.failures[0].contains("EM-27"),
                "{:?}",
                report.failures
            );
        }

        /// The duplicate-body case, which is the whole reason the check
        /// counts rather than asking "is it there?". A bead that says the
        /// same thing twice puts two identical bodies in `to_push`; if the
        /// first lands and the second is lost for real, the first one's copy
        /// must not be mistaken for the second's.
        #[test]
        fn the_first_copy_does_not_recover_the_second() {
            let server = MockServer::start();
            // One create answered, then the connection drops.
            server.on_sequence("POST", POST_PATH, vec![(200, r#"{"id":"7-1"}"#.into())]);
            server.on_drop_after("POST", POST_PATH, 1);
            // The mirror shows exactly one copy: the first push's.
            server.on(
                "GET",
                READ_PATH,
                200,
                &format!("[{}]", echo("7-1", "hello")),
            );

            let mut report = PushReport::default();
            push_comments(
                &client(&server),
                &work(&["[br]\nhello", "[br]\nhello"]),
                &mut report,
            );

            assert_eq!(
                report.comments_pushed, 1,
                "only the first landed: {report:?}"
            );
            assert_eq!(report.comments_recovered, 0, "{report:?}");
            assert_eq!(report.failures.len(), 1, "{report:?}");
        }

        /// And the mirror showing both copies means both landed.
        #[test]
        fn both_copies_present_recovers_the_second() {
            let server = MockServer::start();
            server.on_sequence("POST", POST_PATH, vec![(200, r#"{"id":"7-1"}"#.into())]);
            server.on_drop_after("POST", POST_PATH, 1);
            server.on(
                "GET",
                READ_PATH,
                200,
                &format!("[{},{}]", echo("7-1", "hello"), echo("7-2", "hello")),
            );

            let mut report = PushReport::default();
            push_comments(
                &client(&server),
                &work(&["[br]\nhello", "[br]\nhello"]),
                &mut report,
            );

            assert!(report.failures.is_empty(), "{report:?}");
            assert_eq!(report.comments_pushed, 2, "{report:?}");
            assert_eq!(report.comments_recovered, 1, "{report:?}");
        }
    }

    /// A link write whose response never arrived, and the mirror that shows
    /// the write landed anyway.
    ///
    /// This is the link half of the create recovery this module's doc
    /// describes. `Connection reset by peer` after the server applied the
    /// POST is indistinguishable here from one before it was sent, and
    /// `http::may_retry` refuses to repeat a `POST` for exactly that reason
    /// — so the check is for the write's own effect.
    mod link_writes_whose_response_was_lost {
        use super::*;
        use crate::remote::http::{RetryPolicy, Token};
        use crate::remote::link_diff::LinkChange;
        use crate::remote::youtrack::links::LinkKind;
        use test_support::mock_http::MockServer;

        const ADD_PATH: &str = "/api/issues/EM-27/links/173-1s/issues?fields=idReadable";
        const READ_PATH: &str =
            "/api/issues/EM-27?fields=links(id,direction,linkType(id,name),issues(id,idReadable))";

        fn types() -> LinkTypes {
            LinkTypes {
                subtask: "173-3".into(),
                depend: "173-1".into(),
                relates: "173-0".into(),
            }
        }

        fn plan_adding_em_29() -> ReconcilePlan {
            ReconcilePlan {
                link_changes: vec![crate::remote::plan::IssueLinkPlan {
                    bead_id: "bds-1".into(),
                    remote_id: "EM-27".into(),
                    changes: vec![LinkChange::Add {
                        kind: LinkKind::Depend,
                        target_readable: "EM-29".into(),
                    }],
                }],
                ..ReconcilePlan::default()
            }
        }

        fn client(server: &MockServer) -> HttpClient {
            HttpClient::new(&server.base_url(), Token::new("t"), RetryPolicy::none())
        }

        #[test]
        fn a_landed_add_is_reported_as_done_not_failed() {
            let server = MockServer::start();
            server.on_drop("POST", ADD_PATH);
            server.on(
                "GET",
                READ_PATH,
                200,
                r#"{"links":[{"id":"173-1s","direction":"OUTWARD","linkType":{"id":"173-1","name":"Depend"},"issues":[{"id":"3-221","idReadable":"EM-29"}]}]}"#,
            );

            let mut report = PushReport::default();
            push_links(
                &client(&server),
                &types(),
                &plan_adding_em_29(),
                &mut report,
            );

            assert!(
                report.failures.is_empty(),
                "the link is on the mirror; nothing failed: {:?}",
                report.failures
            );
            assert_eq!(report.links_added.len(), 1, "{report:?}");
            assert!(
                report.links_added[0].contains("EM-27 + blocks EM-29"),
                "{:?}",
                report.links_added
            );
        }

        #[test]
        fn a_landed_removal_is_reported_as_done_not_failed() {
            let server = MockServer::start();
            server.on_drop("DELETE", "/api/issues/EM-27/links/173-1s/issues/3-221");
            // The bucket is gone from the read, so the removal was applied.
            server.on("GET", READ_PATH, 200, r#"{"links":[]}"#);

            let plan = ReconcilePlan {
                link_changes: vec![crate::remote::plan::IssueLinkPlan {
                    bead_id: "bds-1".into(),
                    remote_id: "EM-27".into(),
                    changes: vec![LinkChange::Remove {
                        kind: LinkKind::Depend,
                        target_internal_id: "3-221".into(),
                        target_readable: "EM-29".into(),
                    }],
                }],
                ..ReconcilePlan::default()
            };
            let mut report = PushReport::default();
            push_links(&client(&server), &types(), &plan, &mut report);

            assert!(report.failures.is_empty(), "{report:?}");
            assert_eq!(report.links_removed.len(), 1, "{report:?}");
            assert!(
                report.links_removed[0].contains("EM-27 - blocks EM-29"),
                "{:?}",
                report.links_removed
            );
        }

        /// The recovered write says so. A run that silently reported a
        /// dropped response as an ordinary success would hide a real network
        /// fault behind a clean report.
        #[test]
        fn a_recovered_write_says_the_answer_was_lost() {
            let server = MockServer::start();
            server.on_drop("POST", ADD_PATH);
            server.on(
                "GET",
                READ_PATH,
                200,
                r#"{"links":[{"id":"173-1s","direction":"OUTWARD","linkType":{"id":"173-1","name":"Depend"},"issues":[{"id":"3-221","idReadable":"EM-29"}]}]}"#,
            );

            let mut report = PushReport::default();
            push_links(
                &client(&server),
                &types(),
                &plan_adding_em_29(),
                &mut report,
            );

            assert!(
                report.links_added[0].contains(LOST_ANSWER),
                "{:?}",
                report.links_added
            );
        }

        #[test]
        fn an_add_that_never_landed_is_still_a_failure() {
            let server = MockServer::start();
            server.on_drop("POST", ADD_PATH);
            server.on("GET", READ_PATH, 200, r#"{"links":[]}"#);

            let mut report = PushReport::default();
            push_links(
                &client(&server),
                &types(),
                &plan_adding_em_29(),
                &mut report,
            );

            assert!(report.links_added.is_empty(), "{report:?}");
            assert_eq!(report.failures.len(), 1, "{report:?}");
            assert!(
                report.failures[0].contains("EM-27 + blocks EM-29"),
                "{:?}",
                report.failures
            );
        }
    }
}
