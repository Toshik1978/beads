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

/// `GET /api/issueLinkTypes` — the path `LinkTypes::resolve` requests.
pub const LINK_TYPES_PATH: &str =
    "/api/issueLinkTypes?fields=id,name,sourceToTarget,targetToSource,directed&$top=100";

/// The reference instance's link types, ids included.
///
/// Ids are instance-local and must never appear in `src/`, which is why they
/// live here. `Duplicate` is deliberately absent: br resolves exactly three
/// types by name and ignores everything else.
pub const LINK_TYPES: &str = r#"[
    {"id":"173-0","name":"Relates","sourceToTarget":"relates to","directed":false},
    {"id":"173-1","name":"Depend","sourceToTarget":"is required for","targetToSource":"depends on","directed":true},
    {"id":"173-3","name":"Subtask","sourceToTarget":"parent for","targetToSource":"subtask of","directed":true}
]"#;

/// `GET /api/admin/projects` — the path a create resolves `EM`'s database id
/// through. A create body addresses its project by internal id; `remote.yaml`
/// names it by the short name a human reads off a YouTrack URL.
pub const PROJECTS_PATH: &str = "/api/admin/projects?fields=id,name,shortName&$top=500";

/// The reference instance's project list.
pub const PROJECTS: &str = r#"[{"id":"0-1","name":"EasyMoney","shortName":"EM"}]"#;

/// One page of the reconciliation fetch for project `EM`, at `page_size: 100`
/// — the settings in `tests/fixtures/remote_em.yaml`.
///
/// The sort clause is part of the contract, not an implementation detail:
/// `$skip`/`$top` over an unordered collection is not a partition, and
/// YouTrack orders by `updated` unless told otherwise, so an issue touched
/// mid-fetch would be seen twice or missed. It is spelled out literally here
/// — and only here — so that four test files cannot drift from each other
/// while all four claim to pin it. Only `ISSUE_FIELDS` is imported, because a
/// selector copied by hand is a selector that goes stale silently.
#[must_use]
pub fn issues_path(skip: u32) -> String {
    use beads::remote::youtrack::fetch::ISSUE_FIELDS;
    format!(
        "/api/issues?query=project:%20EM%20sort%20by:%20created%20asc\
         &fields={ISSUE_FIELDS}&$skip={skip}&$top=100"
    )
}

/// Write `tests/fixtures/remote_em.yaml` into `beads_dir`, pointed at the
/// loopback mock instead of the unresolvable host the tracked copy names.
///
/// # Panics
/// Panics if the file cannot be written — a test cannot proceed without it.
pub fn write_remote_config(beads_dir: &std::path::Path, base_url: &str) {
    let template = include_str!("../../tests/fixtures/remote_em.yaml");
    std::fs::write(
        beads_dir.join("remote.yaml"),
        template.replace("https://example.invalid", base_url),
    )
    .expect("write remote.yaml");
}
