//! The single field list `br show` renders, shared by both renderers.
//!
//! bds-04l.9. `show` has two independent renderers -- `format_issue_details`
//! (Plain, chosen when stdout is not a terminal) and `IssuePanel::print`
//! (Rich, chosen when it is). They shared no field list, and the Rich one
//! omitted nine fields the Plain one emitted, most visibly Design, Acceptance
//! Criteria and Notes: exactly where a planned issue's content lives. So the
//! human-facing path was the lossy one and the piped path was complete, which
//! is backwards.
//!
//! Everything here is derived from `Issue` alone, deliberately. `IssuePanel`
//! is also constructed via `IssuePanel::new` from a bare `Issue` with no
//! `IssueDetails` (see `src/cli/commands/show.rs`), so a list that needed
//! `IssueDetails` could not feed it. Labels and comments stay outside this
//! module for that reason -- they genuinely come from `IssueDetails`.
//!
//! Adding a field here makes it appear in the Rich panel automatically, and
//! `plain_renderer_covers_every_shared_field` in
//! `src/cli/commands/show.rs` fails until the Plain renderer emits it too.

use crate::model::Issue;
use crate::util::time::to_local;

/// One scalar metadata row: a label and its rendered value.
///
/// Rendered as `"{label}: {value}"` by Plain and as an aligned two-column row
/// by Rich, so the label text is shared but the layout is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataRow {
    pub label: &'static str,
    pub value: String,
}

/// One prose block, rendered under its own heading by both renderers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProseSection<'a> {
    pub heading: &'static str,
    pub body: &'a str,
}

/// The scalar metadata rows for `issue`, in render order.
///
/// Absent and empty values are skipped, so an issue with no defer date does
/// not render an empty `Deferred until:` row.
#[must_use]
pub fn metadata_rows(issue: &Issue) -> Vec<MetadataRow> {
    let mut rows = Vec::new();
    let mut push = |label: &'static str, value: String| {
        rows.push(MetadataRow { label, value });
    };

    if let Some(owner) = issue.owner.as_deref().filter(|value| !value.is_empty()) {
        push("Owner", owner.to_string());
    }
    if let Some(reference) = issue
        .external_ref
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        push("Ref", reference.to_string());
    }
    if let Some(defer) = issue.defer_until {
        push(
            "Deferred until",
            to_local(defer).format("%Y-%m-%d").to_string(),
        );
    }
    if let Some(closed) = issue.closed_at {
        let reason = issue.close_reason.as_deref().unwrap_or("closed");
        push(
            "Closed",
            format!("{} ({reason})", to_local(closed).format("%Y-%m-%d")),
        );
    }

    rows
}

/// The prose sections for `issue`, in render order.
#[must_use]
pub fn prose_sections(issue: &Issue) -> Vec<ProseSection<'_>> {
    [
        ("Design", issue.design.as_deref()),
        ("Acceptance Criteria", issue.acceptance_criteria.as_deref()),
        ("Notes", issue.notes.as_deref()),
    ]
    .into_iter()
    .filter_map(|(heading, body)| {
        body.filter(|text| !text.is_empty())
            .map(|body| ProseSection { heading, body })
    })
    .collect()
}
