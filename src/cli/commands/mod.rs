use crate::config::OpenStorageResult;
use crate::error::BeadsError;
use crate::format::sanitize_terminal_text;
use crate::model::Issue;
use crate::output::OutputContext;
use crate::storage::{IssueUpdate, SqliteStorage};
use crate::sync::auto_import_if_stale;
use crate::util::id::{IdExistence, IdResolver};
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

pub mod blocked;
pub mod close;
pub mod comments;
pub mod completions;
pub mod config;
pub mod create;
pub mod delete;
pub mod dep;
pub mod detach;
pub mod epic;
pub mod history;
pub mod info;
pub mod init;
pub mod label;
pub mod list;
pub mod ready;
pub mod rename;
pub mod reopen;
pub mod search;
pub mod show;
pub mod stale;
pub mod stats;
pub mod sync;
pub mod update;
pub mod version;
pub mod vocabulary;

/// Describe a routed fan-out that failed partway through, in terms of which
/// targets were written (bds-j1m, bds-3x6).
///
/// Every route-aware command opens, mutates and finalizes one workspace before
/// it opens the next, so once an earlier route has committed there is nothing
/// to roll back — each route is a separate database and a separate
/// transaction. The only honest response is to say what landed.
///
/// `route_targets` holds each route's issue IDs **as the caller supplied
/// them**, grouped per route in execution order. That is deliberately not the
/// resolved IDs: a route that never ran was never opened, so its inputs were
/// never resolved, and reporting canonical IDs for the routes that ran beside
/// raw inputs for the ones that did not would make one field mean two things.
///
/// `attempted_route_has_mutated[i]` says whether route `i` actually wrote
/// anything, so its length is the number of routes attempted and its last
/// entry belongs to the route that failed. A route that succeeded without
/// writing — every issue already closed, every detach a no-op — is reported as
/// untouched, not as written: `applied` means "this landed and cannot be
/// undone", and a route that did nothing has nothing to undo.
///
/// The failing route's own targets are reported as possibly-partly-written
/// when it had already committed something, and as untouched when it had not —
/// the distinction a caller needs to decide what to re-run.
///
/// When no earlier route wrote anything, the cause is returned unchanged.
/// Nothing partial happened across routes, and wrapping a plain failure in a
/// partial-application error would tell the caller to inspect damage that does
/// not exist. That deliberately leaves the single-route case reporting exactly
/// what it reported before, matching the unrouted path — and it means an
/// intra-route half-write is not reported here either, matching what the
/// unrouted path does with the same failure.
pub(crate) fn partial_route_failure(
    route_targets: &[Vec<String>],
    attempted_route_has_mutated: &[bool],
    error: BeadsError,
) -> BeadsError {
    debug_assert!(
        !attempted_route_has_mutated.is_empty()
            && attempted_route_has_mutated.len() <= route_targets.len(),
        "a route must have been attempted, and no more routes attempted than exist"
    );
    let failed_index = attempted_route_has_mutated.len() - 1;

    let mut applied = Vec::new();
    let mut not_applied = Vec::new();
    for (targets, has_mutated) in route_targets
        .iter()
        .zip(attempted_route_has_mutated)
        .take(failed_index)
    {
        if *has_mutated {
            applied.extend(targets.iter().cloned());
        } else {
            not_applied.extend(targets.iter().cloned());
        }
    }
    if applied.is_empty() {
        return error;
    }

    let failed_route_targets = route_targets[failed_index].clone();
    let uncertain = if attempted_route_has_mutated[failed_index] {
        failed_route_targets
    } else {
        not_applied.extend(failed_route_targets);
        Vec::new()
    };
    not_applied.extend(route_targets[failed_index + 1..].concat());

    BeadsError::PartiallyApplied(Box::new(crate::error::PartialApplication {
        applied,
        uncertain,
        not_applied,
        source: error,
    }))
}

/// Report a post-mutation auto-flush failure without corrupting command stdout.
///
/// The data mutation has already succeeded by the time this is called. The
/// safest remaining action is to make the sync debt visible on stderr and leave
/// the operator with an explicit `sync --flush-only` recovery path.
pub fn report_auto_flush_failure(
    ctx: &OutputContext,
    beads_dir: &Path,
    jsonl_path: &Path,
    error: &BeadsError,
) {
    tracing::warn!(
        beads_dir = %beads_dir.display(),
        jsonl_path = %jsonl_path.display(),
        error = %error,
        "Mutation succeeded but auto-flush failed"
    );

    if ctx.is_quiet() {
        return;
    }

    let message = "Mutation succeeded, but automatic JSONL export failed. \
                   Fix the export problem, run `br sync --flush-only`, then commit \
                   the updated .beads/issues.jsonl.";
    let error_text = error.to_string();

    if ctx.is_json() {
        let payload = serde_json::json!({
            "warning": {
                "code": "AUTO_FLUSH_FAILED",
                "message": message,
                "beads_dir": beads_dir.display().to_string(),
                "jsonl_path": jsonl_path.display().to_string(),
                "error": error_text,
                "recovery": "Run br sync --flush-only after fixing the export problem before committing .beads/issues.jsonl"
            }
        });
        eprintln!(
            "{}",
            serde_json::to_string(&payload).unwrap_or_else(|_| {
                "{\"warning\":{\"code\":\"AUTO_FLUSH_FAILED\"}}".to_string()
            })
        );
        return;
    }

    let warning = format!(
        "Warning: {message} JSONL path: {}. Error: {error_text}",
        jsonl_path.display()
    );
    eprintln!("{}", sanitize_terminal_text(&warning));
}

/// Whether `id` names a live issue, a tombstone, or nothing at all.
fn issue_id_existence(storage: &SqliteStorage, id: &str) -> crate::Result<IdExistence> {
    if storage.live_id_exists(id)? {
        Ok(IdExistence::Live)
    } else if storage.id_exists(id)? {
        Ok(IdExistence::Tombstone)
    } else {
        Ok(IdExistence::Missing)
    }
}

/// Resolve an issue ID from a potentially partial input.
pub(super) fn resolve_issue_id(
    storage: &SqliteStorage,
    resolver: &IdResolver,
    input: &str,
) -> crate::Result<String> {
    resolver
        .resolve_fallible(
            input,
            |id| issue_id_existence(storage, id),
            |hash| storage.find_ids_by_hash(hash),
            |former| storage.find_id_by_former_id(former),
        )
        .map(|resolved| resolved.id)
}

pub(super) fn resolve_issue_ids(
    storage: &SqliteStorage,
    resolver: &IdResolver,
    inputs: &[String],
) -> crate::Result<Vec<String>> {
    resolver
        .resolve_all_fallible(
            inputs,
            |id| issue_id_existence(storage, id),
            |hash| storage.find_ids_by_hash(hash),
            |former| storage.find_id_by_former_id(former),
        )
        .map(|resolved| resolved.into_iter().map(|entry| entry.id).collect())
}

/// Attach `issue_id` beneath `parent_id`, renumbering it to match.
///
/// The invariant this project maintains is that a dotted prefix always names
/// the real parent, and having a parent always implies a dotted ID. An issue
/// that kept its old ID after gaining a new parent would violate that: it
/// would claim (by its former dotted prefix, or by having none at all) a
/// parent it no longer has, which is exactly the divergence `close`'s
/// dot-notation child check trips over.
///
/// A thin wrapper around [`SqliteStorage::attach_to_parent`], which does the
/// real work: setting the parent-child dep and renaming the issue inside one
/// transaction (an earlier version of this function called
/// `set_parent_with_options` then `rename_issue` as two separately committed
/// operations, which left the dep pointing at the new parent with the ID
/// unmoved if the rename failed after the dep had already committed -- see
/// that method's doc comment) and bumping the target parent's child counter
/// so a second attach into the same parent does not recompute the slot the
/// first one just took.
///
/// Returns the id `issue_id` holds after the call -- the renumbered id, or
/// `issue_id` unchanged if it was already a dotted child of `parent_id`.
///
/// # Errors
///
/// Propagates storage failures, including a self-dependency or cycle from
/// the parent-set step and an ID collision from the rename step.
pub(crate) fn attach_to_parent(
    storage: &mut SqliteStorage,
    issue_id: &str,
    parent_id: &str,
    actor: &str,
) -> crate::Result<String> {
    storage.attach_to_parent(issue_id, parent_id, actor)
}

pub(super) fn rebuild_blocked_cache_after_partial_mutation(
    storage: &mut SqliteStorage,
    cache_dirty: bool,
    command: &str,
) -> crate::Result<()> {
    if !cache_dirty {
        return Ok(());
    }

    match storage.mark_blocked_cache_stale() {
        Ok(()) => {
            tracing::debug!(
                command = command,
                "Blocked cache repair deferred after partial mutation; cache remains marked stale"
            );
            Ok(())
        }
        Err(mark_error) => {
            tracing::warn!(
                command = command,
                error = %mark_error,
                "Failed to pre-mark blocked cache stale before rebuilding after partial mutation"
            );
            storage
                .rebuild_blocked_cache(true)
                .map(|_| ())
                .map_err(|rebuild_err| crate::error::BeadsError::WithContext {
                    context: format!(
                        "failed to rebuild blocked cache after partial {command} mutation; \
                         pre-marking it stale also failed: {mark_error}"
                    ),
                    source: Box::new(rebuild_err),
                })
        }
    }
}

pub(super) fn preserve_blocked_cache_on_error<T>(
    storage: &mut SqliteStorage,
    cache_dirty: bool,
    command: &str,
    result: crate::Result<T>,
) -> crate::Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(operation_err) => {
            if let Err(rebuild_err) =
                rebuild_blocked_cache_after_partial_mutation(storage, cache_dirty, command)
            {
                return Err(crate::error::BeadsError::WithContext {
                    context: format!(
                        "failed to preserve blocked cache after partial {command} mutation; original operation error: {operation_err}"
                    ),
                    source: Box::new(rebuild_err),
                });
            }
            Err(operation_err)
        }
    }
}

pub(super) fn finalize_batched_blocked_cache_refresh(
    storage: &mut SqliteStorage,
    cache_dirty: bool,
    command: &str,
) -> crate::Result<()> {
    if !cache_dirty {
        return Ok(());
    }

    if !storage.blocked_cache_marked_stale().unwrap_or(false)
        && let Err(mark_error) = storage.mark_blocked_cache_stale()
    {
        tracing::warn!(
            command = command,
            error = %mark_error,
            "Failed to pre-mark blocked cache stale before batched refresh"
        );
        return storage
            .rebuild_blocked_cache(true)
            .map(|_| ())
            .map_err(|rebuild_err| crate::error::BeadsError::WithContext {
                context: format!(
                    "failed to rebuild blocked cache after successful batched {command} mutation; \
                     leaving the cache stale also failed first: {mark_error}"
                ),
                source: Box::new(rebuild_err),
            });
    }

    match storage.ensure_blocked_cache_fresh() {
        Ok(rebuilt) => {
            tracing::debug!(
                command = command,
                rebuilt = rebuilt,
                "Blocked cache refreshed after successful batched mutation"
            );
            Ok(())
        }
        Err(rebuild_error) => {
            tracing::warn!(
                command = command,
                error = %rebuild_error,
                "Blocked cache refresh failed after successful batched mutation; preserving stale marker"
            );
            storage.mark_blocked_cache_stale().map_err(|mark_error| {
                crate::error::BeadsError::WithContext {
                    context: format!(
                        "failed to preserve blocked cache stale marker after successful batched {command} mutation; \
                         original refresh error: {rebuild_error}"
                    ),
                    source: Box::new(mark_error),
                }
            })
        }
    }
}

pub(super) fn update_issues_atomically_with_recovery(
    storage_ctx: &mut OpenStorageResult,
    allow_recovery: bool,
    command: &str,
    updates: &[(String, IssueUpdate)],
    actor: &str,
) -> crate::Result<Vec<Issue>> {
    retry_mutation_with_jsonl_recovery(storage_ctx, allow_recovery, command, None, |storage| {
        storage.update_issues_atomically(updates, actor)
    })
}

fn should_attempt_mutation_jsonl_recovery(
    storage_ctx: &OpenStorageResult,
    operation_err: &BeadsError,
    probe_err: Option<&BeadsError>,
) -> bool {
    matches!(operation_err, BeadsError::Database(_))
        && (storage_ctx.should_attempt_jsonl_recovery(operation_err)
            || probe_err.is_some_and(|err| storage_ctx.should_attempt_jsonl_recovery(err)))
}

pub(super) fn auto_import_storage_ctx_if_stale(
    storage_ctx: &mut OpenStorageResult,
    cli: &crate::config::CliOverrides,
) -> crate::Result<()> {
    // Issue #229: skip auto-import in --no-db mode.  The in-memory database
    // was just populated from the JSONL file during `open_storage_with_cli`,
    // so there is no staleness to detect.  Running the staleness probe here
    // is actively harmful because `compute_staleness_refreshing_witnesses`
    // calls `get_metadata` via `query_row_with_params`, which routes through
    // the previous engine's prepared-statement fast path.  On in-memory databases
    // that fast path can warm up cached root-page references that become
    // stale after the bulk import's DELETE + INSERT cycle, causing subsequent
    // `get_issue_from_conn` calls inside write transactions to silently
    // return zero rows — the mechanism behind the "Issue not found" errors
    // on `br --no-db update`.
    if storage_ctx.no_db {
        return Ok(());
    }

    let config_layer = storage_ctx.load_config(cli)?;
    let no_auto_import = crate::config::no_auto_import_from_layer(&config_layer).unwrap_or(false);
    let allow_external_jsonl = crate::config::implicit_external_jsonl_allowed(
        &storage_ctx.paths.beads_dir,
        &storage_ctx.paths.db_path,
        &storage_ctx.paths.jsonl_path,
    );
    let expected_prefix = crate::config::id_config_from_layer(&config_layer).prefix;

    auto_import_if_stale(
        &mut storage_ctx.storage,
        &storage_ctx.paths.beads_dir,
        &storage_ctx.paths.jsonl_path,
        Some(expected_prefix.as_str()),
        allow_external_jsonl,
        cli.allow_stale.unwrap_or(false),
        no_auto_import,
    )
}

pub(super) fn cli_for_routed_workspace(
    cli: &crate::config::CliOverrides,
    is_external: bool,
) -> crate::config::CliOverrides {
    let mut route_cli = cli.clone();
    if is_external {
        route_cli.db = None;
        route_cli.read_only_fast_open = false;
    }
    route_cli
}

pub(super) fn auto_import_external_projects_if_stale(
    config_layer: &crate::config::ConfigLayer,
    local_beads_dir: &Path,
    cli: &crate::config::CliOverrides,
) {
    if cli.allow_stale.unwrap_or(false)
        || cli.no_auto_import.unwrap_or(false)
        || cli.no_db.unwrap_or(false)
        || crate::config::no_db_from_layer(config_layer).unwrap_or(false)
        || crate::config::no_auto_import_from_layer(config_layer).unwrap_or(false)
    {
        return;
    }

    for (project, beads_dir) in
        crate::config::external_project_beads_dirs(config_layer, local_beads_dir)
    {
        let paths = match crate::config::ConfigPaths::resolve(&beads_dir, None) {
            Ok(paths) => paths,
            Err(error) => {
                tracing::warn!(
                    project = %project,
                    path = %beads_dir.display(),
                    error = %error,
                    "Skipping external project auto-import because path resolution failed"
                );
                continue;
            }
        };

        if !paths.db_path.is_file() && !paths.jsonl_path.is_file() {
            continue;
        }

        let mut route_cli = cli_for_routed_workspace(cli, true);
        let routed_write_lock = match acquire_routed_workspace_write_lock(
            &beads_dir,
            true,
            route_cli.lock_timeout,
        ) {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(
                    project = %project,
                    path = %beads_dir.display(),
                    error = %error,
                    "Skipping external project auto-import because the workspace write lock could not be acquired"
                );
                continue;
            }
        };
        routed_write_lock.mark_cli_write_lock_held(&mut route_cli);

        let mut storage_ctx = match crate::config::open_storage_with_cli(&beads_dir, &route_cli) {
            Ok(storage_ctx) => storage_ctx,
            Err(error) => {
                tracing::warn!(
                    project = %project,
                    path = %beads_dir.display(),
                    error = %error,
                    "Skipping external project auto-import because storage could not be opened"
                );
                continue;
            }
        };

        if let Err(error) = auto_import_storage_ctx_if_stale(&mut storage_ctx, &route_cli) {
            tracing::warn!(
                project = %project,
                path = %beads_dir.display(),
                error = %error,
                "External project auto-import failed; dependency status will use the current database state"
            );
        }
    }
}

pub(super) fn external_project_db_paths_after_auto_import_if_needed(
    storage: &SqliteStorage,
    config_layer: &crate::config::ConfigLayer,
    beads_dir: &Path,
    cli: &crate::config::CliOverrides,
) -> crate::Result<HashMap<String, PathBuf>> {
    if !storage.has_external_dependencies(true)? {
        return Ok(HashMap::new());
    }

    auto_import_external_projects_if_stale(config_layer, beads_dir, cli);
    Ok(crate::config::external_project_db_paths(
        config_layer,
        beads_dir,
    ))
}

pub(super) struct RoutedWorkspaceWriteLock {
    _lock: Option<File>,
    beads_dir: Option<PathBuf>,
}

impl RoutedWorkspaceWriteLock {
    #[must_use]
    pub(super) const fn local() -> Self {
        Self {
            _lock: None,
            beads_dir: None,
        }
    }

    pub(super) fn mark_cli_write_lock_held(&self, cli: &mut crate::config::CliOverrides) {
        if let Some(beads_dir) = &self.beads_dir {
            cli.held_write_lock_beads_dir = Some(beads_dir.clone());
        }
    }
}

pub(super) fn acquire_routed_workspace_write_lock(
    beads_dir: &Path,
    is_external: bool,
    lock_timeout_ms: Option<u64>,
) -> crate::Result<RoutedWorkspaceWriteLock> {
    if !is_external {
        return Ok(RoutedWorkspaceWriteLock::local());
    }

    let lock_path = beads_dir.join(".write.lock");
    let file =
        crate::sync::blocking_write_lock_with_timeout(beads_dir, lock_timeout_ms).map_err(|err| {
            BeadsError::Config(format!(
                "Routed external workspace is busy: target write lock at {} could not be acquired: {err}",
                lock_path.display()
            ))
        })?;
    Ok(RoutedWorkspaceWriteLock {
        _lock: Some(file),
        beads_dir: Some(beads_dir.to_path_buf()),
    })
}

pub(super) fn retry_mutation_with_jsonl_recovery<T, F>(
    storage_ctx: &mut OpenStorageResult,
    allow_recovery: bool,
    command: &str,
    probe_issue_id: Option<&str>,
    mut operation: F,
) -> crate::Result<T>
where
    F: FnMut(&mut SqliteStorage) -> crate::Result<T>,
{
    match operation(&mut storage_ctx.storage) {
        Ok(value) => Ok(value),
        Err(operation_err) => {
            if !allow_recovery || !matches!(operation_err, BeadsError::Database(_)) {
                return Err(operation_err);
            }

            let mut recovery_signal =
                should_attempt_mutation_jsonl_recovery(storage_ctx, &operation_err, None);
            let mut probe_error: Option<BeadsError> = None;

            if !recovery_signal && let Some(issue_id) = probe_issue_id {
                match storage_ctx
                    .storage
                    .probe_issue_mutation_write_path(issue_id)
                {
                    Ok(()) => return Err(operation_err),
                    Err(probe_err) => {
                        recovery_signal = should_attempt_mutation_jsonl_recovery(
                            storage_ctx,
                            &operation_err,
                            Some(&probe_err),
                        );
                        probe_error = Some(probe_err);
                    }
                }
            }

            if !recovery_signal {
                return Err(operation_err);
            }

            let issue_id_label = probe_issue_id.unwrap_or("<none>");
            let probe_error_display = probe_error
                .as_ref()
                .map_or_else(|| "n/a".to_string(), std::string::ToString::to_string);
            tracing::warn!(
                command = command,
                issue_id = issue_id_label,
                original_error = %operation_err,
                probe_error = %probe_error_display,
                db_path = %storage_ctx.paths.db_path.display(),
                jsonl_path = %storage_ctx.paths.jsonl_path.display(),
                "Mutation hit a recoverable database corruption path; rebuilding from JSONL and retrying once"
            );

            let original_error = operation_err.to_string();
            storage_ctx.recover_database_from_jsonl().map_err(|recovery_err| {
                BeadsError::WithContext {
                    context: probe_issue_id.map_or_else(
                        || {
                            format!(
                                "automatic database recovery failed after {command} write; original write error: {original_error}"
                            )
                        },
                        |issue_id| {
                        format!(
                            "automatic database recovery failed after {command} write for issue '{issue_id}'; original write error: {original_error}"
                        )
                        },
                    ),
                    source: Box::new(recovery_err),
                }
            })?;

            operation(&mut storage_ctx.storage)
        }
    }
}

/// Resolve `--exclude-label` and its three siblings (bds-3rt).
///
/// `list`, `search` and `ready` all call this rather than each converting the
/// flags themselves, so `--exclude-type bug` cannot come to mean one thing on
/// one command and another on the next. It is also the only place issue-type
/// strings are parsed for exclusion, which is what makes a typo an error on all
/// three at once instead of a silently ineffective filter on two of them.
///
/// # Errors
///
/// Returns a validation error if any `--exclude-type` value is not a known issue
/// type.
pub fn resolve_exclusion_filters(
    args: &crate::cli::ExclusionArgs,
) -> crate::error::Result<crate::storage::ExclusionFilters> {
    let mut types = Vec::with_capacity(args.exclude_type.len());
    for value in &args.exclude_type {
        types.push(value.parse::<crate::model::IssueType>()?);
    }
    Ok(crate::storage::ExclusionFilters {
        labels: args.exclude_label.clone(),
        types,
        no_labels: args.no_labels,
        no_parent: args.no_parent,
    })
}

/// Resolve `--created-after` and its seven siblings onto a `ListFilters`
/// (bds-lf1).
///
/// `br list` and `br search` each build their own `ListFilters`, and both call
/// this rather than parsing the flags themselves: the two must agree on what
/// `--updated-after -7d` means down to the instant, or the same argument would
/// select a different set in each command. `apply_date_range_filters` writing
/// the fields is also what keeps the eight flags from being enumerated twice.
///
/// # Errors
///
/// Returns a validation error naming the offending flag if any value is not a
/// timestamp, a date, a relative offset or a known keyword.
pub fn apply_date_range_filters(
    filters: &mut crate::storage::ListFilters,
    args: &crate::cli::DateRangeArgs,
) -> crate::error::Result<()> {
    use crate::util::time::{RangeBound, parse_range_bound};

    let lower = |value: &Option<String>, field: &str| {
        value
            .as_deref()
            .map(|value| parse_range_bound(value, field, RangeBound::Lower))
            .transpose()
    };
    filters.created_after = lower(&args.created_after, "created_after")?;
    filters.updated_after = lower(&args.updated_after, "updated_after")?;
    filters.closed_after = lower(&args.closed_after, "closed_after")?;
    filters.defer_after = lower(&args.defer_after, "defer_after")?;

    let upper = |value: &Option<String>, field: &str| {
        value
            .as_deref()
            .map(|value| parse_range_bound(value, field, RangeBound::Upper))
            .transpose()
    };
    filters.created_before = upper(&args.created_before, "created_before")?;
    filters.updated_before = upper(&args.updated_before, "updated_before")?;
    filters.closed_before = upper(&args.closed_before, "closed_before")?;
    filters.defer_before = upper(&args.defer_before, "defer_before")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        acquire_routed_workspace_write_lock, finalize_batched_blocked_cache_refresh,
        partial_route_failure, preserve_blocked_cache_on_error,
        rebuild_blocked_cache_after_partial_mutation, retry_mutation_with_jsonl_recovery,
        should_attempt_mutation_jsonl_recovery,
    };
    use crate::config::{CliOverrides, OpenStorageResult, open_storage_with_cli};
    use crate::error::BeadsError;
    use crate::model::Issue;
    use crate::storage::SqliteStorage;
    use crate::storage::conn::{Connection, DbError};
    use crate::sync::{ExportConfig, export_to_jsonl_with_policy};
    use chrono::Utc;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    /// Three routes; the first writes, the second fails before writing
    /// anything. The first route's targets are written and unrecoverable, the
    /// second and third are untouched.
    #[test]
    fn partial_route_failure_reports_untouched_routes_when_the_failure_wrote_nothing() {
        let routes = vec![
            vec!["bd-1".to_string(), "bd-2".to_string()],
            vec!["bd-3".to_string()],
            vec!["bd-4".to_string()],
        ];

        let error = partial_route_failure(
            &routes,
            &[true, false],
            BeadsError::validation("if-status", "guard rejected"),
        );

        let BeadsError::PartiallyApplied(partial) = error else {
            panic!("expected a partial-application error, got: {error}");
        };
        assert_eq!(
            partial.applied,
            vec!["bd-1".to_string(), "bd-2".to_string()]
        );
        assert!(partial.uncertain.is_empty());
        assert_eq!(
            partial.not_applied,
            vec!["bd-3".to_string(), "bd-4".to_string()]
        );
    }

    /// Same shape, except the failing route had already committed something.
    /// Its own targets move out of "untouched" and into "uncertain" — a route
    /// is atomic in its primary write but not across the follow-up steps.
    #[test]
    fn partial_route_failure_reports_a_half_written_route_as_uncertain() {
        let routes = vec![
            vec!["bd-1".to_string()],
            vec!["bd-2".to_string(), "bd-3".to_string()],
            vec!["bd-4".to_string()],
        ];

        let error = partial_route_failure(
            &routes,
            &[true, true],
            BeadsError::validation("label", "no table"),
        );

        let BeadsError::PartiallyApplied(partial) = error else {
            panic!("expected a partial-application error, got: {error}");
        };
        assert_eq!(partial.applied, vec!["bd-1".to_string()]);
        assert_eq!(
            partial.uncertain,
            vec!["bd-2".to_string(), "bd-3".to_string()]
        );
        assert_eq!(partial.not_applied, vec!["bd-4".to_string()]);
    }

    /// A route that succeeded without writing anything — every issue already
    /// closed, every detach a no-op — is untouched, not applied. Reporting it
    /// as written would send the caller looking for damage that is not there,
    /// and would misreport what a re-run is safe to include.
    #[test]
    fn partial_route_failure_does_not_call_a_no_op_route_applied() {
        let routes = vec![
            vec!["bd-1".to_string()],
            vec!["bd-2".to_string()],
            vec!["bd-3".to_string()],
        ];

        let error = partial_route_failure(
            &routes,
            &[false, true, false],
            BeadsError::validation("ids", "not found"),
        );

        let BeadsError::PartiallyApplied(partial) = error else {
            panic!("expected a partial-application error, got: {error}");
        };
        assert_eq!(partial.applied, vec!["bd-2".to_string()]);
        assert!(partial.uncertain.is_empty());
        assert_eq!(
            partial.not_applied,
            vec!["bd-1".to_string(), "bd-3".to_string()],
            "the no-op first route belongs with the untouched routes"
        );
    }

    /// Nothing written anywhere is not a partial application, even when a
    /// later route is the one that failed: the cause is returned untouched
    /// rather than dressed up as damage the caller now has to inspect.
    #[test]
    fn partial_route_failure_passes_the_cause_through_when_no_route_wrote_anything() {
        let routes = vec![
            vec!["bd-1".to_string()],
            vec!["bd-2".to_string()],
            vec!["bd-3".to_string()],
        ];

        for attempted in [vec![true], vec![false, true], vec![false, false]] {
            let error = partial_route_failure(
                &routes,
                &attempted,
                BeadsError::validation("if-status", "guard rejected"),
            );

            assert!(
                matches!(error, BeadsError::Validation { .. }),
                "expected the cause unchanged for {attempted:?}, got: {error}"
            );
        }
    }

    fn storage_ctx_with_exported_issue() -> (TempDir, OpenStorageResult) {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Scope the initial storage so the connection is closed before
        // recovery opens a new one at the same path.  The previous engine tracked pages
        // by file path, so an older connection causes BusySnapshot.
        {
            let mut storage = SqliteStorage::open(&db_path).expect("storage");
            let issue = Issue {
                id: "bd-1".to_string(),
                title: "test".to_string(),
                ..Issue::default()
            };
            storage
                .create_issue(&issue, "tester")
                .expect("create issue");
            let export_config = ExportConfig {
                beads_dir: Some(beads_dir.clone()),
                ..Default::default()
            };
            export_to_jsonl_with_policy(&storage, &jsonl_path, &export_config)
                .expect("export jsonl");
        }

        let storage_ctx =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage ctx");
        (temp, storage_ctx)
    }

    fn write_single_issue_jsonl(path: &Path, id: &str, title: &str) {
        let now = Utc::now();
        let issue = Issue {
            id: id.to_string(),
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            ..Issue::default()
        };
        let json = serde_json::to_string(&issue).expect("serialize issue");
        fs::write(path, format!("{json}\n")).expect("write jsonl");
    }

    #[test]
    fn routed_workspace_write_lock_respects_external_timeout() -> std::result::Result<(), String> {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let _held = crate::sync::blocking_write_lock_with_timeout(&beads_dir, None)
            .expect("hold write lock");
        let result = acquire_routed_workspace_write_lock(&beads_dir, true, Some(1));
        let err = result.err().ok_or_else(|| {
            "external routed lock should wait for and time out on held lock".to_string()
        })?;
        let message = err.to_string();
        assert!(
            message.contains("Routed external workspace is busy")
                && message.contains("target write lock")
                && message.contains("Timed out after 1ms waiting for write lock"),
            "{message}"
        );
        Ok(())
    }

    #[test]
    fn routed_workspace_write_lock_marks_cli_for_fast_open_recovery() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        write_single_issue_jsonl(
            &jsonl_path,
            "bd-routed",
            "Recovered under routed write lock",
        );

        let routed_write_lock =
            acquire_routed_workspace_write_lock(&beads_dir, true, Some(1)).expect("routed lock");
        let mut cli = CliOverrides {
            lock_timeout: Some(1),
            read_only_fast_open: true,
            ..CliOverrides::default()
        };
        routed_write_lock.mark_cli_write_lock_held(&mut cli);

        let storage_ctx =
            open_storage_with_cli(&beads_dir, &cli).expect("recovery should reuse routed lock");
        let issue = storage_ctx
            .storage
            .get_issue("bd-routed")
            .expect("query issue")
            .expect("issue should be rebuilt from JSONL");

        assert_eq!(issue.title, "Recovered under routed write lock");
        assert!(db_path.is_file(), "database should be rebuilt from JSONL");
    }

    #[test]
    fn partial_mutation_rebuild_skips_clean_state() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        rebuild_blocked_cache_after_partial_mutation(&mut storage, false, "close")
            .expect("clean state should not rebuild");
    }

    #[test]
    fn preserve_returns_original_error_when_cache_is_marked_stale() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        let result: crate::Result<()> = Err(BeadsError::validation("ids", "boom"));
        let err = preserve_blocked_cache_on_error::<()>(&mut storage, true, "close", result)
            .expect_err("operation should still fail");

        assert!(matches!(err, BeadsError::Validation { .. }));
    }

    #[test]
    fn preserve_surfaces_rebuild_failure_when_stale_marker_write_also_fails() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("conn");
        conn.execute("DROP TABLE blocked_issues_cache")
            .expect("drop blocked cache table");
        conn.execute("DROP TABLE metadata")
            .expect("drop metadata table");

        let result: crate::Result<()> = Err(BeadsError::validation("ids", "boom"));
        let err = preserve_blocked_cache_on_error::<()>(&mut storage, true, "reopen", result)
            .expect_err("rebuild failure should be surfaced");

        assert!(
            matches!(err, BeadsError::WithContext { .. }),
            "expected WithContext, got {err:?}"
        );
        if let BeadsError::WithContext { context, .. } = err {
            assert!(context.contains("partial reopen mutation"));
            assert!(context.contains("Validation failed: ids: boom"));
        }

        let metadata_probe = storage.get_metadata("blocked_cache_state");
        assert!(
            metadata_probe.is_err(),
            "metadata lookup should fail once the metadata table has been dropped"
        );
    }

    #[test]
    fn finalize_batched_refresh_rebuilds_when_cache_table_is_missing() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("conn");
        conn.execute("DROP TABLE blocked_issues_cache")
            .expect("drop blocked cache table");

        finalize_batched_blocked_cache_refresh(&mut storage, true, "close")
            .expect("batched refresh should recreate missing cache table");

        assert!(
            !storage.blocked_cache_marked_stale().unwrap(),
            "successful finalization should clear the stale marker"
        );
        let table_exists = storage
            .execute_raw_query(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'blocked_issues_cache'",
            )
            .expect("query sqlite_master");
        assert_eq!(
            table_exists.len(),
            1,
            "blocked cache table should be recreated"
        );
    }

    #[test]
    fn finalize_batched_refresh_clears_preexisting_stale_marker() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .mark_blocked_cache_stale()
            .expect("mark cache stale before finalization");

        finalize_batched_blocked_cache_refresh(&mut storage, true, "close")
            .expect("pre-marked stale cache should be rebuilt cleanly");

        assert!(
            !storage.blocked_cache_marked_stale().unwrap(),
            "successful finalization should clear a preexisting stale marker"
        );
    }

    #[test]
    fn retry_mutation_recovers_from_recoverable_database_error() {
        let (_temp, mut storage_ctx) = storage_ctx_with_exported_issue();
        let mut attempts = 0;

        let result = retry_mutation_with_jsonl_recovery(
            &mut storage_ctx,
            true,
            "test-mutation",
            Some("bd-1"),
            |_storage| {
                attempts += 1;
                if attempts == 1 {
                    Err(BeadsError::Database(DbError::DatabaseCorrupt {
                        detail: "synthetic corruption".to_string(),
                    }))
                } else {
                    Ok("recovered")
                }
            },
        )
        .expect("recovered mutation");

        assert_eq!(result, "recovered");
        assert_eq!(attempts, 2);
        assert!(
            storage_ctx
                .storage
                .get_issue("bd-1")
                .expect("load issue")
                .is_some()
        );
    }

    #[test]
    fn mutation_recovery_can_be_signaled_by_probe_after_constraint_style_error() {
        let (_temp, storage_ctx) = storage_ctx_with_exported_issue();
        let operation_err = BeadsError::Database(DbError::Internal(
            "constraint verification failed".to_string(),
        ));
        let probe_err = BeadsError::Database(DbError::Internal(
            "database disk image is malformed".to_string(),
        ));

        assert!(
            !should_attempt_mutation_jsonl_recovery(&storage_ctx, &operation_err, None),
            "constraint-style write errors should not recover without a corruption probe"
        );
        assert!(
            should_attempt_mutation_jsonl_recovery(&storage_ctx, &operation_err, Some(&probe_err)),
            "a recoverable rollback-only write probe should trigger JSONL recovery"
        );
    }
}
