//! Detach command implementation.
//!
//! Moves an issue out from under its parent so the parent can be closed
//! without `--force`. A dotted ID (`ab-xxx.1`) makes a hierarchy claim just by
//! its shape, so detaching one mints a flat ID via the same [`IdGenerator`]
//! `br create` uses, renames the issue to it (which also drops the old ID into
//! `former_ids` so it keeps resolving), and drops the `parent-child`
//! dependency. A flat ID makes no such claim, so detaching one only drops the
//! dependency. An issue with no parent by either measure is a successful
//! no-op: detaching twice in a row must succeed both times.
//!
//! The dep-removal and the rename are two separate `mutate` transactions
//! (`detach_one` below), so a crash between them leaves an issue with its
//! `parent-child` edge already gone but its ID still dotted. That is
//! deliberately self-healing rather than a bug to fix: a re-run sees
//! `parent == None && is_dotted`, so it skips the no-op early return (which
//! only fires when the ID is already flat) and falls through to complete the
//! rename. Detaching the same ID any number of times converges to the same
//! end state.

use crate::cli::DetachArgs;
use crate::cli::commands::{
    acquire_routed_workspace_write_lock, auto_import_storage_ctx_if_stale,
    finalize_batched_blocked_cache_refresh, preserve_blocked_cache_on_error, resolve_issue_ids,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::model::DependencyType;
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::util::id::{IdGenerator, IdResolver, ResolverConfig};
use serde::Serialize;

/// What happened to one requested ID.
#[derive(Debug, Clone, Serialize)]
struct DetachOutcome {
    old_id: String,
    /// Absent when nothing was renamed — a flat child, or an issue with no parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    new_id: Option<String>,
    /// `renamed`, `dep_removed`, or `no_parent`.
    action: String,
}

/// Execute the detach command.
///
/// # Errors
///
/// Returns an error if no issue IDs were given, an ID cannot be resolved, or
/// the underlying storage operations fail.
pub fn execute(
    args: &DetachArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let use_structured_output = json || ctx.is_json();

    if args.ids.is_empty() {
        return Err(BeadsError::validation("ids", "no issue IDs provided"));
    }

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;

    // Detach never routes to an external workspace today: every ID it acts on
    // is resolved against the local DB. `acquire_routed_workspace_write_lock`
    // with `is_external = false` is a cheap no-op that keeps this command's
    // shape consistent with the other ID-taking mutating commands.
    let _routed_write_lock =
        acquire_routed_workspace_write_lock(&beads_dir, false, cli.lock_timeout)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, cli)?;

    let config_layer = storage_ctx.load_config(cli)?;
    let actor = config::resolve_actor(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix.clone()));
    let id_gen = IdGenerator::new(id_config);

    let resolved_ids = resolve_issue_ids(&storage_ctx.storage, &resolver, &args.ids)?;

    let mut outcomes = Vec::with_capacity(resolved_ids.len());
    let mut cache_dirty = false;
    for issue_id in &resolved_ids {
        let result = detach_one(&mut storage_ctx.storage, &id_gen, issue_id, &actor);
        // A later ID in the batch can fail (e.g. a descendant renamed out from
        // under it by an earlier ID in the same batch — see the module test).
        // Earlier IDs already committed their mutation by this point, so on
        // error the cache must be left marked stale rather than silently
        // wrong, and any no-db JSONL debt from those earlier IDs must still be
        // flushed before the error propagates. `preserve_blocked_cache_on_error`
        // is the same helper `reopen`/`close` use for this exact window.
        let outcome = match preserve_blocked_cache_on_error(
            &mut storage_ctx.storage,
            cache_dirty,
            "detach",
            result,
        ) {
            Ok(outcome) => outcome,
            Err(err) => {
                if let Err(flush_err) = storage_ctx.flush_no_db_if_dirty() {
                    tracing::warn!(
                        error = %flush_err,
                        original_error = %err,
                        "failed to flush no-db JSONL after a mid-batch detach error"
                    );
                }
                return Err(err);
            }
        };
        if outcome.action != "no_parent" {
            cache_dirty = true;
        }
        outcomes.push(outcome);
    }

    finalize_batched_blocked_cache_refresh(&mut storage_ctx.storage, cache_dirty, "detach")?;
    storage_ctx.flush_no_db_if_dirty()?;

    if let Some(last) = outcomes.last() {
        crate::util::set_last_touched_id(&beads_dir, &last.old_id);
    }

    if use_structured_output {
        if ctx.is_json() {
            ctx.json_pretty(&outcomes);
        } else {
            let json_ctx = OutputContext::from_flags(true, false, true);
            json_ctx.json_pretty(&outcomes);
        }
    } else {
        for outcome in &outcomes {
            let old_id = sanitize_terminal_inline(&outcome.old_id);
            match (outcome.action.as_str(), outcome.new_id.as_deref()) {
                ("renamed", Some(new_id)) => {
                    let new_id = sanitize_terminal_inline(new_id);
                    ctx.success(&format!("Detached {old_id} -> {new_id}"));
                }
                ("dep_removed", _) => {
                    ctx.success(&format!("Detached {old_id} (parent link removed)"));
                }
                _ => {
                    ctx.print_line(&format!("  {old_id} has no parent -- nothing to detach"));
                }
            }
        }
    }

    Ok(())
}

/// Find the immediate `parent-child` parent of `issue_id`, if any.
///
/// Beads encodes parent relationships as dependency rows: the canonical
/// `ParentChild` type with the child as the dependent and the parent as the
/// `depends_on` target. This mirrors `inheritance::find_immediate_parent_id`,
/// which is private to that module and not reusable here.
fn current_parent(storage: &SqliteStorage, issue_id: &str) -> Result<Option<String>> {
    let deps = storage.get_dependencies_full(issue_id)?;
    Ok(deps
        .into_iter()
        .find(|dep| matches!(dep.dep_type, DependencyType::ParentChild))
        .map(|dep| dep.depends_on_id))
}

fn detach_one(
    storage: &mut SqliteStorage,
    id_gen: &IdGenerator,
    issue_id: &str,
    actor: &str,
) -> Result<DetachOutcome> {
    let parent = current_parent(storage, issue_id)?;
    let is_dotted = issue_id.contains('.');

    if parent.is_none() && !is_dotted {
        return Ok(DetachOutcome {
            old_id: issue_id.to_string(),
            new_id: None,
            action: "no_parent".to_string(),
        });
    }

    // Drop the edge first: `rename_issue` rewrites `dependencies.issue_id`, so
    // removing the edge afterward would mean chasing the row to its new ID.
    if let Some(parent_id) = &parent {
        storage.remove_dependency(issue_id, parent_id)?;
    }

    if !is_dotted {
        return Ok(DetachOutcome {
            old_id: issue_id.to_string(),
            new_id: None,
            action: "dep_removed".to_string(),
        });
    }

    let issue = storage
        .get_issue(issue_id)?
        .ok_or_else(|| BeadsError::IssueNotFound {
            id: issue_id.to_string(),
        })?;

    // The same generator `br create` uses with no parent, so a detached issue
    // is indistinguishable from one that was never a child.
    let new_id = id_gen.generate(
        &issue.title,
        issue.description.as_deref(),
        issue.created_by.as_deref(),
        issue.created_at,
        storage.count_issues()?,
        |candidate| storage.id_exists(candidate),
    )?;
    storage.rename_issue(issue_id, &new_id, actor)?;

    Ok(DetachOutcome {
        old_id: issue_id.to_string(),
        new_id: Some(new_id),
        action: "renamed".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands;
    use crate::config::CliOverrides;
    use crate::model::{Issue, IssueType, Priority, Status};
    use crate::output::OutputContext;
    use crate::storage::SqliteStorage;
    use chrono::Utc;
    use tempfile::TempDir;

    fn make_issue(id: &str, title: &str) -> Issue {
        let now = Utc::now();
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: now,
            updated_at: now,
            ..Issue::default()
        }
    }

    /// A later ID in a batch can fail after an earlier one already committed.
    ///
    /// This used to rely on `grandchild_id` becoming unresolvable simply
    /// because `rename_issue` cascades a rename to the whole subtree: once
    /// the batch processed `child_id`, the literal string `grandchild_id`
    /// stopped naming any *live* row. But bds-a23.10 gave every renamed
    /// descendant its own `former_ids`-bearing tombstone at the vacated
    /// address, so `grandchild_id` still named *something* — `get_issue`
    /// (a plain `WHERE id = ?` with no status filter) found that tombstone
    /// and `detach_one` happily "detached" it, since it never checks
    /// `Status::Tombstone` before treating a `get_issue` hit as live. That
    /// masking is exactly why this test previously stopped failing the way
    /// it expected to; see bds-a23.13.
    ///
    /// The fix here does not lean on that masked path at all. Instead
    /// `grandchild_id` is tombstoned *in place* (`delete_issue`, which flips
    /// `status` without moving the ID) before the batch runs. The rename
    /// cascade's own doc comment spells out what happens next: a descendant
    /// that is *already* a tombstone when the cascade reaches it is still
    /// moved (`rewrite_issue_id` relocates its row to `<new child id>.1`
    /// unconditionally) but is deliberately **not** re-tombstoned at the
    /// vacated address — re-tombstoning an already-tombstoned node would
    /// double the dead-row count on every ancestor rename for no new
    /// information, so `rename_issue_in_tx` skips it whenever
    /// `pre_move.status == Status::Tombstone`. So after `child_id`'s
    /// detach commits, `grandchild_id` names *nothing at all* — no live row,
    /// no tombstone — and the second detach's `get_issue` call genuinely
    /// returns `None`, giving `IssueNotFound` with no masking to route
    /// around.
    ///
    /// `grandchild_id` still has to resolve at the batch's up-front
    /// `resolve_issue_ids` step, which runs before either ID is touched: a
    /// tombstone resolves to itself there (`IdResolver`'s tombstone
    /// fallback), so passing an already-tombstoned ID into the batch is
    /// legitimate, not a resolver trick. And `child_id` still commits
    /// first for the mundane reason that the batch loop is a plain
    /// sequential `for` over `resolved_ids`: `child_id` is processed and
    /// its `mutate` transaction commits before `grandchild_id` is looked at
    /// all.
    #[test]
    fn execute_marks_the_blocked_cache_stale_when_a_later_id_in_the_batch_fails() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");

        let epic_id = "bd-epic";
        let child_id = "bd-epic.1";
        let grandchild_id = "bd-epic.1.1";

        storage
            .create_issue(&make_issue(epic_id, "Epic"), "tester")
            .expect("create epic");
        storage
            .create_issue(&make_issue(child_id, "Child"), "tester")
            .expect("create child");
        storage
            .create_issue(&make_issue(grandchild_id, "Grandchild"), "tester")
            .expect("create grandchild");
        storage
            .add_dependency(child_id, epic_id, "parent-child", "tester")
            .expect("child -> epic dep");
        storage
            .add_dependency(grandchild_id, child_id, "parent-child", "tester")
            .expect("grandchild -> child dep");
        // Tombstone the grandchild in place (its ID does not move) so the
        // rename cascade below carries it along without re-tombstoning it,
        // vacating `grandchild_id` for real instead of leaving a redirect
        // that `get_issue` would still find.
        storage
            .delete_issue(grandchild_id, "tester", "test setup", None)
            .expect("tombstone grandchild in place");
        drop(storage);

        let args = DetachArgs {
            ids: vec![child_id.to_string(), grandchild_id.to_string()],
        };
        let overrides = CliOverrides {
            db: Some(db_path.clone()),
            ..CliOverrides::default()
        };
        let err = execute(&args, false, &overrides, &ctx).expect_err("second ID must fail");
        assert!(
            matches!(err, BeadsError::IssueNotFound { .. }),
            "expected IssueNotFound for the stale grandchild_id string, got: {err:?}"
        );

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        assert!(
            storage.blocked_cache_marked_stale().unwrap(),
            "the first ID's committed dep-removal and rename must leave the \
             blocked cache marked stale, not silently wrong, when a later ID \
             in the same batch fails"
        );

        // The first ID's mutation really did commit despite the batch erroring:
        // `child_id` no longer names a live issue, only a tombstone left by
        // `rename_issue`.
        let child_after = storage.get_issue(child_id).unwrap();
        assert!(
            child_after.is_some_and(|issue| issue.status == Status::Tombstone),
            "child_id should have been renamed away by the first, successful iteration"
        );
    }
}
