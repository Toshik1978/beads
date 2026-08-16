//! `br remote` — mirroring this workspace into an external tracker.
//!
//! This layer owns three things and nothing else: reading `.beads/remote.yaml`,
//! collecting the workspace's own vocabulary out of SQLite, and printing what
//! the backend reports. Every decision about *what* to write to YouTrack lives
//! in `crate::remote::youtrack`, which never sees a `SqliteStorage` or an
//! `OutputContext`.

use crate::cli::{RemoteCommands, RemoteInitArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::Issue;
use crate::output::OutputContext;
use crate::remote::config::RemoteConfig;
use crate::remote::http::HttpClient;
use crate::remote::plan::{ReconcilePlan, build_plan};
use crate::remote::youtrack::admin::AdminClient;
use crate::remote::youtrack::fetch::fetch_snapshot;
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
            let issues = hydrated_issues(&storage_ctx.storage)?;
            let http = HttpClient::from_env(&cfg)?;
            // Both of these are GETs, and nothing below them writes. The e2e
            // proves it from the other side, by asserting the mock server saw
            // no state-changing method at all.
            let types = LinkTypes::resolve(&http)?;
            // The non-fatal fetch on purpose: a remote value this config
            // cannot map is exactly what `status` exists to report, so it
            // must land in the plan rather than abort the command.
            let snapshot = fetch_snapshot(&http, &cfg, &types)?;
            let plan = build_plan(&cfg, &issues, snapshot, &types);
            print_status(&cfg, &plan, &http, json, ctx);
            Ok(())
        }
        RemoteCommands::Push(_) | RemoteCommands::Pull(_) | RemoteCommands::Sync(_) => {
            Err(BeadsError::ExternalCommand {
                command: "br remote".to_string(),
                reason: "this subcommand is declared but not implemented yet; \
                     `br remote init` and `br remote status` are the ones that run today"
                    .to_string(),
            })
        }
    }
}

fn options_from(args: &RemoteInitArgs) -> InitOptions {
    InitOptions {
        allow_shared_bundle: args.allow_shared_bundle,
        keep_project_defaults: args.keep_project_defaults,
        dry_run: args.dry_run,
    }
}

/// The distinct `issue_type` and `status` values this workspace actually
/// holds — every issue, closed and deferred included, because a value the
/// maps do not cover blocks a push whether or not the issue is still open.
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
    }
    Ok(vocab)
}

/// Every issue with its relations and labels attached.
///
/// `list_issues` returns bare rows: it does not populate `dependencies`,
/// `labels` or `comments`, and the link differ is only as good as the
/// relations it is handed. The three export readers below are the same ones
/// the JSONL export uses, and they cost three queries rather than 3N.
fn hydrated_issues(storage: &SqliteStorage) -> Result<Vec<Issue>> {
    let mut issues = storage.get_all_issues_for_export()?;
    let mut dependencies = storage.get_all_dependency_records()?;
    let mut labels = storage.get_labels_for_export()?;
    for issue in &mut issues {
        if let Some(rows) = dependencies.remove(&issue.id) {
            issue.dependencies = rows;
        }
        if let Some(names) = labels.remove(&issue.id) {
            issue.labels = names;
        }
    }
    Ok(issues)
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

    ctx.print_line(&format!(
        "sharedness scanned across {} project(s): {}",
        report.projects_seen.len(),
        if report.projects_seen.is_empty() {
            "none".to_string()
        } else {
            report.projects_seen.join(", ")
        }
    ));
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
        ctx.print_line(&format!(
            "{}: bundle '{}' was shared, so the project now uses a copy named '{}'",
            clone.field_name, clone.source, clone.clone
        ));
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
