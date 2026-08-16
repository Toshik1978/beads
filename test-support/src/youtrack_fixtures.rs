//! Response bodies captured verbatim from a live YouTrack Cloud instance.
//!
//! Each of these drives a branch the `br remote` implementation takes on
//! purpose. They are stored as literals rather than paraphrased so that a
//! test cannot pass against an error shape the API never produces.

/// Re-creating a custom field prototype that already exists.
/// `br remote init` treats this as *already present*, not as a failure.
pub const MUST_BE_UNIQUE: &str = r#"{"error":"must-be-unique","error_description":"A field with the name 'Beads ID' and the 'string' type already exists. Enter a different name or select another data type.","error_developer_message":"A field with the name 'Beads ID' and the 'string' type already exists. Enter a different name or select another data type.","error_field":"name"}"#;

/// Setting `defaultValues` by name instead of by database id.
/// The fix is to read the bundle's `values(id,name)` and resolve first.
pub const DEFAULT_VALUES_BY_NAME: &str = r#"{"error":"Bad Request","error_description":"YouTrack is unable to locate an EnumBundleElement-type entity unless its ID is also provided","error_developer_message":"YouTrack is unable to locate an EnumBundleElement-type entity unless its ID is also provided"}"#;

/// Deleting a link addressed by `idReadable`. The same call with the internal
/// database id succeeds, which is why the reconciliation fetch requests both.
pub const LINK_DELETE_NOT_FOUND: &str =
    r#"{"error":"Not Found","error_description":"Entity with id EM-5 not found"}"#;

/// `POST /api/issuesGetter/count` while the count is still being computed.
pub const COUNT_PENDING: &str = r#"{"count":-1,"$type":"IssueCountResponse"}"#;

/// The same endpoint once it has settled.
pub const COUNT_SETTLED: &str = r#"{"count":0,"$type":"IssueCountResponse"}"#;
