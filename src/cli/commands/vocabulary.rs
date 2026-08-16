//! `br statuses` and `br types` — print the vocabulary this project accepts
//! (bds-b4h).
//!
//! These are worth more here than in the implementation they were ported from,
//! because the answer is project-specific rather than a constant. `Status::Custom`
//! means any status string is accepted *unless* `.beads/policy.yaml` enumerates a
//! set under `workflow.statuses` with `workflow.strict: true` — and until now
//! there was no way to ask which of those two worlds you were in, let alone what
//! the set was.
//!
//! That is also the hole bds-npo left. It deleted an `InvalidStatus` error whose
//! suggestion hard-coded the built-in vocabulary, on the grounds that in a strict
//! workspace that list is the wrong list and in an ordinary one there is nothing
//! to reject. Both halves of that argument are true, and both are reasons to
//! answer the question *here*, where the policy is in hand, instead of guessing
//! from an error path.
//!
//! `br types` is the flatter of the two: `policy.yaml` has no type vocabulary
//! today, so it prints the built-in set and says plainly that any other value is
//! accepted. That is not a placeholder for a future policy key — it is the
//! honest answer, and printing it is the point.

use crate::close_policy::{self, Workflow};
use crate::config;
use crate::error::Result;
use crate::output::OutputContext;
use serde::Serialize;

/// One entry in the effective status vocabulary.
#[derive(Debug, Clone, Serialize)]
struct StatusEntry {
    name: String,
    /// Shipped by `br` itself, as opposed to appearing only in `policy.yaml`.
    builtin: bool,
    /// Listed in `workflow.statuses`.
    in_policy: bool,
    /// Usable on this project right now. Identical to `in_policy` when the
    /// policy is enforcing, and true for everything when it is not.
    allowed: bool,
}

#[derive(Debug, Clone, Serialize)]
struct StatusesOutput {
    /// `workflow.strict` as configured.
    strict: bool,
    /// Whether the policy actually rejects anything: `strict` **and** a non-empty
    /// `workflow.statuses`. A strict policy with no status list enforces nothing,
    /// which is a trap worth reporting separately from `strict` itself.
    enforced: bool,
    /// True when any string is a valid status, which is the unconfigured default.
    any_value_accepted: bool,
    /// The statuses `br ready` treats as actionable (`workflow.status_groups.ready`).
    ready_group: Vec<String>,
    statuses: Vec<StatusEntry>,
}

#[derive(Debug, Clone, Serialize)]
struct TypeEntry {
    name: String,
    builtin: bool,
}

#[derive(Debug, Clone, Serialize)]
struct TypesOutput {
    /// Always true today: `IssueType::Custom` accepts anything and `policy.yaml`
    /// has no type vocabulary to narrow it with.
    any_value_accepted: bool,
    types: Vec<TypeEntry>,
}

/// Print the effective status vocabulary.
///
/// # Errors
///
/// Returns an error if the workspace cannot be found or `policy.yaml` cannot be
/// read or parsed.
pub fn statuses(json: bool, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let workflow = close_policy::load_for_beads_dir(&beads_dir)?.workflow;
    let output = collect_statuses(&workflow);

    if json || ctx.is_json() {
        emit_json(json, ctx, &output);
        return Ok(());
    }

    ctx.print_line("Statuses:");
    for entry in &output.statuses {
        let mut notes = Vec::new();
        notes.push(if entry.builtin { "built-in" } else { "policy" });
        if output.enforced {
            notes.push(if entry.allowed {
                "allowed"
            } else {
                "NOT allowed"
            });
        }
        ctx.print_line(&format!("  {:<14} {}", entry.name, notes.join(", ")));
    }

    ctx.print_line("");
    if output.enforced {
        ctx.print_line(
            "workflow.strict is enforcing: a status outside the allowed set is rejected.",
        );
    } else if output.strict {
        ctx.print_line(
            "workflow.strict is set but workflow.statuses is empty, so nothing is enforced \
             and any status value is accepted.",
        );
    } else {
        ctx.print_line(
            "No status policy is configured, so any status value is accepted. Set \
             workflow.strict and workflow.statuses in .beads/policy.yaml to enumerate a set.",
        );
    }
    ctx.print_line(&format!(
        "br ready treats these as actionable: {}",
        output.ready_group.join(", ")
    ));
    // Worth saying whether or not a policy is configured, and especially when one
    // is: a reader who sees `tombstone  NOT allowed` above would otherwise have
    // to wonder whether their policy has broken `br delete`. It has not -- these
    // two are never reached through `--status` in the first place.
    ctx.print_line(
        "closed and tombstone are not settable with `br update --status`: use \
         `br close` and `br delete`, which apply their own transition and \
         capacity checks and rewire dependencies.",
    );

    Ok(())
}

/// Print the issue-type vocabulary.
///
/// # Errors
///
/// Returns an error if the workspace cannot be found. (The workspace is not
/// strictly needed to answer this, but requiring it keeps `br types` consistent
/// with `br statuses`, which is: a caller asking "what may I use *here*" should
/// not get an answer from a command that never looked at *here*.)
pub fn types(json: bool, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    config::discover_beads_dir_with_cli(cli)?;
    let output = TypesOutput {
        any_value_accepted: true,
        types: crate::cli::ISSUE_TYPE_CANDIDATES
            .iter()
            .map(|(name, _)| TypeEntry {
                name: (*name).to_string(),
                builtin: true,
            })
            .collect(),
    };

    if json || ctx.is_json() {
        emit_json(json, ctx, &output);
        return Ok(());
    }

    ctx.print_line("Issue types:");
    for entry in &output.types {
        ctx.print_line(&format!("  {:<14} built-in", entry.name));
    }
    ctx.print_line("");
    ctx.print_line(
        "Any other value is also accepted and stored as-is: there is no type \
         vocabulary in .beads/policy.yaml to narrow this, unlike statuses.",
    );

    Ok(())
}

/// Merge the built-in vocabulary with `workflow.statuses`.
///
/// Built-ins come first in their declared order and the policy-only values
/// follow, so the output reads as "what br ships, then what this project added"
/// rather than in whatever order the YAML happened to be written.
fn collect_statuses(workflow: &Workflow) -> StatusesOutput {
    let enforced = workflow.is_enforced();

    let mut statuses: Vec<StatusEntry> = crate::cli::STATUS_CANDIDATES
        .iter()
        .map(|(name, _)| {
            let in_policy = workflow.allows(name);
            StatusEntry {
                name: (*name).to_string(),
                builtin: true,
                in_policy,
                allowed: !enforced || in_policy,
            }
        })
        .collect();

    for configured in &workflow.statuses {
        let normalized = configured.trim().to_lowercase();
        if normalized.is_empty() || statuses.iter().any(|entry| entry.name == normalized) {
            continue;
        }
        statuses.push(StatusEntry {
            name: normalized,
            builtin: false,
            in_policy: true,
            allowed: true,
        });
    }

    StatusesOutput {
        strict: workflow.strict,
        enforced,
        any_value_accepted: !enforced,
        ready_group: workflow.ready_status_group(),
        statuses,
    }
}

/// `--json` on a command whose `OutputContext` may not be in JSON mode: the same
/// shape `detach` and `rename` use.
fn emit_json<T: Serialize>(json: bool, ctx: &OutputContext, payload: &T) {
    if ctx.is_json() {
        ctx.json_pretty(payload);
    } else if json {
        OutputContext::from_flags(true, false, true).json_pretty(payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IssueType, Status};

    /// The vocabulary tables are hand-written lists beside two enums. The
    /// exhaustive-match reminders in `cli::mod` catch a variant being *added*;
    /// this catches an entry going stale the other way — a value that no longer
    /// parses to a built-in, which would make `br statuses` label a `Custom`
    /// value as `built-in`.
    #[test]
    fn every_listed_status_parses_back_to_a_builtin() {
        for (name, _) in crate::cli::STATUS_CANDIDATES {
            let parsed: Status = name.parse().expect("built-in statuses must parse");
            assert!(
                !matches!(parsed, Status::Custom(_)),
                "{name} is listed as built-in but parses to Status::Custom"
            );
            assert_eq!(
                parsed.as_str(),
                *name,
                "{name} must round-trip: the table is what `br statuses` prints, and the \
                 wire value is what the database stores"
            );
        }
    }

    #[test]
    fn every_listed_issue_type_parses_back_to_a_builtin() {
        for (name, _) in crate::cli::ISSUE_TYPE_CANDIDATES {
            let parsed: IssueType = name.parse().expect("built-in types must parse");
            assert!(
                !matches!(parsed, IssueType::Custom(_)),
                "{name} is listed as built-in but parses to IssueType::Custom"
            );
            assert_eq!(parsed.as_str(), *name);
        }
    }

    #[test]
    fn an_unconfigured_workflow_accepts_anything_and_says_so() {
        let output = collect_statuses(&Workflow::default());
        assert!(!output.strict);
        assert!(!output.enforced);
        assert!(output.any_value_accepted);
        assert_eq!(output.ready_group, vec!["open".to_string()]);
        assert!(
            output.statuses.iter().all(|entry| entry.allowed),
            "with nothing enforced, nothing can be disallowed"
        );
        assert!(output.statuses.iter().all(|entry| entry.builtin));
    }

    /// The reported vocabulary is the project's, not the built-in one — which is
    /// the whole reason this command exists rather than a static list in a help
    /// string.
    #[test]
    fn a_strict_workflow_reports_its_own_vocabulary() {
        let workflow = Workflow {
            strict: true,
            statuses: vec!["open".to_string(), "Rework".to_string()],
            ..Workflow::default()
        };

        let output = collect_statuses(&workflow);
        assert!(output.enforced);
        assert!(!output.any_value_accepted);

        let allowed: Vec<&str> = output
            .statuses
            .iter()
            .filter(|entry| entry.allowed)
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            allowed,
            vec!["open", "rework"],
            "a built-in outside the policy set has to be reported as not allowed, and a \
             policy-only value has to appear at all: {:?}",
            output.statuses
        );

        let rework = output
            .statuses
            .iter()
            .find(|entry| entry.name == "rework")
            .expect("policy-only status must be listed");
        assert!(
            !rework.builtin,
            "a value that exists only in policy.yaml must not be labelled built-in"
        );

        let closed = output
            .statuses
            .iter()
            .find(|entry| entry.name == "closed")
            .expect("built-ins stay listed");
        assert!(closed.builtin && !closed.allowed);
    }

    /// `strict: true` with an empty status list enforces nothing. Reported as its
    /// own state rather than folded into either extreme, because a project that
    /// set `strict` and expected enforcement has a real problem and this is the
    /// command that can show it.
    #[test]
    fn strict_with_no_status_list_is_reported_as_not_enforcing() {
        let workflow = Workflow {
            strict: true,
            ..Workflow::default()
        };

        let output = collect_statuses(&workflow);
        assert!(output.strict);
        assert!(!output.enforced);
        assert!(output.any_value_accepted);
    }
}
