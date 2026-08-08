//! `br rename` — change an issue's ID (bds-r6h).
//!
//! Every moving part of this already shipped and is already exercised:
//! [`SqliteStorage::rename_issue`] rewrites the row, cascades the new prefix
//! onto every descendant deepest-first, leaves a tombstone at the vacated ID,
//! appends the old ID to `former_ids` so it keeps resolving, and clamps
//! `updated_at` so the change survives the next import. `br detach` drives all
//! of it. What was missing was the front door: there was no supported way for a
//! person to change an ID, and hand-editing `issues.jsonl` — which does work for
//! field edits since bds-n8d — cannot fan a new ID out into hierarchical child
//! IDs, dependency rows or free text.
//!
//! ## What this command refuses, and why each refusal is here
//!
//! - **A tombstone.** Same `ensure_issue_mutable` gate every other mutating
//!   command goes through. `detach` records at length why skipping it is a real
//!   hazard rather than a formality; the same reasoning applies verbatim.
//! - **An occupied target.** `rename_issue` checks `id_exists`, which counts
//!   tombstones, so a rename onto a vacated ID is refused too. That is correct:
//!   a tombstone is the redirect that keeps the old ID resolving, and writing
//!   over it would silently break it.
//! - **A move within the hierarchy.** The invariant this project maintains is
//!   that a dotted prefix always names the real parent and having a parent always
//!   implies a dotted ID (see `commands::attach_to_parent`). `br rename bd-abc
//!   bd-xyz.1` would mint a dotted ID with no `parent-child` row behind it, and
//!   `br rename bd-p.1 bd-flat` would leave the row pointing at a flat ID. Both
//!   are reparenting, and reparenting already has two commands that do it
//!   properly with the cascade — `br update --parent` and `br detach`. So rename
//!   changes an issue's *name*, never its position.
//! - **A prefix change.** `br sync --rename-prefix` already rewrites prefixes
//!   across a whole workspace. Doing it one issue at a time would leave a row
//!   whose prefix disagrees with its workspace, which is the shape
//!   `PrefixMismatch` exists to reject on import.
//!
//! `--dry-run` exists because a rename cascades: the blast radius is every
//! descendant, and it should be inspectable before it happens rather than
//! afterwards.

use crate::cli::RenameArgs;
use crate::cli::commands::{
    acquire_routed_workspace_write_lock, auto_import_storage_ctx_if_stale,
    finalize_batched_blocked_cache_refresh, preserve_blocked_cache_on_error,
    report_auto_flush_failure, resolve_issue_id,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::util::id::{IdResolver, ResolverConfig, normalize_id, parse_id};
use serde::Serialize;
use std::path::Path;

/// What the rename did, or would do under `--dry-run`.
#[derive(Debug, Clone, Serialize)]
struct RenameOutcome {
    old_id: String,
    new_id: String,
    /// The cascade, as `(old, new)` pairs, deepest-first — the order the rename
    /// applies them in. Empty for a leaf.
    descendants: Vec<RenamedDescendant>,
    /// True when nothing was written.
    dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
struct RenamedDescendant {
    old_id: String,
    new_id: String,
}

/// Execute the rename command.
///
/// # Errors
///
/// Returns an error if either ID is malformed, the source cannot be resolved or
/// is a tombstone, the target is taken, or the rename would move the issue
/// within the hierarchy or change its prefix.
pub fn execute(
    args: &RenameArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let use_structured_output = json || ctx.is_json();
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;

    // Routed like the other mutating commands, but over one ID rather than a
    // batch: `group_issue_inputs_by_route` is what decides whether `old_id`
    // belongs to this workspace or to an external one, and a rename has to
    // happen in the workspace that owns the row.
    let routed = config::routing::group_issue_inputs_by_route(
        std::slice::from_ref(&args.old_id),
        &beads_dir,
    )?;
    let batch = routed
        .into_iter()
        .next()
        .ok_or_else(|| BeadsError::internal("routing produced no batch for a single ID"))?;

    let mut batch_cli = cli.clone();
    if batch.is_external {
        let normalized_local =
            dunce::canonicalize(&beads_dir).unwrap_or_else(|_| beads_dir.clone());
        let normalized_batch =
            dunce::canonicalize(&batch.beads_dir).unwrap_or_else(|_| batch.beads_dir.clone());
        if normalized_batch != normalized_local {
            batch_cli.db = None;
        }
    }

    let outcome = execute_route(args, &batch_cli, ctx, &batch.beads_dir, batch.is_external)?;

    if !outcome.dry_run {
        // The post-rename identity, as `detach` and `update` also record: the
        // vacated ID is now a tombstone, and pointing the next bare-ID command
        // at a tombstone is wrong even though `former_ids` papers over it.
        crate::util::set_last_touched_id(&beads_dir, &outcome.new_id);
    }

    if use_structured_output {
        if ctx.is_json() {
            ctx.json_pretty(&outcome);
        } else {
            OutputContext::from_flags(true, false, true).json_pretty(&outcome);
        }
        return Ok(());
    }

    let old_id = sanitize_terminal_inline(&outcome.old_id);
    let new_id = sanitize_terminal_inline(&outcome.new_id);
    if outcome.dry_run {
        ctx.print_line(&format!("Would rename {old_id} -> {new_id}"));
    } else {
        ctx.success(&format!("Renamed {old_id} -> {new_id}"));
    }
    for descendant in &outcome.descendants {
        ctx.print_line(&format!(
            "  {} -> {}",
            sanitize_terminal_inline(&descendant.old_id),
            sanitize_terminal_inline(&descendant.new_id)
        ));
    }
    if !outcome.descendants.is_empty() && outcome.dry_run {
        ctx.print_line(&format!(
            "  ({} descendant(s) would move with it)",
            outcome.descendants.len()
        ));
    }

    Ok(())
}

fn execute_route(
    args: &RenameArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
    auto_flush_external: bool,
) -> Result<RenameOutcome> {
    let _routed_write_lock =
        acquire_routed_workspace_write_lock(beads_dir, auto_flush_external, cli.lock_timeout)?;
    let mut storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, cli)?;

    let config_layer = storage_ctx.load_config(cli)?;
    let actor = config::resolve_actor(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));

    // `old_id` goes through the resolver, so a partial ID, a hash fragment or a
    // former ID all work — the same way they do everywhere else. `new_id` does
    // not: it is a name being chosen, not a row being found, and letting the
    // resolver expand it would silently rename onto something else.
    let old_id = resolve_issue_id(&storage_ctx.storage, &resolver, &args.old_id)?;
    let new_id = normalize_id(&args.new_id);

    let mut plan = plan_rename(&storage_ctx.storage, &old_id, &new_id)?;

    if args.dry_run {
        plan.dry_run = true;
        return Ok(plan);
    }

    let result = storage_ctx
        .storage
        .rename_issue(&old_id, &new_id, &actor)
        .map(|()| plan);
    // A rename rewrites `dependencies`, so the blocked cache is stale either
    // way: on success it needs rebuilding, and on failure it has to be marked
    // stale rather than left silently wrong.
    let outcome =
        preserve_blocked_cache_on_error(&mut storage_ctx.storage, true, "rename", result)?;

    finalize_batched_blocked_cache_refresh(&mut storage_ctx.storage, true, "rename")?;
    storage_ctx.flush_no_db_if_dirty()?;
    if auto_flush_external && let Err(error) = storage_ctx.auto_flush_if_enabled() {
        report_auto_flush_failure(
            ctx,
            &storage_ctx.paths.beads_dir,
            &storage_ctx.paths.jsonl_path,
            &error,
        );
    }

    Ok(outcome)
}

/// Check every refusal and describe the cascade, without writing anything.
///
/// Shared by the real path and `--dry-run` so the two cannot disagree about what
/// is legal: a dry run that approved a rename the real run then refused would be
/// worse than having no dry run at all.
fn plan_rename(storage: &SqliteStorage, old_id: &str, new_id: &str) -> Result<RenameOutcome> {
    if old_id == new_id {
        return Err(BeadsError::validation(
            "new_id",
            format!("{new_id} is already this issue's ID"),
        ));
    }

    storage.ensure_issue_mutable(old_id, "rename")?;

    let old_parsed = parse_id(old_id)?;
    let new_parsed = parse_id(new_id)?;

    if old_parsed.prefix != new_parsed.prefix {
        return Err(BeadsError::validation(
            "new_id",
            format!(
                "cannot change an issue's prefix with rename ('{}' -> '{}'). \
                 Use `br sync --rename-prefix` to rewrite a workspace's prefix; \
                 doing it one issue at a time would leave a row whose prefix \
                 disagrees with its workspace.",
                old_parsed.prefix, new_parsed.prefix
            ),
        ));
    }

    if old_parsed.parent() != new_parsed.parent() {
        return Err(BeadsError::validation(
            "new_id",
            format!(
                "cannot move {old_id} to {new_id}: rename changes an issue's name, \
                 not its place in the hierarchy. A dotted ID always names its real \
                 parent, so this would leave the ID and the parent-child link \
                 disagreeing. Use `br update {old_id} --parent <id>` to reparent, \
                 or `br detach {old_id}` to give it an independent ID."
            ),
        ));
    }

    if storage.id_exists(new_id)? {
        return Err(BeadsError::validation(
            "new_id",
            format!(
                "{new_id} is taken. If it looks free, it may be a tombstone left \
                 by an earlier rename or delete -- that tombstone is what keeps \
                 the old ID resolving, so it is not reusable."
            ),
        ));
    }

    // Deepest-first, matching the order `rename_issue` applies them: a caller
    // reading a dry run sees the same sequence the real run will take.
    let descendants = storage
        .descendant_ids(old_id)?
        .into_iter()
        .map(|descendant| {
            let suffix = descendant.strip_prefix(old_id).ok_or_else(|| {
                BeadsError::internal(format!("{descendant} is not beneath {old_id}"))
            })?;
            Ok(RenamedDescendant {
                new_id: format!("{new_id}{suffix}"),
                old_id: descendant,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(RenameOutcome {
        old_id: old_id.to_string(),
        new_id: new_id.to_string(),
        descendants,
        dry_run: false,
    })
}
