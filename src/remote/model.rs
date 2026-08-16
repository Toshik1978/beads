//! The backend-neutral shape of one mirrored issue.
//!
//! Everything above the backend seam — pairing, the value diff, the link
//! differ, the printed plan — reads this type and never a raw
//! `serde_json::Value`. The one backend (`crate::remote::youtrack`) is
//! responsible for producing it; see `youtrack::fetch::fetch_all_issues`.

use crate::remote::youtrack::links::RemoteLink;
use crate::remote::youtrack::mapping::RemoteIssueFields;
use chrono::{DateTime, Utc};

/// A remote issue br could not read, and why.
///
/// Produced only by the non-fatal fetch. A bundle value with no beads
/// preimage makes an issue unreadable, and `br remote status` — the command
/// whose entire job is to report exactly that — must print it rather than
/// abort on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmappableIssue {
    pub id_readable: String,
    /// The refusal from the mapping layer, which already names the field,
    /// the offending value and the config key that would cover it.
    pub reason: String,
}

/// One non-fatal fetch: the issues br could read, and the ones it could not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteSnapshot {
    pub issues: Vec<RemoteIssue>,
    pub unmappable: Vec<UnmappableIssue>,
}

/// One remote issue, parsed.
///
/// `updated` is the single value the direction rule in `crate::remote::diff`
/// compares against a bead's `updated_at`. YouTrack reports it as **epoch
/// milliseconds**, and it is parsed into a `DateTime<Utc>` at the boundary so
/// nothing downstream has to remember that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteIssue {
    /// Internal database id, e.g. `"3-24"`. Required by a link removal.
    pub id: String,
    /// Human-readable id, e.g. `"EM-5"`. This is what a bead's
    /// `external_ref` holds.
    pub id_readable: String,
    pub summary: String,
    pub description: Option<String>,
    pub updated: DateTime<Utc>,
    pub comments_count: u32,
    pub labels: Vec<String>,
    pub links: Vec<RemoteLink>,
    pub fields: RemoteIssueFields,
}

impl RemoteIssue {
    /// A minimal issue carrying only its readable id, for tests that care
    /// about one field at a time.
    #[cfg(test)]
    #[must_use]
    pub fn for_test(id_readable: &str) -> Self {
        Self {
            id: format!("3-{}", id_readable.rsplit('-').next().unwrap_or("0")),
            id_readable: id_readable.to_string(),
            summary: String::new(),
            description: None,
            updated: DateTime::<Utc>::UNIX_EPOCH,
            comments_count: 0,
            labels: Vec::new(),
            links: Vec::new(),
            fields: RemoteIssueFields {
                title: String::new(),
                description: None,
                status: crate::model::Status::default(),
                issue_type: crate::model::IssueType::default(),
                priority: crate::model::Priority::default(),
                design: None,
                acceptance_criteria: None,
                notes: None,
                close_reason: None,
                beads_id: None,
            },
        }
    }
}
