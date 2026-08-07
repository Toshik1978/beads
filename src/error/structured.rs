//! Structured error output for AI coding agents.
//!
//! Provides machine-parseable error information with:
//! - Error codes for categorization
//! - Hints for self-correction
//! - Retryability flags
//! - Context for debugging
//!
//! # Design Patterns (from `mcp_agent_mail`)
//!
//! This module adapts the structured error pattern from `mcp_agent_mail`.
//! Key concepts:
//!
//! - Intent detection: Recognize common agent mistakes
//! - O(1) validation: Precomputed valid value sets
//! - Levenshtein suggestions: Find similar IDs
//! - Graceful defaults: Auto-fix what you can

#![allow(clippy::option_if_let_else, clippy::manual_map, clippy::manual_find)]

use crate::error::BeadsError;
use crate::format::sanitize_terminal_text;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::LazyLock;

const PRIORITY_SHORT_HINT: &str = "Priority must be 0-4 (0=critical, 4=backlog).";

#[must_use]
fn flag_value_hint(flag: &str, detected: &str) -> String {
    format!("Did you mean --{flag} {detected}?")
}

/// Machine-readable error codes.
///
/// These codes are stable and can be used for programmatic error handling.
/// Format: `SCREAMING_SNAKE_CASE` for easy parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    // === Database Errors (exit code 2) ===
    /// Database file not found
    DatabaseNotFound,
    /// Database is locked by another process
    DatabaseLocked,
    /// Database schema version mismatch
    SchemaMismatch,
    /// Database operation failed
    DatabaseError,
    /// Beads workspace not initialized
    NotInitialized,
    /// Already initialized
    AlreadyInitialized,

    // === Issue Errors (exit code 3) ===
    /// Issue with specified ID not found
    IssueNotFound,
    /// Partial ID matches multiple issues
    AmbiguousId,
    /// Issue ID collision on create
    IdCollision,
    /// Invalid issue ID format
    InvalidId,

    // === Validation Errors (exit code 4) ===
    /// Field validation failed
    ValidationFailed,
    /// Priority out of range (0-4)
    InvalidPriority,
    /// Required field missing
    RequiredField,

    // === Dependency Errors (exit code 5) ===
    /// Dependency cycle detected
    CycleDetected,
    /// Dependency target not found
    DependencyNotFound,
    /// Cannot delete: has dependents
    HasDependents,
    /// Issue cannot depend on itself
    SelfDependency,
    /// Duplicate dependency
    DuplicateDependency,

    // === Sync/JSONL Errors (exit code 6) ===
    /// JSONL parse error
    JsonlParseError,
    /// Prefix mismatch during import
    PrefixMismatch,
    /// Import collision detected
    ImportCollision,
    /// Conflict detected between local database changes and newer JSONL
    SyncConflict,
    /// Conflict markers in JSONL
    ConflictMarkers,
    /// Path traversal attempt blocked
    PathTraversal,

    // === Config Errors (exit code 7) ===
    /// Configuration error
    ConfigError,
    /// Config file not found
    ConfigNotFound,
    /// Config parse error
    ConfigParseError,

    // === I/O Errors (exit code 8) ===
    /// File I/O error
    IoError,
    /// JSON serialization error
    JsonError,
    /// YAML parsing error
    YamlError,

    // === Operational Errors ===
    /// Cooperative shutdown is already in progress
    ShuttingDown,
    /// All requested items were skipped; nothing to do
    NothingToDo,

    // === Policy Errors (exit code 4) ===
    /// Closure-time policy gate fired (issue #274)
    PolicyViolation,
    /// Atomic workflow capacity/admission guard fired (GitHub #384)
    WorkflowCapacityExceeded,

    // === Internal Errors (exit code 1) ===
    /// Unexpected internal error
    InternalError,
}

impl ErrorCode {
    /// Get the string representation for JSON output.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            // Database
            Self::DatabaseNotFound => "DATABASE_NOT_FOUND",
            Self::DatabaseLocked => "DATABASE_LOCKED",
            Self::SchemaMismatch => "SCHEMA_MISMATCH",
            Self::DatabaseError => "DATABASE_ERROR",
            Self::NotInitialized => "NOT_INITIALIZED",
            Self::AlreadyInitialized => "ALREADY_INITIALIZED",
            // Issue
            Self::IssueNotFound => "ISSUE_NOT_FOUND",
            Self::AmbiguousId => "AMBIGUOUS_ID",
            Self::IdCollision => "ID_COLLISION",
            Self::InvalidId => "INVALID_ID",
            // Validation
            Self::ValidationFailed => "VALIDATION_FAILED",
            Self::InvalidPriority => "INVALID_PRIORITY",
            Self::RequiredField => "REQUIRED_FIELD",
            // Dependency
            Self::CycleDetected => "CYCLE_DETECTED",
            Self::DependencyNotFound => "DEPENDENCY_NOT_FOUND",
            Self::HasDependents => "HAS_DEPENDENTS",
            Self::SelfDependency => "SELF_DEPENDENCY",
            Self::DuplicateDependency => "DUPLICATE_DEPENDENCY",
            // Sync
            Self::JsonlParseError => "JSONL_PARSE_ERROR",
            Self::PrefixMismatch => "PREFIX_MISMATCH",
            Self::ImportCollision => "IMPORT_COLLISION",
            Self::SyncConflict => "SYNC_CONFLICT",
            Self::ConflictMarkers => "CONFLICT_MARKERS",
            Self::PathTraversal => "PATH_TRAVERSAL",
            // Config
            Self::ConfigError => "CONFIG_ERROR",
            Self::ConfigNotFound => "CONFIG_NOT_FOUND",
            Self::ConfigParseError => "CONFIG_PARSE_ERROR",
            // I/O
            Self::IoError => "IO_ERROR",
            Self::JsonError => "JSON_ERROR",
            Self::YamlError => "YAML_ERROR",
            // Operational
            Self::ShuttingDown => "SHUTTING_DOWN",
            Self::NothingToDo => "NOTHING_TO_DO",
            // Policy
            Self::PolicyViolation => "POLICY_VIOLATION",
            Self::WorkflowCapacityExceeded => "WORKFLOW_CAPACITY_EXCEEDED",
            // Internal
            Self::InternalError => "INTERNAL_ERROR",
        }
    }

    /// Whether this error is potentially retryable.
    ///
    /// Retryable means the agent might succeed if it:
    /// - Waits and retries (e.g., database locked)
    /// - Fixes the input and retries (e.g., validation error)
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::DatabaseLocked
                | Self::ValidationFailed
                | Self::InvalidPriority
                | Self::RequiredField
                | Self::AmbiguousId
                | Self::WorkflowCapacityExceeded
                | Self::ShuttingDown
        )
    }

    /// Get the exit code for this error category.
    ///
    /// Exit codes are grouped by error category:
    /// - 1: Internal/unknown errors
    /// - 2: Database errors
    /// - 3: Issue errors
    /// - 4: Validation errors
    /// - 5: Dependency errors
    /// - 6: Sync/JSONL errors
    /// - 7: Config errors
    /// - 8: I/O errors
    /// - 130: Cooperative shutdown after SIGINT
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            // Database (2)
            Self::DatabaseNotFound
            | Self::DatabaseLocked
            | Self::SchemaMismatch
            | Self::DatabaseError
            | Self::NotInitialized
            | Self::AlreadyInitialized => 2,
            // Issue / Operational (3)
            Self::IssueNotFound
            | Self::AmbiguousId
            | Self::IdCollision
            | Self::InvalidId
            | Self::NothingToDo => 3,
            Self::ShuttingDown => 130,
            // Validation (4)
            Self::ValidationFailed
            | Self::InvalidPriority
            | Self::RequiredField
            | Self::PolicyViolation
            | Self::WorkflowCapacityExceeded => 4,
            // Dependency (5)
            Self::CycleDetected
            | Self::DependencyNotFound
            | Self::HasDependents
            | Self::SelfDependency
            | Self::DuplicateDependency => 5,
            // Sync (6)
            Self::JsonlParseError
            | Self::PrefixMismatch
            | Self::ImportCollision
            | Self::SyncConflict
            | Self::ConflictMarkers
            | Self::PathTraversal => 6,
            // Config (7)
            Self::ConfigError | Self::ConfigNotFound | Self::ConfigParseError => 7,
            // I/O (8)
            Self::IoError | Self::JsonError | Self::YamlError => 8,
            // Internal (1)
            Self::InternalError => 1,
        }
    }
}

/// Structured error for machine-parseable output.
///
/// Provides AI coding agents with:
/// - Machine-readable error code
/// - Human-readable message
/// - Context-aware hint for self-correction
/// - Retryability flag
/// - Structured context data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuredError {
    /// Machine-readable error code
    pub code: ErrorCode,
    /// Human-readable error message
    pub message: String,
    /// Optional hint for fixing the error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Whether the operation can be retried
    pub retryable: bool,
    /// Additional context data
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

impl StructuredError {
    /// Create a new structured error from a `BeadsError`.
    #[must_use]
    pub fn from_error(err: &BeadsError) -> Self {
        let (code, context) = Self::extract_code_and_context(err);
        let hint = Self::generate_hint(Self::hint_source(err), context.as_ref());

        Self {
            code,
            message: err.to_string(),
            hint,
            retryable: code.is_retryable(),
            context,
        }
    }

    fn hint_source(err: &BeadsError) -> &BeadsError {
        match err {
            BeadsError::WithContext { source, .. } => source
                .downcast_ref::<BeadsError>()
                .map_or(err, Self::hint_source),
            _ => err,
        }
    }

    fn add_wrapper_context(wrapper_context: &str, inner_context: Option<Value>) -> Value {
        match inner_context {
            Some(Value::Object(mut object)) => {
                object.insert(
                    "wrapper_context".to_string(),
                    Value::String(wrapper_context.to_string()),
                );
                Value::Object(object)
            }
            Some(other) => json!({
                "wrapper_context": wrapper_context,
                "source_context": other,
            }),
            None => json!({
                "wrapper_context": wrapper_context,
            }),
        }
    }

    /// Create a structured error with similar ID suggestions.
    #[must_use]
    pub fn issue_not_found(searched_id: &str, existing_ids: &[String]) -> Self {
        let similar = find_similar_ids(searched_id, existing_ids, 3);

        let hint = if similar.is_empty() {
            Some("Run 'br list' to see available issues.".to_string())
        } else if similar.len() == 1 {
            Some(format!("Did you mean '{}'?", similar[0]))
        } else {
            Some(format!("Did you mean one of: {}?", similar.join(", ")))
        };

        let context = json!({
            "searched_id": searched_id,
            "similar_ids": similar,
        });

        Self {
            code: ErrorCode::IssueNotFound,
            message: format!("Issue not found: {searched_id}"),
            hint,
            retryable: false,
            context: Some(context),
        }
    }

    /// Add "did you mean" suggestions to an `ISSUE_NOT_FOUND` error.
    ///
    /// A no-op for every other code, and for an error whose context is missing
    /// the `searched_id` it would compare against.
    ///
    /// This exists because the searched ID and the set of IDs that exist are
    /// known in different places. `BeadsError::IssueNotFound` is constructed at
    /// 31 sites, most of which are storage functions holding one ID and no
    /// catalogue; the candidate set is only cheaply available at the point the
    /// error is finally rendered. Rather than widen the error variant and make
    /// all 31 sites answer a question they cannot, the suggestion is attached
    /// once, here, on the way out.
    ///
    /// The hint shaping -- singular, plural, and the `br list` fallback when
    /// nothing is close -- is not reimplemented: it comes from
    /// [`Self::issue_not_found`], which was written for exactly this and had
    /// never been called.
    #[must_use]
    pub fn with_id_suggestions(mut self, existing_ids: &[String]) -> Self {
        if self.code != ErrorCode::IssueNotFound {
            return self;
        }

        let Some(searched) = self
            .context
            .as_ref()
            .and_then(|context| context.get("searched_id"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
        else {
            return self;
        };

        let enriched = Self::issue_not_found(&searched, existing_ids);
        self.hint = enriched.hint;

        // Merge rather than replace: a wrapped error carries `wrapper_context`
        // in the same object, and losing it would trade one diagnostic for
        // another.
        self.context = match (self.context.take(), enriched.context) {
            (Some(Value::Object(mut current)), Some(Value::Object(added))) => {
                current.extend(added);
                Some(Value::Object(current))
            }
            (_, added) => added,
        };

        self
    }

    /// Serialize to JSON value.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "error": {
                "code": self.code.as_str(),
                "message": self.message,
                "hint": self.hint,
                "retryable": self.retryable,
                "context": self.context,
            }
        })
    }

    /// Format for human-readable output.
    #[must_use]
    pub fn to_human(&self, color: bool) -> String {
        let mut output = String::new();

        if color {
            // Red for error
            output.push_str("\x1b[31mError:\x1b[0m ");
        } else {
            output.push_str("Error: ");
        }

        output.push_str(&sanitize_terminal_text(&self.message));

        if let Some(hint) = &self.hint {
            output.push('\n');
            if color {
                // Yellow for hint
                output.push_str("\x1b[33mHint:\x1b[0m ");
            } else {
                output.push_str("Hint: ");
            }
            output.push_str(&sanitize_terminal_text(hint));
        }

        output
    }

    /// Extract error code and context from a `BeadsError`.
    #[allow(clippy::too_many_lines)]
    fn extract_code_and_context(err: &BeadsError) -> (ErrorCode, Option<Value>) {
        match err {
            BeadsError::DatabaseNotFound { path } => (
                ErrorCode::DatabaseNotFound,
                Some(json!({"path": path.display().to_string()})),
            ),
            BeadsError::DatabaseLocked { path } => (
                ErrorCode::DatabaseLocked,
                Some(json!({"path": path.display().to_string()})),
            ),
            BeadsError::SchemaMismatch { expected, found } => (
                ErrorCode::SchemaMismatch,
                Some(json!({"expected": expected, "found": found})),
            ),
            BeadsError::Database(_) => (ErrorCode::DatabaseError, None),
            BeadsError::NotInitialized => (ErrorCode::NotInitialized, None),
            BeadsError::AlreadyInitialized { path } => (
                ErrorCode::AlreadyInitialized,
                Some(json!({"path": path.display().to_string()})),
            ),
            BeadsError::IssueNotFound { id } => {
                (ErrorCode::IssueNotFound, Some(json!({"searched_id": id})))
            }
            BeadsError::AmbiguousId { partial, matches } => (
                ErrorCode::AmbiguousId,
                Some(json!({"partial_id": partial, "matches": matches})),
            ),
            BeadsError::IdCollision { id } => (ErrorCode::IdCollision, Some(json!({"id": id}))),
            BeadsError::InvalidId { id } => (ErrorCode::InvalidId, Some(json!({"id": id}))),
            BeadsError::Validation { field, reason } => (
                ErrorCode::ValidationFailed,
                Some(json!({"field": field, "reason": reason})),
            ),
            BeadsError::ValidationErrors { errors } => (
                ErrorCode::ValidationFailed,
                Some(json!({
                    "errors": errors.iter()
                        .map(|e| json!({"field": e.field, "message": e.message}))
                        .collect::<Vec<_>>()
                })),
            ),
            BeadsError::InvalidPriority { priority } => {
                let hint = Some(detect_priority_intent(priority).map_or_else(
                    || PRIORITY_SHORT_HINT.to_string(),
                    |detected| flag_value_hint("priority", detected),
                ));

                (
                    ErrorCode::InvalidPriority,
                    Some(serde_json::json!({
                        "priority": priority,
                        "hint": hint
                    })),
                )
            }
            BeadsError::JsonlParse { line, reason } => (
                ErrorCode::JsonlParseError,
                Some(json!({"line": line, "reason": reason})),
            ),
            BeadsError::PrefixMismatch { expected, found } => (
                ErrorCode::PrefixMismatch,
                Some(json!({"expected": expected, "found": found})),
            ),
            BeadsError::ImportCollision { count } => (
                ErrorCode::ImportCollision,
                Some(json!({"collision_count": count})),
            ),
            BeadsError::SyncConflict { message } => {
                (ErrorCode::SyncConflict, Some(json!({"message": message})))
            }
            BeadsError::DependencyCycle { path } => {
                (ErrorCode::CycleDetected, Some(json!({"cycle_path": path})))
            }
            BeadsError::HasDependents { id, count } => (
                ErrorCode::HasDependents,
                Some(json!({"id": id, "dependent_count": count})),
            ),
            BeadsError::SelfDependency { id } => {
                (ErrorCode::SelfDependency, Some(json!({"id": id})))
            }
            BeadsError::DependencyNotFound { id } => {
                (ErrorCode::DependencyNotFound, Some(json!({"id": id})))
            }
            BeadsError::DuplicateDependency { from, to } => (
                ErrorCode::DuplicateDependency,
                Some(json!({"from": from, "to": to})),
            ),
            BeadsError::ShuttingDown => (
                ErrorCode::ShuttingDown,
                Some(json!({"shutdown_requested": true})),
            ),
            BeadsError::NothingToDo { reason } => {
                (ErrorCode::NothingToDo, Some(json!({"reason": reason})))
            }
            BeadsError::PolicyViolation {
                issue_id,
                summary,
                violations,
            } => (
                ErrorCode::PolicyViolation,
                Some(json!({
                    "issue_id": issue_id,
                    "summary": summary,
                    "violations": violations,
                })),
            ),
            BeadsError::WorkflowCapacityExceeded { violation } => (
                ErrorCode::WorkflowCapacityExceeded,
                Some(serde_json::to_value(violation).unwrap_or_else(|_| {
                    json!({
                        "issue_id": violation.issue_id,
                        "capacity_name": violation.capacity_name,
                    })
                })),
            ),
            BeadsError::Config(_) => (ErrorCode::ConfigError, None),
            BeadsError::ExternalCommand { command, reason } => (
                ErrorCode::IoError,
                Some(json!({"command": command, "reason": reason})),
            ),
            BeadsError::Internal { message } => {
                (ErrorCode::InternalError, Some(json!({"message": message})))
            }
            BeadsError::Io(_) => (ErrorCode::IoError, None),
            BeadsError::Json(_) => (ErrorCode::JsonError, None),
            BeadsError::Yaml(_) => (ErrorCode::YamlError, None),
            BeadsError::WithContext { context, source } => {
                if let Some(source_err) = source.downcast_ref::<BeadsError>() {
                    let (code, inner_context) = Self::extract_code_and_context(source_err);
                    (
                        code,
                        Some(Self::add_wrapper_context(context, inner_context)),
                    )
                } else if source.downcast_ref::<std::io::Error>().is_some() {
                    (
                        ErrorCode::IoError,
                        Some(Self::add_wrapper_context(context, None)),
                    )
                } else if source.downcast_ref::<serde_json::Error>().is_some() {
                    (
                        ErrorCode::JsonError,
                        Some(Self::add_wrapper_context(context, None)),
                    )
                } else if source.downcast_ref::<serde_yml::Error>().is_some() {
                    (
                        ErrorCode::YamlError,
                        Some(Self::add_wrapper_context(context, None)),
                    )
                } else {
                    (
                        ErrorCode::InternalError,
                        Some(Self::add_wrapper_context(context, None)),
                    )
                }
            }
        }
    }

    /// Generate context-aware hint from error.
    /// The hint shown to a human, most specific answer first.
    ///
    /// The arms below are consulted *before* [`BeadsError::suggestion`], and
    /// the order is the whole point (bds-b0m). It used to be the other way
    /// round, and `suggestion()` is `Some` for exactly the priority, status
    /// and type variants these arms were written for — so
    /// `detect_priority_intent` resolved "high" to 1 and the answer was
    /// thrown away, leaving the user the static range to map onto themselves.
    ///
    /// `suggestion()` remains the right answer for the dozen variants with no
    /// arm here, and the fallback for an arm that returns `None` because it
    /// could not detect anything. It is a default, not a pre-emption.
    fn generate_hint(err: &BeadsError, context: Option<&Value>) -> Option<String> {
        Self::specific_hint(err, context).or_else(|| err.suggestion().map(ToString::to_string))
    }

    /// The context-aware hint for errors that can compute a better one than
    /// their static [`BeadsError::suggestion`]. `None` means "nothing more
    /// specific to say", which sends [`Self::generate_hint`] to the fallback.
    fn specific_hint(err: &BeadsError, context: Option<&Value>) -> Option<String> {
        match err {
            BeadsError::IssueNotFound { .. } => {
                Some("Run 'br list' to see available issues.".to_string())
            }
            // `None` rather than PRIORITY_SHORT_HINT when nothing is
            // detected: the fallback says the same thing at more length, and
            // an arm that always answers cannot be overridden by anything.
            BeadsError::InvalidPriority { priority } => detect_priority_intent(priority)
                .map(|detected| flag_value_hint("priority", detected)),
            BeadsError::HasDependents { id, .. } => {
                if let Some(ctx) = context
                    && let Some(count) = ctx.get("dependent_count")
                {
                    return Some(format!(
                        "Use --force or --cascade to delete anyway, or close {count} dependents first."
                    ));
                }
                Some(format!("Use --force or --cascade to delete '{id}' anyway."))
            }
            BeadsError::NothingToDo { reason } => {
                // The reason string carries the per-issue skip explanations
                // (issue #380). Pick the hint that matches what actually
                // happened instead of unconditionally claiming "already
                // closed or not found" — that wording sent operators hunting
                // for a nonexistent state bug when the skip was really a
                // dependency block.
                if reason.contains("blocked by") {
                    Some(
                        "Skipped issue(s) have open blocking dependencies. Close the blockers first, or re-run with --force to close anyway."
                            .to_string(),
                    )
                } else if reason.contains("open children") || reason.contains("child issue") {
                    Some(
                        "Skipped issue(s) have open children. Close the children first, or re-run with --force to close anyway."
                            .to_string(),
                    )
                } else {
                    Some("All specified issues were already closed or not found.".to_string())
                }
            }
            BeadsError::ShuttingDown => {
                Some("Retry after starting a fresh br process.".to_string())
            }
            BeadsError::JsonlParse { line, .. } => Some(format!(
                "Check line {line} of the JSONL file for syntax errors."
            )),
            _ => None,
        }
    }
}

// === Precomputed Valid Values (O(1) lookup) ===

/// Priority synonyms for intent detection.
static PRIORITY_SYNONYMS: LazyLock<std::collections::HashMap<&'static str, &'static str>> =
    LazyLock::new(|| {
        [
            ("critical", "0"),
            ("crit", "0"),
            ("urgent", "0"),
            ("highest", "0"),
            ("high", "1"),
            ("important", "1"),
            ("medium", "2"),
            ("normal", "2"),
            ("default", "2"),
            ("low", "3"),
            ("minor", "3"),
            ("backlog", "4"),
            ("lowest", "4"),
            ("trivial", "4"),
        ]
        .into_iter()
        .collect()
    });

// === Intent Detection ===

/// Detect what priority the user likely meant.
fn detect_priority_intent(input: &str) -> Option<&'static str> {
    let lower = input.to_lowercase();

    // Already valid
    if ["0", "1", "2", "3", "4"].contains(&lower.as_str()) {
        return match lower.as_str() {
            "0" => Some("0"),
            "1" => Some("1"),
            "2" => Some("2"),
            "3" => Some("3"),
            "4" => Some("4"),
            _ => None,
        };
    }

    // P0-P4 format
    if lower.starts_with('p') && lower.len() == 2 {
        let digit = lower.chars().nth(1)?;
        if digit.is_ascii_digit() && digit <= '4' {
            return match digit {
                '0' => Some("0"),
                '1' => Some("1"),
                '2' => Some("2"),
                '3' => Some("3"),
                '4' => Some("4"),
                _ => None,
            };
        }
    }

    // Synonym lookup
    PRIORITY_SYNONYMS.get(lower.as_str()).copied()
}

// === Levenshtein Distance ===

/// Calculate the Damerau-Levenshtein (optimal string alignment) distance.
///
/// This is used to find similar IDs when an issue is not found. Adjacent
/// transpositions cost 1 rather than 2, which matters here: a transposed pair
/// is one of the commonest ways to mistype an opaque ID by hand, and at the
/// distance threshold [`find_similar_ids`] uses, charging it 2 would put every
/// transposition out of reach.
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_len = a.chars().count();
    let b_len = b.chars().count();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    // Levenshtein distance matrix
    let mut matrix = vec![vec![0; b_len + 1]; a_len + 1];

    for (i, row) in matrix.iter_mut().enumerate().take(a_len + 1) {
        row[0] = i;
    }
    for (j, item) in matrix[0].iter_mut().enumerate().take(b_len + 1) {
        *item = j;
    }

    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();

    for (i, a_char) in a_chars.iter().enumerate() {
        for (j, b_char) in b_chars.iter().enumerate() {
            let cost = usize::from(a_char != b_char);
            matrix[i + 1][j + 1] = std::cmp::min(
                std::cmp::min(matrix[i][j + 1] + 1, matrix[i + 1][j] + 1),
                matrix[i][j] + cost,
            );

            // Transposition of an adjacent pair, charged as one edit.
            if i > 0
                && j > 0
                && *a_char == b_chars[j - 1]
                && a_chars[i - 1] == *b_char
                && let transposed = matrix[i - 1][j - 1] + 1
                && transposed < matrix[i + 1][j + 1]
            {
                matrix[i + 1][j + 1] = transposed;
            }
        }
    }

    matrix[a_len][b_len]
}

/// The edit distance at which two IDs stop being plausible typos of each other.
///
/// One edit: a single substitution, insertion, deletion, or transposition.
///
/// The value this replaced -- a hardcoded `<= 3` -- was unusable. Beads IDs are
/// a shared prefix and a short random suffix (`bds-k7e`), so two *unrelated*
/// IDs differ by exactly the suffix length. With a 3-character suffix every ID
/// in a workspace sits at distance 3 from every other, and a bound of 3
/// therefore matched the whole workspace: one mistyped character produced "Did
/// you mean one of:" followed by three alphabetically-first strangers.
///
/// Scaling the bound with ID length was tried first and is also wrong, which is
/// worth recording because it looks reasonable. Length is the wrong signal: the
/// prefix is shared by every ID in a workspace, so it contributes length
/// without contributing anything that distinguishes one ID from another. A
/// workspace whose prefix happens to be long would relax the bound to 2 while
/// still having only 3 characters that differ -- and at distance 2 of 3
/// characters, unrelated IDs start matching again. An end-to-end test in a
/// temp directory, where the prefix is derived from the directory name, caught
/// exactly that.
///
/// A flat single edit needs no heuristic and cannot be inflated by a prefix.
/// Two typos in an opaque identifier is not a near miss worth guessing at.
const SUGGESTION_MAX_DISTANCE: usize = 1;

/// Find IDs similar to the searched ID using edit distance.
///
/// Returns up to `max_suggestions` IDs within [`SUGGESTION_MAX_DISTANCE`] of
/// `searched`, closest first.
pub fn find_similar_ids(
    searched: &str,
    existing: &[String],
    max_suggestions: usize,
) -> Vec<String> {
    let mut candidates: Vec<(usize, &str)> = existing
        .iter()
        .map(|id| (levenshtein_distance(searched, id), id.as_str()))
        .filter(|(dist, _)| *dist <= SUGGESTION_MAX_DISTANCE)
        .collect();

    // Sort by distance, then alphabetically
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));

    candidates
        .into_iter()
        .take(max_suggestions)
        .map(|(_, id)| id.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn test_error_code_as_str() {
        assert_eq!(ErrorCode::IssueNotFound.as_str(), "ISSUE_NOT_FOUND");
        assert_eq!(ErrorCode::CycleDetected.as_str(), "CYCLE_DETECTED");
        assert_eq!(ErrorCode::NotInitialized.as_str(), "NOT_INITIALIZED");
        assert_eq!(ErrorCode::ShuttingDown.as_str(), "SHUTTING_DOWN");
    }

    #[test]
    fn test_error_code_is_retryable() {
        assert!(!ErrorCode::IssueNotFound.is_retryable());
        assert!(!ErrorCode::CycleDetected.is_retryable());
        assert!(ErrorCode::DatabaseLocked.is_retryable());
        assert!(ErrorCode::ValidationFailed.is_retryable());
        assert!(ErrorCode::InvalidPriority.is_retryable());
        assert!(ErrorCode::ShuttingDown.is_retryable());
    }

    #[test]
    fn test_error_code_exit_codes() {
        assert_eq!(ErrorCode::NotInitialized.exit_code(), 2);
        assert_eq!(ErrorCode::IssueNotFound.exit_code(), 3);
        assert_eq!(ErrorCode::ValidationFailed.exit_code(), 4);
        assert_eq!(ErrorCode::CycleDetected.exit_code(), 5);
        assert_eq!(ErrorCode::JsonlParseError.exit_code(), 6);
        assert_eq!(ErrorCode::ConfigError.exit_code(), 7);
        assert_eq!(ErrorCode::IoError.exit_code(), 8);
        assert_eq!(ErrorCode::ShuttingDown.exit_code(), 130);
        assert_eq!(ErrorCode::InternalError.exit_code(), 1);
    }

    #[test]
    fn test_structured_error_to_json() {
        let err = StructuredError {
            code: ErrorCode::IssueNotFound,
            message: "Issue not found: bd-abc".to_string(),
            hint: Some("Did you mean 'bd-abd'?".to_string()),
            retryable: false,
            context: Some(json!({"searched_id": "bd-abc"})),
        };
        let json = err.to_json();
        assert_eq!(json["error"]["code"], "ISSUE_NOT_FOUND");
        assert_eq!(json["error"]["hint"], "Did you mean 'bd-abd'?");
        assert!(!json["error"]["retryable"].as_bool().unwrap());
    }

    #[test]
    fn workflow_capacity_error_preserves_machine_readable_evidence() {
        let err = BeadsError::WorkflowCapacityExceeded {
            violation: Box::new(crate::close_policy::WorkflowCapacityViolation {
                issue_id: "bd-next".to_string(),
                from_status: Some("open".to_string()),
                to_status: "in_progress".to_string(),
                capacity_kind: "status".to_string(),
                capacity_name: "in_progress".to_string(),
                scope: "repository".to_string(),
                counting_mode: "all".to_string(),
                current: 2,
                prospective: 3,
                soft_limit: Some(1),
                hard_limit: 2,
                policy_path: "workflow.capacity.statuses.in_progress".to_string(),
            }),
        };

        let structured = StructuredError::from_error(&err);
        let context = structured.context.expect("capacity evidence");
        assert_eq!(structured.code, ErrorCode::WorkflowCapacityExceeded);
        assert!(structured.retryable);
        assert_eq!(structured.code.exit_code(), 4);
        assert_eq!(context["issue_id"], "bd-next");
        assert_eq!(context["current"], 2);
        assert_eq!(context["prospective"], 3);
        assert_eq!(context["hard_limit"], 2);
        assert_eq!(
            context["policy_path"],
            "workflow.capacity.statuses.in_progress"
        );
    }

    #[test]
    fn test_levenshtein_distance() {
        assert_eq!(levenshtein_distance("", ""), 0);
        assert_eq!(levenshtein_distance("abc", "abc"), 0);
        assert_eq!(levenshtein_distance("abc", "abd"), 1);
        assert_eq!(levenshtein_distance("abc", "abcd"), 1);
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn test_find_similar_ids() {
        let existing = vec![
            "bd-abc123".to_string(),
            "bd-xyz789".to_string(),
            "bd-abc124".to_string(),
            "bd-def456".to_string(),
        ];

        let suggestions = find_similar_ids("bd-abc12", &existing, 3);
        assert!(!suggestions.is_empty());
        // bd-abc123 and bd-abc124 should be closest (distance 1)
        assert!(suggestions.contains(&"bd-abc123".to_string()));
    }

    /// The defect that made the suggester unusable, and the reason it had to be
    /// fixed before being wired up rather than after.
    ///
    /// IDs are a fixed prefix plus a short random suffix, so two unrelated IDs
    /// differ by exactly the suffix length -- 3 here. Under the old hardcoded
    /// `distance <= 3` every ID in a workspace was "similar" to every other,
    /// and a single mistyped character produced three strangers.
    #[test]
    fn unrelated_short_ids_are_not_suggested_for_each_other() {
        let existing = vec![
            "bds-k7e".to_string(),
            "bds-cn6".to_string(),
            "bds-i3g".to_string(),
            "bds-ja3".to_string(),
            "bds-yyz".to_string(),
        ];

        // One wrong character: the near-miss, and only the near-miss.
        assert_eq!(find_similar_ids("bds-k7x", &existing, 3), vec!["bds-k7e"]);

        // Nothing within one edit: no suggestion at all, so the caller falls
        // back to the `br list` hint rather than inventing candidates.
        assert!(find_similar_ids("bds-q42", &existing, 3).is_empty());
    }

    /// Transpositions are the reason the distance is Damerau rather than plain
    /// Levenshtein: under the latter a swapped pair costs 2, which at this
    /// threshold would mean no suggestion for one of the commonest typos.
    #[test]
    fn a_transposed_pair_is_one_edit_away() {
        assert_eq!(levenshtein_distance("bds-k7e", "bds-7ke"), 1);

        let existing = vec!["bds-k7e".to_string(), "bds-cn6".to_string()];
        assert_eq!(find_similar_ids("bds-7ke", &existing, 3), vec!["bds-k7e"]);
    }

    /// A long shared prefix must not buy a looser bound -- the regression that
    /// a length-scaled threshold introduced, and that the end-to-end tests
    /// caught in a temp directory whose name became the workspace prefix.
    #[test]
    fn a_long_shared_prefix_does_not_loosen_the_bound() {
        let existing = vec!["tmpcp2h0t-0c8".to_string(), "tmpcp2h0t-04o".to_string()];

        // Two edits away from the second ID, one from the first.
        assert_eq!(
            find_similar_ids("tmpcp2h0t-0cx", &existing, 3),
            vec!["tmpcp2h0t-0c8"]
        );
    }

    #[test]
    fn issue_not_found_suggestions_reach_the_rendered_error() {
        let base = StructuredError::from_error(&BeadsError::IssueNotFound {
            id: "bds-k7x".to_string(),
        });
        assert_eq!(
            base.hint.as_deref(),
            Some("Run 'br list' to see available issues.")
        );

        let enriched = base.with_id_suggestions(&["bds-k7e".to_string(), "bds-cn6".to_string()]);
        assert_eq!(enriched.hint.as_deref(), Some("Did you mean 'bds-k7e'?"));
        let context = enriched.context.as_ref().unwrap();
        assert_eq!(context["searched_id"], "bds-k7x");
        assert_eq!(context["similar_ids"], json!(["bds-k7e"]));
    }

    #[test]
    fn with_id_suggestions_keeps_the_br_list_hint_when_nothing_is_close() {
        let enriched = StructuredError::from_error(&BeadsError::IssueNotFound {
            id: "bds-q42".to_string(),
        })
        .with_id_suggestions(&["bds-k7e".to_string()]);

        assert_eq!(
            enriched.hint.as_deref(),
            Some("Run 'br list' to see available issues.")
        );
        assert_eq!(enriched.context.as_ref().unwrap()["similar_ids"], json!([]));
    }

    #[test]
    fn with_id_suggestions_ignores_errors_that_are_not_issue_not_found() {
        let err = StructuredError::from_error(&BeadsError::NotInitialized);
        let before = err.clone();
        let after = err.with_id_suggestions(&["bds-k7e".to_string()]);
        assert_eq!(after.hint, before.hint);
        assert_eq!(after.context, before.context);
    }

    #[test]
    fn test_detect_priority_intent() {
        assert_eq!(detect_priority_intent("high"), Some("1"));
        assert_eq!(detect_priority_intent("critical"), Some("0"));
        assert_eq!(detect_priority_intent("P2"), Some("2"));
        assert_eq!(detect_priority_intent("p3"), Some("3"));
        assert_eq!(detect_priority_intent("2"), Some("2"));
        assert_eq!(detect_priority_intent("xyz"), None);
    }

    #[test]
    fn test_detect_priority_intent_all_digits() {
        for (digit, expected) in [("0", "0"), ("1", "1"), ("2", "2"), ("3", "3"), ("4", "4")] {
            assert_eq!(detect_priority_intent(digit), Some(expected));
        }
    }

    #[test]
    fn test_detect_priority_intent_all_p_prefixed() {
        for (input, expected) in [
            ("p0", "0"),
            ("P0", "0"),
            ("p1", "1"),
            ("P1", "1"),
            ("p2", "2"),
            ("P2", "2"),
            ("p3", "3"),
            ("P3", "3"),
            ("p4", "4"),
            ("P4", "4"),
        ] {
            assert_eq!(
                detect_priority_intent(input),
                Some(expected),
                "input: {input}"
            );
        }
    }

    #[test]
    fn test_detect_priority_intent_rejects_malformed() {
        assert_eq!(detect_priority_intent("p5"), None);
        assert_eq!(detect_priority_intent("P5"), None);
        assert_eq!(detect_priority_intent("px"), None);
        assert_eq!(detect_priority_intent("p10"), None);
        assert_eq!(detect_priority_intent("5"), None);
        assert_eq!(detect_priority_intent("9"), None);
        assert_eq!(detect_priority_intent(""), None);
        assert_eq!(detect_priority_intent("p"), None);
        assert_eq!(detect_priority_intent("P"), None);
    }

    // These four asserted the behaviour of `StructuredError` constructors that
    // nothing ever called (bds-k7e). Rewritten against `from_error`, the path
    // the CLI actually renders through, so they now assert what a user sees.

    #[test]
    fn not_initialized_error_points_at_br_init() {
        let err = StructuredError::from_error(&BeadsError::NotInitialized);
        assert_eq!(err.code, ErrorCode::NotInitialized);
        assert!(err.hint.as_ref().unwrap().contains("br init"));
    }

    #[test]
    fn invalid_priority_error_is_retryable_and_names_the_detected_value() {
        let err = StructuredError::from_error(&BeadsError::InvalidPriority {
            priority: "high".to_string(),
        });
        assert_eq!(err.code, ErrorCode::InvalidPriority);
        assert!(err.retryable);
        // "high" is detectable, so the hint says so rather than reciting the
        // range and leaving the reader to map "high" onto it. This assertion
        // used to be the opposite, pinning the static text with a comment
        // pointing at bds-b0m; that is the bug, now fixed.
        assert_eq!(err.hint.as_deref(), Some("Did you mean --priority 1?"));
    }

    #[test]
    fn undetectable_priority_falls_back_to_the_static_range() {
        let err = StructuredError::from_error(&BeadsError::InvalidPriority {
            priority: "zzzqqq".to_string(),
        });
        // Nothing to detect, so `specific_hint` declines and
        // `BeadsError::suggestion()` answers. Losing that fallback would be
        // the obvious way to break this fix.
        assert_eq!(
            err.hint.as_deref(),
            Some("Use a priority between 0 (critical) and 4 (backlog)")
        );
    }

    /// Inverting hint precedence (bds-b0m) routes `HasDependents` through
    /// `generate_hint`'s own arm instead of the static suggestion. The arm's
    /// text is more specific — it names the id or the dependent count — but
    /// it must not drop `--cascade` on the way, since that is the other way
    /// out of this error.
    #[test]
    fn has_dependents_hint_offers_both_escapes() {
        let err = StructuredError::from_error(&BeadsError::HasDependents {
            id: "bd-abc123".to_string(),
            count: 3,
        });
        assert_eq!(err.code, ErrorCode::HasDependents);
        let hint = err.hint.as_ref().unwrap();
        assert!(hint.contains("--force"), "hint lost --force: {hint}");
        assert!(hint.contains("--cascade"), "hint lost --cascade: {hint}");
    }

    #[test]
    fn ambiguous_id_error_carries_the_matches() {
        let err = StructuredError::from_error(&BeadsError::AmbiguousId {
            partial: "bd-ab".to_string(),
            matches: vec!["bd-abc".to_string(), "bd-abd".to_string()],
        });
        assert_eq!(err.code, ErrorCode::AmbiguousId);
        assert!(err.retryable);
        assert!(err.context.as_ref().unwrap()["matches"].is_array());
    }

    #[test]
    fn test_structured_error_preserves_wrapped_beads_error_code() {
        let err = BeadsError::WithContext {
            context: "failed to preserve blocked cache after partial close mutation".to_string(),
            source: Box::new(BeadsError::validation("ids", "boom")),
        };

        let structured = StructuredError::from_error(&err);
        let context = structured.context.expect("context");

        assert_eq!(structured.code, ErrorCode::ValidationFailed);
        assert!(structured.retryable);
        assert_eq!(context["field"], "ids");
        assert_eq!(context["reason"], "boom");
        assert_eq!(
            context["wrapper_context"],
            "failed to preserve blocked cache after partial close mutation"
        );
    }

    #[test]
    fn test_structured_error_preserves_wrapped_io_error_code() {
        let err = BeadsError::WithContext {
            context: "failed to rename recovered database".to_string(),
            source: Box::new(io::Error::other("disk full")),
        };

        let structured = StructuredError::from_error(&err);
        let context = structured.context.expect("context");

        assert_eq!(structured.code, ErrorCode::IoError);
        assert_eq!(
            context["wrapper_context"],
            "failed to rename recovered database"
        );
    }

    #[test]
    fn test_to_human_output() {
        let err = StructuredError {
            code: ErrorCode::IssueNotFound,
            message: "Issue not found: bd-abc".to_string(),
            hint: Some("Did you mean 'bd-abd'?".to_string()),
            retryable: false,
            context: None,
        };

        let plain = err.to_human(false);
        assert!(plain.contains("Error: Issue not found: bd-abc"));
        assert!(plain.contains("Hint: Did you mean 'bd-abd'?"));

        let colored = err.to_human(true);
        assert!(colored.contains("\x1b[31m")); // Red color code
        assert!(colored.contains("\x1b[33m")); // Yellow color code
    }
}
