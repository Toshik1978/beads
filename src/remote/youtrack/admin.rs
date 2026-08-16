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

const FIELD_LIST_PATH: &str =
    "/api/admin/customFieldSettings/customFields?fields=id,name,fieldType(id)&$top=500";
const FIELD_CREATE_PATH: &str =
    "/api/admin/customFieldSettings/customFields?fields=id,name,fieldType(id)";

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

    /// Find the project by its short name.
    ///
    /// # Errors
    /// Returns `RemoteError::Config` when no project carries that short name.
    pub fn resolve_project(&self, short_name: &str) -> Result<ProjectRef, RemoteError> {
        let value = self.http.get_json(
            "/api/admin/projects?fields=id,name,shortName&$top=500",
            "project list",
        )?;
        json_array(Some(&value))
            .iter()
            .find(|p| json_str(p, "shortName") == short_name)
            .map(|p| ProjectRef {
                id: json_str(p, "id").to_string(),
                short_name: short_name.to_string(),
                name: json_str(p, "name").to_string(),
            })
            .ok_or_else(|| {
                RemoteError::Config(format!(
                    "no project with short name '{short_name}' is visible to this token"
                ))
            })
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

    fn list_field_prototypes(&self) -> Result<Vec<FieldRef>, RemoteError> {
        let value = self.http.get_json(FIELD_LIST_PATH, "custom field list")?;
        Ok(json_array(Some(&value))
            .iter()
            .map(|f| FieldRef {
                id: json_str(f, "id").to_string(),
                name: json_str(f, "name").to_string(),
                field_type: f
                    .get("fieldType")
                    .map(|t| json_str(t, "id"))
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect())
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
        "/api/admin/customFieldSettings/customFields?fields=id,name,fieldType(id)&$top=500";

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
    fn a_project_is_resolved_by_short_name_and_a_missing_one_is_named() {
        let server = MockServer::start();
        server.on(
            "GET",
            "/api/admin/projects?fields=id,name,shortName&$top=500",
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
}
