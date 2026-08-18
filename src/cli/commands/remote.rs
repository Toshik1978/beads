//! `br remote` — mirroring this workspace into an external tracker.
//!
//! This layer owns four things and nothing else: reading `.beads/remote.yaml`,
//! collecting the workspace's own vocabulary out of SQLite, assembling one
//! fetched picture of both sides per verb, and printing what came back. Every
//! decision about *what* to write to YouTrack lives in
//! `crate::remote::youtrack`, and every decision about *when* in
//! `crate::remote::execute`; neither ever sees an `OutputContext`.
//!
//! ## Where `--dry-run` is enforced
//!
//! In exactly one place per mutating verb: a single branch immediately before
//! the call into `execute_pull`/`execute_push`, after the plan has been
//! printed. Not per call site — a check spread across the twenty writes those
//! two functions make is a check a twenty-first write can forget, silently, and
//! the only thing that would notice is a user watching a `--dry-run` change
//! their tracker.
//!
//! ## The order of the push refusals
//!
//! `preflight_vocabulary` first, before the network is touched at all; then the
//! plan's per-bead unmapped-value refusal, which can name the bead; then the
//! first-run gate; then the dry-run branch. Each is cheaper and broader than
//! the one after it, and every one of them refuses before a single write.

use crate::cli::{RemoteCommands, RemoteInitArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::output::OutputContext;
use crate::remote::config::RemoteConfig;
use crate::remote::execute::{
    BatchProgress, CreateReport, PullReport, PushReport, Reconciliation, execute_pull,
    execute_push, first_run_gate, reconcile,
};
use crate::remote::http::HttpClient;
use crate::remote::plan::{PlanScope, ReconcilePlan};
use crate::remote::youtrack::admin::AdminClient;
use crate::remote::youtrack::init::{self, InitOptions, InitReport, WorkspaceVocabulary};
use crate::remote::youtrack::links::LinkTypes;
use crate::storage::{ListFilters, SqliteStorage};
use serde::Serialize;

/// Execute the remote command.
///
/// # Errors
///
/// Returns an error when the workspace cannot be discovered, `remote.yaml` is
/// missing or invalid, the credential is unset, or the backend refuses.
pub fn execute(
    command: &RemoteCommands,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let cfg = RemoteConfig::load(&beads_dir)?;

    match command {
        RemoteCommands::Init(args) => {
            let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
            let vocab = workspace_vocabulary(&storage_ctx.storage)?;
            let http = HttpClient::from_env(&cfg)?;
            let admin = AdminClient::new(http);
            let report = init::run(&cfg, &admin, &vocab, options_from(args))?;
            print_init_report(&report, json, ctx);
            Ok(())
        }
        RemoteCommands::Status(_) => {
            let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
            // Every request below is a GET, and nothing writes. The e2e proves
            // it from the other side, by asserting the mock server saw no
            // state-changing method at all.
            let (http, _types, run) = read_both_sides(&cfg, &storage_ctx.storage)?;
            print_status(&cfg, &run.plan, &http, json, ctx);
            Ok(())
        }
        RemoteCommands::Pull(args) => {
            let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
            let layer = storage_ctx.load_config(cli)?;
            let id_config = config::id_config_from_layer(&layer);
            let report = pull(
                &cfg,
                &mut storage_ctx.storage,
                &id_config,
                args.dry_run,
                ctx,
            )?;
            finish("br remote pull", report.is_none_or(|r| r.is_clean()))
        }
        RemoteCommands::Push(args) => {
            let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
            let report = push(
                &cfg,
                &mut storage_ctx.storage,
                args.dry_run,
                args.confirm_initial,
                ctx,
            )?;
            finish("br remote push", report.is_none_or(|r| r.is_clean()))
        }
        // `sync` is `pull` then `push`, in that order, each reconciling
        // afresh. Pull first so an adoption that lands this run is already a
        // bead by the time push computes its link diff — otherwise the
        // newly-adopted issue looks unpaired to the push half and the link
        // differ tries to remove links it has not learned about yet.
        RemoteCommands::Sync(args) => {
            let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
            let layer = storage_ctx.load_config(cli)?;
            let id_config = config::id_config_from_layer(&layer);
            let pulled = pull(
                &cfg,
                &mut storage_ctx.storage,
                &id_config,
                args.dry_run,
                ctx,
            )?;
            let pushed = push(
                &cfg,
                &mut storage_ctx.storage,
                args.dry_run,
                args.confirm_initial,
                ctx,
            )?;
            finish(
                "br remote sync",
                pulled.is_none_or(|r| r.is_clean()) && pushed.is_none_or(|r| r.is_clean()),
            )
        }
    }
}

/// The exit status a verb that reported its own failures returns.
fn finish(command: &str, clean: bool) -> Result<()> {
    if clean {
        return Ok(());
    }
    Err(BeadsError::ExternalCommand {
        command: command.to_string(),
        reason: "some of this run's work did not complete; the lines above name each one. \
                 Everything that did complete is durable, and re-running does only the \
                 remainder."
            .to_string(),
    })
}

/// Build one `Reconciliation` and the client it was read through.
///
/// The client and the resolved link types outlive the reconciliation: a push
/// re-plans after its creates land and needs both again, without paying for a
/// second link-type lookup.
///
/// # Errors
/// Returns whatever the credential lookup, the link-type resolution, the fetch
/// or the storage reads return.
fn read_both_sides(
    cfg: &RemoteConfig,
    storage: &SqliteStorage,
) -> Result<(HttpClient, LinkTypes, Reconciliation)> {
    let http = HttpClient::from_env(cfg)?;
    let types = LinkTypes::resolve(&http)?;
    let run = reconcile(storage, &http, cfg, &types)?;
    Ok((http, types, run))
}

/// `br remote pull`. `Ok(None)` means `--dry-run`: nothing was written.
fn pull(
    cfg: &RemoteConfig,
    storage: &mut SqliteStorage,
    id_config: &crate::util::id::IdConfig,
    dry_run: bool,
    ctx: &OutputContext,
) -> Result<Option<PullReport>> {
    let (http, _types, run) = read_both_sides(cfg, storage)?;
    print_plan(&run.plan, PlanScope::Pull, ctx);
    // THE dry-run check for this verb: one branch, at the single point where
    // execution begins, so a step added to `execute_pull` cannot forget it.
    if dry_run {
        ctx.print_line("br remote pull --dry-run: nothing was written.");
        return Ok(None);
    }
    let report = execute_pull(storage, &http, cfg, id_config, &run.inputs())?;
    print_pull_report(&report, ctx);
    Ok(Some(report))
}

/// `br remote push`. `Ok(None)` means `--dry-run`: nothing was written.
fn push(
    cfg: &RemoteConfig,
    storage: &mut SqliteStorage,
    dry_run: bool,
    confirm_initial: bool,
    ctx: &OutputContext,
) -> Result<Option<PushReport>> {
    // Before the network is touched at all: a value no map covers cannot be
    // written and cannot be read back, so a push discovering it halfway
    // through has already written some of the issues and not others.
    let vocab = workspace_vocabulary(storage)?;
    init::preflight_vocabulary(cfg, &vocab)?;

    let (http, types, run) = read_both_sides(cfg, storage)?;
    refuse_unmapped_locals(&run.plan)?;
    // A first run against the wrong project is not undoable — br has no code
    // path that deletes a YouTrack issue — so the gate comes before any write
    // and after the plan, which is printed so the refusal shows what it
    // refused.
    print_plan(&run.plan, PlanScope::Push, ctx);
    first_run_gate(&run.beads, cfg, confirm_initial).map_err(|refusal| {
        BeadsError::ExternalCommand {
            command: "br remote push".to_string(),
            reason: refusal.render(),
        }
    })?;
    // THE dry-run check for this verb; see `pull`.
    if dry_run {
        ctx.print_line("br remote push --dry-run: nothing was written.");
        return Ok(None);
    }

    let mut progress = |progress: BatchProgress| {
        ctx.print_line(&format!(
            "created {}/{} issue(s)",
            progress.done, progress.total
        ));
    };
    let report = execute_push(storage, &http, cfg, &types, &run.inputs(), &mut progress)?;
    print_push_report(&report, ctx);
    Ok(Some(report))
}

/// Refuse a push that would meet a local value no map covers.
///
/// `preflight_vocabulary` catches the same thing one layer earlier and from
/// the workspace's own vocabulary; this catches it from the plan, which knows
/// *which bead* carries the value. Naming the bead is the difference between a
/// diagnosis and a search.
fn refuse_unmapped_locals(plan: &ReconcilePlan) -> Result<()> {
    if plan.unmapped_local.is_empty() {
        return Ok(());
    }
    let detail: Vec<String> = plan
        .unmapped_local
        .iter()
        .map(|entry| {
            format!(
                "  {}: {} '{}' — add it to {} in remote.yaml",
                entry.bead_id, entry.field, entry.value, entry.map_key
            )
        })
        .collect();
    Err(BeadsError::ExternalCommand {
        command: "br remote push".to_string(),
        reason: format!(
            "this workspace holds values .beads/remote.yaml does not map, and a push \
             cannot write one:\n{}",
            detail.join("\n")
        ),
    })
}

fn print_plan(plan: &ReconcilePlan, scope: PlanScope, ctx: &OutputContext) {
    for line in plan.render_scoped(scope).lines() {
        ctx.print_line(line);
    }
}

fn print_create_report(report: &CreateReport, ctx: &OutputContext) {
    for (bead_id, remote_id) in &report.created {
        ctx.print_line(&format!("created {remote_id} for {bead_id}"));
    }
    for (bead_id, remote_id) in &report.recovered {
        ctx.print_line(&format!(
            "paired {bead_id} with {remote_id}, which an earlier run created but could not \
             record; no issue was created for it"
        ));
    }
    for entry in &report.ambiguous {
        ctx.print_line(&format!("ambiguous: {}", entry.render()));
    }
    for (bead_id, reason) in &report.failed {
        ctx.print_line(&format!(
            "failed to create an issue for {bead_id}: {reason}"
        ));
    }
}

fn print_push_report(report: &PushReport, ctx: &OutputContext) {
    print_create_report(&report.creates, ctx);
    if report.replanned_after > 0 {
        // Said out loud because it is the difference between a mirrored
        // project and a half-mirrored one: the plan printed above was built
        // before these issues existed, so it could not name their links,
        // comments or labels.
        ctx.print_line(&format!(
            "re-planned after {} new pairing(s) so this same push could mirror their links, \
             comments and labels",
            report.replanned_after
        ));
    }
    for entry in &report.fields_updated {
        ctx.print_line(&format!("updated {entry}"));
    }
    for entry in report.links_added.iter().chain(&report.links_removed) {
        ctx.print_line(&format!("link {entry}"));
    }
    if report.comments_pushed > 0 {
        ctx.print_line(&format!("pushed {} comment(s)", report.comments_pushed));
    }
    if report.comments_recovered > 0 {
        // Said out loud because the alternative readings are both wrong: a
        // silent success hides that the connection failed at all, and a
        // failure would name work that is done.
        ctx.print_line(&format!(
            "{} of those were already on the mirror — the write had landed and only its answer \
             was lost",
            report.comments_recovered
        ));
    }
    for entry in &report.tombstones_marked {
        ctx.print_line(&format!(
            "marked {entry} as {} and commented on it; no issue was deleted",
            "deleted in beads"
        ));
    }
    if !report.tags_skipped.is_empty() {
        ctx.print_line(&format!(
            "labels br could not create or attach (permission-gated; the issues were still \
             written): {}",
            report.tags_skipped.join(", ")
        ));
    }
    if report.left_to_pull > 0 || report.comments_left_to_pull > 0 {
        // Otherwise nothing distinguishes a `[YouTrack wins]` line in the plan
        // above from work this push actually did — and that applies to a
        // comment pull exactly as much as a field pull, so both are named
        // here rather than just the one the count used to cover.
        let mut left = Vec::new();
        if report.left_to_pull > 0 {
            left.push(format!("{} field change(s)", report.left_to_pull));
        }
        if report.comments_left_to_pull > 0 {
            left.push(format!("{} comment(s)", report.comments_left_to_pull));
        }
        let what = left.join(" and ");
        if report.replanned_after > 0 {
            // The plan printed above predates this push's creates; the counts
            // here come from the fresh read taken after they landed (see
            // `execute_push`'s re-plan), which can disagree with what is
            // "above" if the remote changed in between. Say so rather than
            // pointing at a plan these numbers may not match.
            ctx.print_line(&format!(
                "{what} are marked [YouTrack wins] in a fresh read taken after this push's \
                 creates landed, which can differ from the plan printed above; `br remote pull` \
                 is what applies them"
            ));
        } else {
            ctx.print_line(&format!(
                "{what} above are marked [YouTrack wins] and are `br remote pull`'s work; this \
                 push did not apply them"
            ));
        }
    }
    for failure in &report.failures {
        ctx.print_line(&format!("failed: {failure}"));
    }
}

fn print_pull_report(report: &PullReport, ctx: &OutputContext) {
    for (bead_id, remote_id) in &report.recovered {
        ctx.print_line(&format!(
            "paired {bead_id} with {remote_id}, which an earlier push created but could not \
             record; it was not adopted as a second bead"
        ));
    }
    for entry in &report.fields_applied {
        ctx.print_line(&format!("YouTrack wins — {entry}"));
    }
    for (remote_id, bead_id) in &report.adopted {
        ctx.print_line(&format!("adopted {remote_id} as {bead_id}"));
    }
    if report.comments_imported > 0 {
        ctx.print_line(&format!("imported {} comment(s)", report.comments_imported));
    }
    // Refusals and deferrals are first-class output, not diagnostics: an issue
    // that silently never arrives is a user watching for it, run after run.
    for entry in &report.refused {
        ctx.print_line(&format!("refused: {entry}"));
    }
    for entry in &report.deferred {
        ctx.print_line(&format!("deferred: {entry}"));
    }
    for entry in &report.ambiguous {
        ctx.print_line(&format!("ambiguous: {}", entry.render()));
    }
    for failure in &report.failures {
        ctx.print_line(&format!("failed: {failure}"));
    }
}

fn options_from(args: &RemoteInitArgs) -> InitOptions {
    InitOptions {
        allow_shared_bundle: args.allow_shared_bundle,
        keep_project_defaults: args.keep_project_defaults,
        dry_run: args.dry_run,
    }
}

/// The distinct `issue_type`, `status` and `priority` values this workspace
/// actually holds — every issue, closed and deferred included, because a
/// value the maps do not cover blocks a push whether or not the issue is
/// still open.
///
/// `priorities` is populated even though `Priority`'s 0..=4 range is meant to
/// be closed: `Priority` derives `#[serde(transparent)] Deserialize` with no
/// bound of its own, and nothing re-validates a row already read back out of
/// `beads.db`. A hand-edited `issues.jsonl` is not the live vector —
/// `IssueValidator` rejects an out-of-range value at JSONL import, and
/// SQLite's own `CHECK(priority >= 0 AND priority <= 4)` rejects one on
/// write — but neither guard runs again on a plain read, so a row that
/// reached storage by some other path (a corrupted `beads.db`, a future
/// migration bug) would come back out unchecked. `preflight_vocabulary`'s
/// priority loop is the belt-and-braces check for exactly that row; leaving
/// this set empty made the loop dead code no path ever populated.
fn workspace_vocabulary(storage: &SqliteStorage) -> Result<WorkspaceVocabulary> {
    let filters = ListFilters {
        include_closed: true,
        include_deferred: true,
        ..ListFilters::default()
    };
    let mut vocab = WorkspaceVocabulary::default();
    for issue in storage.list_issues(&filters)? {
        vocab.types.insert(issue.issue_type.as_str().to_string());
        vocab.statuses.insert(issue.status.as_str().to_string());
        vocab.priorities.insert(issue.priority.0.to_string());
    }
    Ok(vocab)
}

#[derive(Debug, Serialize)]
struct StatusJson<'a> {
    project: &'a str,
    plan: &'a ReconcilePlan,
    /// Read requests issued. Its companion is the point: `br remote status`
    /// reports and never writes, so `writes` is always 0.
    reads: u32,
    writes: u32,
}

fn print_status(
    cfg: &RemoteConfig,
    plan: &ReconcilePlan,
    http: &HttpClient,
    json: bool,
    ctx: &OutputContext,
) {
    if json {
        ctx.json(&StatusJson {
            project: &cfg.project,
            plan,
            reads: http.read_count(),
            writes: http.write_count(),
        });
        return;
    }
    for line in plan.render().lines() {
        ctx.print_line(line);
    }
}

#[derive(Debug, Serialize)]
struct InitJson<'a> {
    dry_run: bool,
    prototypes_created: &'a [String],
    fields_attached: &'a [String],
    values_added: Vec<ValuesAddedJson<'a>>,
    clones: Vec<CloneJson<'a>>,
    defaults_changed: Vec<DefaultJson<'a>>,
    projects_seen: &'a [String],
    notes: &'a [String],
}

#[derive(Debug, Serialize)]
struct ValuesAddedJson<'a> {
    field: &'a str,
    values: &'a [String],
}

#[derive(Debug, Serialize)]
struct CloneJson<'a> {
    field: &'a str,
    source: &'a str,
    clone: &'a str,
    /// `true` when `clone` already existed — an orphan left by an earlier,
    /// interrupted run — and this run adopted it rather than creating it.
    adopted: bool,
}

#[derive(Debug, Serialize)]
struct DefaultJson<'a> {
    field: &'a str,
    old: &'a [String],
    new: &'a str,
}

fn print_init_report(report: &InitReport, json: bool, ctx: &OutputContext) {
    if json {
        let payload = InitJson {
            dry_run: report.dry_run,
            prototypes_created: &report.prototypes_created,
            fields_attached: &report.fields_attached,
            values_added: report
                .values_added
                .iter()
                .map(|(field, values)| ValuesAddedJson {
                    field,
                    values: values.as_slice(),
                })
                .collect(),
            clones: report
                .clones
                .iter()
                .map(|c| CloneJson {
                    field: &c.field_name,
                    source: &c.source,
                    clone: &c.clone,
                    adopted: c.adopted,
                })
                .collect(),
            defaults_changed: report
                .defaults_changed
                .iter()
                .map(|c| DefaultJson {
                    field: &c.field_name,
                    old: &c.old,
                    new: &c.new,
                })
                .collect(),
            projects_seen: &report.projects_seen,
            notes: &report.notes,
        };
        ctx.json(&payload);
        return;
    }

    // The sharedness-scan disclosure is not repeated here: `init::run` already
    // announces it to stderr (as "bundle sharedness scanned across …") before
    // the first bundle write, which is the moment that disclosure exists to
    // serve — by the time this report prints, any write it should have
    // informed has already happened. `report.projects_seen` still carries the
    // same data for `--json` callers below.
    if report.dry_run {
        ctx.print_line("br remote init --dry-run: nothing was written.");
    }
    print_list(
        ctx,
        "field prototypes created (INSTANCE-WIDE — visible in every project)",
        &report.prototypes_created,
    );
    print_list(
        ctx,
        "fields attached to the project",
        &report.fields_attached,
    );
    for (field, values) in &report.values_added {
        ctx.print_line(&format!("{field}: added {}", values.join(", ")));
    }
    for clone in &report.clones {
        if clone.adopted {
            ctx.print_line(&format!(
                "{}: bundle '{}' was shared, so the project now uses '{}', a copy an earlier \
                 interrupted run left behind and this run adopted rather than duplicated",
                clone.field_name, clone.source, clone.clone
            ));
        } else {
            ctx.print_line(&format!(
                "{}: bundle '{}' was shared, so the project now uses a copy named '{}'",
                clone.field_name, clone.source, clone.clone
            ));
        }
    }
    for change in &report.defaults_changed {
        let old = if change.old.is_empty() {
            "(none)".to_string()
        } else {
            change.old.join(", ")
        };
        ctx.print_line(&format!(
            "default for '{}': {old} → {} (a new issue created in the web UI adopts as this)",
            change.field_name, change.new
        ));
    }
    for note in &report.notes {
        ctx.print_line(note);
    }
    if report.prototypes_created.is_empty()
        && report.fields_attached.is_empty()
        && report.values_added.is_empty()
        && report.clones.is_empty()
        && report.defaults_changed.is_empty()
    {
        ctx.print_line("the remote project already matches this workspace's maps; nothing to do.");
    }
}

fn print_list(ctx: &OutputContext, label: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    ctx.print_line(&format!("{label}: {}", values.join(", ")));
}
