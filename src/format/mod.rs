//! Output formatting for `beads`.
//!
//! Supports human-readable text output, machine-parseable JSON, and CSV export.
//! Robot mode sends clean JSON to stdout with diagnostics to stderr.
//!
//! # Output Types
//!
//! These types match the classic bd JSON schemas for CLI compatibility:
//! - [`IssueWithCounts`] - Issue with dependency/dependent counts (list/search)
//! - [`IssueDetails`] - Issue with full relations (show)
//! - [`BlockedIssue`] - Issue with blocking info (blocked)
//! - [`Statistics`] - Aggregate stats (stats/status)
//!
//! # CSV Output
//!
//! The [`csv`] module provides CSV formatting with:
//! - Configurable field selection via `--fields`
//! - Proper escaping of commas, quotes, and newlines
//!
//! # Rich Output
//!
//! Enhanced terminal output using `rich_rust` lives under
//! `crate::output::components`:
//! - Tables with styled columns for issue lists
//! - Panels for detailed issue views, including rendered markdown via
//!   [`markdown::render_markdown_text`]
//! - Trees for dependency visualization
//! - Consistent theming via [`crate::output::Theme`]
//!
//! Output mode is determined by [`crate::output::OutputContext`]:
//! - Rich: TTY with colors enabled
//! - Plain: TTY with `--no-color` or not a TTY
//! - JSON: `--json` flag
//! - Quiet: `--quiet` flag

pub mod csv;
pub mod markdown;
mod output;
pub mod show_fields;
mod text;

pub use output::{
    BlockedIssue, BlockedIssueOutput, Breakdown, BreakdownEntry, IssueDetails, IssueWithCounts,
    IssueWithDependencyMetadata, ReadyIssue, RecentActivity, StaleIssue, Statistics, StatsSummary,
};
pub use text::{
    TextFormatOptions, format_issue_line_with, format_issue_long_with, format_issue_pretty_with,
    format_priority, format_priority_badge, format_priority_label, format_status_icon,
    format_status_icon_colored, format_status_label, format_type_badge, format_type_badge_colored,
    format_type_label, sanitize_terminal_inline, sanitize_terminal_text, terminal_height,
    terminal_width, truncate_title,
};

#[cfg(test)]
pub use text::format_issue_line;

// Markdown rendering
pub use markdown::render_markdown_text;
