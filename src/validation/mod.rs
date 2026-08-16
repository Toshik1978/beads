//! Validation helpers for `beads`.
//!
//! These routines enforce classic bd data constraints and return
//! structured validation errors without mutating storage.
//!
//! # Sync Safety Guarantees
//!
//! The sync subsystem enforces these invariants by design:
//! - **No git operations**: br sync NEVER executes git commands
//! - **Path confinement**: All I/O stays within `.beads/` (unless explicitly opted-in)
//! - **No .git access**: Sync code paths never read from or write to `.git/`
//!
//! Sync path safety is enforced in `sync::path`, not here.

use crate::error::ValidationError;
use crate::model::{Comment, Issue, Priority, Status};
use crate::util::id::MAX_ID_LENGTH;

const TITLE_MAX_CHARS: usize = 500;
const ACTOR_MAX_CHARS: usize = 200;
const CUSTOM_VARIANT_MAX_CHARS: usize = 50;
pub(crate) const ISSUE_LABEL_MAX_COUNT: usize = 64;

/// Validates issue fields and invariants.
pub struct IssueValidator;

impl IssueValidator {
    /// Validate an issue and return all validation errors found.
    ///
    /// # Errors
    ///
    /// Returns a `Vec<ValidationError>` if any validation rules are violated.
    pub fn validate(issue: &Issue) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();

        // ID: Required, max length, prefix-hash format.
        if issue.id.trim().is_empty() {
            errors.push(ValidationError::new("id", "cannot be empty"));
        }
        if issue.id.len() > MAX_ID_LENGTH {
            errors.push(ValidationError::new(
                "id",
                format!("exceeds {MAX_ID_LENGTH} characters"),
            ));
        }
        if !issue.id.is_empty() && !is_valid_id_format(&issue.id) {
            errors.push(ValidationError::new(
                "id",
                "invalid format (expected prefix-hash)",
            ));
        }

        validate_issue_text_fields(issue, &mut errors);

        // Priority: 0-4 range.
        if issue.priority.0 < Priority::CRITICAL.0 || issue.priority.0 > Priority::BACKLOG.0 {
            errors.push(ValidationError::new("priority", "must be 0-4"));
        }

        // Timestamps: created_at <= updated_at.
        if issue.updated_at < issue.created_at {
            errors.push(ValidationError::new(
                "updated_at",
                "cannot be before created_at",
            ));
        }

        if issue.status == Status::Closed && issue.closed_at.is_none() {
            errors.push(ValidationError::new(
                "closed_at",
                "closed issues must set closed_at",
            ));
        }

        if !matches!(issue.status, Status::Closed | Status::Tombstone) && issue.closed_at.is_some()
        {
            errors.push(ValidationError::new(
                "closed_at",
                "only closed or tombstone issues may set closed_at",
            ));
        }

        // Closed timestamps: closed_at must not precede created_at.
        if let Some(closed_at) = issue.closed_at
            && closed_at < issue.created_at
        {
            errors.push(ValidationError::new(
                "closed_at",
                "cannot be before created_at",
            ));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn validate_issue_text_fields(issue: &Issue, errors: &mut Vec<ValidationError>) {
    // Title: Required, max 500 chars.
    if issue.title.trim().is_empty() {
        errors.push(ValidationError::new("title", "cannot be empty"));
    }
    if issue.title.chars().count() > TITLE_MAX_CHARS {
        errors.push(ValidationError::new("title", "exceeds 500 characters"));
    }
    reject_nul("title", &issue.title, errors);

    // Long-text fields (description, design, acceptance_criteria, notes) are
    // unbounded by design — these capture full specs, RFC text, agent
    // session transcripts, etc. A prior 100KB cap rejected legitimate
    // pre-existing records on JSONL rebuild and blocked workspace recovery
    // (the previous engine's .beads had nine records up to 554KB that were valid
    // bead bodies, not corruption). We still reject NUL bytes for SQLite
    // compatibility.
    if let Some(s) = issue.description.as_deref() {
        reject_nul("description", s, errors);
    }
    if let Some(s) = issue.design.as_deref() {
        reject_nul("design", s, errors);
    }
    if let Some(s) = issue.acceptance_criteria.as_deref() {
        reject_nul("acceptance_criteria", s, errors);
    }
    if let Some(s) = issue.notes.as_deref() {
        reject_nul("notes", s, errors);
    }
    reject_nul("status", issue.status.as_str(), errors);
    validate_custom_status(&issue.status, errors);
    reject_nul("issue_type", issue.issue_type.as_str(), errors);
    validate_custom_issue_type(&issue.issue_type, errors);
    reject_bounded_chars_opt("owner", issue.owner.as_deref(), ACTOR_MAX_CHARS, errors);
    reject_bounded_chars_opt(
        "created_by",
        issue.created_by.as_deref(),
        ACTOR_MAX_CHARS,
        errors,
    );
    validate_external_ref(issue.external_ref.as_deref(), errors);
    validate_issue_labels(issue, errors);
}

fn validate_external_ref(external_ref: Option<&str>, errors: &mut Vec<ValidationError>) {
    if let Some(external_ref) = external_ref {
        reject_nul("external_ref", external_ref, errors);
        if external_ref.len() > 200 {
            errors.push(ValidationError::new(
                "external_ref",
                "exceeds 200 characters",
            ));
        }
        if external_ref.chars().any(char::is_whitespace) {
            errors.push(ValidationError::new(
                "external_ref",
                "cannot contain whitespace",
            ));
        }
    }
}

fn reject_nul(field: &str, value: &str, errors: &mut Vec<ValidationError>) {
    if value.contains('\0') {
        errors.push(ValidationError::new(field, "cannot contain NUL bytes"));
    }
}

fn reject_bounded_chars_opt(
    field: &str,
    value: Option<&str>,
    max_chars: usize,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(value) = value {
        reject_nul(field, value, errors);
        if value.chars().count() > max_chars {
            errors.push(ValidationError::new(
                field,
                format!("exceeds {max_chars} characters"),
            ));
        }
    }
}

fn validate_custom_status(status: &Status, errors: &mut Vec<ValidationError>) {
    if let Status::Custom(value) = status
        && value.chars().count() > CUSTOM_VARIANT_MAX_CHARS
    {
        errors.push(ValidationError::new(
            "status",
            "custom status exceeds 50 characters",
        ));
    }
}

fn validate_custom_issue_type(
    issue_type: &crate::model::IssueType,
    errors: &mut Vec<ValidationError>,
) {
    if let crate::model::IssueType::Custom(value) = issue_type
        && value.chars().count() > CUSTOM_VARIANT_MAX_CHARS
    {
        errors.push(ValidationError::new(
            "issue_type",
            "custom issue type exceeds 50 characters",
        ));
    }
}

fn validate_issue_labels(issue: &Issue, errors: &mut Vec<ValidationError>) {
    if issue.labels.len() > ISSUE_LABEL_MAX_COUNT {
        errors.push(ValidationError::new(
            "labels",
            format!("exceeds {ISSUE_LABEL_MAX_COUNT} labels"),
        ));
    }

    for (idx, label) in issue.labels.iter().enumerate() {
        if let Err(err) = LabelValidator::validate(label) {
            errors.push(ValidationError::new(
                "labels",
                format!("label at index {idx}: {}", err.message),
            ));
        }
    }
}

/// Validates a single label value.
pub struct LabelValidator;

impl LabelValidator {
    /// Validate a label for length and allowed characters.
    ///
    /// # Errors
    ///
    /// Returns a `ValidationError` if the label is invalid.
    pub fn validate(label: &str) -> Result<(), ValidationError> {
        if label.is_empty() {
            return Err(ValidationError::new("label", "cannot be empty"));
        }

        if label.len() > 50 {
            return Err(ValidationError::new("label", "exceeds 50 characters"));
        }

        if !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':')
        {
            return Err(ValidationError::new(
                "label",
                "invalid characters (only alphanumeric, hyphen, underscore, colon allowed)",
            ));
        }

        Ok(())
    }
}

/// Validates comment fields.
pub struct CommentValidator;

impl CommentValidator {
    /// Validate a comment and return all validation errors found.
    ///
    /// # Errors
    ///
    /// Returns a `Vec<ValidationError>` if any validation rules are violated.
    pub fn validate(comment: &Comment) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();

        if comment.id <= 0 {
            errors.push(ValidationError::new("id", "must be positive"));
        }

        if comment.issue_id.trim().is_empty() {
            errors.push(ValidationError::new("issue_id", "cannot be empty"));
        }

        if comment.body.trim().is_empty() {
            errors.push(ValidationError::new("content", "cannot be empty"));
        }

        // Comment bodies are unbounded — same reasoning as long-text issue
        // fields above. Reject only NUL bytes for SQLite compatibility.
        reject_nul("content", &comment.body, &mut errors);

        if comment.author.trim().is_empty() {
            errors.push(ValidationError::new("author", "cannot be empty"));
        }

        if comment.author.len() > 200 {
            errors.push(ValidationError::new("author", "exceeds 200 characters"));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[must_use]
pub fn is_valid_id_format(id: &str) -> bool {
    crate::util::id::is_valid_id_format(id)
}

// =============================================================================
// SYNC SAFETY VALIDATION
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IssueType, Status};
    use chrono::{TimeZone, Utc};

    fn base_issue() -> Issue {
        Issue {
            id: "bd-abc123".to_string(),
            content_hash: None,
            title: "Test issue".to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            owner: None,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            created_by: None,
            updated_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            closed_at: None,
            close_reason: None,
            defer_until: None,
            external_ref: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            former_ids: vec![],
            labels: Vec::new(),
            dependencies: Vec::new(),
            comments: Vec::new(),
        }
    }

    #[test]
    fn issue_validation_rejects_empty_title() {
        let mut issue = base_issue();
        issue.title = " ".to_string();

        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "title"));
    }

    #[test]
    fn issue_validation_counts_title_limit_in_chars_not_utf8_bytes() {
        let mut issue = base_issue();
        issue.title = "\u{1f980}".repeat(500);
        assert!(IssueValidator::validate(&issue).is_ok());

        issue.title = "\u{1f980}".repeat(501);
        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "title"));
    }

    #[test]
    fn issue_validation_rejects_nul_in_content_hash_fields() {
        let mut issue = base_issue();
        issue.title = "nul\0title".to_string();
        issue.description = Some("nul\0description".to_string());
        issue.design = Some("nul\0design".to_string());
        issue.acceptance_criteria = Some("nul\0acceptance".to_string());
        issue.notes = Some("nul\0notes".to_string());
        issue.status = Status::Custom("nul\0status".to_string());
        issue.issue_type = IssueType::Custom("nul\0type".to_string());
        issue.owner = Some("nul\0owner".to_string());
        issue.created_by = Some("nul\0creator".to_string());
        issue.external_ref = Some("nul\0external".to_string());

        let errors = IssueValidator::validate(&issue).unwrap_err();
        let fields: Vec<_> = errors.iter().map(|err| err.field.as_str()).collect();
        for field in [
            "title",
            "description",
            "design",
            "acceptance_criteria",
            "notes",
            "status",
            "issue_type",
            "owner",
            "created_by",
            "external_ref",
        ] {
            assert!(fields.contains(&field), "missing NUL rejection for {field}");
        }
    }

    #[test]
    fn issue_validation_rejects_invalid_id() {
        let mut issue = base_issue();
        issue.id = "invalid".to_string();

        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "id"));
    }

    #[test]
    fn issue_validation_rejects_priority_out_of_range() {
        let mut issue = base_issue();
        issue.priority = Priority(9);

        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "priority"));
    }

    #[test]
    fn issue_validation_accepts_arbitrarily_large_description() {
        // Long-text fields (description / design / acceptance_criteria /
        // notes) are intentionally unbounded — spec write-ups, RFC text,
        // and agent session transcripts routinely exceed any small cap.
        let mut issue = base_issue();
        issue.description = Some("x".repeat(600_000));

        IssueValidator::validate(&issue).expect("long descriptions must validate cleanly");
    }

    #[test]
    fn issue_validation_rejects_closed_without_closed_at() {
        let mut issue = base_issue();
        issue.status = Status::Closed;

        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "closed_at"));
    }

    #[test]
    fn issue_validation_rejects_non_terminal_closed_at() {
        let mut issue = base_issue();
        issue.closed_at = Some(issue.updated_at);

        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "closed_at"));
    }

    #[test]
    fn issue_validation_allows_tombstone_without_closed_at() {
        let mut issue = base_issue();
        issue.status = Status::Tombstone;

        assert!(IssueValidator::validate(&issue).is_ok());
    }

    #[test]
    fn label_validation_rejects_invalid_characters() {
        let err = LabelValidator::validate("bad label").unwrap_err();
        assert_eq!(err.field, "label");

        let err = LabelValidator::validate("has/slash").unwrap_err();
        assert_eq!(err.field, "label");
    }

    #[test]
    fn label_validation_rejects_empty() {
        let err = LabelValidator::validate("").unwrap_err();
        assert_eq!(err.field, "label");
    }

    #[test]
    fn label_validation_allows_namespaced_labels() {
        assert!(LabelValidator::validate("team:backend").is_ok());
    }

    #[test]
    fn label_validation_rejects_path_style_labels() {
        let err = LabelValidator::validate("sys/stat").unwrap_err();
        assert_eq!(err.field, "label");
    }

    #[test]
    fn comment_validation_rejects_empty_body() {
        let comment = Comment {
            id: 1,
            issue_id: "bd-abc123".to_string(),
            author: "tester".to_string(),
            body: " ".to_string(),
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        };

        let errors = CommentValidator::validate(&comment).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "content"));
    }

    #[test]
    fn issue_validation_collects_multiple_errors() {
        let mut issue = base_issue();
        issue.id = String::new();
        issue.title = String::new();
        issue.priority = Priority(9);
        issue.updated_at = Utc.with_ymd_and_hms(2025, 12, 31, 0, 0, 0).unwrap();

        let errors = IssueValidator::validate(&issue).unwrap_err();
        let fields: Vec<_> = errors.iter().map(|err| err.field.as_str()).collect();
        assert!(fields.contains(&"id"));
        assert!(fields.contains(&"title"));
        assert!(fields.contains(&"priority"));
        assert!(fields.contains(&"updated_at"));
    }

    #[test]
    fn issue_validation_rejects_external_ref_whitespace() {
        let mut issue = base_issue();
        issue.external_ref = Some("gh 12".to_string());

        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "external_ref"));
    }

    #[test]
    fn id_format_validation_accepts_classic_ids() {
        assert!(is_valid_id_format("bd-abc123"));
        assert!(is_valid_id_format("beads9-0a9"));
    }

    #[test]
    fn id_format_validation_rejects_invalid_ids() {
        assert!(!is_valid_id_format("BD-abc123"));
        assert!(!is_valid_id_format("bd-ABC"));
        // 1 char hash is now allowed (min 1)
        assert!(is_valid_id_format("bd-1"));
        // 9 char hash is allowed (max 40 for hierarchical IDs)
        assert!(is_valid_id_format("bd-abc123456"));

        assert!(!is_valid_id_format("bd_abc"));
        assert!(!is_valid_id_format("bd-abc.def"));
        assert!(!is_valid_id_format("bd-abc.1a"));

        // 26 char hash is now valid (within max 40)
        assert!(is_valid_id_format("bd-abc12345678901234567890123456"));

        // Too long (41 chars) - exceeds max 40
        assert!(!is_valid_id_format(
            "bd-abc123456789012345678901234567890123456789"
        ));
    }

    #[test]
    fn id_format_validation_accepts_long_hash() {
        // Fallback generates 12+ chars. Should be accepted.
        assert!(is_valid_id_format("bd-abc123456789"));
    }

    // =========================================================================
    // SYNC SAFETY INVARIANTS (source-level guards on the sync module)
    // =========================================================================

    /// This test verifies the core safety invariant: no git commands in sync code.
    ///
    /// It uses static analysis (grep) to prove that `Command::new("git")` does
    /// not appear in the sync module.
    #[test]
    fn sync_safety_no_git_commands_in_sync_module() {
        use std::process::Command;

        // Search for git command invocations in sync module
        let output = Command::new("grep")
            .args(["-r", "Command::new.*git", "src/sync/"])
            .output();

        match output {
            Ok(result) => {
                // grep returns exit code 1 when no matches found (which is what we want)
                // grep returns exit code 0 when matches found (which is a failure)
                let stdout = String::from_utf8_lossy(&result.stdout);
                assert!(
                    result.status.code() == Some(1) || stdout.is_empty(),
                    "SAFETY VIOLATION: Found git commands in sync module:\n{stdout}"
                );
            }
            Err(_) => {
                // If grep isn't available, skip this test with a warning
                // This can happen in some CI environments
                eprintln!("Warning: grep not available, skipping static analysis test");
            }
        }
    }

    /// Verify no runtime git dependencies exist in Cargo.toml [dependencies] section.
    ///
    /// Note: Build-time dependencies (like vergen-gix) are allowed since they
    /// don't affect sync runtime behavior.
    #[test]
    fn sync_safety_no_git_library_dependencies() {
        let cargo_toml = std::fs::read_to_string("Cargo.toml").unwrap_or_default();

        // Extract only the [dependencies] section (not [build-dependencies] or [dev-dependencies])
        let deps_section = cargo_toml
            .lines()
            .skip_while(|line| !line.starts_with("[dependencies]"))
            .skip(1) // Skip the [dependencies] header
            .take_while(|line| !line.starts_with('[')) // Stop at next section
            .collect::<Vec<_>>()
            .join("\n");

        // Check for common git library crates in runtime dependencies only
        let git_crates = ["git2 ", "gitoxide ", "gix ", "libgit2 "];

        for crate_name in git_crates {
            let crate_name = crate_name.trim();
            assert!(
                !deps_section.contains(crate_name),
                "SAFETY VIOLATION: Found git library dependency '{crate_name}' in runtime [dependencies]"
            );
        }
    }
}
