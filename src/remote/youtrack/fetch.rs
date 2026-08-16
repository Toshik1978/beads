//! The paged issue fetch: every issue in the project, in one neutral shape.
//!
//! Two details of the YouTrack response drive this whole module.
//!
//! `updated` is **epoch milliseconds**, not RFC 3339. Comparing a string
//! against an integer would not fail loudly; it would just always pick one
//! side, and the direction rule in `crate::remote::diff` would be silently
//! wrong in one direction forever. It is converted once, here.
//!
//! And the issue GET returns *every* field the project defines — `Assignee`,
//! `Subsystem`, both `Versions` fields — with null or empty values, so fields
//! are selected by name and never by position. That is
//! `mapping::custom_field_by_name`'s job, and this module simply hands it the
//! raw element.

use crate::remote::config::RemoteConfig;
use crate::remote::error::RemoteError;
use crate::remote::http::HttpClient;
use crate::remote::model::{RemoteIssue, RemoteSnapshot, UnmappableIssue};
use crate::remote::youtrack::links::{LinkTypes, parse_links};
use crate::remote::youtrack::mapping::reverse_fields;
use crate::remote::youtrack::tags::labels_from_tags;
use chrono::{DateTime, Utc};
use serde_json::Value;

/// The `fields=` selector for the reconciliation fetch. One place, because a
/// dropped selector is silent: the key is simply absent from the response and
/// the field reads as empty rather than erroring.
///
/// `issues(id,idReadable)` is the one that matters most. Without the internal
/// id a link removal cannot be issued at all — it 404s as if the link were
/// already gone — so `link_diff` carries it and this selector is what makes it
/// available.
pub const ISSUE_FIELDS: &str = "id,idReadable,summary,description,updated,commentsCount,\
tags(id,name),links(id,direction,linkType(id,name),issues(id,idReadable)),\
customFields(name,$type,value(name,text,id))";

/// The sort clause appended to every page query.
///
/// `$skip`/`$top` paging over an unordered collection is not a partition.
/// YouTrack's default ordering is by `updated`, which is the one attribute
/// that changes while a fetch is in flight: an issue touched mid-fetch moves
/// to the front and is either seen twice or missed entirely. Neither failure
/// is loud — a duplicate `idReadable` makes a *paired* issue look like an
/// adoption candidate, and a dropped one makes a live bead look dangling.
/// `created` never changes, so ordering by it makes the pages a real
/// partition.
const SORT_CLAUSE: &str = "%20sort%20by:%20created%20asc";

/// Fetch every issue in the configured project, refusing on the first one br
/// cannot read.
///
/// This is the strict half of the pair. Use it where acting on a partial
/// picture is worse than stopping — `push`, `pull` and `sync`, which write.
/// `br remote status` uses [`fetch_snapshot`] instead, because a command
/// whose job is to report what cannot be mapped must not be disabled by it.
///
/// # Errors
/// Returns whatever `http` returns on transport or HTTP failure, and
/// `RemoteError::Config` — naming the issue — when any issue carries a bundle
/// value with no beads preimage or an `updated` that is not a representable
/// instant.
pub fn fetch_all_issues(
    http: &HttpClient,
    cfg: &RemoteConfig,
    types: &LinkTypes,
) -> Result<Vec<RemoteIssue>, RemoteError> {
    let snapshot = fetch_snapshot(http, cfg, types)?;
    if let Some(first) = snapshot.unmappable.first() {
        return Err(RemoteError::Config(format!(
            "{} of {} issue(s) cannot be read with this remote.yaml; the first is {}. \
             Run `br remote status` to list every one of them.",
            snapshot.unmappable.len(),
            snapshot.unmappable.len() + snapshot.issues.len(),
            first.reason
        )));
    }
    Ok(snapshot.issues)
}

/// Fetch every issue in the configured project, paged at `cfg.page_size`,
/// collecting rather than raising the ones br cannot read.
///
/// The loop stops on the first short page: a page returning fewer items than
/// were asked for is the last one, and a further request would be wasted.
///
/// An issue whose `Type`, `State` or `Priority` has no beads preimage lands in
/// [`RemoteSnapshot::unmappable`] instead of aborting the run, so
/// `br remote status` can print it. Every other failure — transport, HTTP
/// status, a response body that is not a JSON array — is still fatal.
///
/// # Errors
/// Returns whatever `http` returns on transport or HTTP failure, and
/// `RemoteError::Config` when a page body is not a JSON array.
pub fn fetch_snapshot(
    http: &HttpClient,
    cfg: &RemoteConfig,
    types: &LinkTypes,
) -> Result<RemoteSnapshot, RemoteError> {
    let mut snapshot = RemoteSnapshot::default();
    let mut skip = 0_u32;
    loop {
        let path = format!(
            "/api/issues?query=project:%20{}{SORT_CLAUSE}&fields={ISSUE_FIELDS}&$skip={skip}&$top={}",
            cfg.project, cfg.page_size
        );
        let raw = http.get_json(&path, "issues")?;
        let page = raw.as_array().ok_or_else(|| {
            // An empty project answers `[]`. A non-array body is a different
            // instance answering, a proxy interposing, or an API change — and
            // reading it as "no issues" would report every paired bead as
            // dangling and every bead as pending creation.
            RemoteError::Config(format!(
                "the issue list endpoint returned a JSON {}, not an array; \
                 check that {} is a YouTrack base URL",
                json_kind(&raw),
                cfg.url
            ))
        })?;
        let count = u32::try_from(page.len()).unwrap_or(u32::MAX);
        for element in page {
            match parse_issue(cfg, element, types) {
                Ok(issue) => snapshot.issues.push(issue),
                Err(RemoteError::Config(reason)) => {
                    snapshot.unmappable.push(UnmappableIssue {
                        id_readable: string_field(element, "idReadable"),
                        reason,
                    });
                }
                Err(other) => return Err(other),
            }
        }
        if count < cfg.page_size {
            return Ok(snapshot);
        }
        skip += cfg.page_size;
    }
}

/// Parse one element of an issue page.
///
/// Every refusal names the issue. The mapping layer cannot: it is handed one
/// field's value with no idea which issue carried it, and on a 500-issue
/// project `State 'In Review' has no beads mapping` with no id is not a
/// diagnosis, it is a search.
fn parse_issue(
    cfg: &RemoteConfig,
    raw: &Value,
    types: &LinkTypes,
) -> Result<RemoteIssue, RemoteError> {
    let id_readable = string_field(raw, "idReadable");
    let millis = raw.get("updated").and_then(Value::as_i64).unwrap_or(0);
    let updated = DateTime::<Utc>::from_timestamp_millis(millis).ok_or_else(|| {
        RemoteError::Config(format!(
            "issue {id_readable} reports updated={millis}, which is not a representable instant"
        ))
    })?;
    let fields = reverse_fields(cfg, raw).map_err(|err| match err {
        RemoteError::Config(message) => RemoteError::Config(format!("{id_readable}: {message}")),
        other => other,
    })?;

    Ok(RemoteIssue {
        id: string_field(raw, "id"),
        id_readable,
        summary: string_field(raw, "summary"),
        description: raw
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        updated,
        comments_count: raw
            .get("commentsCount")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
        labels: labels_from_tags(raw),
        links: parse_links(raw, types),
        fields,
    })
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Object(_) => "object",
        Value::Array(_) => "array",
    }
}

fn string_field(raw: &Value, key: &str) -> String {
    raw.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::http::{HttpClient, RetryPolicy, Token};
    use test_support::mock_http::MockServer;

    fn config() -> RemoteConfig {
        RemoteConfig::from_yaml_str(include_str!("../../../tests/fixtures/remote_em.yaml"))
            .expect("valid config")
    }

    /// The reference instance's link type ids. Ids live in tests, never in
    /// `src/` — see `links::tests`.
    fn link_types() -> LinkTypes {
        LinkTypes {
            subtask: "173-3".into(),
            depend: "173-1".into(),
            relates: "173-0".into(),
        }
    }

    fn page_path(skip: u32, top: u32) -> String {
        format!(
            "/api/issues?query=project:%20EM{SORT_CLAUSE}&fields={ISSUE_FIELDS}&$skip={skip}&$top={top}"
        )
    }

    fn issue(n: u32) -> serde_json::Value {
        serde_json::json!({
            "id": format!("3-{n}"),
            "idReadable": format!("EM-{n}"),
            "summary": format!("issue {n}"),
            "updated": 1_786_881_829_147_i64,
            "commentsCount": 0,
            "tags": [],
            "links": [],
            "customFields": [
                {"name":"Type","value":{"name":"Task"}},
                {"name":"State","value":{"name":"Open"}},
                {"name":"Priority","value":{"name":"Major"}},
                {"name":"Assignee","value":null},
                {"name":"Fix versions","value":[]}
            ]
        })
    }

    fn page(from: u32, count: u32) -> String {
        let items: Vec<_> = (from..from + count).map(issue).collect();
        serde_json::Value::Array(items).to_string()
    }

    fn client(server: &MockServer) -> HttpClient {
        HttpClient::new(&server.base_url(), Token::new("t"), RetryPolicy::none())
    }

    #[test]
    fn paging_advances_skip_and_stops_on_a_short_page() {
        let server = MockServer::start();
        server.on("GET", &page_path(0, 100), 200, &page(0, 100));
        server.on("GET", &page_path(100, 100), 200, &page(100, 100));
        server.on("GET", &page_path(200, 100), 200, &page(200, 50));

        let http = client(&server);
        let issues = fetch_all_issues(&http, &config(), &link_types()).expect("fetch");

        assert_eq!(issues.len(), 250);
        assert_eq!(
            server.requests().len(),
            3,
            "a short page ends the loop; a fourth request would be wasted"
        );
    }

    #[test]
    fn updated_parses_from_epoch_millis() {
        let server = MockServer::start();
        server.on("GET", &page_path(0, 100), 200, &page(0, 1));
        let http = client(&server);
        let issues = fetch_all_issues(&http, &config(), &link_types()).expect("fetch");

        assert_eq!(
            issues[0].updated.timestamp_millis(),
            1_786_881_829_147,
            "updated is epoch MILLIS, not RFC 3339 and not seconds"
        );
    }

    #[test]
    fn unprovisioned_project_fields_are_ignored() {
        let server = MockServer::start();
        server.on("GET", &page_path(0, 100), 200, &page(0, 1));
        let http = client(&server);
        let issues = fetch_all_issues(&http, &config(), &link_types()).expect("fetch");

        assert_eq!(issues[0].fields.issue_type, crate::model::IssueType::Task);
        assert!(issues[0].fields.design.is_none());
    }

    #[test]
    fn the_field_selector_requests_both_link_identifiers() {
        // Dropping `issues(id,...)` does not error — the key is simply absent
        // and every removal then fails with a 404 that looks like the link was
        // already gone. Pin the selector.
        assert!(
            ISSUE_FIELDS.contains("links(id,direction,linkType(id,name),issues(id,idReadable))")
        );
        assert!(ISSUE_FIELDS.contains("commentsCount"));
        assert!(ISSUE_FIELDS.contains("tags(id,name)"));
        assert!(ISSUE_FIELDS.contains("updated"));
    }

    #[test]
    fn both_link_identifiers_survive_the_parse() {
        let server = MockServer::start();
        let mut element = issue(4);
        element["links"] = serde_json::json!([
            {"id":"173-3t","direction":"INWARD","linkType":{"id":"173-3","name":"Subtask"},
             "issues":[{"id":"3-20","idReadable":"EM-1"}]}
        ]);
        element["tags"] = serde_json::json!([{"id":"6-1","name":"mirror"}]);
        element["commentsCount"] = serde_json::json!(3);
        let body = serde_json::Value::Array(vec![element]).to_string();
        server.on("GET", &page_path(0, 100), 200, &body);

        let http = client(&server);
        let issues = fetch_all_issues(&http, &config(), &link_types()).expect("fetch");

        assert_eq!(issues[0].links.len(), 1);
        assert_eq!(issues[0].links[0].issues[0].id, "3-20");
        assert_eq!(issues[0].links[0].issues[0].id_readable, "EM-1");
        assert_eq!(issues[0].labels, ["mirror"]);
        assert_eq!(issues[0].comments_count, 3);
        assert_eq!(issues[0].id, "3-4");
    }

    #[test]
    fn every_page_asks_for_a_stable_order() {
        // Without it, `$skip`/`$top` is not a partition: YouTrack orders by
        // `updated` by default, so an issue touched mid-fetch is either seen
        // twice or missed, and neither is loud.
        let server = MockServer::start();
        server.on("GET", &page_path(0, 100), 200, &page(0, 1));
        fetch_all_issues(&client(&server), &config(), &link_types()).expect("fetch");

        let path = &server.requests()[0].path;
        assert!(
            path.contains("sort%20by:%20created%20asc"),
            "the page query must pin an immutable sort key: {path}"
        );
    }

    #[test]
    fn an_unmappable_issue_is_collected_by_the_snapshot_and_names_its_id() {
        let server = MockServer::start();
        let mut bad = issue(9);
        bad["customFields"][1] = serde_json::json!({"name":"State","value":{"name":"In Review"}});
        let body = serde_json::Value::Array(vec![issue(1), bad]).to_string();
        server.on("GET", &page_path(0, 100), 200, &body);

        let snapshot = fetch_snapshot(&client(&server), &config(), &link_types()).expect("fetch");

        assert_eq!(snapshot.issues.len(), 1, "the readable issue still lands");
        assert_eq!(snapshot.unmappable.len(), 1);
        assert_eq!(snapshot.unmappable[0].id_readable, "EM-9");
        let reason = &snapshot.unmappable[0].reason;
        assert!(reason.contains("EM-9"), "must name the issue: {reason}");
        assert!(
            reason.contains("In Review"),
            "must name the offending value: {reason}"
        );
        assert!(
            reason.contains("status_map"),
            "must name the map to extend: {reason}"
        );
    }

    #[test]
    fn the_strict_fetch_still_refuses_an_unmappable_issue_by_name() {
        let server = MockServer::start();
        let mut bad = issue(9);
        bad["customFields"][1] = serde_json::json!({"name":"State","value":{"name":"In Review"}});
        let body = serde_json::Value::Array(vec![bad]).to_string();
        server.on("GET", &page_path(0, 100), 200, &body);

        let err =
            fetch_all_issues(&client(&server), &config(), &link_types()).expect_err("must refuse");
        let message = err.to_string();
        assert!(message.contains("EM-9"), "must name the issue: {message}");
        assert!(
            message.contains("br remote status"),
            "must point at the command that lists them all: {message}"
        );
    }

    #[test]
    fn a_non_array_body_is_an_error_not_an_empty_project() {
        // Read as "no issues", this would report every paired bead as dangling
        // and every bead as pending creation.
        let server = MockServer::start();
        server.on("GET", &page_path(0, 100), 200, r#"{"error":"Not Found"}"#);

        let err =
            fetch_snapshot(&client(&server), &config(), &link_types()).expect_err("must refuse");
        let message = err.to_string();
        assert!(
            message.contains("object"),
            "must name the shape it got: {message}"
        );
    }
}
