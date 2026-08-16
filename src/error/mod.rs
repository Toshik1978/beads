//! Error types and handling for `beads`.
//!
//! This module provides structured errors that match the classic bd
//! behavior for JSON error output compatibility.
//!
//! # Design
//!
//! - Uses `thiserror` for derive-based error types
//! - Provides recovery hints for user-facing errors
//! - Matches bd's exit code conventions
//! - Provides structured JSON output for AI coding agents

mod context;
mod structured;

pub use context::ResultExt;
pub use structured::{ErrorCode, StructuredError};

use std::path::PathBuf;
use thiserror::Error;

/// Primary error type for `beads` operations.
///
/// Design: Structured variants for common cases.
#[derive(Error, Debug)]
pub enum BeadsError {
    // === Storage Errors ===
    /// Database file not found at the specified path.
    #[error("Database not found at '{path}'")]
    DatabaseNotFound { path: PathBuf },

    /// Database is locked by another process.
    #[error("Database is locked: {path}")]
    DatabaseLocked { path: PathBuf },

    /// Database schema version doesn't match expected.
    #[error("Schema version mismatch: expected {expected}, found {found}")]
    SchemaMismatch { expected: i32, found: i32 },

    /// `SQLite` database error.
    #[error("Database error: {0}")]
    Database(#[from] crate::storage::conn::DbError),

    // === Issue Errors ===
    /// Issue with the specified ID was not found.
    #[error("Issue not found: {id}")]
    IssueNotFound { id: String },

    /// Attempted to create an issue with an ID that already exists.
    #[error("Issue ID collision: {id}")]
    IdCollision { id: String },

    /// Partial ID matches multiple issues.
    #[error("Ambiguous ID '{partial}': matches {matches:?}")]
    AmbiguousId {
        partial: String,
        matches: Vec<String>,
    },

    /// Issue ID format is invalid.
    #[error("Invalid issue ID format: {id}")]
    InvalidId { id: String },

    // === Validation Errors ===
    /// Field validation failed.
    #[error("Validation failed: {field}: {reason}")]
    Validation { field: String, reason: String },

    /// Multiple validation errors occurred.
    #[error("Validation errors: {errors:?}")]
    ValidationErrors { errors: Vec<ValidationError> },

    /// Priority out of valid range (0-4).
    #[error("Priority must be 0-4, got: {priority}")]
    InvalidPriority { priority: String },

    /// A compare-and-set guard on `br update` did not hold, so nothing was
    /// written (bds-o9a).
    ///
    /// Distinct from [`Self::IssueNotFound`] on purpose: a caller retrying a
    /// guarded update needs to tell "someone else got there first" (retry, or
    /// concede) from "there is nothing to update" (stop), and a shared error
    /// would make both look the same.
    #[error("Precondition failed on {id}: expected {field} to be '{expected}', found '{actual}'")]
    PreconditionFailed {
        id: String,
        field: String,
        expected: String,
        actual: String,
    },

    // === JSONL Errors ===
    /// Failed to parse a line in the JSONL file.
    #[error("JSONL parse error at line {line}: {reason}")]
    JsonlParse { line: usize, reason: String },

    /// Issue prefix doesn't match expected prefix.
    #[error("Prefix mismatch: expected '{expected}', found '{found}'")]
    PrefixMismatch { expected: String, found: String },

    /// Import found conflicting issues.
    #[error("Import collision: {count} issues have conflicting content")]
    ImportCollision { count: usize },

    /// Conflict detected between local and external changes.
    #[error("Sync conflict: {message}")]
    SyncConflict { message: String },

    // === Dependency Errors ===
    /// Adding the dependency would create a cycle.
    #[error("Cycle detected in dependencies: {path}")]
    DependencyCycle { path: String },

    /// Cannot delete an issue that has dependents.
    #[error("Cannot delete: {id} has {count} dependents")]
    HasDependents { id: String, count: usize },

    /// Self-referential dependency.
    #[error("Issue cannot depend on itself: {id}")]
    SelfDependency { id: String },

    /// Dependency target not found.
    #[error("Dependency target not found: {id}")]
    DependencyNotFound { id: String },

    /// Duplicate dependency.
    #[error("Dependency already exists: {from} -> {to}")]
    DuplicateDependency { from: String, to: String },

    // === Configuration Errors ===
    /// Configuration file error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// External command failed or returned unusable output.
    #[error("External command failed: {command}: {reason}")]
    ExternalCommand { command: String, reason: String },

    /// Internal consistency check failed.
    #[error("Internal error: {message}")]
    Internal { message: String },

    /// Beads workspace not initialized.
    #[error("Beads not initialized: run 'br init' first")]
    NotInitialized,

    /// Already initialized.
    #[error("Already initialized at '{path}'")]
    AlreadyInitialized { path: PathBuf },

    // === I/O Errors ===
    /// File system I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// YAML parsing error.
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yml::Error),

    // === Wrapped errors (for gradual migration) ===
    /// Error with additional context.
    #[error("{context}: {source}")]
    WithContext {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    // === Operational Errors ===
    /// Operation refused because cooperative shutdown has already been requested.
    #[error("Shutdown requested")]
    ShuttingDown,

    /// All requested items were skipped (already closed, not found, etc.).
    #[error("Nothing to do: {reason}")]
    NothingToDo { reason: String },

    /// Some of a batch succeeded and some did not (bds-yo8).
    ///
    /// Reported only when the caller opted into continuing past failures
    /// (`br close --continue`), and deliberately *not* folded into
    /// [`Self::NothingToDo`]: "nothing to do" in front of "closed 4 issue(s)"
    /// is a lie, and an agent reading only the error code needs to know that
    /// part of the work landed before it decides whether to retry the whole
    /// batch.
    #[error("Partially completed: {reason}")]
    PartiallyCompleted { reason: String },

    /// A command fanned out across routed workspaces failed after at least one
    /// route had already committed (bds-j1m).
    ///
    /// Distinct from [`Self::PartiallyCompleted`], which reports a batch the
    /// caller explicitly asked to continue past failures. This one is not
    /// opt-in and not retryable: it reports damage the caller did not ask for
    /// and cannot undo, because each routed workspace is its own database and
    /// its own transaction, so nothing rolls back a route that already landed.
    ///
    /// Boxed to keep [`BeadsError`] small — three id lists plus a nested error
    /// would otherwise set the size of every `Result` in the crate.
    #[error("{}", .0.render())]
    PartiallyApplied(Box<PartialApplication>),

    // === Policy Errors ===
    /// One or more `workflow.required_fields` gates fired for a status
    /// transition (issue #312/#388) — raised from any command that changes
    /// an issue's status, `br close` included.
    ///
    /// Display format intentionally repeats the gate that fired and a
    /// short explanation so terminal output stays readable; structured
    /// callers should serialise the inner [`crate::close_policy::PolicyViolation`]s
    /// via [`StructuredError::context`].
    #[error("Policy violation closing {issue_id}: {summary}")]
    PolicyViolation {
        issue_id: String,
        summary: String,
        violations: Vec<crate::close_policy::PolicyViolation>,
    },

    /// A status transition would exceed an atomically enforced workflow
    /// capacity or cross-queue admission threshold (GitHub #384).
    #[error("{violation}")]
    WorkflowCapacityExceeded {
        violation: Box<crate::close_policy::WorkflowCapacityViolation>,
    },
}

/// Which targets a fan-out command wrote before it failed (bds-j1m).
///
/// The three buckets are deliberately not two: a route that failed *after* its
/// field update committed but *before* its label or re-parent work finished is
/// neither cleanly applied nor cleanly untouched, and collapsing it into either
/// bucket would state something the command cannot know.
#[derive(Debug)]
pub struct PartialApplication {
    /// Targets whose route committed in full before the failure. These are
    /// written and cannot be rolled back.
    pub applied: Vec<String>,
    /// Targets in the route that failed after it had already written
    /// something. A route is atomic in its field update but not across the
    /// label and re-parent steps that follow it, so these may be partly
    /// updated.
    pub uncertain: Vec<String>,
    /// Targets whose route was never reached, or whose route failed before
    /// writing anything. These are untouched.
    pub not_applied: Vec<String>,
    /// The failure that stopped the fan-out.
    pub source: BeadsError,
}

impl PartialApplication {
    /// Render the operator-facing message.
    ///
    /// Each bucket is named on its own line because the whole point is that
    /// the caller must read them; a single-line summary invites skipping to
    /// the cause, which is the part that does *not* say what landed.
    #[must_use]
    pub fn render(&self) -> String {
        let mut message = String::from(
            "Partially applied across routed workspaces: routes commit independently, \
             so nothing rolls back what already landed.",
        );
        for (label, ids) in [
            ("written", &self.applied),
            ("possibly partly written", &self.uncertain),
            ("not written", &self.not_applied),
        ] {
            if !ids.is_empty() {
                message.push_str(&format!("\n  {label}: {}", render_id_list(ids)));
            }
        }
        message.push_str(&format!("\n  cause: {}", self.source));
        message
    }
}

/// Join ids for an operator-facing list, naming at most eight.
///
/// The full lists are always present in the structured `context`, so truncating
/// here loses nothing a machine consumer needs.
fn render_id_list(ids: &[String]) -> String {
    const MAX_NAMED: usize = 8;
    if ids.len() <= MAX_NAMED {
        return ids.join(", ");
    }
    format!(
        "{}, (+{} more)",
        ids[..MAX_NAMED].join(", "),
        ids.len() - MAX_NAMED
    )
}

impl BeadsError {
    /// Returns true if the error is transient and can be retried.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Database(e) => e.is_transient(),
            Self::ShuttingDown => true,
            Self::Io(e) => {
                matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                )
            }
            _ => false,
        }
    }
}

/// A single field validation error.
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// The field that failed validation.
    pub field: String,
    /// The reason for the validation failure.
    pub message: String,
}

impl ValidationError {
    /// Create a new validation error.
    #[must_use]
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for ValidationError {}

impl BeadsError {
    /// Can the user fix this without code changes?
    #[must_use]
    pub const fn is_user_recoverable(&self) -> bool {
        matches!(
            self,
            Self::DatabaseNotFound { .. }
                | Self::NotInitialized
                | Self::IssueNotFound { .. }
                | Self::Validation { .. }
                | Self::InvalidPriority { .. }
                | Self::PrefixMismatch { .. }
                | Self::AmbiguousId { .. }
                | Self::PolicyViolation { .. }
                | Self::WorkflowCapacityExceeded { .. }
                | Self::PreconditionFailed { .. }
                | Self::PartiallyCompleted { .. }
                | Self::PartiallyApplied(_)
        )
    }

    /// Should we suggest re-running with --force?
    #[must_use]
    pub const fn suggests_force(&self) -> bool {
        matches!(
            self,
            Self::HasDependents { .. }
                | Self::ImportCollision { .. }
                | Self::AlreadyInitialized { .. }
        )
    }

    /// Human-friendly suggestion for fixing this error.
    #[must_use]
    pub const fn suggestion(&self) -> Option<&'static str> {
        match self {
            Self::NotInitialized => Some("Run: br init"),
            Self::DatabaseNotFound { .. } => Some("Check path or run: br init"),
            Self::AmbiguousId { .. } => Some("Provide more characters of the ID"),
            Self::HasDependents { .. } => Some("Use --force or --cascade to delete anyway"),
            Self::ImportCollision { .. } => Some("Use --force to overwrite or resolve manually"),
            Self::DependencyCycle { .. } => Some("Remove one dependency to break the cycle"),
            Self::SelfDependency { .. } => Some("An issue cannot depend on itself"),
            Self::AlreadyInitialized { .. } => Some("Use --force to reinitialize"),
            Self::InvalidPriority { .. } => {
                Some("Use a priority between 0 (critical) and 4 (backlog)")
            }
            Self::PolicyViolation { .. } => Some(
                "Fix the violation(s) above and retry, or supply the missing --transition-comment / satisfy the workflow's required fields for this transition.",
            ),
            Self::WorkflowCapacityExceeded { .. } => Some(
                "Drain the named queue before admitting fresh work; inspect it with `br list --status <status>`.",
            ),
            Self::PartiallyCompleted { .. } => Some(
                "Part of the batch succeeded. Fix the reported items and re-run; the ones that already succeeded will be reported as already closed and will not be redone.",
            ),
            Self::PartiallyApplied(_) => Some(
                "Do not re-run the same command: the targets listed as written have already moved, so a guard such as --if-status will now reject them too. Re-run against the remaining targets only.",
            ),
            Self::PreconditionFailed { .. } => Some(
                "Nothing was written. Re-read the issue with `br show` and decide whether to retry against the value it holds now.",
            ),
            _ => None,
        }
    }

    /// Get the exit code for this error.
    ///
    /// Delegates to [`ErrorCode::exit_code()`] via [`StructuredError`] for
    /// consistent, categorized exit codes (1–8).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        StructuredError::from_error(self).code.exit_code()
    }

    /// Create a validation error for a specific field.
    #[must_use]
    pub fn validation(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Validation {
            field: field.into(),
            reason: reason.into(),
        }
    }

    /// Create an external command failure.
    #[must_use]
    pub fn external_command(command: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::ExternalCommand {
            command: command.into(),
            reason: reason.into(),
        }
    }

    /// Create an internal consistency error.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }

    /// Create from multiple validation errors.
    #[must_use]
    pub fn from_validation_errors(errors: Vec<ValidationError>) -> Self {
        if errors.is_empty() {
            Self::ValidationErrors { errors }
        } else if errors.len() == 1 {
            let err = &errors[0];
            Self::Validation {
                field: err.field.clone(),
                reason: err.message.clone(),
            }
        } else {
            Self::ValidationErrors { errors }
        }
    }
}

/// Result type using `BeadsError`.
pub type Result<T> = std::result::Result<T, BeadsError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = BeadsError::IssueNotFound {
            id: "bd-abc123".to_string(),
        };
        assert_eq!(err.to_string(), "Issue not found: bd-abc123");
    }

    #[test]
    fn test_validation_error() {
        let err = BeadsError::validation("title", "cannot be empty");
        assert_eq!(err.to_string(), "Validation failed: title: cannot be empty");
    }

    #[test]
    fn test_external_command_uses_io_error_code() {
        let err = BeadsError::external_command("git", "failed to resolve ref");
        let structured = StructuredError::from_error(&err);

        assert_eq!(structured.code, ErrorCode::IoError);
        assert_eq!(err.exit_code(), 8);
    }

    #[test]
    fn test_internal_uses_internal_error_code() {
        let err = BeadsError::internal("routed command produced mismatched counts");
        let structured = StructuredError::from_error(&err);

        assert_eq!(structured.code, ErrorCode::InternalError);
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn test_user_recoverable() {
        let recoverable = BeadsError::NotInitialized;
        assert!(recoverable.is_user_recoverable());

        let not_recoverable =
            BeadsError::Database(crate::storage::conn::DbError::Internal("test".to_string()));
        assert!(!not_recoverable.is_user_recoverable());
    }

    #[test]
    fn test_suggestion() {
        let err = BeadsError::NotInitialized;
        assert_eq!(err.suggestion(), Some("Run: br init"));

        let err = BeadsError::AmbiguousId {
            partial: "bd-a".to_string(),
            matches: vec!["bd-abc".to_string(), "bd-abd".to_string()],
        };
        assert_eq!(err.suggestion(), Some("Provide more characters of the ID"));
    }

    #[test]
    fn test_validation_error_struct() {
        let err = ValidationError::new("priority", "must be 0-4");
        assert_eq!(err.to_string(), "priority: must be 0-4");
    }
}
