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

use crate::cli::DetachArgs;
use crate::cli::commands::{
    acquire_routed_workspace_write_lock, auto_import_storage_ctx_if_stale,
    finalize_batched_blocked_cache_refresh, resolve_issue_ids,
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
        let outcome = detach_one(&mut storage_ctx.storage, &id_gen, issue_id, &actor)?;
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
