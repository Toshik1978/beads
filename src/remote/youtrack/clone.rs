//! Cloning a shared bundle, and the empty-project predicate that gates it.
//!
//! Two facts here were established against a live instance and both contradict
//! what the REST documentation implies.
//!
//! `defaultValues` cannot be set by **name**. The verified response is *"YouTrack
//! is unable to locate an `EnumBundleElement`-type entity unless its ID is also
//! provided"*. This is the one place in the whole design that needs a database
//! id where a name would seem natural — issue-level custom fields accept plain
//! names perfectly well — so the temptation to reuse that shape is real and it
//! does not work.
//!
//! `POST /api/issuesGetter/count` returns `{"count": -1}` while it is still
//! computing, so the emptiness predicate polls. If the budget runs out the
//! answer is `Unknown`, never `Empty`: cloning a project that turns out to hold
//! issues is precisely the failure this predicate exists to prevent.

use crate::remote::error::RemoteError;
use crate::remote::youtrack::admin::{AdminClient, ProjectRef, json_str};
use crate::remote::youtrack::bundles::{BundleKind, BundleRef, BundleValue, ProjectFieldBundle};
use serde_json::json;
use std::time::Duration;

/// How many times to re-ask `issuesGetter/count` before giving up.
const COUNT_POLL_ATTEMPTS: u32 = 6;

/// The default base of the poll backoff. Overridable per client through
/// [`AdminClient::with_count_poll_delay`], which is how the tests that only
/// care about the *verdict* avoid paying for six real sleeps.
pub(crate) const COUNT_POLL_BASE_DELAY: Duration = Duration::from_millis(200);

/// Page size for [`AdminClient::list_bundles`].
const LIST_BUNDLES_PAGE_SIZE: u32 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emptiness {
    Empty,
    HasIssues(u64),
    /// The count never settled. Callers must refuse, not assume.
    Unknown,
}

impl AdminClient {
    /// Whether `project` holds any issue.
    ///
    /// `POST /api/issuesGetter/count` returns `{"count": -1}` while the count
    /// is still being computed, so this polls with bounded backoff. There is
    /// no cheaper synchronous alternative — `Project` exposes no
    /// `issuesCount` field.
    ///
    /// **This is a read that happens to be a POST**, and it is the one request
    /// `br remote init --dry-run` still issues. YouTrack puts the query in a
    /// body, so the count has no GET form; nothing about it changes remote
    /// state. A dry run needs the answer because the shared-bundle branch it
    /// is reporting on is chosen by it.
    ///
    /// # Errors
    /// Returns `RemoteError` on transport or HTTP failure. A count that never
    /// settles is `Ok(Emptiness::Unknown)`, not an error, so the caller can
    /// report it precisely.
    pub fn project_is_empty(&self, project: &ProjectRef) -> Result<Emptiness, RemoteError> {
        let body = json!({ "query": format!("project: {}", project.short_name) });
        for attempt in 0..COUNT_POLL_ATTEMPTS {
            let value = self.http().post_json(
                "/api/issuesGetter/count?fields=count",
                &body,
                "issue count",
            )?;
            match value.get("count").and_then(serde_json::Value::as_i64) {
                Some(0) => return Ok(Emptiness::Empty),
                Some(n) if n > 0 => return Ok(Emptiness::HasIssues(n.unsigned_abs())),
                // -1 means "still computing"; anything else means "no idea".
                _ => {}
            }
            let delay = self.count_poll_base_delay() * 2_u32.saturating_pow(attempt);
            if attempt + 1 < COUNT_POLL_ATTEMPTS && !delay.is_zero() {
                std::thread::sleep(delay);
            }
        }
        Ok(Emptiness::Unknown)
    }

    /// Create a verbatim copy of `source` named `new_name`.
    ///
    /// One request carrying every value inline, not one request per value.
    ///
    /// # Errors
    /// Returns `RemoteError` on transport or HTTP failure.
    pub fn clone_bundle(
        &self,
        source: &BundleRef,
        new_name: &str,
    ) -> Result<BundleRef, RemoteError> {
        let values: Vec<serde_json::Value> = source
            .values
            .iter()
            .map(|v| value_body(v, source.kind))
            .collect();
        let body = json!({ "name": new_name, "values": values });
        let path = format!(
            "/api/admin/customFieldSettings/bundles/{}?fields=id,name",
            source.kind.segment()
        );
        let created = self
            .http()
            .post_json(&path, &body, &format!("bundle '{new_name}'"))?;
        Ok(BundleRef {
            id: json_str(&created, "id").to_string(),
            name: new_name.to_string(),
            kind: source.kind,
            // The clone's own element ids differ from the source's; callers
            // that need them (set_default_values) re-read the bundle.
            values: Vec::new(),
        })
    }

    /// Every bundle of `kind` on the instance, with its values by name, paged.
    ///
    /// Used by `init` to notice a bundle it created on an earlier run that
    /// died before it could re-point the project at it. YouTrack permits
    /// duplicate bundle names, so without this read a re-run would create a
    /// second identically-named copy and the first would be orphaned for
    /// good — and an unpaginated `$top=500` read has exactly that failure
    /// mode: a clone living past the first page reads as absent, so the
    /// orphan leak this read exists to close would come right back on any
    /// instance with more than 500 bundles of one kind.
    ///
    /// There is no by-name query to switch to instead — the YouTrack REST API
    /// documents only `fields`/`$top`/`$skip` for this resource, unlike
    /// `/api/tags` or `/api/admin/projects` — so this loops on `$skip`/`$top`
    /// the same way `fetch::fetch_snapshot` does, stopping on the first short
    /// page; it carries no sort clause because the instance's bundles are a
    /// small, effectively static collection for the life of one run.
    ///
    /// # Errors
    /// Returns `RemoteError` on transport or HTTP failure.
    pub fn list_bundles(&self, kind: BundleKind) -> Result<Vec<BundleRef>, RemoteError> {
        let mut bundles = Vec::new();
        let mut skip = 0_u32;
        loop {
            let path = format!(
                "/api/admin/customFieldSettings/bundles/{}?fields=id,name,values(id,name)&$skip={skip}&$top={LIST_BUNDLES_PAGE_SIZE}",
                kind.segment()
            );
            let value = self.http().get_json(&path, "bundle list")?;
            let page = crate::remote::youtrack::admin::json_array(Some(&value));
            let count = u32::try_from(page.len()).unwrap_or(u32::MAX);
            bundles.extend(page.iter().map(|raw| BundleRef {
                id: json_str(raw, "id").to_string(),
                name: json_str(raw, "name").to_string(),
                kind,
                values: crate::remote::youtrack::bundles::parse_bundle_values(raw.get("values")),
            }));
            if count < LIST_BUNDLES_PAGE_SIZE {
                return Ok(bundles);
            }
            skip += LIST_BUNDLES_PAGE_SIZE;
        }
    }

    /// Re-read a bundle's values, so a freshly cloned bundle's own element ids
    /// are known.
    ///
    /// # Errors
    /// Returns `RemoteError` on transport or HTTP failure.
    pub fn read_bundle(&self, bundle: &BundleRef) -> Result<BundleRef, RemoteError> {
        let path = format!(
            "/api/admin/customFieldSettings/bundles/{}/{}\
?fields=id,name,values(id,name,ordinal,color(id,background,foreground),isResolved)",
            bundle.kind.segment(),
            bundle.id
        );
        let value = self.http().get_json(&path, "bundle values")?;
        Ok(BundleRef {
            id: json_str(&value, "id").to_string(),
            name: json_str(&value, "name").to_string(),
            kind: bundle.kind,
            values: crate::remote::youtrack::bundles::parse_bundle_values(value.get("values")),
        })
    }

    /// Point `field`'s project instance at `bundle`.
    ///
    /// The nested `bundle` object carries its own `$type` as well as the
    /// outer one; see [`BundleKind::bundle_type`] for why omitting it answers
    /// HTTP 500 rather than a validation error.
    ///
    /// # Errors
    /// Returns `RemoteError` on transport or HTTP failure.
    pub fn repoint_field_bundle(
        &self,
        field: &ProjectFieldBundle,
        project: &ProjectRef,
        bundle: &BundleRef,
    ) -> Result<(), RemoteError> {
        let type_name = project_field_type(bundle.kind);
        let path = format!(
            "/api/admin/projects/{}/customFields/{}?fields=id,bundle(id,name)",
            project.id, field.instance_id
        );
        let body = json!({
            "bundle": { "id": bundle.id, "$type": bundle.kind.bundle_type() },
            "$type": type_name,
        });
        self.http()
            .post_json(&path, &body, &format!("re-point '{}'", field.field_name))
            .map(|_| ())
    }

    /// Set `instance_id`'s defaults to the named values.
    ///
    /// Names are resolved to element database ids against `bundle` first: the
    /// API rejects a bare name with *"YouTrack is unable to locate an
    /// `EnumBundleElement`-type entity unless its ID is also provided"*.
    ///
    /// # Errors
    /// Returns `RemoteError::Config` when a name is not in the bundle — before
    /// issuing any request, since the request is known to fail. Returns other
    /// `RemoteError`s on transport or HTTP failure.
    pub fn set_default_values(
        &self,
        project: &ProjectRef,
        instance_id: &str,
        field_type: &str,
        bundle: &BundleRef,
        names: &[&str],
    ) -> Result<(), RemoteError> {
        let mut defaults = Vec::with_capacity(names.len());
        for name in names {
            let value = bundle
                .values
                .iter()
                .find(|v| v.name == *name)
                .ok_or_else(|| {
                    RemoteError::Config(format!(
                        "cannot set the default to '{name}': the bundle '{}' has no such value",
                        bundle.name
                    ))
                })?;
            // Id only. A `name` key here is what the API refuses.
            defaults.push(json!({ "id": value.id, "$type": bundle.kind.element_type() }));
        }
        let path = format!(
            "/api/admin/projects/{}/customFields/{instance_id}?fields=field(name),defaultValues(id,name)",
            project.id
        );
        let body = json!({ "defaultValues": defaults, "$type": field_type });
        self.http()
            .post_json(&path, &body, "project default values")
            .map(|_| ())
    }
}

/// The `$type` a project's field instance carries for a bundle of this kind.
#[must_use]
pub fn project_field_type(kind: BundleKind) -> &'static str {
    match kind {
        BundleKind::Enum => "EnumProjectCustomField",
        BundleKind::State => "StateProjectCustomField",
    }
}

fn value_body(value: &BundleValue, kind: BundleKind) -> serde_json::Value {
    let mut body = json!({
        "name": value.name,
        "ordinal": value.ordinal,
        "$type": kind.element_type(),
    });
    if !value.color.is_null() {
        body["color"] = value.color.clone();
    }
    if let Some(resolved) = value.is_resolved {
        body["isResolved"] = json!(resolved);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::http::{HttpClient, RetryPolicy, Token};
    use crate::remote::youtrack::admin::{AdminClient, ProjectRef};
    use crate::remote::youtrack::bundles::{BundleKind, BundleRef, BundleValue};
    use test_support::mock_http::MockServer;
    use test_support::youtrack_fixtures::{COUNT_PENDING, COUNT_SETTLED, DEFAULT_VALUES_BY_NAME};

    /// The backoff is set to zero: every test here asserts on the *verdict* or
    /// the request sequence, and paying six real sleeps to reach the same
    /// answer costs 6.2 seconds and proves nothing extra. The default is
    /// exercised by `the_default_poll_delay_is_the_documented_one`.
    fn admin(server: &MockServer) -> AdminClient {
        AdminClient::new(HttpClient::new(
            &server.base_url(),
            Token::new("t"),
            RetryPolicy::none(),
        ))
        .with_count_poll_delay(Duration::ZERO)
    }

    fn project() -> ProjectRef {
        ProjectRef {
            id: "0-1".into(),
            short_name: "EM".into(),
            name: "EasyMoney".into(),
        }
    }

    fn state_bundle() -> BundleRef {
        BundleRef {
            id: "165-0".into(),
            name: "States".into(),
            kind: BundleKind::State,
            values: vec![
                BundleValue {
                    id: "166-0".into(),
                    name: "Submitted".into(),
                    ordinal: 0,
                    color: serde_json::json!({"id":"0","background":"#fff","foreground":"#444"}),
                    is_resolved: Some(false),
                },
                BundleValue {
                    id: "166-1".into(),
                    name: "Open".into(),
                    ordinal: 1,
                    color: serde_json::json!({"id":"0","background":"#fff","foreground":"#444"}),
                    is_resolved: Some(false),
                },
            ],
        }
    }

    #[test]
    fn list_bundles_pages_past_the_first_five_hundred() {
        // The original bug: `list_bundles` asked for `$top=500` with no
        // `$skip`, so a bundle living past the first page read as absent —
        // and since `init` uses this read specifically to notice a bundle it
        // already cloned on an earlier, interrupted run, that meant the
        // orphan-leak bug it exists to close would come back on any instance
        // with more than 500 bundles of one kind. This pins that the listing
        // itself now pages, and that the first page's URL carries an
        // explicit `&$skip=0`.
        let server = MockServer::start();
        let first_page: Vec<_> = (0..500)
            .map(|n| serde_json::json!({"id": format!("165-{n}"), "name": format!("Bundle {n}")}))
            .collect();
        server.on(
            "GET",
            "/api/admin/customFieldSettings/bundles/state?fields=id,name,values(id,name)&$skip=0&$top=500",
            200,
            &serde_json::Value::Array(first_page).to_string(),
        );
        server.on(
            "GET",
            "/api/admin/customFieldSettings/bundles/state?fields=id,name,values(id,name)&$skip=500&$top=500",
            200,
            r#"[{"id":"165-999","name":"EasyMoney: States","values":[]}]"#,
        );

        let bundles = admin(&server)
            .list_bundles(BundleKind::State)
            .expect("list must page past the boundary");
        assert!(
            bundles.iter().any(|b| b.id == "165-999"),
            "a bundle beyond the first page must be visible: {bundles:?}"
        );
    }

    #[test]
    fn the_clone_carries_every_value_inline_in_one_request() {
        let server = MockServer::start();
        server.on(
            "POST",
            "/api/admin/customFieldSettings/bundles/state?fields=id,name",
            200,
            r#"{"id":"165-9","name":"EasyMoney: States"}"#,
        );

        let clone = admin(&server)
            .clone_bundle(&state_bundle(), "EasyMoney: States")
            .expect("clone");
        assert_eq!(clone.id, "165-9");

        let posts = server.write_requests();
        assert_eq!(posts.len(), 1, "one request, not one per value");
        let body: serde_json::Value = serde_json::from_str(&posts[0].body).expect("json body");
        let values = body["values"].as_array().expect("values array");
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["name"], "Submitted");
        assert_eq!(values[0]["ordinal"], 0);
        assert_eq!(
            values[0]["isResolved"], false,
            "states must carry isResolved verbatim"
        );
        assert!(
            values[0]["color"].is_object(),
            "colour must be carried verbatim"
        );
    }

    #[test]
    fn the_repoint_sets_the_bundle_on_the_project_field_instance() {
        let server = MockServer::start();
        server.on(
            "POST",
            "/api/admin/projects/0-1/customFields/189-9?fields=id,bundle(id,name)",
            200,
            r#"{"id":"189-9","bundle":{"id":"165-9","name":"EasyMoney: States"}}"#,
        );

        let field = ProjectFieldBundle {
            instance_id: "189-9".into(),
            field_id: "161-2".into(),
            field_name: "State".into(),
            bundle: state_bundle(),
        };
        let clone = BundleRef {
            id: "165-9".into(),
            name: "EasyMoney: States".into(),
            kind: BundleKind::State,
            values: Vec::new(),
        };
        admin(&server)
            .repoint_field_bundle(&field, &project(), &clone)
            .expect("repoint");

        let posts = server.write_requests();
        assert_eq!(posts.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&posts[0].body).expect("json body");
        assert_eq!(body["bundle"]["id"], "165-9");
        assert_eq!(body["$type"], "StateProjectCustomField");
        // The nested `$type` is not decoration: without it this POST answers
        // HTTP 500 `java.lang.InstantiationException` against a real server.
        assert_eq!(body["bundle"]["$type"], "StateBundle");
    }

    #[test]
    fn an_enum_repoint_names_the_enum_bundle_type_inside_the_nested_object() {
        let server = MockServer::start();
        server.on(
            "POST",
            "/api/admin/projects/0-1/customFields/189-15?fields=id,bundle(id,name)",
            200,
            r#"{"id":"189-15","bundle":{"id":"163-2","name":"Beads: Types"}}"#,
        );

        let field = ProjectFieldBundle {
            instance_id: "189-15".into(),
            field_id: "161-1".into(),
            field_name: "Type".into(),
            bundle: BundleRef {
                id: "163-1".into(),
                name: "Types".into(),
                kind: BundleKind::Enum,
                values: Vec::new(),
            },
        };
        let clone = BundleRef {
            id: "163-2".into(),
            name: "Beads: Types".into(),
            kind: BundleKind::Enum,
            values: Vec::new(),
        };
        admin(&server)
            .repoint_field_bundle(&field, &project(), &clone)
            .expect("repoint");

        let posts = server.write_requests();
        assert_eq!(posts.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&posts[0].body).expect("json body");
        assert_eq!(body["$type"], "EnumProjectCustomField");
        assert_eq!(body["bundle"]["$type"], "EnumBundle");
    }

    #[test]
    fn default_values_are_emitted_by_id_never_by_name() {
        let server = MockServer::start();
        server.on(
            "POST",
            "/api/admin/projects/0-1/customFields/189-9?fields=field(name),defaultValues(id,name)",
            200,
            r#"{"defaultValues":[{"id":"166-1","name":"Open"}]}"#,
        );

        admin(&server)
            .set_default_values(
                &project(),
                "189-9",
                "StateProjectCustomField",
                &state_bundle(),
                &["Open"],
            )
            .expect("set defaults");

        let posts = server.write_requests();
        assert_eq!(posts.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&posts[0].body).expect("json body");
        let defaults = body["defaultValues"]
            .as_array()
            .expect("defaultValues array");
        assert_eq!(
            defaults[0]["id"], "166-1",
            "must resolve Open to its element id"
        );
        assert!(
            defaults[0].get("name").is_none(),
            "a bare name is rejected by the API: {}",
            posts[0].body
        );
    }

    #[test]
    fn an_unknown_default_name_fails_before_the_request() {
        let server = MockServer::start();
        let err = admin(&server)
            .set_default_values(
                &project(),
                "189-9",
                "StateProjectCustomField",
                &state_bundle(),
                &["Nope"],
            )
            .expect_err("must fail");
        assert!(err.to_string().contains("Nope"), "{err}");
        assert!(
            server.write_requests().is_empty(),
            "must not issue a request it knows will fail"
        );
    }

    #[test]
    fn the_by_name_rejection_is_recognisable() {
        // Guards the fixture: if this body ever stops being what the API
        // returns, the branch above is no longer justified.
        assert!(DEFAULT_VALUES_BY_NAME.contains("unless its ID is also provided"));
    }

    #[test]
    fn a_cloned_bundle_is_re_read_for_its_own_element_ids() {
        let server = MockServer::start();
        server.on(
            "GET",
            "/api/admin/customFieldSettings/bundles/state/165-9?fields=id,name,values(id,name,ordinal,color(id,background,foreground),isResolved)",
            200,
            r#"{"id":"165-9","name":"EasyMoney: States","values":[
                {"id":"166-40","name":"Submitted","ordinal":0,"isResolved":false},
                {"id":"166-41","name":"Open","ordinal":1,"isResolved":false}]}"#,
        );

        let stub = BundleRef {
            id: "165-9".into(),
            name: "EasyMoney: States".into(),
            kind: BundleKind::State,
            values: Vec::new(),
        };
        let full = admin(&server).read_bundle(&stub).expect("read back");
        let ids: Vec<&str> = full.values.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(
            ids,
            ["166-40", "166-41"],
            "the clone's element ids are its own, not the source's"
        );
    }

    #[test]
    fn the_count_endpoint_is_polled_past_its_pending_value() {
        let server = MockServer::start();
        server.on_sequence(
            "POST",
            "/api/issuesGetter/count?fields=count",
            vec![
                (200, COUNT_PENDING.into()),
                (200, COUNT_PENDING.into()),
                (200, COUNT_SETTLED.into()),
            ],
        );

        let result = admin(&server).project_is_empty(&project()).expect("count");
        assert_eq!(result, Emptiness::Empty);
        assert_eq!(
            server.write_requests().len(),
            3,
            "must poll until it settles"
        );
    }

    #[test]
    fn a_positive_count_reports_how_many_issues_block_the_clone() {
        let server = MockServer::start();
        server.on(
            "POST",
            "/api/issuesGetter/count?fields=count",
            200,
            r#"{"count":42,"$type":"IssueCountResponse"}"#,
        );

        let result = admin(&server).project_is_empty(&project()).expect("count");
        assert_eq!(result, Emptiness::HasIssues(42));
    }

    #[test]
    fn a_count_that_never_settles_is_unknown_not_empty() {
        let server = MockServer::start();
        server.on(
            "POST",
            "/api/issuesGetter/count?fields=count",
            200,
            COUNT_PENDING,
        );

        let result = admin(&server).project_is_empty(&project()).expect("count");
        assert_eq!(
            result,
            Emptiness::Unknown,
            "exhausting the budget must never be reported as Empty — that would clone"
        );
        assert_eq!(
            server.write_requests().len(),
            COUNT_POLL_ATTEMPTS as usize,
            "the whole budget must be spent before giving up"
        );
    }

    #[test]
    fn the_default_poll_delay_is_the_documented_one() {
        // The tests above inject zero so they cost nothing; this pins the value
        // a real run actually backs off from, which is what makes that
        // injection safe rather than a way of testing a different program.
        let server = MockServer::start();
        let client = AdminClient::new(HttpClient::new(
            &server.base_url(),
            Token::new("t"),
            RetryPolicy::none(),
        ));
        assert_eq!(client.count_poll_base_delay(), Duration::from_millis(200));
    }
}
