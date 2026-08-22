//! Core data types for `beads`.
//!
//! This module defines the fundamental types used throughout the application:
//! - `Issue` - The core work item
//! - `Status` - Issue lifecycle states
//! - `IssueType` - Categories of issues
//! - `Dependency` - Relationships between issues
//! - `Comment` - Issue comments

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

pub mod search;
pub mod sort;

/// Deserialize an optional metadata string, coercing a degenerate empty (or
/// whitespace-only) string to `None`.
///
/// Legacy JSONL written by older `br`/`bd` versions serialized absent
/// dependency metadata as `"metadata":""` rather than omitting the field or
/// writing `"{}"`. The empty string is not valid JSON, so downstream consumers
/// that parse `metadata` as JSON (e.g. the JSONL → SQLite rebuild/import path)
/// would reject such records with an opaque `EOF while parsing` error.
///
/// Treating `""` as absent is lossless: a present-but-empty metadata field
/// carries no information, and the import path already materializes `None` as
/// the empty JSON object `"{}"`. Genuine metadata (any non-blank string) is
/// preserved verbatim so real data is never discarded.
fn deserialize_optional_metadata<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(match value {
        Some(s) if s.trim().is_empty() => None,
        other => other,
    })
}

/// Issue lifecycle status.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Open,
    InProgress,
    Blocked,
    Deferred,
    Draft,
    Closed,
    #[serde(rename = "tombstone")]
    Tombstone,
    #[serde(rename = "pinned")]
    Pinned,
    #[serde(untagged)]
    Custom(String),
}

impl<'de> Deserialize<'de> for Status {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match Self::known_value(&value) {
            Some(status) => status,
            None => Self::Custom(value.to_lowercase()),
        })
    }
}

impl Status {
    pub(crate) fn known_value(value: &str) -> Option<Self> {
        Some(match value.to_lowercase().as_str() {
            "open" => Self::Open,
            "in_progress" | "inprogress" => Self::InProgress,
            "blocked" => Self::Blocked,
            "deferred" => Self::Deferred,
            "draft" => Self::Draft,
            "closed" => Self::Closed,
            "tombstone" => Self::Tombstone,
            "pinned" => Self::Pinned,
            _ => return None,
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Deferred => "deferred",
            Self::Draft => "draft",
            Self::Closed => "closed",
            Self::Tombstone => "tombstone",
            Self::Pinned => "pinned",
            Self::Custom(value) => value,
        }
    }

    /// Rank table for sorting, in declaration order. Rendered into a SQL `CASE`
    /// expression by `SortField::sql_fragments`; kept in step with `sort_rank`
    /// by `status_sort_rank_matches_the_sql_rank_table`.
    pub const SORT_RANKS: &'static [(&'static str, u8)] = &[
        ("open", 0),
        ("in_progress", 1),
        ("blocked", 2),
        ("deferred", 3),
        ("draft", 4),
        ("closed", 5),
        ("tombstone", 6),
        ("pinned", 7),
    ];

    /// Rank shared by every `Custom` status, so they sort after all known ones.
    pub const CUSTOM_SORT_RANK: u8 = 99;

    /// Sort position of this status. Exhaustive by construction: a new variant
    /// fails to compile until it is ranked here and added to `SORT_RANKS`.
    #[must_use]
    pub const fn sort_rank(&self) -> u8 {
        match self {
            Self::Open => 0,
            Self::InProgress => 1,
            Self::Blocked => 2,
            Self::Deferred => 3,
            Self::Draft => 4,
            Self::Closed => 5,
            Self::Tombstone => 6,
            Self::Pinned => 7,
            Self::Custom(_) => Self::CUSTOM_SORT_RANK,
        }
    }

    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Closed | Self::Tombstone)
    }

    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Open | Self::InProgress)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for Status {
    type Err = crate::error::BeadsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::known_value(s).unwrap_or_else(|| Self::Custom(s.to_lowercase())))
    }
}

/// Issue priority (0=Critical, 4=Backlog).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub struct Priority(pub i32);

impl Default for Priority {
    fn default() -> Self {
        Self::MEDIUM
    }
}

impl Priority {
    pub const CRITICAL: Self = Self(0);
    pub const HIGH: Self = Self(1);
    pub const MEDIUM: Self = Self(2);
    pub const LOW: Self = Self(3);
    pub const BACKLOG: Self = Self(4);
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "P{}", self.0)
    }
}

impl FromStr for Priority {
    type Err = crate::error::BeadsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_uppercase();
        let val = s.strip_prefix('P').unwrap_or(&s);

        match val.parse::<i32>() {
            Ok(p) if (0..=4).contains(&p) => Ok(Self(p)),
            Ok(p) => Err(crate::error::BeadsError::InvalidPriority {
                priority: p.to_string(),
            }),
            Err(_) => Err(crate::error::BeadsError::InvalidPriority {
                priority: val.to_string(),
            }),
        }
    }
}

/// Issue type category.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IssueType {
    #[default]
    Task,
    Bug,
    Feature,
    Epic,
    Chore,
    Docs,
    Question,
    #[serde(untagged)]
    Custom(String),
}

impl<'de> Deserialize<'de> for IssueType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match Self::known_value(&value) {
            Some(issue_type) => issue_type,
            None => Self::Custom(value.to_lowercase()),
        })
    }
}

impl IssueType {
    pub(crate) fn known_value(value: &str) -> Option<Self> {
        Some(match value.to_lowercase().as_str() {
            "task" => Self::Task,
            "bug" => Self::Bug,
            "feature" => Self::Feature,
            "epic" => Self::Epic,
            "chore" => Self::Chore,
            "docs" => Self::Docs,
            "question" => Self::Question,
            _ => return None,
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Task => "task",
            Self::Bug => "bug",
            Self::Feature => "feature",
            Self::Epic => "epic",
            Self::Chore => "chore",
            Self::Docs => "docs",
            Self::Question => "question",
            Self::Custom(value) => value,
        }
    }

    /// Rank table for sorting, in declaration order. See `Status::SORT_RANKS`.
    pub const SORT_RANKS: &'static [(&'static str, u8)] = &[
        ("task", 0),
        ("bug", 1),
        ("feature", 2),
        ("epic", 3),
        ("chore", 4),
        ("docs", 5),
        ("question", 6),
    ];

    /// Rank shared by every `Custom` type, so they sort after all known ones.
    pub const CUSTOM_SORT_RANK: u8 = 99;

    /// Sort position of this type. Exhaustive by construction.
    #[must_use]
    pub const fn sort_rank(&self) -> u8 {
        match self {
            Self::Task => 0,
            Self::Bug => 1,
            Self::Feature => 2,
            Self::Epic => 3,
            Self::Chore => 4,
            Self::Docs => 5,
            Self::Question => 6,
            Self::Custom(_) => Self::CUSTOM_SORT_RANK,
        }
    }
}

impl fmt::Display for IssueType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for IssueType {
    type Err = crate::error::BeadsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::known_value(s).unwrap_or_else(|| Self::Custom(s.to_lowercase())))
    }
}

/// Dependency relationship type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DependencyType {
    Blocks,
    ParentChild,
    ConditionalBlocks,
    WaitsFor,
    Related,
    DiscoveredFrom,
    RepliesTo,
    RelatesTo,
    Duplicates,
    Supersedes,
    CausedBy,
    #[serde(untagged)]
    Custom(String),
}

impl<'de> Deserialize<'de> for DependencyType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let normalized = value.to_lowercase();
        Ok(match normalized.as_str() {
            "blocks" => Self::Blocks,
            "parent-child" => Self::ParentChild,
            "conditional-blocks" => Self::ConditionalBlocks,
            "waits-for" => Self::WaitsFor,
            "related" => Self::Related,
            "discovered-from" => Self::DiscoveredFrom,
            "replies-to" => Self::RepliesTo,
            "relates-to" => Self::RelatesTo,
            "duplicates" => Self::Duplicates,
            "supersedes" => Self::Supersedes,
            "caused-by" => Self::CausedBy,
            _ => Self::Custom(normalized),
        })
    }
}

impl DependencyType {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Blocks => "blocks",
            Self::ParentChild => "parent-child",
            Self::ConditionalBlocks => "conditional-blocks",
            Self::WaitsFor => "waits-for",
            Self::Related => "related",
            Self::DiscoveredFrom => "discovered-from",
            Self::RepliesTo => "replies-to",
            Self::RelatesTo => "relates-to",
            Self::Duplicates => "duplicates",
            Self::Supersedes => "supersedes",
            Self::CausedBy => "caused-by",
            Self::Custom(value) => value,
        }
    }

    #[must_use]
    pub const fn is_blocking(&self) -> bool {
        matches!(
            self,
            Self::Blocks | Self::ParentChild | Self::ConditionalBlocks | Self::WaitsFor
        )
    }
}

impl fmt::Display for DependencyType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for DependencyType {
    type Err = crate::error::BeadsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "blocks" => Ok(Self::Blocks),
            "parent-child" => Ok(Self::ParentChild),
            "conditional-blocks" => Ok(Self::ConditionalBlocks),
            "waits-for" => Ok(Self::WaitsFor),
            "related" => Ok(Self::Related),
            "discovered-from" => Ok(Self::DiscoveredFrom),
            "replies-to" => Ok(Self::RepliesTo),
            "relates-to" => Ok(Self::RelatesTo),
            "duplicates" => Ok(Self::Duplicates),
            "supersedes" => Ok(Self::Supersedes),
            "caused-by" => Ok(Self::CausedBy),
            other => Ok(Self::Custom(other.to_string())),
        }
    }
}

/// The primary issue entity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Issue {
    /// Unique ID (e.g., "bd-abc123").
    pub id: String,

    /// Content hash for deduplication and sync.
    #[serde(skip)]
    pub content_hash: Option<String>,

    /// Title (1-500 chars).
    pub title: String,

    /// Detailed description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Technical design notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design: Option<String>,

    /// Acceptance criteria.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_criteria: Option<String>,

    /// Additional notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,

    /// Workflow status.
    #[serde(default)]
    pub status: Status,

    /// Priority (0=Critical, 4=Backlog).
    #[serde(default)]
    pub priority: Priority,

    /// Issue type (bug, feature, etc.).
    #[serde(default)]
    pub issue_type: IssueType,

    /// Issue owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,

    /// Creation timestamp.
    pub created_at: DateTime<Utc>,

    /// Creator username.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,

    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,

    /// Closure timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,

    /// Reason for closure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,

    /// Defer until date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_until: Option<DateTime<Utc>>,

    /// External reference (e.g., JIRA-123).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,

    /// Source repository for multi-repo support — basename of the
    /// canonicalized parent of `.beads/`. Stable across clones of the
    /// same repo on different machines (different absolute paths
    /// produce the same basename). See [`canonical_source_repo`] in
    /// `cli::commands::create`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_repo: Option<String>,

    // Tombstone fields
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_type: Option<String>,

    /// Every ID this issue has previously held, oldest first.
    ///
    /// Populated by `rename_issue`. Lets a reference written before a rename —
    /// in a git commit message, a PR body, another issue's prose — still resolve
    /// to the issue it named. Outlives the tombstone left at the old ID, which
    /// `br delete --hard` may collect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub former_ids: Vec<String>,

    // Relations (for export/display, not always in DB table directly)
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub dependencies: Vec<Dependency>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub comments: Vec<Comment>,
}

impl Default for Issue {
    fn default() -> Self {
        Self {
            id: String::new(),
            content_hash: None,
            title: String::new(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::default(),
            priority: Priority::default(),
            issue_type: IssueType::default(),
            owner: None,
            created_at: Utc::now(),
            created_by: None,
            updated_at: Utc::now(),
            closed_at: None,
            close_reason: None,
            defer_until: None,
            external_ref: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            former_ids: Vec::new(),
            labels: Vec::new(),
            dependencies: Vec::new(),
            comments: Vec::new(),
        }
    }
}

impl Issue {
    /// Compute the deterministic content hash for this issue.
    ///
    /// Uses br's canonical field order with length-prefixed field encoding for
    /// unambiguous deduplication.
    /// Excludes IDs, timestamps, relations, and tombstone metadata.
    ///
    /// Delegates to [`crate::util::hash::content_hash`] for the canonical implementation.
    #[must_use]
    pub fn compute_content_hash(&self) -> String {
        crate::util::content_hash(self)
    }

    /// Compare two issues using sync semantics instead of raw struct equality.
    ///
    /// This ignores derived or volatile audit fields that would otherwise make
    /// semantically identical issues look different across import/export
    /// boundaries, while still comparing the full synced payload including
    /// labels, dependencies, comments, and user-visible timestamps like `defer_until`.
    ///
    /// `former_ids` is compared (order matters — it is an append-only,
    /// oldest-first history, not a set): without this, a JSONL record whose
    /// only delta is a new rename hop looks identical to the stored issue
    /// and `merge_issue`'s 3-way merge (`sync/mod.rs`) short-circuits to
    /// `Keep(left)` before ever looking at which side actually changed,
    /// silently dropping the new former ID (bds-a23.6). Deliberately not
    /// mirrored into `compute_content_hash`: that hash exists to match
    /// issues by *content* for dedup (`detect_collision`'s phase-3 hash
    /// match). Its doc comment's "Fields excluded" list
    /// ([`crate::util::hash::content_hash`]) does not name `former_ids`
    /// explicitly, but it excludes `id` for the same reason — an issue's
    /// current or former identity is not "content" — and `former_ids` is
    /// exactly that: an ID-history field, not content. Widening the hash
    /// would also invalidate every stored `content_hash`/`export_hashes`
    /// value in every existing database, forcing a full re-export;
    /// `sync_equals` carries no such stored state to invalidate.
    #[must_use]
    pub fn sync_equals(&self, other: &Self) -> bool {
        if self.id != other.id
            || self.title != other.title
            || self.description != other.description
            || self.design != other.design
            || self.acceptance_criteria != other.acceptance_criteria
            || self.notes != other.notes
            || self.status != other.status
            || self.priority != other.priority
            || self.issue_type != other.issue_type
            || self.owner != other.owner
            || self.created_by != other.created_by
            || self.closed_at != other.closed_at
            || self.close_reason != other.close_reason
            || self.defer_until != other.defer_until
            || self.external_ref != other.external_ref
            || self.source_repo != other.source_repo
            || self.deleted_at != other.deleted_at
            || self.deleted_by != other.deleted_by
            || self.delete_reason != other.delete_reason
            || self.original_type != other.original_type
            || self.former_ids != other.former_ids
        {
            return false;
        }

        // Fast path for relations: if lengths differ, they can't be equal
        if self.dependencies.len() != other.dependencies.len()
            || self.comments.len() != other.comments.len()
        {
            return false;
        }

        // Compare labels (order independent)
        if !Self::labels_equal(&self.labels, &other.labels) {
            return false;
        }

        // Compare dependencies (order independent)
        let mut self_deps = self.dependencies.clone();
        self_deps.sort_by(|left, right| {
            left.issue_id
                .cmp(&right.issue_id)
                .then_with(|| left.depends_on_id.cmp(&right.depends_on_id))
                .then_with(|| left.dep_type.as_str().cmp(right.dep_type.as_str()))
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.created_by.cmp(&right.created_by))
                .then_with(|| left.metadata.cmp(&right.metadata))
                .then_with(|| left.thread_id.cmp(&right.thread_id))
        });
        let mut other_deps = other.dependencies.clone();
        other_deps.sort_by(|left, right| {
            left.issue_id
                .cmp(&right.issue_id)
                .then_with(|| left.depends_on_id.cmp(&right.depends_on_id))
                .then_with(|| left.dep_type.as_str().cmp(right.dep_type.as_str()))
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.created_by.cmp(&right.created_by))
                .then_with(|| left.metadata.cmp(&right.metadata))
                .then_with(|| left.thread_id.cmp(&right.thread_id))
        });
        if self_deps != other_deps {
            return false;
        }

        // Compare comments (order independent)
        let mut self_comments = self.comments.clone();
        self_comments.sort_by(|left, right| {
            left.issue_id
                .cmp(&right.issue_id)
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.author.cmp(&right.author))
                .then_with(|| left.body.cmp(&right.body))
                .then_with(|| left.id.cmp(&right.id))
        });
        let mut other_comments = other.comments.clone();
        other_comments.sort_by(|left, right| {
            left.issue_id
                .cmp(&right.issue_id)
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.author.cmp(&right.author))
                .then_with(|| left.body.cmp(&right.body))
                .then_with(|| left.id.cmp(&right.id))
        });
        if self_comments != other_comments {
            return false;
        }

        true
    }

    /// Order-independent, duplicate-insensitive label comparison, factored
    /// out of [`Self::sync_equals`] to keep that function under clippy's
    /// line-count lint.
    fn labels_equal(left: &[String], right: &[String]) -> bool {
        let mut left = left.to_vec();
        left.sort_unstable();
        left.dedup();
        let mut right = right.to_vec();
        right.sort_unstable();
        right.dedup();
        left == right
    }

    /// Check if this issue is a tombstone that has exceeded its TTL.
    #[must_use]
    pub fn is_expired_tombstone(&self, retention_days: Option<u64>) -> bool {
        if self.status != Status::Tombstone {
            return false;
        }

        let Some(days) = retention_days else {
            return false;
        };

        if days == 0 {
            return false; // Keep forever if 0 (though usually means disabled/immediate, assume safe default)
        }

        let Some(deleted_at) = self.deleted_at else {
            return false; // Keep if deletion time is unknown
        };

        // Clamp days to a safe maximum to avoid panic in Duration::days().
        // chrono::Duration can handle up to ~292,000 years, but we'll clamp to
        // something extremely safe for an issue tracker (e.g., 1000 years).
        let max_safe_days = 365_u64 * 1000;
        let days_i64 = i64::try_from(days.min(max_safe_days)).unwrap_or(365_000);
        let expiration_time = deleted_at + chrono::Duration::days(days_i64);
        Utc::now() > expiration_time
    }
}

/// Epic completion status with child counts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpicStatus {
    pub epic: Issue,
    pub total_children: usize,
    pub closed_children: usize,
    pub eligible_for_close: bool,
}

/// Relationship between two issues.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dependency {
    /// The issue that has the dependency (source).
    pub issue_id: String,

    /// The issue being depended on (target).
    pub depends_on_id: String,

    /// Type of dependency.
    #[serde(rename = "type")]
    pub dep_type: DependencyType,

    /// Creation timestamp.
    pub created_at: DateTime<Utc>,

    /// Creator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,

    /// Optional metadata (JSON).
    ///
    /// A degenerate empty (or whitespace-only) string is coerced to `None` on
    /// deserialization to tolerate legacy JSONL that wrote `"metadata":""`
    /// instead of omitting the field. See [`deserialize_optional_metadata`].
    #[serde(
        default,
        deserialize_with = "deserialize_optional_metadata",
        skip_serializing_if = "Option::is_none"
    )]
    pub metadata: Option<String>,

    /// Thread ID for conversation linking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// A comment on an issue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Comment {
    pub id: i64,
    pub issue_id: String,
    pub author: String,
    #[serde(rename = "text")]
    pub body: String,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn status_custom_roundtrip() {
        let status: Status = serde_json::from_str("\"custom_status\"").unwrap();
        assert_eq!(status, Status::Custom("custom_status".to_string()));
        let serialized = serde_json::to_string(&status).unwrap();
        assert_eq!(serialized, "\"custom_status\"");

        let mixed_case: Status = serde_json::from_str("\"QaReview\"").unwrap();
        assert_eq!(mixed_case, Status::Custom("qareview".to_string()));
    }

    #[test]
    fn issue_type_custom_deserialize_normalizes_spelling() {
        let issue_type: IssueType = serde_json::from_str("\"Odd_Type\"").unwrap();
        assert_eq!(issue_type, IssueType::Custom("odd_type".to_string()));
    }

    #[test]
    fn dependency_type_custom_deserialize_normalizes_spelling() {
        let dep_type: DependencyType = serde_json::from_str("\"Odd-Dep\"").unwrap();
        assert_eq!(dep_type, DependencyType::Custom("odd-dep".to_string()));
    }

    #[test]
    fn issue_deserialize_defaults_missing_fields() {
        let json = r#"{
            "id": "bd-123",
            "title": "Test issue",
            "status": "open",
            "priority": 2,
            "issue_type": "task",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert!(issue.description.is_none());
        assert!(issue.labels.is_empty());
        assert!(issue.dependencies.is_empty());
        assert!(issue.comments.is_empty());
    }

    #[test]
    fn test_issue_serialization() {
        let issue = Issue {
            id: "bd-123".to_string(),
            content_hash: Some("abc".to_string()),
            title: "Test Issue".to_string(),
            description: Some("Desc".to_string()),
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            owner: None,
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            created_by: None,
            updated_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
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
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        };

        let json = serde_json::to_string(&issue).unwrap();
        // Check key fields
        assert!(json.contains("\"id\":\"bd-123\""));
        assert!(json.contains("\"title\":\"Test Issue\""));
        assert!(json.contains("\"status\":\"open\""));
        assert!(json.contains("\"priority\":2"));
        assert!(json.contains("\"issue_type\":\"task\""));
        // Check omission
        assert!(!json.contains("content_hash"));
        assert!(!json.contains("design"));
        assert!(!json.contains("labels")); // empty vec skipped
    }

    #[test]
    fn test_priority_serialization() {
        let p = Priority::CRITICAL;
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "0");
    }

    #[test]
    fn test_dependency_type_serialization() {
        let d = DependencyType::Blocks;
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, "\"blocks\"");

        let d = DependencyType::ParentChild;
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, "\"parent-child\"");
    }

    // ========================================================================
    // STATUS ENUM TESTS
    // ========================================================================

    #[test]
    fn test_status_from_str_open() {
        assert_eq!(Status::from_str("open").unwrap(), Status::Open);
        assert_eq!(Status::from_str("OPEN").unwrap(), Status::Open);
        assert_eq!(Status::from_str("Open").unwrap(), Status::Open);
    }

    #[test]
    fn test_status_from_str_in_progress() {
        assert_eq!(Status::from_str("in_progress").unwrap(), Status::InProgress);
        assert_eq!(Status::from_str("IN_PROGRESS").unwrap(), Status::InProgress);
        assert_eq!(Status::from_str("inprogress").unwrap(), Status::InProgress);
    }

    #[test]
    fn test_status_from_str_blocked() {
        assert_eq!(Status::from_str("blocked").unwrap(), Status::Blocked);
        assert_eq!(Status::from_str("BLOCKED").unwrap(), Status::Blocked);
    }

    #[test]
    fn test_status_from_str_deferred() {
        assert_eq!(Status::from_str("deferred").unwrap(), Status::Deferred);
        assert_eq!(Status::from_str("DEFERRED").unwrap(), Status::Deferred);
    }

    #[test]
    fn test_status_from_str_closed() {
        assert_eq!(Status::from_str("closed").unwrap(), Status::Closed);
        assert_eq!(Status::from_str("CLOSED").unwrap(), Status::Closed);
    }

    #[test]
    fn test_status_from_str_tombstone() {
        assert_eq!(Status::from_str("tombstone").unwrap(), Status::Tombstone);
        assert_eq!(Status::from_str("TOMBSTONE").unwrap(), Status::Tombstone);
    }

    #[test]
    fn test_status_from_str_pinned() {
        assert_eq!(Status::from_str("pinned").unwrap(), Status::Pinned);
        assert_eq!(Status::from_str("PINNED").unwrap(), Status::Pinned);
    }

    #[test]
    fn test_status_from_str_unknown_becomes_custom() {
        let result = Status::from_str("invalid_status").unwrap();
        assert_eq!(result, Status::Custom("invalid_status".to_string()));

        let mixed_case = Status::from_str("QaReview").unwrap();
        assert_eq!(mixed_case, Status::Custom("qareview".to_string()));
    }

    #[test]
    fn test_status_display() {
        assert_eq!(Status::Open.to_string(), "open");
        assert_eq!(Status::InProgress.to_string(), "in_progress");
        assert_eq!(Status::Blocked.to_string(), "blocked");
        assert_eq!(Status::Deferred.to_string(), "deferred");
        assert_eq!(Status::Closed.to_string(), "closed");
        assert_eq!(Status::Tombstone.to_string(), "tombstone");
        assert_eq!(Status::Pinned.to_string(), "pinned");
        assert_eq!(Status::Custom("custom".to_string()).to_string(), "custom");
    }

    #[test]
    fn test_status_is_terminal() {
        assert!(Status::Closed.is_terminal());
        assert!(Status::Tombstone.is_terminal());
        assert!(!Status::Open.is_terminal());
        assert!(!Status::InProgress.is_terminal());
        assert!(!Status::Blocked.is_terminal());
        assert!(!Status::Deferred.is_terminal());
        assert!(!Status::Pinned.is_terminal());
        assert!(!Status::Custom("custom".to_string()).is_terminal());
    }

    #[test]
    fn test_status_is_active() {
        assert!(Status::Open.is_active());
        assert!(Status::InProgress.is_active());
        assert!(!Status::Blocked.is_active());
        assert!(!Status::Deferred.is_active());
        assert!(!Status::Closed.is_active());
        assert!(!Status::Tombstone.is_active());
        assert!(!Status::Pinned.is_active());
        assert!(!Status::Custom("custom".to_string()).is_active());
    }

    #[test]
    fn test_status_as_str() {
        assert_eq!(Status::Open.as_str(), "open");
        assert_eq!(Status::InProgress.as_str(), "in_progress");
        assert_eq!(Status::Blocked.as_str(), "blocked");
        assert_eq!(Status::Deferred.as_str(), "deferred");
        assert_eq!(Status::Closed.as_str(), "closed");
        assert_eq!(Status::Tombstone.as_str(), "tombstone");
        assert_eq!(Status::Pinned.as_str(), "pinned");
        assert_eq!(
            Status::Custom("my_status".to_string()).as_str(),
            "my_status"
        );
    }

    // ========================================================================
    // PRIORITY TESTS
    // ========================================================================

    #[test]
    fn test_priority_from_str_with_p_prefix() {
        assert_eq!(Priority::from_str("P0").unwrap(), Priority::CRITICAL);
        assert_eq!(Priority::from_str("P1").unwrap(), Priority::HIGH);
        assert_eq!(Priority::from_str("P2").unwrap(), Priority::MEDIUM);
        assert_eq!(Priority::from_str("P3").unwrap(), Priority::LOW);
        assert_eq!(Priority::from_str("P4").unwrap(), Priority::BACKLOG);
    }

    #[test]
    fn test_priority_from_str_lowercase_p_prefix() {
        assert_eq!(Priority::from_str("p0").unwrap(), Priority::CRITICAL);
        assert_eq!(Priority::from_str("p1").unwrap(), Priority::HIGH);
        assert_eq!(Priority::from_str("p2").unwrap(), Priority::MEDIUM);
        assert_eq!(Priority::from_str("p3").unwrap(), Priority::LOW);
        assert_eq!(Priority::from_str("p4").unwrap(), Priority::BACKLOG);
    }

    #[test]
    fn test_priority_from_str_without_prefix() {
        assert_eq!(Priority::from_str("0").unwrap(), Priority::CRITICAL);
        assert_eq!(Priority::from_str("1").unwrap(), Priority::HIGH);
        assert_eq!(Priority::from_str("2").unwrap(), Priority::MEDIUM);
        assert_eq!(Priority::from_str("3").unwrap(), Priority::LOW);
        assert_eq!(Priority::from_str("4").unwrap(), Priority::BACKLOG);
    }

    #[test]
    fn test_priority_from_str_invalid_too_high() {
        let result = Priority::from_str("5");
        assert!(result.is_err());
        let result = Priority::from_str("P5");
        assert!(result.is_err());
    }

    #[test]
    fn test_priority_from_str_invalid_negative() {
        let result = Priority::from_str("-1");
        assert!(result.is_err());
    }

    #[test]
    fn test_priority_from_str_invalid_text() {
        let result = Priority::from_str("high");
        assert!(result.is_err());
        let result = Priority::from_str("critical");
        assert!(result.is_err());
    }

    #[test]
    fn test_priority_display() {
        assert_eq!(Priority::CRITICAL.to_string(), "P0");
        assert_eq!(Priority::HIGH.to_string(), "P1");
        assert_eq!(Priority::MEDIUM.to_string(), "P2");
        assert_eq!(Priority::LOW.to_string(), "P3");
        assert_eq!(Priority::BACKLOG.to_string(), "P4");
    }

    #[test]
    fn test_priority_ordering() {
        assert!(Priority::CRITICAL < Priority::HIGH);
        assert!(Priority::HIGH < Priority::MEDIUM);
        assert!(Priority::MEDIUM < Priority::LOW);
        assert!(Priority::LOW < Priority::BACKLOG);
    }

    #[test]
    fn test_priority_default() {
        let p = Priority::default();
        assert_eq!(p, Priority(2));
    }

    // ========================================================================
    // ISSUE TYPE TESTS
    // ========================================================================

    #[test]
    fn test_issue_type_from_str_all_variants() {
        // Only these 5 types are valid via FromStr for bd conformance
        assert_eq!(IssueType::from_str("task").unwrap(), IssueType::Task);
        assert_eq!(IssueType::from_str("bug").unwrap(), IssueType::Bug);
        assert_eq!(IssueType::from_str("feature").unwrap(), IssueType::Feature);
        assert_eq!(IssueType::from_str("epic").unwrap(), IssueType::Epic);
        assert_eq!(IssueType::from_str("chore").unwrap(), IssueType::Chore);
        // docs and question are now accepted
        assert_eq!(IssueType::from_str("docs").unwrap(), IssueType::Docs);
        assert_eq!(
            IssueType::from_str("question").unwrap(),
            IssueType::Question
        );
    }

    #[test]
    fn test_issue_type_from_str_case_insensitive() {
        assert_eq!(IssueType::from_str("TASK").unwrap(), IssueType::Task);
        assert_eq!(IssueType::from_str("BUG").unwrap(), IssueType::Bug);
        assert_eq!(IssueType::from_str("Feature").unwrap(), IssueType::Feature);
    }

    #[test]
    fn test_issue_type_from_str_custom_accepted() {
        // Custom/unknown types are accepted as IssueType::Custom
        let result = IssueType::from_str("custom_type");
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            IssueType::Custom("custom_type".to_string())
        );

        let mixed_case = IssueType::from_str("Odd_Type").unwrap();
        assert_eq!(mixed_case, IssueType::Custom("odd_type".to_string()));
    }

    #[test]
    fn test_issue_type_display() {
        assert_eq!(IssueType::Task.to_string(), "task");
        assert_eq!(IssueType::Bug.to_string(), "bug");
        assert_eq!(IssueType::Feature.to_string(), "feature");
        assert_eq!(IssueType::Epic.to_string(), "epic");
        assert_eq!(IssueType::Chore.to_string(), "chore");
        assert_eq!(IssueType::Docs.to_string(), "docs");
        assert_eq!(IssueType::Question.to_string(), "question");
        assert_eq!(
            IssueType::Custom("my_type".to_string()).to_string(),
            "my_type"
        );
    }

    #[test]
    fn test_issue_type_as_str() {
        assert_eq!(IssueType::Task.as_str(), "task");
        assert_eq!(IssueType::Bug.as_str(), "bug");
        assert_eq!(IssueType::Feature.as_str(), "feature");
        assert_eq!(IssueType::Epic.as_str(), "epic");
        assert_eq!(IssueType::Chore.as_str(), "chore");
        assert_eq!(IssueType::Docs.as_str(), "docs");
        assert_eq!(IssueType::Question.as_str(), "question");
        assert_eq!(IssueType::Custom("custom".to_string()).as_str(), "custom");
    }

    #[test]
    fn test_issue_type_default() {
        assert_eq!(IssueType::default(), IssueType::Task);
    }

    // ========================================================================
    // DEPENDENCY TYPE TESTS
    // ========================================================================

    #[test]
    fn test_dependency_type_from_str_all_variants() {
        assert_eq!(
            DependencyType::from_str("blocks").unwrap(),
            DependencyType::Blocks
        );
        assert_eq!(
            DependencyType::from_str("parent-child").unwrap(),
            DependencyType::ParentChild
        );
        assert_eq!(
            DependencyType::from_str("conditional-blocks").unwrap(),
            DependencyType::ConditionalBlocks
        );
        assert_eq!(
            DependencyType::from_str("waits-for").unwrap(),
            DependencyType::WaitsFor
        );
        assert_eq!(
            DependencyType::from_str("related").unwrap(),
            DependencyType::Related
        );
        assert_eq!(
            DependencyType::from_str("discovered-from").unwrap(),
            DependencyType::DiscoveredFrom
        );
        assert_eq!(
            DependencyType::from_str("replies-to").unwrap(),
            DependencyType::RepliesTo
        );
        assert_eq!(
            DependencyType::from_str("relates-to").unwrap(),
            DependencyType::RelatesTo
        );
        assert_eq!(
            DependencyType::from_str("duplicates").unwrap(),
            DependencyType::Duplicates
        );
        assert_eq!(
            DependencyType::from_str("supersedes").unwrap(),
            DependencyType::Supersedes
        );
        assert_eq!(
            DependencyType::from_str("caused-by").unwrap(),
            DependencyType::CausedBy
        );
    }

    #[test]
    fn test_dependency_type_from_str_custom() {
        let result = DependencyType::from_str("my-custom-dep").unwrap();
        assert_eq!(result, DependencyType::Custom("my-custom-dep".to_string()));

        let mixed_case = DependencyType::from_str("My-Custom-Dep").unwrap();
        assert_eq!(
            mixed_case,
            DependencyType::Custom("my-custom-dep".to_string())
        );
    }

    #[test]
    fn test_dependency_type_is_blocking() {
        assert!(DependencyType::Blocks.is_blocking());
        assert!(DependencyType::ParentChild.is_blocking());
        assert!(DependencyType::ConditionalBlocks.is_blocking());
        assert!(DependencyType::WaitsFor.is_blocking());
        assert!(!DependencyType::Related.is_blocking());
        assert!(!DependencyType::DiscoveredFrom.is_blocking());
        assert!(!DependencyType::RepliesTo.is_blocking());
        assert!(!DependencyType::RelatesTo.is_blocking());
        assert!(!DependencyType::Duplicates.is_blocking());
        assert!(!DependencyType::Supersedes.is_blocking());
        assert!(!DependencyType::CausedBy.is_blocking());
        assert!(!DependencyType::Custom("custom".to_string()).is_blocking());
    }

    #[test]
    fn test_dependency_type_display() {
        assert_eq!(DependencyType::Blocks.to_string(), "blocks");
        assert_eq!(DependencyType::ParentChild.to_string(), "parent-child");
        assert_eq!(
            DependencyType::ConditionalBlocks.to_string(),
            "conditional-blocks"
        );
        assert_eq!(DependencyType::WaitsFor.to_string(), "waits-for");
        assert_eq!(DependencyType::Related.to_string(), "related");
        assert_eq!(
            DependencyType::DiscoveredFrom.to_string(),
            "discovered-from"
        );
        assert_eq!(DependencyType::RepliesTo.to_string(), "replies-to");
        assert_eq!(DependencyType::RelatesTo.to_string(), "relates-to");
        assert_eq!(DependencyType::Duplicates.to_string(), "duplicates");
        assert_eq!(DependencyType::Supersedes.to_string(), "supersedes");
        assert_eq!(DependencyType::CausedBy.to_string(), "caused-by");
        assert_eq!(
            DependencyType::Custom("custom".to_string()).to_string(),
            "custom"
        );
    }

    // ========================================================================
    // ISSUE CONTENT HASH TESTS
    // ========================================================================

    fn create_test_issue() -> Issue {
        Issue {
            id: "bd-test".to_string(),
            content_hash: None,
            title: "Test Title".to_string(),
            description: Some("Test Description".to_string()),
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            owner: None,
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            created_by: None,
            updated_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
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
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        }
    }

    #[test]
    fn test_issue_content_hash_deterministic() {
        let issue1 = create_test_issue();
        let issue2 = create_test_issue();

        let hash1 = issue1.compute_content_hash();
        let hash2 = issue2.compute_content_hash();

        assert_eq!(hash1, hash2, "Same content should produce same hash");
        assert!(!hash1.is_empty(), "Hash should not be empty");
    }

    #[test]
    fn test_issue_content_hash_changes_on_title_update() {
        let issue1 = create_test_issue();
        let mut issue2 = create_test_issue();
        issue2.title = "Different Title".to_string();

        let hash1 = issue1.compute_content_hash();
        let hash2 = issue2.compute_content_hash();

        assert_ne!(
            hash1, hash2,
            "Different title should produce different hash"
        );
    }

    #[test]
    fn test_issue_content_hash_changes_on_description_update() {
        let issue1 = create_test_issue();
        let mut issue2 = create_test_issue();
        issue2.description = Some("Different Description".to_string());

        let hash1 = issue1.compute_content_hash();
        let hash2 = issue2.compute_content_hash();

        assert_ne!(
            hash1, hash2,
            "Different description should produce different hash"
        );
    }

    #[test]
    fn test_issue_content_hash_changes_on_status_update() {
        let issue1 = create_test_issue();
        let mut issue2 = create_test_issue();
        issue2.status = Status::Closed;

        let hash1 = issue1.compute_content_hash();
        let hash2 = issue2.compute_content_hash();

        assert_ne!(
            hash1, hash2,
            "Different status should produce different hash"
        );
    }

    #[test]
    fn test_issue_content_hash_changes_on_priority_update() {
        let issue1 = create_test_issue();
        let mut issue2 = create_test_issue();
        issue2.priority = Priority::CRITICAL;

        let hash1 = issue1.compute_content_hash();
        let hash2 = issue2.compute_content_hash();

        assert_ne!(
            hash1, hash2,
            "Different priority should produce different hash"
        );
    }

    #[test]
    fn test_issue_content_hash_unchanged_by_timestamps() {
        let issue1 = create_test_issue();
        let mut issue2 = create_test_issue();
        issue2.created_at = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        issue2.updated_at = Utc.timestamp_opt(1_800_000_000, 0).unwrap();

        let hash1 = issue1.compute_content_hash();
        let hash2 = issue2.compute_content_hash();

        assert_eq!(hash1, hash2, "Different timestamps should NOT change hash");
    }

    #[test]
    fn test_issue_content_hash_unchanged_by_id() {
        let issue1 = create_test_issue();
        let mut issue2 = create_test_issue();
        issue2.id = "bd-different".to_string();

        let hash1 = issue1.compute_content_hash();
        let hash2 = issue2.compute_content_hash();

        assert_eq!(hash1, hash2, "Different ID should NOT change hash");
    }

    #[test]
    fn test_issue_sync_equals_ignores_audit_timestamps_and_relation_order() {
        let mut issue1 = create_test_issue();
        issue1.labels = vec!["backend".to_string(), "bug".to_string()];
        issue1.dependencies = vec![
            Dependency {
                issue_id: issue1.id.clone(),
                depends_on_id: "bd-parent".to_string(),
                dep_type: DependencyType::Blocks,
                created_at: Utc.timestamp_opt(1_700_000_100, 0).unwrap(),
                created_by: Some("alice".to_string()),
                metadata: Some("{\"source\":\"cli\"}".to_string()),
                thread_id: Some("br-1".to_string()),
            },
            Dependency {
                issue_id: issue1.id.clone(),
                depends_on_id: "bd-epic".to_string(),
                dep_type: DependencyType::ParentChild,
                created_at: Utc.timestamp_opt(1_700_000_200, 0).unwrap(),
                created_by: Some("alice".to_string()),
                metadata: None,
                thread_id: None,
            },
        ];
        issue1.comments = vec![
            Comment {
                id: 2,
                issue_id: issue1.id.clone(),
                author: "alice".to_string(),
                body: "second".to_string(),
                created_at: Utc.timestamp_opt(1_700_000_200, 0).unwrap(),
            },
            Comment {
                id: 1,
                issue_id: issue1.id.clone(),
                author: "alice".to_string(),
                body: "first".to_string(),
                created_at: Utc.timestamp_opt(1_700_000_100, 0).unwrap(),
            },
        ];

        let mut issue2 = issue1.clone();
        issue2.created_at = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        issue2.updated_at = Utc.timestamp_opt(1_800_000_500, 0).unwrap();
        issue2.labels.reverse();
        issue2.dependencies.reverse();
        issue2.comments.reverse();
        issue2.content_hash = Some("stale-hash".to_string());

        assert!(issue1.sync_equals(&issue2));
        assert!(issue2.sync_equals(&issue1));
    }

    #[test]
    fn test_issue_sync_equals_detects_semantic_changes() {
        let issue1 = create_test_issue();
        let mut issue2 = create_test_issue();
        issue2.defer_until = Some(Utc.timestamp_opt(1_800_000_000, 0).unwrap());

        assert!(!issue1.sync_equals(&issue2));
    }

    #[test]
    fn test_issue_sync_equals_detects_source_repo_changes() {
        let mut issue1 = create_test_issue();
        issue1.source_repo = Some("widget_engine".to_string());

        let mut issue2 = issue1.clone();
        issue2.source_repo = Some("alternate_widget_engine".to_string());

        assert!(!issue1.sync_equals(&issue2));
        assert!(!issue2.sync_equals(&issue1));
    }

    #[test]
    fn test_issue_sync_equals_detects_former_ids_only_change() {
        let issue1 = create_test_issue();
        let mut issue2 = issue1.clone();
        issue2.former_ids = vec!["bd-old".to_string()];

        assert!(
            !issue1.sync_equals(&issue2),
            "a new former_ids entry must be a semantic change, or a rename \
             carried by JSONL cannot outrun an equal-but-stale comparison \
             (bds-a23.6)"
        );
        assert!(!issue2.sync_equals(&issue1));

        // Order matters: it is an append-only, oldest-first history, not a set.
        let mut issue3 = issue1.clone();
        issue3.former_ids = vec!["bd-a".to_string(), "bd-b".to_string()];
        let mut issue4 = issue1.clone();
        issue4.former_ids = vec!["bd-b".to_string(), "bd-a".to_string()];
        assert!(!issue3.sync_equals(&issue4));
    }

    #[test]
    fn test_content_hash_ignores_former_ids_by_design() {
        // former_ids is ID-history metadata, in the same excluded family as
        // `id` itself (see the "Fields excluded" list on
        // crate::util::hash::content_hash) -- content_hash exists to match
        // issues by *content* for dedup, not by which IDs they used to hold.
        // This pins that exclusion as deliberate so a future change does not
        // "fix" it into widening the hash, which would invalidate every
        // stored content_hash / export_hashes value in every existing
        // database.
        let issue1 = create_test_issue();
        let mut issue2 = issue1.clone();
        issue2.former_ids = vec!["bd-old".to_string()];

        assert_eq!(issue1.compute_content_hash(), issue2.compute_content_hash());
    }

    #[test]
    fn test_issue_sync_equals_treats_duplicate_labels_as_equivalent() {
        let mut issue1 = create_test_issue();
        issue1.labels = vec![
            "backend".to_string(),
            "backend".to_string(),
            "urgent".to_string(),
        ];

        let mut issue2 = create_test_issue();
        issue2.labels = vec!["urgent".to_string(), "backend".to_string()];

        assert!(issue1.sync_equals(&issue2));
        assert!(issue2.sync_equals(&issue1));
    }

    // ========================================================================
    // ISSUE TOMBSTONE TESTS
    // ========================================================================

    #[test]
    fn test_is_expired_tombstone_not_tombstone() {
        let issue = create_test_issue();
        assert!(!issue.is_expired_tombstone(Some(30)));
    }

    #[test]
    fn test_is_expired_tombstone_no_retention() {
        let mut issue = create_test_issue();
        issue.status = Status::Tombstone;
        issue.deleted_at = Some(Utc::now() - chrono::Duration::days(100));
        assert!(!issue.is_expired_tombstone(None));
    }

    #[test]
    fn test_is_expired_tombstone_zero_retention() {
        let mut issue = create_test_issue();
        issue.status = Status::Tombstone;
        issue.deleted_at = Some(Utc::now() - chrono::Duration::days(100));
        assert!(!issue.is_expired_tombstone(Some(0)));
    }

    #[test]
    fn test_is_expired_tombstone_no_deleted_at() {
        let mut issue = create_test_issue();
        issue.status = Status::Tombstone;
        assert!(!issue.is_expired_tombstone(Some(30)));
    }

    #[test]
    fn test_is_expired_tombstone_not_expired() {
        let mut issue = create_test_issue();
        issue.status = Status::Tombstone;
        issue.deleted_at = Some(Utc::now() - chrono::Duration::days(10));
        assert!(!issue.is_expired_tombstone(Some(30)));
    }

    #[test]
    fn test_is_expired_tombstone_expired() {
        let mut issue = create_test_issue();
        issue.status = Status::Tombstone;
        issue.deleted_at = Some(Utc::now() - chrono::Duration::days(40));
        assert!(issue.is_expired_tombstone(Some(30)));
    }

    // ========================================================================
    // EVENT TYPE TESTS
    // ========================================================================

    // ========================================================================
    // COMMENT TESTS
    // ========================================================================

    #[test]
    fn test_comment_serialization_roundtrip() {
        let comment = Comment {
            id: 123,
            issue_id: "bd-abc".to_string(),
            author: "testuser".to_string(),
            body: "This is a comment".to_string(),
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        };

        let json = serde_json::to_string(&comment).unwrap();
        let deserialized: Comment = serde_json::from_str(&json).unwrap();

        assert_eq!(comment, deserialized);
    }

    #[test]
    fn test_comment_text_field_renamed() {
        let json = r#"{"id":1,"issue_id":"bd-123","author":"user","text":"comment body","created_at":"2026-01-01T00:00:00Z"}"#;
        let comment: Comment = serde_json::from_str(json).unwrap();
        assert_eq!(comment.body, "comment body");
    }

    // ========================================================================
    // DEPENDENCY TESTS
    // ========================================================================

    #[test]
    fn test_dependency_serialization_roundtrip() {
        let dep = Dependency {
            issue_id: "bd-abc".to_string(),
            depends_on_id: "bd-xyz".to_string(),
            dep_type: DependencyType::Blocks,
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            created_by: Some("testuser".to_string()),
            metadata: None,
            thread_id: None,
        };

        let json = serde_json::to_string(&dep).unwrap();
        let deserialized: Dependency = serde_json::from_str(&json).unwrap();

        assert_eq!(dep, deserialized);
    }

    #[test]
    fn test_dependency_type_field_renamed() {
        let json = r#"{"issue_id":"bd-1","depends_on_id":"bd-2","type":"blocks","created_at":"2026-01-01T00:00:00Z"}"#;
        let dep: Dependency = serde_json::from_str(json).unwrap();
        assert_eq!(dep.dep_type, DependencyType::Blocks);
    }

    #[test]
    fn dependency_empty_string_metadata_coerced_to_none() {
        // Regression for issue #323: legacy JSONL serialized absent dependency
        // metadata as `"metadata":""`, which is not valid JSON. Deserialization
        // must tolerate it by coercing the degenerate empty string to `None`
        // (lossless: the import path materializes `None` as `"{}"`).
        let json = r#"{
            "issue_id": "example-1",
            "depends_on_id": "example-0",
            "type": "blocks",
            "created_at": "2026-01-01T00:00:00Z",
            "created_by": "user",
            "metadata": "",
            "thread_id": ""
        }"#;
        let dep: Dependency = serde_json::from_str(json).expect("empty metadata must deserialize");
        assert_eq!(
            dep.metadata, None,
            "empty-string metadata should become None"
        );
        // thread_id intentionally has no coercion: only `metadata` is parsed as
        // JSON downstream, so only it needs the empty-string tolerance.
    }

    #[test]
    fn dependency_whitespace_metadata_coerced_to_none() {
        let json = r#"{"issue_id":"a","depends_on_id":"b","type":"blocks","created_at":"2026-01-01T00:00:00Z","metadata":"   "}"#;
        let dep: Dependency = serde_json::from_str(json).unwrap();
        assert_eq!(dep.metadata, None);
    }

    #[test]
    fn dependency_real_metadata_preserved() {
        // Genuine JSON metadata must round-trip unchanged (no data loss).
        let json = r#"{"issue_id":"a","depends_on_id":"b","type":"blocks","created_at":"2026-01-01T00:00:00Z","metadata":"{\"k\":\"v\"}"}"#;
        let dep: Dependency = serde_json::from_str(json).unwrap();
        assert_eq!(dep.metadata.as_deref(), Some(r#"{"k":"v"}"#));
        // And it survives a serialize round-trip.
        let reser = serde_json::to_string(&dep).unwrap();
        let dep2: Dependency = serde_json::from_str(&reser).unwrap();
        assert_eq!(dep, dep2);
    }

    // ========================================================================
    // EVENT TESTS
    // ========================================================================

    // ========================================================================
    // EPIC STATUS TESTS
    // ========================================================================

    #[test]
    fn test_epic_status_serialization() {
        let epic_status = EpicStatus {
            epic: create_test_issue(),
            total_children: 10,
            closed_children: 7,
            eligible_for_close: false,
        };

        let json = serde_json::to_string(&epic_status).unwrap();
        assert!(json.contains("\"total_children\":10"));
        assert!(json.contains("\"closed_children\":7"));
        assert!(json.contains("\"eligible_for_close\":false"));
    }

    // ========================================================================
    // SORT RANK TESTS
    // ========================================================================

    #[test]
    fn status_sort_rank_follows_declaration_order() {
        assert_eq!(Status::Open.sort_rank(), 0);
        assert_eq!(Status::InProgress.sort_rank(), 1);
        assert_eq!(Status::Blocked.sort_rank(), 2);
        assert_eq!(Status::Deferred.sort_rank(), 3);
        assert_eq!(Status::Draft.sort_rank(), 4);
        assert_eq!(Status::Closed.sort_rank(), 5);
        assert_eq!(Status::Tombstone.sort_rank(), 6);
        assert_eq!(Status::Pinned.sort_rank(), 7);
    }

    #[test]
    fn status_custom_sorts_last() {
        assert_eq!(Status::Custom("triage".to_string()).sort_rank(), 99);
        assert!(Status::Custom("aaa".to_string()).sort_rank() > Status::Pinned.sort_rank());
    }

    #[test]
    fn status_sort_rank_matches_the_sql_rank_table() {
        for (name, rank) in Status::SORT_RANKS {
            let parsed = Status::known_value(name).expect("table names must be known statuses");
            assert_eq!(parsed.sort_rank(), *rank, "rank mismatch for {name}");
        }
        assert_eq!(
            Status::SORT_RANKS.len(),
            8,
            "every known status must be in the table"
        );
    }

    #[test]
    fn issue_type_sort_rank_follows_declaration_order() {
        assert_eq!(IssueType::Task.sort_rank(), 0);
        assert_eq!(IssueType::Bug.sort_rank(), 1);
        assert_eq!(IssueType::Feature.sort_rank(), 2);
        assert_eq!(IssueType::Epic.sort_rank(), 3);
        assert_eq!(IssueType::Chore.sort_rank(), 4);
        assert_eq!(IssueType::Docs.sort_rank(), 5);
        assert_eq!(IssueType::Question.sort_rank(), 6);
        assert_eq!(IssueType::Custom("spike".to_string()).sort_rank(), 99);
    }

    #[test]
    fn issue_type_sort_rank_matches_the_sql_rank_table() {
        for (name, rank) in IssueType::SORT_RANKS {
            let parsed = IssueType::known_value(name).expect("table names must be known types");
            assert_eq!(parsed.sort_rank(), *rank, "rank mismatch for {name}");
        }
        assert_eq!(
            IssueType::SORT_RANKS.len(),
            7,
            "every known type must be in the table"
        );
    }
}
