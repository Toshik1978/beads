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
//!
//! `detach_one` checks `SqliteStorage::ensure_issue_mutable` before doing
//! anything else, the same "not a tombstone" gate every other mutating
//! command (`remove_dependency`, labels, comments, status) already goes
//! through. Without it, a tombstoned dotted ID with no live parent dep and no
//! `former_ids` redirect -- the shape `br info --projections` tells users to
//! fix by running `br detach` -- would mint a fresh ID, move the tombstone
//! onto it, and plant a second tombstone at the dotted address, all while
//! printing "Detached X -> Y" as if a live issue had moved. A
//! rename-vacated ID is unaffected, since the resolver hands `detach_one` the
//! live successor via `former_ids` rather than the tombstone; a parentless
//! live issue still passes the check and reaches the no-op above.

use crate::cli::DetachArgs;
use crate::cli::commands::{
    acquire_routed_workspace_write_lock, auto_import_storage_ctx_if_stale,
    finalize_batched_blocked_cache_refresh, preserve_blocked_cache_on_error,
    report_auto_flush_failure, resolve_issue_ids,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::model::DependencyType;
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::util::id::{IdGenerator, IdResolver, ResolverConfig};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::Path;

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
    let routed_batches = config::routing::group_issue_inputs_by_route(&args.ids, &beads_dir)?;

    // Detach follows the same routing shape as `reopen`/`close`/`update`: each
    // route is opened, mutated and fully flushed (JSONL debt + blocked cache)
    // before the next route is even opened. A failure in a later route's
    // `execute_route` therefore propagates via `?` without touching what an
    // earlier, already-finalized route committed -- there is nothing left to
    // roll back there, and nothing else in this repo's routed commands rolls
    // an earlier workspace back either. See `execute_route` for the mid-batch
    // (single-workspace) staleness handling this relies on per route.
    let outcomes = if routed_batches.iter().any(|batch| batch.is_external) {
        let normalized_local_beads_dir =
            dunce::canonicalize(&beads_dir).unwrap_or_else(|_| beads_dir.clone());
        let mut routed_outcomes = Vec::new();

        for batch in routed_batches {
            let mut batch_args = args.clone();
            batch_args.ids.clone_from(&batch.issue_inputs);

            let normalized_batch_beads_dir =
                dunce::canonicalize(&batch.beads_dir).unwrap_or_else(|_| batch.beads_dir.clone());
            let mut batch_cli = cli.clone();
            batch_cli.db = if normalized_batch_beads_dir == normalized_local_beads_dir {
                cli.db.clone()
            } else {
                None
            };

            let batch_outcomes = execute_route(
                &batch_args,
                &batch_cli,
                ctx,
                &batch.beads_dir,
                batch.is_external,
            )?;
            routed_outcomes.push((batch.issue_inputs.clone(), batch_outcomes));
        }

        reorder_routed_items_by_requested_inputs(&args.ids, routed_outcomes, "detach routing")?
    } else {
        execute_route(args, cli, ctx, &beads_dir, false)?
    };

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

/// Detach every resolved ID against a single, already-routed workspace.
///
/// Mirrors `reopen`/`close`'s `execute_route`: open storage for `beads_dir`,
/// resolve `args.ids` against it, and mutate each one in a plain sequential
/// loop. A later ID can fail after an earlier one in the same route already
/// committed (a descendant renamed out from under it -- see the module
/// test), so the same three things `execute` used to do inline before
/// routing existed still have to happen here, per route, not just once
/// globally:
///
/// 1. `preserve_blocked_cache_on_error` marks the blocked cache stale on
///    error instead of leaving it silently wrong.
/// 2. `flush_no_db_if_dirty` runs on the error path too, so no-db JSONL debt
///    from the earlier, committed IDs in this route is not lost.
/// 3. `detach_one`'s tombstone guard (`ensure_issue_mutable`) is untouched --
///    routing only changes which storage is opened, not what each ID goes
///    through once it is resolved.
fn execute_route(
    args: &DetachArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
    auto_flush_external: bool,
) -> Result<Vec<DetachOutcome>> {
    let _routed_write_lock =
        acquire_routed_workspace_write_lock(beads_dir, auto_flush_external, cli.lock_timeout)?;
    let mut storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
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
    if auto_flush_external && let Err(error) = storage_ctx.auto_flush_if_enabled() {
        report_auto_flush_failure(
            ctx,
            &storage_ctx.paths.beads_dir,
            &storage_ctx.paths.jsonl_path,
            &error,
        );
    }

    Ok(outcomes)
}

/// Restore the order of routed results to match `requested_inputs`.
///
/// Each route processes its own subset of the requested IDs, so a
/// two-workspace batch comes back as two separate `(inputs, outcomes)` lists
/// that must be interleaved back into the order the caller asked for --
/// otherwise `Detached A -> A'` / `Detached B -> B'` output could print in
/// the wrong order relative to the command line, and `set_last_touched_id`
/// would record the wrong "last" issue. Identical to the same-named helper in
/// `reopen.rs`/`close.rs`/`update.rs`/`label.rs`; duplicated per file rather
/// than shared, matching this repo's existing convention for that helper.
fn reorder_routed_items_by_requested_inputs<T>(
    requested_inputs: &[String],
    routed_items: Vec<(Vec<String>, Vec<T>)>,
    context: &str,
) -> Result<Vec<T>> {
    fn issue_input_text(input: &str) -> String {
        sanitize_terminal_inline(input).into_owned()
    }

    let mut positions_by_input: HashMap<&str, VecDeque<usize>> = HashMap::new();
    for (index, input) in requested_inputs.iter().enumerate() {
        positions_by_input
            .entry(input.as_str())
            .or_default()
            .push_back(index);
    }

    let mut ordered_items: Vec<Option<T>> = (0..requested_inputs.len()).map(|_| None).collect();
    for (batch_inputs, batch_items) in routed_items {
        if batch_inputs.len() != batch_items.len() {
            return Err(BeadsError::internal(format!(
                "{context} produced mismatched issue/result counts"
            )));
        }

        for (input, item) in batch_inputs.into_iter().zip(batch_items) {
            let Some(index) = positions_by_input
                .get_mut(input.as_str())
                .and_then(VecDeque::pop_front)
            else {
                let input = issue_input_text(&input);
                return Err(BeadsError::internal(format!(
                    "{context} returned unexpected issue input {input}"
                )));
            };
            let Some(slot) = ordered_items.get_mut(index) else {
                let input = issue_input_text(&input);
                return Err(BeadsError::internal(format!(
                    "{context} returned out-of-range issue input {input}"
                )));
            };
            *slot = Some(item);
        }
    }

    ordered_items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            item.ok_or_else(|| {
                let input = requested_inputs
                    .get(index)
                    .map(|input| issue_input_text(input))
                    .unwrap_or_else(|| "<unknown>".to_string());
                BeadsError::internal(format!("{context} did not produce a result for {input}"))
            })
        })
        .collect()
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
    // Every other mutating command (`remove_dependency`, labels, comments,
    // status) routes through `ensure_issue_mutable_in_tx` before touching a
    // resolved issue; `detach` is the one that did not, and the reachable
    // shape that exposes it is a tombstoned dotted ID with no parent dep and
    // no `former_ids` redirect -- exactly the legacy divergent shape
    // `br info --projections` tells users to fix by running `br detach`.
    // Without this check that advice mints a fresh flat ID, moves the
    // tombstone onto it, and plants a second tombstone at the dotted
    // address, printing "Detached X -> Y" as though a live issue moved.
    //
    // A rename-vacated ID is unaffected: the resolver prefers its
    // `former_ids` redirect over the tombstone left behind, so `issue_id`
    // here already names the live successor. And an issue with no parent at
    // all still resolves to itself and passes this check (`Some(_) => Ok`),
    // so the no-op path below stays a safe, repeatable no-op.
    storage.ensure_issue_mutable(issue_id, "detach")?;

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

    /// The reachable shape `br info --projections` tells users to fix with
    /// `br detach`: a dotted ID that is a tombstone in place, with no live
    /// `parent-child` dep and no `former_ids` entry redirecting anywhere. The
    /// resolver's tombstone fallback still resolves the literal string to
    /// this dead row (there is nothing else for it to redirect to), so the
    /// only thing standing between this and `detach_one` minting a fresh ID
    /// and moving the tombstone onto it is the `ensure_issue_mutable` guard
    /// added at the top of `detach_one`. This asserts that guard fires with
    /// a clear, ID-naming error instead of a silent no-op or a bogus rename.
    #[test]
    fn execute_rejects_a_genuinely_tombstoned_dotted_id_with_no_redirect() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");

        let dotted_id = "bd-orphan.1";
        storage
            .create_issue(&make_issue(dotted_id, "Orphaned dotted issue"), "tester")
            .expect("create dotted issue");
        // Tombstone in place -- no rename, so no `former_ids` entry is ever
        // written. This is the legacy divergent shape: dotted, dead, and
        // with nothing for the resolver to redirect to but itself.
        storage
            .delete_issue(dotted_id, "tester", "test setup", None)
            .expect("tombstone in place");
        drop(storage);

        let args = DetachArgs {
            ids: vec![dotted_id.to_string()],
        };
        let overrides = CliOverrides {
            db: Some(db_path.clone()),
            ..CliOverrides::default()
        };
        let err = execute(&args, false, &overrides, &ctx).expect_err("tombstone must be rejected");
        match &err {
            BeadsError::Validation { field, reason } => {
                assert_eq!(field, "issue_id");
                assert!(
                    reason.contains("tombstone") && reason.contains(dotted_id),
                    "error should name the tombstone and the id, got: {reason}"
                );
            }
            other => panic!("expected Validation error naming the tombstone, got: {other:?}"),
        }

        // Nothing moved: still a tombstone at the same dotted address, no
        // fresh ID minted anywhere.
        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let issue = storage
            .get_issue(dotted_id)
            .unwrap()
            .expect("dotted id still names the tombstone");
        assert_eq!(issue.status, Status::Tombstone);
    }

    /// The other half of the guard: a rename-vacated ID must keep working.
    /// After a first `detach` renames a dotted child to a flat ID, the old
    /// dotted address becomes exactly the shape the previous test rejects --
    /// a tombstone in place -- except this one *does* carry a `former_ids`
    /// entry pointing at the live successor. The resolver prefers that
    /// redirect over the raw tombstone hit, so `ensure_issue_mutable` sees
    /// the live issue, not the tombstone, and a second `br detach` on the
    /// old id must still succeed.
    #[test]
    fn execute_still_detaches_a_rename_vacated_id_through_its_redirect() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");

        let epic_id = "bd-epic3";
        let child_id = "bd-epic3.1";
        storage
            .create_issue(&make_issue(epic_id, "Epic"), "tester")
            .expect("create epic");
        storage
            .create_issue(&make_issue(child_id, "Child"), "tester")
            .expect("create child");
        storage
            .add_dependency(child_id, epic_id, "parent-child", "tester")
            .expect("child -> epic dep");
        drop(storage);

        let overrides = CliOverrides {
            db: Some(db_path.clone()),
            ..CliOverrides::default()
        };

        // First detach: dotted with a live parent -> renamed to a flat id,
        // leaving a `former_ids` redirect at `child_id`.
        let args = DetachArgs {
            ids: vec![child_id.to_string()],
        };
        execute(&args, false, &overrides, &ctx).expect("first detach renames the child");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let tombstone = storage
            .get_issue(child_id)
            .unwrap()
            .expect("old dotted id still names a row");
        assert_eq!(
            tombstone.status,
            Status::Tombstone,
            "the vacated dotted id is a tombstone, same shape as a dead-end orphan"
        );
        let new_id = storage
            .find_id_by_former_id(child_id)
            .unwrap()
            .expect("former_ids must redirect the old dotted id to the live successor");
        let successor = storage
            .get_issue(&new_id)
            .unwrap()
            .expect("the live successor exists");
        assert_ne!(
            successor.status,
            Status::Tombstone,
            "the redirect target must be the live issue, not another tombstone"
        );
        drop(storage);

        // Second detach, on the same now-vacated dotted id: the resolver
        // must follow `former_ids` to the live flat successor rather than
        // stopping at the tombstone, so this must succeed -- not the
        // Validation error the previous test asserts for a genuine dead end.
        let args = DetachArgs {
            ids: vec![child_id.to_string()],
        };
        execute(&args, false, &overrides, &ctx)
            .expect("detach on a rename-vacated id must resolve through its redirect and succeed");

        // The live successor already has no parent, so the second detach
        // converges on the no-op: it is still live and unrenamed.
        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let successor_after = storage
            .get_issue(&new_id)
            .unwrap()
            .expect("the live successor is untouched by the no-op");
        assert_ne!(successor_after.status, Status::Tombstone);
    }
}
