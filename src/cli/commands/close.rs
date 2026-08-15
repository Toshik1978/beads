//! Close command implementation.

use super::create::read_text_argument_file;
use crate::cli::CloseArgs as CliCloseArgs;
use crate::cli::commands::{
    acquire_routed_workspace_write_lock, auto_import_storage_ctx_if_stale,
    finalize_batched_blocked_cache_refresh, preserve_blocked_cache_on_error,
    report_auto_flush_failure, resolve_issue_id, resolve_issue_ids,
    update_issues_atomically_with_recovery,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::model::{Issue, IssueType, Status};
use crate::output::OutputContext;
use crate::storage::IssueUpdate;
use crate::util::id::{IdResolver, ResolverConfig};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

/// Internal arguments for the close command.
#[derive(Debug, Clone, Default)]
pub struct CloseArgs {
    /// Issue IDs to close
    pub ids: Vec<String>,
    /// Close reason
    pub reason: Option<String>,
    /// New comment committed atomically with the close transition.
    pub transition_comment: Option<String>,
    /// Force close even if blocked
    pub force: bool,
    /// Session ID for `closed_by_session` field
    pub session: Option<String>,
    /// Return newly unblocked issues (single ID only)
    pub suggest_next: bool,
    /// Keep going past a per-issue unresolvable ID, and make the exit code
    /// report whether every requested issue ended up closed (bds-yo8).
    pub keep_going: bool,
}

impl TryFrom<&CliCloseArgs> for CloseArgs {
    type Error = BeadsError;

    /// `--reason-file` is read here, exactly once, before any route is opened.
    /// The same reasoning as `resolve_update_description`: a routed batch and a
    /// JSONL-recovery retry both re-run the close with these args, and re-reading
    /// stdin the second time would find it empty.
    fn try_from(cli: &CliCloseArgs) -> Result<Self> {
        let reason = match cli.reason_file.as_deref() {
            Some(path) => {
                if cli.reason.is_some() {
                    return Err(BeadsError::validation(
                        "reason_file",
                        "cannot be combined with --reason",
                    ));
                }
                Some(read_text_argument_file(
                    path,
                    "reason_file",
                    "close reason",
                )?)
            }
            None => cli.reason.clone(),
        };
        Ok(Self {
            ids: cli.ids.clone(),
            reason,
            transition_comment: cli.transition_comment.clone(),
            force: cli.force,
            session: cli.session.clone(),
            suggest_next: cli.suggest_next,
            keep_going: cli.keep_going,
        })
    }
}

/// Execute the close command from CLI args.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
pub fn execute_cli(
    cli_args: &CliCloseArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let args = CloseArgs::try_from(cli_args)?;
    execute_with_args(&args, json, cli, ctx)
}

/// Result of a close operation for JSON output.
#[derive(Debug, Serialize, Deserialize)]
pub struct CloseResult {
    pub closed: Vec<ClosedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skipped: Vec<SkippedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
}

/// Result of closing with suggest-next.
#[derive(Debug, Serialize, Deserialize)]
pub struct CloseWithSuggestResult {
    pub closed: Vec<ClosedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skipped: Vec<SkippedIssue>,
    pub unblocked: Vec<UnblockedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
}

/// An issue that became unblocked after closing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnblockedIssue {
    pub id: String,
    pub title: String,
    pub priority: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosedIssue {
    pub id: String,
    pub title: String,
    pub status: String,
    pub closed_at: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub close_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedIssue {
    pub id: String,
    pub reason: String,
    /// The issue was already closed (or tombstoned), so nothing needed doing.
    ///
    /// `#[serde(skip)]` on purpose: the `--json` payload's field set is a
    /// consumer contract and this is bookkeeping for `--continue`'s exit code
    /// (bds-yo8), not information a caller asked for -- the `reason` string
    /// already says "already closed" in words.
    #[serde(skip)]
    pub already_terminal: bool,
}

#[allow(dead_code)]
#[derive(Debug, Default)]
struct CloseExecution {
    closed: Vec<ClosedIssue>,
    skipped: Vec<SkippedIssue>,
    unblocked: Vec<UnblockedIssue>,
    ordered_outcomes: Vec<CloseOutcome>,
    capacity_warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
}

#[derive(Debug, Clone)]
enum CloseOutcome {
    Closed(ClosedIssue),
    Skipped(SkippedIssue),
}

fn build_close_json_payload(
    args: &CloseArgs,
    closed_issues: Vec<ClosedIssue>,
    skipped_issues: Vec<SkippedIssue>,
    unblocked_issues: Vec<UnblockedIssue>,
    capacity_warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
) -> Result<String> {
    let json = if args.suggest_next {
        // suggest_next is br-only, so always use the wrapped machine format.
        let result = CloseWithSuggestResult {
            closed: closed_issues,
            skipped: skipped_issues,
            unblocked: unblocked_issues,
            warnings: capacity_warnings,
        };
        serde_json::to_string_pretty(&result)?
    } else if skipped_issues.is_empty() && capacity_warnings.is_empty() {
        // Preserve bd-compatible array output for pure-success closes.
        serde_json::to_string_pretty(&closed_issues)?
    } else {
        // Once skips are present, a bare array loses machine-readable reasons.
        let result = CloseResult {
            closed: closed_issues,
            skipped: skipped_issues,
            warnings: capacity_warnings,
        };
        serde_json::to_string_pretty(&result)?
    };

    Ok(json)
}

fn render_close_json(
    args: &CloseArgs,
    closed_issues: Vec<ClosedIssue>,
    skipped_issues: Vec<SkippedIssue>,
    unblocked_issues: Vec<UnblockedIssue>,
    capacity_warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
) -> Result<()> {
    let json = build_close_json_payload(
        args,
        closed_issues,
        skipped_issues,
        unblocked_issues,
        capacity_warnings,
    )?;
    println!("{json}");
    Ok(())
}

fn emit_close_structured_output(
    args: &CloseArgs,
    closed_issues: Vec<ClosedIssue>,
    skipped_issues: Vec<SkippedIssue>,
    unblocked_issues: Vec<UnblockedIssue>,
    capacity_warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
    ctx: &OutputContext,
) -> Result<()> {
    if args.suggest_next {
        let result = CloseWithSuggestResult {
            closed: closed_issues,
            skipped: skipped_issues,
            unblocked: unblocked_issues,
            warnings: capacity_warnings,
        };
        if ctx.is_json() {
            ctx.json_pretty(&result);
        } else {
            let json_ctx = OutputContext::from_flags(true, false, true);
            json_ctx.json_pretty(&result);
        }
        return Ok(());
    }

    if skipped_issues.is_empty() && capacity_warnings.is_empty() {
        if ctx.is_json() {
            ctx.json_pretty(&closed_issues);
        } else {
            render_close_json(
                args,
                closed_issues,
                skipped_issues,
                unblocked_issues,
                capacity_warnings,
            )?;
        }
        return Ok(());
    }

    let result = CloseResult {
        closed: closed_issues,
        skipped: skipped_issues,
        warnings: capacity_warnings,
    };
    if ctx.is_json() {
        ctx.json_pretty(&result);
    } else {
        let json_ctx = OutputContext::from_flags(true, false, true);
        json_ctx.json_pretty(&result);
    }
    Ok(())
}

fn close_human_message(closed: &ClosedIssue) -> String {
    let id = sanitize_terminal_inline(&closed.id);
    let title = sanitize_terminal_inline(&closed.title);
    let mut message = format!("Closed {}: {}", id.as_ref(), title.as_ref());
    if let Some(reason) = &closed.close_reason {
        let reason = sanitize_terminal_inline(reason);
        message.push_str(" (");
        message.push_str(reason.as_ref());
        message.push(')');
    }
    message
}

fn skipped_human_message(skipped: &SkippedIssue) -> String {
    let id = sanitize_terminal_inline(&skipped.id);
    let reason = sanitize_terminal_inline(&skipped.reason);
    format!("Skipped {}: {}", id.as_ref(), reason.as_ref())
}

fn unblocked_human_line(issue: &UnblockedIssue) -> String {
    let id = sanitize_terminal_inline(&issue.id);
    let title = sanitize_terminal_inline(&issue.title);
    format!("  {}: {}", id.as_ref(), title.as_ref())
}

fn issue_input_text(input: &str) -> String {
    sanitize_terminal_inline(input).into_owned()
}

fn reorder_routed_items_by_requested_inputs<T>(
    requested_inputs: &[String],
    routed_items: Vec<(Vec<String>, Vec<T>)>,
    context: &str,
) -> Result<Vec<T>> {
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

fn compute_batch_closable_ids(
    active_issue_ids: &HashSet<String>,
    internal_blockers_by_id: &HashMap<String, Vec<String>>,
    external_blockers_by_id: &HashMap<String, Vec<String>>,
) -> HashSet<String> {
    let mut closable: HashSet<String> = active_issue_ids
        .iter()
        .filter(|id| {
            external_blockers_by_id
                .get(*id)
                .is_none_or(std::vec::Vec::is_empty)
        })
        .cloned()
        .collect();

    loop {
        let to_remove: Vec<String> = closable
            .iter()
            .filter(|id| {
                internal_blockers_by_id
                    .get(*id)
                    .into_iter()
                    .flatten()
                    .any(|blocker_id| !closable.contains(blocker_id))
            })
            .cloned()
            .collect();

        if to_remove.is_empty() {
            break;
        }

        for id in to_remove {
            closable.remove(&id);
        }
    }

    closable
}

/// Execute the close command with full arguments.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
#[allow(clippy::too_many_lines)]
pub fn execute_with_args(
    args: &CloseArgs,
    use_json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    tracing::info!("Executing close command");
    let use_structured_output = use_json || ctx.is_json();

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut target_inputs = args.ids.clone();
    if target_inputs.is_empty() {
        let last_touched = crate::util::get_last_touched_id(&beads_dir);
        if last_touched.is_empty() {
            return Err(BeadsError::validation(
                "ids",
                "no issue IDs provided and no last-touched issue",
            ));
        }
        target_inputs.push(last_touched);
    }

    if args.suggest_next && target_inputs.len() > 1 {
        return Err(BeadsError::validation(
            "suggest-next",
            "--suggest-next only works with a single issue ID",
        ));
    }
    let routed_batches = config::routing::group_issue_inputs_by_route(&target_inputs, &beads_dir)?;

    let mut closed_issues = Vec::new();
    let mut skipped_issues = Vec::new();
    let mut unblocked_issues = Vec::new();
    let mut capacity_warnings = Vec::new();

    if routed_batches.iter().any(|batch| batch.is_external) {
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

            let execution = execute_route(
                &batch_args,
                &batch_cli,
                ctx,
                &batch.beads_dir,
                batch.is_external,
            )?;
            let CloseExecution {
                unblocked,
                ordered_outcomes,
                capacity_warnings: route_warnings,
                ..
            } = execution;
            routed_outcomes.push((batch.issue_inputs, ordered_outcomes));
            unblocked_issues.extend(unblocked);
            capacity_warnings.extend(route_warnings);
        }

        let ordered_outcomes = reorder_routed_items_by_requested_inputs(
            &target_inputs,
            routed_outcomes,
            "close routing",
        )?;
        for outcome in ordered_outcomes {
            match outcome {
                CloseOutcome::Closed(issue) => closed_issues.push(issue),
                CloseOutcome::Skipped(issue) => skipped_issues.push(issue),
            }
        }
    } else {
        let mut local_args = args.clone();
        local_args.ids = target_inputs;
        let execution = execute_route(&local_args, cli, ctx, &beads_dir, false)?;
        closed_issues = execution.closed;
        skipped_issues = execution.skipped;
        unblocked_issues = execution.unblocked;
        capacity_warnings = execution.capacity_warnings;
    }

    let closed_count = closed_issues.len();
    let skipped_count = skipped_issues.len();
    // Skips that leave the issue *not* closed. See the `--continue` exit-code
    // block at the end of this function.
    let unresolved_count = skipped_issues
        .iter()
        .filter(|skipped| !skipped.already_terminal)
        .count();
    // Capture per-issue skip reasons BEFORE the vectors are moved into the
    // output emitters. When every issue is skipped, the terminal error must
    // carry the real reasons: a generic "all N skipped" used to imply
    // "already closed or not found" even when the skip was actually a
    // dependency block, sending operators down the wrong debugging path
    // (issue #380).
    let skip_summary = summarize_skip_reasons(&skipped_issues);

    if let Some(last_closed) = closed_issues.last() {
        crate::util::set_last_touched_id(&beads_dir, &last_closed.id);
    }

    if use_structured_output {
        emit_close_structured_output(
            args,
            closed_issues,
            skipped_issues,
            unblocked_issues,
            capacity_warnings,
            ctx,
        )?;
    } else if closed_issues.is_empty() && skipped_issues.is_empty() {
        ctx.info("No issues to close.");
    } else {
        for closed in &closed_issues {
            ctx.success(&close_human_message(closed));
        }
        for skipped in &skipped_issues {
            ctx.warning(&skipped_human_message(skipped));
        }
        for warning in &capacity_warnings {
            ctx.warning(&warning.to_string());
        }
        if !unblocked_issues.is_empty() {
            ctx.newline();
            ctx.info(&format!("Unblocked {} issue(s):", unblocked_issues.len()));
            for issue in &unblocked_issues {
                ctx.print_line(&unblocked_human_line(issue));
            }
        }
    }

    // bds-yo8. `--continue` replaces the exit-code rule rather than adding to it.
    //
    // The default rule -- error only when *nothing* closed -- lets a partial batch
    // exit 0, which is defensible interactively and poor in a script. A caller who
    // passed `--continue` intends to inspect the outcome, so for them the question
    // becomes "did every issue I named end up closed?".
    //
    // "Ended up closed" counts issues that were *already* closed. That is what
    // makes `--continue` safe to re-run over a batch that half-succeeded: the
    // second run reports the rest as already closed and exits 0. Under the default
    // rule that same re-run errors, which is why this replaces it instead of
    // stacking on top -- and why the default is left exactly as it was, since
    // changing an exit code under existing callers would be worse than the gap.
    if args.keep_going {
        if unresolved_count > 0 {
            return Err(BeadsError::PartiallyCompleted {
                reason: format!(
                    "closed {closed_count} issue(s); {unresolved_count} not closed — {skip_summary}"
                ),
            });
        }
    } else if closed_count == 0 && skipped_count > 0 {
        return Err(BeadsError::NothingToDo {
            reason: format!("all {skipped_count} issue(s) skipped — {skip_summary}"),
        });
    }

    Ok(())
}

/// Render the per-issue skip reasons for the terminal `NothingToDo` error.
///
/// Lists up to five `id: reason` pairs (sanitized for
/// terminal safety) so the error names WHY each issue was skipped instead of
/// leaving the operator to guess (issue #380). Longer batches get a
/// `+N more` suffix; JSON callers still receive the full skip list in
/// the structured payload.
fn summarize_skip_reasons(skipped: &[SkippedIssue]) -> String {
    const SKIP_SUMMARY_PREVIEW: usize = 5;
    let mut parts: Vec<String> = skipped
        .iter()
        .take(SKIP_SUMMARY_PREVIEW)
        .map(|s| {
            format!(
                "{}: {}",
                sanitize_terminal_inline(&s.id),
                sanitize_terminal_inline(&s.reason)
            )
        })
        .collect();
    if skipped.len() > SKIP_SUMMARY_PREVIEW {
        parts.push(format!("+{} more", skipped.len() - SKIP_SUMMARY_PREVIEW));
    }
    parts.join("; ")
}

#[allow(clippy::too_many_lines)]
fn execute_route(
    args: &CloseArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
    auto_flush_external: bool,
) -> Result<CloseExecution> {
    let _routed_write_lock =
        acquire_routed_workspace_write_lock(beads_dir, auto_flush_external, cli.lock_timeout)?;
    let mut storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, cli)?;

    let config_layer = storage_ctx.load_config(cli)?;
    let actor = config::resolve_actor(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    // bds-yo8. Without `--continue`, one unresolvable ID in a batch of ten fails
    // the command before any of the ten is looked at -- which is the case people
    // reach for `--continue` to fix. With it, each input is resolved on its own
    // and a failure becomes that slot's outcome.
    //
    // `resolved_ids` stays 1:1 with `args.ids`, with an unresolvable input
    // standing in for itself: `ordered_outcomes` is indexed by position and the
    // routed-batch reordering maps outcomes back to inputs positionally, so a
    // compacted list would silently misalign both.
    let mut pre_skipped: HashMap<usize, SkippedIssue> = HashMap::new();
    let resolved_ids = if args.keep_going {
        args.ids
            .iter()
            .enumerate()
            .map(
                |(index, input)| match resolve_issue_id(&storage_ctx.storage, &resolver, input) {
                    Ok(id) => id,
                    Err(error) => {
                        pre_skipped.insert(
                            index,
                            SkippedIssue {
                                id: input.clone(),
                                reason: error.to_string(),
                                already_terminal: false,
                            },
                        );
                        input.clone()
                    }
                },
            )
            .collect()
    } else {
        resolve_issue_ids(&storage_ctx.storage, &resolver, &args.ids)?
    };

    let epic_counts = storage_ctx.storage.get_epic_counts()?;
    let blocked_before: Vec<String> = if args.suggest_next {
        storage_ctx
            .storage
            .get_blocked_issues()?
            .into_iter()
            .map(|(i, _)| i.id)
            .collect()
    } else {
        Vec::new()
    };

    let requested_ids: HashSet<String> = resolved_ids.iter().cloned().collect();
    let mut open_issues: HashMap<String, crate::model::Issue> = HashMap::new();
    let mut internal_blockers_by_id: HashMap<String, Vec<String>> = HashMap::new();
    let mut external_blockers_by_id: HashMap<String, Vec<String>> = HashMap::new();
    let mut closed_issues: Vec<ClosedIssue> = Vec::new();
    let mut skipped_issues: Vec<SkippedIssue> = Vec::new();
    let mut ordered_outcomes = Vec::with_capacity(resolved_ids.len());
    let mut cache_dirty = false;
    let mut capacity_warnings = Vec::new();

    let mut atomic_updates = Vec::new();
    let mut planned_closes = Vec::new();
    for (outcome_index, id) in resolved_ids.iter().enumerate() {
        ordered_outcomes.push(None);
        if let Some(skipped) = pre_skipped.remove(&outcome_index) {
            ordered_outcomes[outcome_index] = Some(CloseOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }
        tracing::info!(id = %id, "Closing issue");

        let issue_result = storage_ctx.storage.get_issue(id);
        let Some(issue) = preserve_blocked_cache_on_error(
            &mut storage_ctx.storage,
            cache_dirty,
            "close",
            issue_result,
        )?
        else {
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: "issue not found".to_string(),
                already_terminal: false,
            };
            ordered_outcomes[outcome_index] = Some(CloseOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        };

        if issue.status.is_terminal() {
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: format!("already {}", issue.status.as_str()),
                // The only skip that is not a failure: the issue is in the
                // state the caller asked for, which is what makes `--continue`
                // safe to re-run over a partially-completed batch.
                already_terminal: true,
            };
            ordered_outcomes[outcome_index] = Some(CloseOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        if !args.force
            && let Some(&(total, closed)) = epic_counts.get(id)
            && closed < total
        {
            let label = if issue.issue_type == IssueType::Epic {
                "epic"
            } else {
                "parent issue"
            };
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: format!(
                    "{label} has {}/{} open children (use `br detach <child-id>` to make a child independent, or --force to close anyway)",
                    total - closed,
                    total
                ),
                already_terminal: false,
            };
            ordered_outcomes[outcome_index] = Some(CloseOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        // Supplementary guard: catch dot-notation children (e.g. `epic.1`,
        // `epic.2`) that exist in the issues table without a formal
        // parent-child dep row. These slip past `epic_counts` because
        // get_epic_counts only scans the dependencies table. Missing-dep
        // children occur with legacy-bd migrations, bulk JSONL imports,
        // and hand-edited JSONL. Without this check, closing the parent
        // silently orphans the open children.
        let requested_dot_children = if args.force {
            Vec::new()
        } else {
            let open_dot_children = storage_ctx.storage.get_open_dot_notation_children(id)?;
            let (requested_children, unrequested_children): (Vec<String>, Vec<String>) =
                open_dot_children
                    .into_iter()
                    .partition(|child_id| requested_ids.contains(child_id));
            if !unrequested_children.is_empty() {
                let label = if issue.issue_type == IssueType::Epic {
                    "epic"
                } else {
                    "parent issue"
                };
                let preview: Vec<String> = unrequested_children.iter().take(5).cloned().collect();
                let suffix = if unrequested_children.len() > preview.len() {
                    format!(", +{} more", unrequested_children.len() - preview.len())
                } else {
                    String::new()
                };
                let skipped = SkippedIssue {
                    id: id.clone(),
                    reason: format!(
                        "{label} has {} open dot-notation child issue(s): {}{} (use `br detach <child-id>` to make a child independent, or --force to close anyway)",
                        unrequested_children.len(),
                        preview.join(", "),
                        suffix
                    ),
                    already_terminal: false,
                };
                ordered_outcomes[outcome_index] = Some(CloseOutcome::Skipped(skipped.clone()));
                skipped_issues.push(skipped);
                continue;
            }
            requested_children
        };

        if args.force {
            open_issues.insert(id.clone(), issue);
            continue;
        }

        // Use *close* blockers, not generic blockers: a `parent-child` edge is
        // hierarchy, not a prerequisite from the parent to the child, so a
        // finished child must be closable even while its parent epic is itself
        // blocked or open (#355). `get_close_blockers` strips the propagated
        // `:parent-blocked` markers while retaining real prerequisite edges on
        // the child and the `:child-open` close-ordering rollup.
        let close_blockers_result = storage_ctx.storage.get_close_blockers(id);
        let mut blocker_ids = preserve_blocked_cache_on_error(
            &mut storage_ctx.storage,
            cache_dirty,
            "close",
            close_blockers_result,
        )?;
        blocker_ids.extend(requested_dot_children);
        blocker_ids.sort();
        blocker_ids.dedup();
        let (internal_blockers, external_blockers): (Vec<String>, Vec<String>) = blocker_ids
            .into_iter()
            .partition(|blocker_id| requested_ids.contains(blocker_id));
        internal_blockers_by_id.insert(id.clone(), internal_blockers);
        external_blockers_by_id.insert(id.clone(), external_blockers);
        open_issues.insert(id.clone(), issue);
    }

    let closable_ids = |open: &HashMap<String, Issue>| -> HashSet<String> {
        let active: HashSet<String> = open.keys().cloned().collect();
        if args.force {
            active
        } else {
            compute_batch_closable_ids(&active, &internal_blockers_by_id, &external_blockers_by_id)
        }
    };
    let batch_closable_ids = closable_ids(&open_issues);

    for (outcome_index, id) in resolved_ids.iter().enumerate() {
        let Some(issue) = open_issues.get(id) else {
            continue;
        };

        if !args.force && !batch_closable_ids.contains(id) {
            let mut blocker_ids = external_blockers_by_id.get(id).cloned().unwrap_or_default();
            if let Some(internal_blockers) = internal_blockers_by_id.get(id) {
                blocker_ids.extend(
                    internal_blockers
                        .iter()
                        .filter(|blocker_id| !batch_closable_ids.contains(*blocker_id))
                        .cloned(),
                );
            }
            blocker_ids.sort();
            blocker_ids.dedup();
            tracing::debug!(blocked_by = ?blocker_ids, "Issue remains blocked in batch close");
            // Name the open blockers AND the way out. Without the explicit
            // remediation this skip used to surface as a bare "all N issue(s)
            // skipped" error whose hint claimed the issue was "already closed
            // or not found" — flatly wrong for a dependency block (#380).
            let reason = if blocker_ids.is_empty() {
                "blocked by dependencies — close the open blocker(s) first, or use --force to close anyway".to_string()
            } else {
                format!(
                    "blocked by: {} — close the open blocker(s) first, or use --force to close anyway",
                    blocker_ids.join(", ")
                )
            };
            let skipped = SkippedIssue {
                id: id.clone(),
                reason,
                already_terminal: false,
            };
            ordered_outcomes[outcome_index] = Some(CloseOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        let now = Utc::now();
        let close_reason = args.reason.clone().unwrap_or_else(|| "done".to_string());
        let update = IssueUpdate {
            status: Some(Status::Closed),
            closed_at: Some(Some(now)),
            close_reason: Some(Some(close_reason.clone())),
            closed_by_session: args.session.clone().map(Some),
            transition_comment: args.transition_comment.clone(),
            skip_cache_rebuild: true,
            ..Default::default()
        };

        atomic_updates.push((id.clone(), update));
        planned_closes.push((outcome_index, id.clone(), issue.clone(), now, close_reason));
    }

    if !atomic_updates.is_empty() {
        let update_result = update_issues_atomically_with_recovery(
            &mut storage_ctx,
            true,
            "close",
            &atomic_updates,
            &actor,
        );
        preserve_blocked_cache_on_error(&mut storage_ctx.storage, false, "close", update_result)?;
        capacity_warnings = storage_ctx.storage.take_capacity_warnings();
        cache_dirty = true;
    }

    for (outcome_index, id, issue, now, close_reason) in planned_closes {
        tracing::info!(id = %id, reason = ?args.reason, "Issue closed");

        let closed = ClosedIssue {
            id: id.clone(),
            title: issue.title.clone(),
            status: "closed".to_string(),
            closed_at: now.to_rfc3339(),
            close_reason: Some(close_reason),
        };
        ordered_outcomes[outcome_index] = Some(CloseOutcome::Closed(closed.clone()));
        closed_issues.push(closed);
    }

    let ordered_outcomes = ordered_outcomes
        .into_iter()
        .map(|outcome| {
            outcome.ok_or_else(|| BeadsError::internal("close batch outcome was not populated"))
        })
        .collect::<Result<Vec<_>>>()?;

    if cache_dirty {
        tracing::info!(
            "Rebuilding blocked cache after closing {} issues",
            closed_issues.len()
        );
        finalize_batched_blocked_cache_refresh(&mut storage_ctx.storage, cache_dirty, "close")?;
    }

    let unblocked_issues: Vec<UnblockedIssue> = if args.suggest_next && !closed_issues.is_empty() {
        let blocked_after_result = storage_ctx.storage.get_blocked_issues();
        let blocked_after = match preserve_blocked_cache_on_error(
            &mut storage_ctx.storage,
            cache_dirty,
            "close",
            blocked_after_result,
        ) {
            Ok(blocked_after) => Some(
                blocked_after
                    .into_iter()
                    .map(|(issue, _)| issue.id)
                    .collect::<Vec<_>>(),
            ),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "Skipping suggest-next calculation after committed close because blocked-cache lookup failed"
                );
                None
            }
        };

        let Some(blocked_after) = blocked_after else {
            storage_ctx.flush_no_db_if_dirty()?;
            return Ok(CloseExecution {
                closed: closed_issues,
                skipped: skipped_issues,
                unblocked: Vec::new(),
                ordered_outcomes,
                capacity_warnings,
            });
        };

        let newly_unblocked: Vec<String> = blocked_before
            .into_iter()
            .filter(|id| !blocked_after.contains(id))
            .collect();

        tracing::debug!(unblocked = ?newly_unblocked, "Issues unblocked by close");

        let mut unblocked = Vec::new();
        for uid in newly_unblocked {
            let issue_result = storage_ctx.storage.get_issue(&uid);
            match preserve_blocked_cache_on_error(
                &mut storage_ctx.storage,
                cache_dirty,
                "close",
                issue_result,
            ) {
                Ok(Some(issue)) if issue.status.is_active() => {
                    unblocked.push(UnblockedIssue {
                        id: issue.id,
                        title: issue.title,
                        priority: issue.priority.0,
                    });
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        issue_id = %uid,
                        error = %error,
                        "Skipping suggest-next candidate after committed close because issue lookup failed"
                    );
                }
            }
        }
        unblocked
    } else {
        Vec::new()
    };

    storage_ctx.flush_no_db_if_dirty()?;
    if auto_flush_external && let Err(error) = storage_ctx.auto_flush_if_enabled() {
        report_auto_flush_failure(
            ctx,
            &storage_ctx.paths.beads_dir,
            &storage_ctx.paths.jsonl_path,
            &error,
        );
    }

    Ok(CloseExecution {
        closed: closed_issues,
        skipped: skipped_issues,
        unblocked: unblocked_issues,
        ordered_outcomes,
        capacity_warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands;
    use crate::config::CliOverrides;
    use crate::model::{DependencyType, Issue, IssueType, Priority, Status};
    use crate::output::OutputContext;
    use crate::storage::SqliteStorage;
    use chrono::Utc;
    use std::env;
    use std::path::PathBuf;

    use tempfile::TempDir;

    struct DirGuard {
        previous: PathBuf,
    }

    impl DirGuard {
        fn new(target: &std::path::Path) -> Self {
            let previous = env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));
            env::set_current_dir(target).expect("set current dir");
            Self { previous }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.previous);
        }
    }

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

    fn make_issue_with_status(id: &str, title: &str, status: Status) -> Issue {
        Issue {
            status,
            ..make_issue(id, title)
        }
    }

    // =========================================================================
    // CloseArgs tests
    // =========================================================================

    #[test]
    fn test_close_args_default() {
        let args = CloseArgs::default();
        assert!(args.ids.is_empty());
        assert!(args.reason.is_none());
        assert!(args.transition_comment.is_none());
        assert!(!args.force);
        assert!(args.session.is_none());
        assert!(!args.suggest_next);
    }

    #[test]
    fn test_close_args_with_all_fields() {
        let args = CloseArgs {
            ids: vec!["bd-abc".to_string(), "bd-xyz".to_string()],
            reason: Some("Fixed in PR #123".to_string()),
            transition_comment: Some("Verified in staging".to_string()),
            force: true,
            session: Some("session-456".to_string()),
            suggest_next: true,
            keep_going: false,
        };
        assert_eq!(args.ids.len(), 2);
        assert_eq!(args.ids[0], "bd-abc");
        assert_eq!(args.reason.as_deref(), Some("Fixed in PR #123"));
        assert_eq!(
            args.transition_comment.as_deref(),
            Some("Verified in staging")
        );
        assert!(args.force);
        assert_eq!(args.session.as_deref(), Some("session-456"));
        assert!(args.suggest_next);
    }

    // =========================================================================
    // CloseResult serialization tests
    // =========================================================================

    #[test]
    fn test_close_result_serialization_empty_skipped_omitted() {
        let result = CloseResult {
            closed: vec![ClosedIssue {
                id: "bd-123".to_string(),
                title: "Test issue".to_string(),
                status: "closed".to_string(),
                closed_at: "2026-01-01T00:00:00Z".to_string(),
                close_reason: None,
            }],
            skipped: vec![],
            warnings: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        // Empty skipped should be omitted due to skip_serializing_if
        assert!(!json.contains("\"skipped\""));
        assert!(json.contains("\"closed\""));
    }

    #[test]
    fn test_close_result_serialization_with_skipped() {
        let result = CloseResult {
            closed: vec![],
            skipped: vec![SkippedIssue {
                id: "bd-456".to_string(),
                reason: "already closed".to_string(),
                already_terminal: false,
            }],
            warnings: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"skipped\""));
        assert!(json.contains("\"reason\":\"already closed\""));
    }

    #[test]
    fn test_close_result_roundtrip() {
        let result = CloseResult {
            closed: vec![
                ClosedIssue {
                    id: "bd-a".to_string(),
                    title: "First".to_string(),
                    status: "closed".to_string(),
                    closed_at: "2026-01-01T00:00:00Z".to_string(),
                    close_reason: Some("Done".to_string()),
                },
                ClosedIssue {
                    id: "bd-b".to_string(),
                    title: "Second".to_string(),
                    status: "closed".to_string(),
                    closed_at: "2026-01-02T00:00:00Z".to_string(),
                    close_reason: None,
                },
            ],
            skipped: vec![SkippedIssue {
                id: "bd-c".to_string(),
                reason: "blocked by: bd-d".to_string(),
                already_terminal: false,
            }],
            warnings: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: CloseResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.closed.len(), 2);
        assert_eq!(parsed.skipped.len(), 1);
        assert_eq!(parsed.closed[0].id, "bd-a");
        assert_eq!(parsed.closed[0].close_reason.as_deref(), Some("Done"));
        assert!(parsed.closed[1].close_reason.is_none());
    }

    // =========================================================================
    // CloseWithSuggestResult serialization tests
    // =========================================================================

    #[test]
    fn test_close_with_suggest_result_serialization() {
        let result = CloseWithSuggestResult {
            closed: vec![ClosedIssue {
                id: "bd-parent".to_string(),
                title: "Parent task".to_string(),
                status: "closed".to_string(),
                closed_at: "2026-01-15T10:00:00Z".to_string(),
                close_reason: Some("Completed".to_string()),
            }],
            skipped: vec![],
            unblocked: vec![
                UnblockedIssue {
                    id: "bd-child1".to_string(),
                    title: "Child task 1".to_string(),
                    priority: 1,
                },
                UnblockedIssue {
                    id: "bd-child2".to_string(),
                    title: "Child task 2".to_string(),
                    priority: 2,
                },
            ],
            warnings: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"unblocked\""));
        assert!(json.contains("\"bd-child1\""));
        assert!(json.contains("\"bd-child2\""));
        assert!(json.contains("\"priority\":1"));
        assert!(json.contains("\"priority\":2"));
        // Empty skipped should be omitted
        assert!(!json.contains("\"skipped\""));
    }

    #[test]
    fn test_close_with_suggest_result_empty_unblocked() {
        let result = CloseWithSuggestResult {
            closed: vec![],
            skipped: vec![SkippedIssue {
                id: "bd-x".to_string(),
                reason: "not found".to_string(),
                already_terminal: false,
            }],
            unblocked: vec![],
            warnings: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        // unblocked is not marked skip_serializing_if, so it should appear as empty array
        assert!(json.contains("\"unblocked\":[]"));
        assert!(json.contains("\"skipped\""));
    }

    // =========================================================================
    // ClosedIssue serialization tests
    // =========================================================================

    #[test]
    fn test_closed_issue_serialization_with_reason() {
        let issue = ClosedIssue {
            id: "bd-test".to_string(),
            title: "Test issue".to_string(),
            status: "closed".to_string(),
            closed_at: "2026-01-17T08:00:00Z".to_string(),
            close_reason: Some("Fixed in commit abc123".to_string()),
        };
        let json = serde_json::to_string(&issue).unwrap();
        assert!(json.contains("\"close_reason\":\"Fixed in commit abc123\""));
    }

    #[test]
    fn test_closed_issue_serialization_without_reason() {
        let issue = ClosedIssue {
            id: "bd-test".to_string(),
            title: "Test issue".to_string(),
            status: "closed".to_string(),
            closed_at: "2026-01-17T08:00:00Z".to_string(),
            close_reason: None,
        };
        let json = serde_json::to_string(&issue).unwrap();
        // close_reason should be omitted due to skip_serializing_if
        assert!(!json.contains("close_reason"));
    }

    #[test]
    fn test_closed_issue_all_fields() {
        let issue = ClosedIssue {
            id: "beads-xyz".to_string(),
            title: "Multi-word title with special chars: <>&".to_string(),
            status: "closed".to_string(),
            closed_at: "2026-12-31T23:59:59Z".to_string(),
            close_reason: Some("End of year cleanup".to_string()),
        };
        let json = serde_json::to_string(&issue).unwrap();
        let parsed: ClosedIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "beads-xyz");
        assert!(parsed.title.contains("<>&"));
        assert_eq!(parsed.status, "closed");
        assert!(parsed.closed_at.contains("2026-12-31"));
    }

    #[test]
    fn close_human_messages_sanitize_terminal_controls() {
        let closed = ClosedIssue {
            id: "bd-close\x1b[2J".to_string(),
            title: "bad\rtitle\x08".to_string(),
            status: "closed".to_string(),
            closed_at: "2026-12-31T23:59:59Z".to_string(),
            close_reason: Some("done\nnext\x07\u{9b}".to_string()),
        };
        let skipped = SkippedIssue {
            id: "bd-skip\x1b[2J".to_string(),
            reason: "blocked\rby\nterminal\x07".to_string(),
            already_terminal: false,
        };
        let unblocked = UnblockedIssue {
            id: "bd-unblock\x1b[2J".to_string(),
            title: "ready\nlater\x08".to_string(),
            priority: 1,
        };

        let close_message = close_human_message(&closed);
        let skipped_message = skipped_human_message(&skipped);
        let unblocked_line = unblocked_human_line(&unblocked);

        for text in [&close_message, &skipped_message, &unblocked_line] {
            assert!(!text.chars().any(char::is_control));
            assert!(text.contains("\\u{1b}[2J"));
        }
        assert!(close_message.contains("\\r"));
        assert!(close_message.contains("\\u{8}"));
        assert!(close_message.contains("\\n"));
        assert!(close_message.contains("\\u{7}"));
        assert!(close_message.contains("\\u{9b}"));
        assert!(skipped_message.contains("\\r"));
        assert!(skipped_message.contains("\\n"));
        assert!(skipped_message.contains("\\u{7}"));
        assert!(unblocked_line.contains("\\n"));
        assert!(unblocked_line.contains("\\u{8}"));
    }

    #[test]
    fn reorder_routed_items_sanitizes_missing_input_error() {
        let requested = vec!["bd-close\x1b[2J\nbad".to_string(), "bd-ok".to_string()];
        let routed_items = vec![(vec!["bd-ok".to_string()], vec!["ok"])];

        let err =
            reorder_routed_items_by_requested_inputs(&requested, routed_items, "close routing")
                .unwrap_err();

        assert!(
            matches!(err, BeadsError::Internal { .. }),
            "unexpected error: {err:?}"
        );
        if let BeadsError::Internal { message } = err {
            assert!(!message.chars().any(char::is_control));
            assert!(message.contains("\\u{1b}[2J"));
            assert!(message.contains("\\n"));
        }
    }

    #[test]
    fn reorder_routed_items_sanitizes_unexpected_input_error() {
        let requested = vec!["bd-ok".to_string()];
        let routed_items = vec![(vec!["bd-close\x1b[2J\nbad".to_string()], vec!["bad"])];

        let err =
            reorder_routed_items_by_requested_inputs(&requested, routed_items, "close routing")
                .unwrap_err();

        assert!(
            matches!(err, BeadsError::Internal { .. }),
            "unexpected error: {err:?}"
        );
        if let BeadsError::Internal { message } = err {
            assert!(!message.chars().any(char::is_control));
            assert!(message.contains("\\u{1b}[2J"));
            assert!(message.contains("\\n"));
        }
    }

    // =========================================================================
    // SkippedIssue serialization tests
    // =========================================================================

    #[test]
    fn test_skipped_issue_serialization() {
        let skipped = SkippedIssue {
            id: "bd-skip".to_string(),
            reason: "already closed".to_string(),
            already_terminal: false,
        };
        let json = serde_json::to_string(&skipped).unwrap();
        assert!(json.contains("\"id\":\"bd-skip\""));
        assert!(json.contains("\"reason\":\"already closed\""));
    }

    #[test]
    fn test_skipped_issue_blocked_reason() {
        let skipped = SkippedIssue {
            id: "bd-blocked".to_string(),
            reason: "blocked by: bd-dep1, bd-dep2".to_string(),
            already_terminal: false,
        };
        let json = serde_json::to_string(&skipped).unwrap();
        let parsed: SkippedIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "bd-blocked");
        assert!(parsed.reason.contains("bd-dep1"));
        assert!(parsed.reason.contains("bd-dep2"));
    }

    // =========================================================================
    // UnblockedIssue serialization tests
    // =========================================================================

    #[test]
    fn test_unblocked_issue_serialization() {
        let unblocked = UnblockedIssue {
            id: "bd-next".to_string(),
            title: "Next task".to_string(),
            priority: 1,
        };
        let json = serde_json::to_string(&unblocked).unwrap();
        assert!(json.contains("\"id\":\"bd-next\""));
        assert!(json.contains("\"title\":\"Next task\""));
        assert!(json.contains("\"priority\":1"));
    }

    #[test]
    fn test_unblocked_issue_priority_boundaries() {
        for priority in [0, 1, 2, 3, 4] {
            let unblocked = UnblockedIssue {
                id: format!("bd-p{priority}"),
                title: format!("Priority {priority} task"),
                priority,
            };
            let json = serde_json::to_string(&unblocked).unwrap();
            let parsed: UnblockedIssue = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.priority, priority);
        }
    }

    // =========================================================================
    // Edge case tests
    // =========================================================================

    #[test]
    fn test_close_result_multiple_closed_multiple_skipped() {
        let result = CloseResult {
            closed: vec![
                ClosedIssue {
                    id: "bd-1".to_string(),
                    title: "Task 1".to_string(),
                    status: "closed".to_string(),
                    closed_at: "2026-01-01T00:00:00Z".to_string(),
                    close_reason: None,
                },
                ClosedIssue {
                    id: "bd-2".to_string(),
                    title: "Task 2".to_string(),
                    status: "closed".to_string(),
                    closed_at: "2026-01-01T00:00:01Z".to_string(),
                    close_reason: Some("Batch close".to_string()),
                },
            ],
            skipped: vec![
                SkippedIssue {
                    id: "bd-3".to_string(),
                    reason: "issue not found".to_string(),
                    already_terminal: false,
                },
                SkippedIssue {
                    id: "bd-4".to_string(),
                    reason: "already tombstone".to_string(),
                    already_terminal: false,
                },
            ],
            warnings: vec![],
        };
        let json = serde_json::to_string_pretty(&result).unwrap();
        let parsed: CloseResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.closed.len(), 2);
        assert_eq!(parsed.skipped.len(), 2);
    }

    #[test]
    fn test_render_close_json_preserves_bare_array_for_pure_success() {
        let json = build_close_json_payload(
            &CloseArgs::default(),
            vec![ClosedIssue {
                id: "bd-1".to_string(),
                title: "Task 1".to_string(),
                status: "closed".to_string(),
                closed_at: "2026-01-01T00:00:00Z".to_string(),
                close_reason: Some("done".to_string()),
            }],
            vec![],
            vec![],
            vec![],
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_array());
    }

    #[test]
    fn test_close_result_shape_with_skipped_is_wrapped() {
        let json = build_close_json_payload(
            &CloseArgs::default(),
            vec![ClosedIssue {
                id: "bd-1".to_string(),
                title: "Task 1".to_string(),
                status: "closed".to_string(),
                closed_at: "2026-01-01T00:00:00Z".to_string(),
                close_reason: Some("done".to_string()),
            }],
            vec![SkippedIssue {
                id: "bd-2".to_string(),
                reason: "blocked by: bd-3".to_string(),
                already_terminal: false,
            }],
            vec![],
            vec![],
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_object());
        assert_eq!(parsed["closed"][0]["id"], "bd-1");
        assert_eq!(parsed["skipped"][0]["id"], "bd-2");
    }

    #[test]
    fn test_close_args_clone() {
        let args = CloseArgs {
            ids: vec!["bd-clone".to_string()],
            reason: Some("Clone test".to_string()),
            transition_comment: Some("Fresh transition evidence".to_string()),
            force: true,
            session: Some("sess".to_string()),
            suggest_next: true,
            keep_going: false,
        };
        let cloned = args.clone();
        assert_eq!(cloned.ids, args.ids);
        assert_eq!(cloned.reason, args.reason);
        assert_eq!(cloned.transition_comment, args.transition_comment);
        assert_eq!(cloned.force, args.force);
        assert_eq!(cloned.session, args.session);
        assert_eq!(cloned.suggest_next, args.suggest_next);
    }

    #[test]
    fn test_close_args_debug_impl() {
        let args = CloseArgs::default();
        let debug_str = format!("{args:?}");
        assert!(debug_str.contains("CloseArgs"));
        assert!(debug_str.contains("ids"));
        assert!(debug_str.contains("reason"));
    }

    #[test]
    fn execute_with_args_closes_requested_blocker_chain_in_one_batch() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-blocker", "Batch blocker"), "tester")
            .expect("create blocker");
        storage
            .create_issue(&make_issue("bd-blocked", "Batch blocked"), "tester")
            .expect("create blocked");
        storage
            .add_dependency(
                "bd-blocked",
                "bd-blocker",
                DependencyType::Blocks.as_str(),
                "tester",
            )
            .expect("add dependency");
        storage.rebuild_blocked_cache(true).expect("rebuild cache");
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-blocked".to_string(), "bd-blocker".to_string()],
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx).expect("close batch");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let blocker = storage
            .get_issue("bd-blocker")
            .expect("get blocker")
            .expect("blocker exists");
        let blocked_issue = storage
            .get_issue("bd-blocked")
            .expect("get blocked")
            .expect("blocked exists");

        assert_eq!(blocker.status, Status::Closed);
        assert_eq!(blocked_issue.status, Status::Closed);
    }

    #[test]
    fn execute_route_preserves_request_order_for_mixed_close_outcomes() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-close-first", "First"), "tester")
            .expect("create first");
        let mut skipped = make_issue_with_status("bd-close-skip", "Skip", Status::Closed);
        skipped.closed_at = Some(Utc::now());
        storage
            .create_issue(&skipped, "tester")
            .expect("create skipped");
        storage
            .create_issue(&make_issue("bd-close-last", "Last"), "tester")
            .expect("create last");
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec![
                "bd-close-first".to_string(),
                "bd-close-skip".to_string(),
                "bd-close-last".to_string(),
            ],
            ..CloseArgs::default()
        };
        let execution = execute_route(&args, &CliOverrides::default(), &ctx, &beads_dir, false)
            .expect("mixed close batch");

        assert!(matches!(
            &execution.ordered_outcomes[0],
            CloseOutcome::Closed(issue) if issue.id == "bd-close-first"
        ));
        assert!(matches!(
            &execution.ordered_outcomes[1],
            CloseOutcome::Skipped(issue) if issue.id == "bd-close-skip"
        ));
        assert!(matches!(
            &execution.ordered_outcomes[2],
            CloseOutcome::Closed(issue) if issue.id == "bd-close-last"
        ));
    }

    #[test]
    fn execute_with_args_closes_requested_dot_notation_child_with_parent() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-parent", "Legacy parent"), "tester")
            .expect("create parent");
        storage
            .create_issue(&make_issue("bd-parent.1", "Legacy child"), "tester")
            .expect("create dot child");
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-parent".to_string(), "bd-parent.1".to_string()],
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect("close parent and dot child in one batch");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let parent = storage
            .get_issue("bd-parent")
            .expect("get parent")
            .expect("parent exists");
        let child = storage
            .get_issue("bd-parent.1")
            .expect("get child")
            .expect("child exists");

        assert_eq!(parent.status, Status::Closed);
        assert_eq!(child.status, Status::Closed);
    }

    #[test]
    fn execute_with_args_keeps_parent_blocked_by_unrequested_dot_notation_child() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-parent", "Legacy parent"), "tester")
            .expect("create parent");
        storage
            .create_issue(&make_issue("bd-parent.1", "Legacy child"), "tester")
            .expect("create dot child");
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-parent".to_string()],
            ..CloseArgs::default()
        };
        let err = execute_with_args(&args, true, &CliOverrides::default(), &ctx)
            .expect_err("parent-only close should remain blocked by dot child");
        assert!(matches!(err, BeadsError::NothingToDo { .. }));

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let parent = storage
            .get_issue("bd-parent")
            .expect("get parent")
            .expect("parent exists");
        let child = storage
            .get_issue("bd-parent.1")
            .expect("get child")
            .expect("child exists");

        assert_eq!(parent.status, Status::Open);
        assert_eq!(child.status, Status::Open);
    }

    #[test]
    fn execute_with_args_closes_child_even_when_parent_epic_is_blocked() {
        // Regression for #355: a finished child must be closable even while its
        // parent epic is itself blocked by an unrelated prerequisite. The
        // `parent-child` edge is hierarchy, not a prerequisite from parent to
        // child, so the propagated `:parent-blocked` marker must not gate the
        // child's own closure.
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        // A deferred prerequisite that blocks the parent epic.
        storage
            .create_issue(
                &make_issue_with_status("bd-blocker", "Deferred blocker", Status::Deferred),
                "tester",
            )
            .expect("create blocker");
        // The parent epic, blocked by the deferred prerequisite.
        let mut parent = make_issue("bd-parent", "Parent epic");
        parent.issue_type = IssueType::Epic;
        storage
            .create_issue(&parent, "tester")
            .expect("create parent");
        storage
            .add_dependency(
                "bd-parent",
                "bd-blocker",
                DependencyType::Blocks.as_str(),
                "tester",
            )
            .expect("add blocks dependency");
        // A child of the parent epic, reviewed-complete and ready to close.
        storage
            .create_issue(&make_issue("bd-child", "Child task"), "tester")
            .expect("create child");
        storage
            .add_dependency(
                "bd-child",
                "bd-parent",
                DependencyType::ParentChild.as_str(),
                "tester",
            )
            .expect("add parent-child dependency");
        storage.rebuild_blocked_cache(true).expect("rebuild cache");
        // The child IS propagated `parent-blocked` in the readiness graph...
        assert!(
            storage
                .get_blockers("bd-child")
                .expect("get blockers")
                .contains(&"bd-parent".to_string()),
            "child should inherit a parent-blocked readiness marker"
        );
        // ...but it must NOT be a *close* blocker.
        assert!(
            storage
                .get_close_blockers("bd-child")
                .expect("get close blockers")
                .is_empty(),
            "a blocked parent must not gate the child's closure"
        );
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-child".to_string()],
            reason: Some("Child done".to_string()),
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect("child close should succeed despite blocked parent");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let child = storage
            .get_issue("bd-child")
            .expect("get child")
            .expect("child exists");
        let parent = storage
            .get_issue("bd-parent")
            .expect("get parent")
            .expect("parent exists");
        assert_eq!(child.status, Status::Closed, "child should be closed");
        assert_eq!(
            parent.status,
            Status::Open,
            "parent epic should remain open/blocked"
        );
    }

    #[test]
    fn execute_with_args_returns_nothing_to_do_when_all_requested_issues_are_skipped() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        let mut issue = make_issue("bd-closed", "Already closed");
        issue.status = Status::Closed;
        issue.closed_at = Some(Utc::now());
        storage
            .create_issue(&issue, "tester")
            .expect("create closed issue");

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-closed".to_string()],
            ..CloseArgs::default()
        };

        let err = execute_with_args(&args, true, &CliOverrides::default(), &ctx)
            .expect_err("all-skipped close should fail");
        assert!(matches!(err, BeadsError::NothingToDo { .. }));
    }

    // =========================================================================
    // Workflow gate enforcement at close (issue #312, layer 2 / beads#319)
    // =========================================================================

    const GATE_POLICY_YAML: &str = r#"workflow:
  strict: true
  gates:
    "in_review -> closed":
      require_all:
        - ci_green
"#;

    fn setup_gate_repo(temp: &TempDir, status: Status) -> std::path::PathBuf {
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");
        let beads_dir = temp.path().join(".beads");
        std::fs::write(beads_dir.join("policy.yaml"), GATE_POLICY_YAML).expect("write policy");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue_with_status("bd-1", "Gated", status), "tester")
            .expect("create issue");
        drop(storage);
        db_path
    }

    /// bds-04l.23. This test previously asserted the opposite: that an
    /// unsatisfied `ci_green` gate BLOCKS the close. It could only ever block.
    /// A gate was satisfied by a recorded provider verdict, and nothing in the
    /// shipped binary recorded one -- `record_scoped_gate_result` had exactly
    /// one caller, in this test module. The doc comments pointed at `br gate
    /// report` / `br gate list` for that, and neither existed anywhere in
    /// src/. So a `gates:` block was a permanent, uninspectable block on
    /// closing an issue, which is why the engine was removed rather than given
    /// the missing commands.
    #[test]
    fn workflow_gates_in_policy_no_longer_block_a_close() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let db_path = setup_gate_repo(&temp, Status::Custom("in_review".to_string()));
        let _guard = DirGuard::new(temp.path());
        let ctx = OutputContext::from_flags(false, false, true);

        let args = CloseArgs {
            ids: vec!["bd-1".to_string()],
            reason: Some("done".to_string()),
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect("a policy.yaml carrying workflow.gates must no longer block the close");

        let storage = SqliteStorage::open(&db_path).expect("reopen");
        assert_eq!(
            storage.get_issue("bd-1").unwrap().unwrap().status,
            Status::Closed,
            "the issue must actually close"
        );
    }

    #[test]
    fn close_unaffected_when_transition_not_gated() {
        // The gate only guards `in_review -> closed`; closing an `open` issue is
        // an `open -> closed` move with no rule, so it must proceed.
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let db_path = setup_gate_repo(&temp, Status::Open);
        let _guard = DirGuard::new(temp.path());
        let ctx = OutputContext::from_flags(false, false, true);

        let args = CloseArgs {
            ids: vec!["bd-1".to_string()],
            reason: Some("done".to_string()),
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect("open -> closed is not gated and must succeed");

        let storage = SqliteStorage::open(&db_path).expect("reopen");
        assert_eq!(
            storage.get_issue("bd-1").unwrap().unwrap().status,
            Status::Closed
        );
    }

    #[test]
    fn close_unaffected_with_no_policy_file() {
        // Backward-compat: no policy.yaml at all → close behaves as before.
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        {
            let mut storage = SqliteStorage::open(&db_path).expect("storage");
            storage
                .create_issue(
                    &make_issue_with_status(
                        "bd-1",
                        "Plain",
                        Status::Custom("in_review".to_string()),
                    ),
                    "tester",
                )
                .expect("create");
        }
        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-1".to_string()],
            reason: Some("done".to_string()),
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect("close must succeed with no policy file");
        let storage = SqliteStorage::open(&db_path).expect("reopen");
        assert_eq!(
            storage.get_issue("bd-1").unwrap().unwrap().status,
            Status::Closed
        );
    }
}
