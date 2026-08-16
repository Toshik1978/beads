//! Schema provisioning for a YouTrack project.
//!
//! Custom field *prototypes* are global to the instance; only the attachment
//! is per-project. Creating `Design` therefore makes it visible in every
//! project's administration, which is documented rather than avoided — there
//! is no per-project field namespace to use instead.
//!
//! Two adjacent POSTs carry opposite shape rules, and both were verified
//! against a live instance. The prototype POST must **omit** the `$type`
//! discriminator — sending `{"$type":"TextCustomField"}` is answered with
//! *"Cannot interpret value as …CustomField / Error in field unknown"* — while
//! the attach POST one call later **requires** it. Getting either backwards
//! produces an error that reads like a bug in the payload rather than a bug in
//! the `$type`.

use crate::remote::error::RemoteError;
use crate::remote::http::HttpClient;
use serde_json::json;

/// The four prose fields, carried as `text` rather than `string`: `string` is
/// single-line and would flatten multi-paragraph prose.
pub const PROSE_FIELDS: [&str; 4] = ["Design", "Acceptance Criteria", "Notes", "Close Reason"];

/// A human affordance only — written best-effort, never read.
pub const BEADS_ID_FIELD: &str = "Beads ID";

/// Every field br provisions, in the order it creates them.
pub const ALL_FIELDS: [(&str, &str); 5] = [
    (BEADS_ID_FIELD, "string"),
    ("Design", "text"),
    ("Acceptance Criteria", "text"),
    ("Notes", "text"),
    ("Close Reason", "text"),
];

const FIELD_LIST_FIELDS: &str = "id,name,fieldType(id)";
const FIELD_CREATE_PATH: &str =
    "/api/admin/customFieldSettings/customFields?fields=id,name,fieldType(id)";

/// Page size for the field-prototype enumeration. The prototype list has no
/// name-query affordance the way `/api/admin/projects` and `/api/tags` do —
/// verified against the YouTrack REST API docs, which document only
/// `fields`/`$top`/`$skip` for this resource — so this pages rather than
/// switching to a targeted lookup.
const FIELD_LIST_PAGE_SIZE: u32 = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    pub id: String,
    pub short_name: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRef {
    pub id: String,
    pub name: String,
    pub field_type: String,
}

#[derive(Debug)]
pub struct AdminClient {
    http: HttpClient,
    count_poll_base_delay: std::time::Duration,
}

/// A JSON array as a slice, or an empty slice for anything else.
///
/// Written as a helper rather than `value.as_array().unwrap_or(&vec![])`
/// because that form allocates a `Vec` on every call and only compiles at all
/// thanks to temporary lifetime extension inside a single expression.
pub(crate) fn json_array(value: Option<&serde_json::Value>) -> &[serde_json::Value] {
    value
        .and_then(serde_json::Value::as_array)
        .map_or(&[][..], Vec::as_slice)
}

/// A string-valued key of a JSON object, or `""`.
pub(crate) fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
}

/// Page size for [`resolve_project_by_short_name`].
const RESOLVE_PROJECT_PAGE_SIZE: u32 = 500;

/// Find the project by its short name, paged.
///
/// A free function rather than a method so `execute::resolve_project_id` —
/// which addresses a create body's project by database id, needs no other
/// part of `AdminClient`, and holds only a borrowed `HttpClient` rather than
/// owning one — can share it instead of carrying its own unpaginated copy of
/// the same read. [`AdminClient::resolve_project`] is a thin wrapper over
/// this for callers that already have a client.
///
/// `/api/admin/projects` does accept a `query=` name search (verified against
/// the YouTrack REST API docs), the same affordance `/api/tags` offers
/// `tags::fetch_tag_by_name`, and it would cost one request instead of a
/// sweep on a large instance. It is not used here: `query=` on this resource
/// matches against both `name` and `shortName` substrings, so it is
/// recoverable only by the same post-filter the tag lookup already needs —
/// but this read is called exactly once per run (unlike the tag recovery
/// path, which exists specifically because enumeration had already failed
/// once), so the case for paying a search endpoint's looser matching is
/// weaker here. Pages instead, the same way `fetch::fetch_snapshot` does,
/// stopping on the first short page. Unlike that fetch, this carries no sort
/// clause: an admin project list is a small, effectively static collection
/// for the life of one run, not a set of issues someone else is editing
/// concurrently, so there is no tail this instance's own writes could shift
/// between pages.
///
/// # Errors
/// Returns `RemoteError::Config` when no project carries that short name.
pub(crate) fn resolve_project_by_short_name(
    http: &HttpClient,
    short_name: &str,
) -> Result<ProjectRef, RemoteError> {
    let mut skip = 0_u32;
    loop {
        let path = format!(
            "/api/admin/projects?fields=id,name,shortName&$skip={skip}&$top={RESOLVE_PROJECT_PAGE_SIZE}"
        );
        let value = http.get_json(&path, "project list")?;
        let page = json_array(Some(&value));
        if let Some(found) = page.iter().find(|p| json_str(p, "shortName") == short_name) {
            return Ok(ProjectRef {
                id: json_str(found, "id").to_string(),
                short_name: short_name.to_string(),
                name: json_str(found, "name").to_string(),
            });
        }
        let count = u32::try_from(page.len()).unwrap_or(u32::MAX);
        if count < RESOLVE_PROJECT_PAGE_SIZE {
            return Err(RemoteError::Config(format!(
                "no project with short name '{short_name}' is visible to this token"
            )));
        }
        skip += RESOLVE_PROJECT_PAGE_SIZE;
    }
}

impl AdminClient {
    #[must_use]
    pub fn new(http: HttpClient) -> Self {
        Self {
            http,
            count_poll_base_delay: crate::remote::youtrack::clone::COUNT_POLL_BASE_DELAY,
        }
    }

    /// Borrow the underlying client, so callers can assert on request counts.
    #[must_use]
    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    /// Override how long `project_is_empty` waits between polls.
    ///
    /// Production keeps the default. Tests set it to zero: the six-attempt
    /// budget with the real 200ms base spends 6.2 seconds asleep proving that
    /// a count which never settles is reported as `Unknown`, and what that
    /// test is about is the verdict, not the wall clock.
    #[must_use]
    pub fn with_count_poll_delay(mut self, delay: std::time::Duration) -> Self {
        self.count_poll_base_delay = delay;
        self
    }

    /// The base delay `project_is_empty` backs off from.
    #[must_use]
    pub(crate) fn count_poll_base_delay(&self) -> std::time::Duration {
        self.count_poll_base_delay
    }

    /// Find the project by its short name, paged.
    ///
    /// # Errors
    /// Returns `RemoteError::Config` when no project carries that short name.
    pub fn resolve_project(&self, short_name: &str) -> Result<ProjectRef, RemoteError> {
        resolve_project_by_short_name(&self.http, short_name)
    }

    /// Ensure all five prototypes exist, creating only the missing ones.
    ///
    /// # Errors
    /// Returns `RemoteError` on transport failure or a non-absorbable HTTP
    /// error. A `must-be-unique` response is absorbed: it means another admin
    /// created the field between our read and our write.
    pub fn ensure_field_prototypes(&self) -> Result<Vec<FieldRef>, RemoteError> {
        let mut existing = self.list_field_prototypes()?;

        for (name, field_type) in ALL_FIELDS {
            if existing.iter().any(|f| f.name == name) {
                continue;
            }
            // NB: no `$type`. A subtype here is rejected with
            // "Cannot interpret value as ...CustomField / Error in field unknown".
            let body = json!({ "name": name, "fieldType": { "id": field_type } });
            match self
                .http
                .post_json(FIELD_CREATE_PATH, &body, &format!("custom field '{name}'"))
            {
                Ok(_) | Err(RemoteError::AlreadyExists { .. }) => {}
                Err(err) => return Err(err),
            }
            // Re-read rather than trusting the POST's echo: the absorbed
            // already-exists path has no body of its own to learn the id from.
            existing = self.list_field_prototypes()?;
        }

        Ok(ALL_FIELDS
            .iter()
            .filter_map(|(name, _)| existing.iter().find(|f| f.name == *name).cloned())
            .collect())
    }

    /// Every custom field prototype the instance defines, paged.
    ///
    /// An unpaginated `$top=500` read here reads its own truncation as
    /// absence: a prototype past the first page would look missing, and
    /// `ensure_field_prototypes` would try to create a duplicate that then
    /// 409s as `must-be-unique` — the same failure shape `tags::TagCache`
    /// had. This loops on `$skip`/`$top` for the same reason `fetch_all_tags`
    /// does, stopping on the first short page; it carries no sort clause for
    /// the same reason `resolve_project_by_short_name` does not — the
    /// prototype list is a small, effectively static collection for the life
    /// of one run.
    ///
    /// `pub(crate)` rather than private: `init::missing_field_prototypes` is
    /// the same read under a different return type — the names present, not
    /// the `FieldRef`s themselves — and shares this rather than carrying its
    /// own unpaginated copy.
    ///
    /// # Errors
    /// Returns `RemoteError` on transport or HTTP failure.
    pub(crate) fn list_field_prototypes(&self) -> Result<Vec<FieldRef>, RemoteError> {
        let mut fields = Vec::new();
        let mut skip = 0_u32;
        loop {
            let path = format!(
                "/api/admin/customFieldSettings/customFields?fields={FIELD_LIST_FIELDS}\
&$skip={skip}&$top={FIELD_LIST_PAGE_SIZE}"
            );
            let value = self.http.get_json(&path, "custom field list")?;
            let page = json_array(Some(&value));
            let count = u32::try_from(page.len()).unwrap_or(u32::MAX);
            fields.extend(page.iter().map(|f| {
                FieldRef {
                    id: json_str(f, "id").to_string(),
                    name: json_str(f, "name").to_string(),
                    field_type: f
                        .get("fieldType")
                        .map(|t| json_str(t, "id"))
                        .unwrap_or_default()
                        .to_string(),
                }
            }));
            if count < FIELD_LIST_PAGE_SIZE {
                return Ok(fields);
            }
            skip += FIELD_LIST_PAGE_SIZE;
        }
    }

    /// Attach every provisioned field to `project`.
    ///
    /// # Errors
    /// Returns `RemoteError` on transport or HTTP failure. An already-attached
    /// field is absorbed the same way an already-created prototype is.
    pub fn attach_fields_to_project(
        &self,
        project: &ProjectRef,
        fields: &[FieldRef],
    ) -> Result<(), RemoteError> {
        let path = format!(
            "/api/admin/projects/{}/customFields?fields=id,field(id,name)",
            project.id
        );
        for field in fields {
            // NB: `$type` is REQUIRED here — the opposite rule to the
            // prototype POST one call earlier.
            let type_name = if field.name == BEADS_ID_FIELD {
                "SimpleProjectCustomField"
            } else {
                "TextProjectCustomField"
            };
            let body = json!({
                "field": { "id": field.id },
                "canBeEmpty": true,
                "$type": type_name,
            });
            match self
                .http
                .post_json(&path, &body, &format!("attach '{}'", field.name))
            {
                Ok(_) | Err(RemoteError::AlreadyExists { .. }) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::http::{HttpClient, RetryPolicy, Token};
    use test_support::mock_http::MockServer;
    use test_support::youtrack_fixtures::MUST_BE_UNIQUE;

    const FIELDS_PATH: &str =
        "/api/admin/customFieldSettings/customFields?fields=id,name,fieldType(id)&$skip=0&$top=500";

    fn fields_page_path(skip: u32) -> String {
        format!(
            "/api/admin/customFieldSettings/customFields?fields=id,name,fieldType(id)&$skip={skip}&$top=500"
        )
    }

    fn client(server: &MockServer) -> AdminClient {
        AdminClient::new(HttpClient::new(
            &server.base_url(),
            Token::new("t"),
            RetryPolicy::none(),
        ))
    }

    #[test]
    fn a_bare_instance_gets_five_prototypes_and_five_attaches() {
        let server = MockServer::start();
        server.on("GET", FIELDS_PATH, 200, "[]");
        server.on(
            "POST",
            "/api/admin/customFieldSettings/customFields?fields=id,name,fieldType(id)",
            200,
            r#"{"id":"161-11","name":"x","fieldType":{"id":"text"}}"#,
        );
        server.on(
            "POST",
            "/api/admin/projects/0-1/customFields?fields=id,field(id,name)",
            200,
            r#"{"id":"233-0"}"#,
        );

        let admin = client(&server);
        let project = ProjectRef {
            id: "0-1".into(),
            short_name: "EM".into(),
            name: "EasyMoney".into(),
        };
        let fields = admin.ensure_field_prototypes().expect("prototypes");
        // The list GET always answers `[]`, so nothing is ever learned back:
        // this test is about the requests issued, not about what they return.
        let attachable = if fields.is_empty() {
            ALL_FIELDS
                .iter()
                .enumerate()
                .map(|(i, (name, field_type))| FieldRef {
                    id: format!("161-{i}"),
                    name: (*name).to_string(),
                    field_type: (*field_type).to_string(),
                })
                .collect()
        } else {
            fields
        };
        admin
            .attach_fields_to_project(&project, &attachable)
            .expect("attach");

        let posts = server.write_requests();
        let prototypes: Vec<_> = posts
            .iter()
            .filter(|r| {
                r.path
                    .starts_with("/api/admin/customFieldSettings/customFields")
            })
            .collect();
        assert_eq!(prototypes.len(), 5, "one POST per missing prototype");
        for request in &prototypes {
            assert!(
                !request.body.contains("$type"),
                "the prototype POST must omit $type; a subtype is rejected outright: {}",
                request.body
            );
        }

        let attaches: Vec<_> = posts
            .iter()
            .filter(|r| r.path.contains("/projects/0-1/customFields"))
            .collect();
        assert_eq!(attaches.len(), 5);
        assert_eq!(
            attaches
                .iter()
                .filter(|r| r.body.contains("TextProjectCustomField"))
                .count(),
            4,
            "the four prose fields attach as text"
        );
        assert_eq!(
            attaches
                .iter()
                .filter(|r| r.body.contains("SimpleProjectCustomField"))
                .count(),
            1,
            "Beads ID attaches as a simple string field"
        );
        for request in &attaches {
            assert!(
                request.body.contains(r#""canBeEmpty":true"#),
                "{}",
                request.body
            );
        }
    }

    #[test]
    fn an_instance_that_already_has_every_prototype_writes_nothing() {
        let server = MockServer::start();
        let existing = serde_json::json!([
            {"id":"161-11","name":"Beads ID","fieldType":{"id":"string"}},
            {"id":"161-12","name":"Design","fieldType":{"id":"text"}},
            {"id":"161-13","name":"Acceptance Criteria","fieldType":{"id":"text"}},
            {"id":"161-14","name":"Notes","fieldType":{"id":"text"}},
            {"id":"161-15","name":"Close Reason","fieldType":{"id":"text"}},
        ]);
        server.on("GET", FIELDS_PATH, 200, &existing.to_string());

        let admin = client(&server);
        let fields = admin.ensure_field_prototypes().expect("prototypes");

        assert_eq!(fields.len(), 5);
        assert!(
            server.write_requests().is_empty(),
            "nothing missing means zero writes, got {:?}",
            server.write_requests()
        );
    }

    #[test]
    fn the_prose_fields_are_provisioned_as_text_not_string() {
        // `string` is single-line: a multi-paragraph `design` written into one
        // would be flattened on arrival, which no later step could recover.
        for name in PROSE_FIELDS {
            let entry = ALL_FIELDS
                .iter()
                .find(|(field, _)| *field == name)
                .expect("every prose field is provisioned");
            assert_eq!(entry.1, "text", "{name} must be a text field");
        }
        assert_eq!(
            ALL_FIELDS
                .iter()
                .find(|(field, _)| *field == BEADS_ID_FIELD)
                .expect("Beads ID is provisioned")
                .1,
            "string"
        );
    }

    #[test]
    fn must_be_unique_is_absorbed_as_already_present() {
        let server = MockServer::start();
        server.on_sequence(
            "POST",
            "/api/admin/customFieldSettings/customFields?fields=id,name,fieldType(id)",
            vec![
                (409, MUST_BE_UNIQUE.to_string()),
                (
                    200,
                    r#"{"id":"161-12","name":"Design","fieldType":{"id":"text"}}"#.into(),
                ),
            ],
        );
        // The second GET re-reads the list so the racing admin's field is picked up.
        server.on_sequence(
            "GET",
            FIELDS_PATH,
            vec![
                (200, "[]".into()),
                (
                    200,
                    r#"[{"id":"161-11","name":"Beads ID","fieldType":{"id":"string"}}]"#.into(),
                ),
            ],
        );

        let admin = client(&server);
        let result = admin.ensure_field_prototypes();
        assert!(
            result.is_ok(),
            "a racing admin must not fail the run: {result:?}"
        );
    }

    #[test]
    fn field_prototype_listing_pages_past_the_first_five_hundred() {
        // The original bug: `list_field_prototypes` asked for `$top=500` with
        // no `$skip`, so a prototype past the first page was invisible and
        // `ensure_field_prototypes` would try to create a duplicate that then
        // 409s forever. This pins that the listing itself now pages.
        let server = MockServer::start();
        let first_page: Vec<_> = (0..500)
            .map(|n| {
                serde_json::json!({"id": format!("161-{n}"), "name": format!("Field {n}"),
                    "fieldType": {"id": "string"}})
            })
            .collect();
        server.on(
            "GET",
            &fields_page_path(0),
            200,
            &serde_json::Value::Array(first_page).to_string(),
        );
        server.on(
            "GET",
            &fields_page_path(500),
            200,
            r#"[{"id":"161-11","name":"Beads ID","fieldType":{"id":"string"}},
                {"id":"161-12","name":"Design","fieldType":{"id":"text"}},
                {"id":"161-13","name":"Acceptance Criteria","fieldType":{"id":"text"}},
                {"id":"161-14","name":"Notes","fieldType":{"id":"text"}},
                {"id":"161-15","name":"Close Reason","fieldType":{"id":"text"}}]"#,
        );

        let admin = client(&server);
        let fields = admin.ensure_field_prototypes().expect("prototypes");

        assert_eq!(
            fields.len(),
            5,
            "every provisioned prototype must be visible, even on the second page"
        );
        assert!(
            server.write_requests().is_empty(),
            "a prototype the paginated listing actually found must not be re-created: {:?}",
            server.write_requests()
        );
    }

    #[test]
    fn a_project_is_resolved_by_short_name_and_a_missing_one_is_named() {
        let server = MockServer::start();
        server.on(
            "GET",
            "/api/admin/projects?fields=id,name,shortName&$skip=0&$top=500",
            200,
            r#"[{"id":"0-0","name":"Sandbox","shortName":"SB"},
                {"id":"0-1","name":"EasyMoney","shortName":"EM"}]"#,
        );

        let admin = client(&server);
        let project = admin.resolve_project("EM").expect("resolve");
        assert_eq!(
            project,
            ProjectRef {
                id: "0-1".into(),
                short_name: "EM".into(),
                name: "EasyMoney".into(),
            }
        );

        let err = admin.resolve_project("NOPE").expect_err("must fail");
        assert!(err.to_string().contains("NOPE"), "{err}");
    }

    #[test]
    fn resolve_project_pages_past_the_first_five_hundred() {
        // The original bug: `resolve_project` asked for `$top=500` with no
        // `$skip`, so a project past the first page could never be resolved
        // no matter how it was spelled. This pins that it now pages, and that
        // the first page's URL carries an explicit `&$skip=0` — the
        // convention every paged admin read in this subsystem uses, so a
        // paginated and an unpaginated read of the same resource are never
        // silently indistinguishable on the wire.
        let server = MockServer::start();
        let first_page: Vec<_> = (0..500)
            .map(|n| {
                serde_json::json!({"id": format!("0-{n}"), "name": format!("Project {n}"),
                    "shortName": format!("P{n}")})
            })
            .collect();
        server.on(
            "GET",
            "/api/admin/projects?fields=id,name,shortName&$skip=0&$top=500",
            200,
            &serde_json::Value::Array(first_page).to_string(),
        );
        server.on(
            "GET",
            "/api/admin/projects?fields=id,name,shortName&$skip=500&$top=500",
            200,
            r#"[{"id":"0-999","name":"Arrived Late","shortName":"LATE"}]"#,
        );

        let admin = client(&server);
        let project = admin
            .resolve_project("LATE")
            .expect("a project beyond the first page must still resolve");
        assert_eq!(project.id, "0-999");
    }
}
