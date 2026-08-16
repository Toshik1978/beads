//! Reconciling the configured maps against a project's bundles.
//!
//! Comparison is by exact name, because YouTrack bundle values are
//! case-sensitive. That is right and it has one sharp edge: a map naming
//! `epic` against a bundle already holding `Epic` would report `epic` missing
//! and create a near-identical twin. Any case-insensitive match that is not
//! also exact is therefore refused, naming both spellings, so the author fixes
//! the map instead of polluting the bundle.
//!
//! `isResolved` is the other thing worth getting right first time: setting it
//! wrong fails *silently*. YouTrack simply never treats closed issues as
//! resolved, and every board and saved search quietly disagrees with beads.

use crate::remote::config::RemoteConfig;
use crate::remote::error::RemoteError;
use crate::remote::youtrack::admin::{AdminClient, ProjectRef, json_array};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleKind {
    Enum,
    State,
}

impl BundleKind {
    /// The URL segment: `/api/admin/customFieldSettings/bundles/{segment}`.
    #[must_use]
    pub fn segment(self) -> &'static str {
        match self {
            Self::Enum => "enum",
            Self::State => "state",
        }
    }

    /// The `$type` a value of this bundle carries.
    #[must_use]
    pub fn element_type(self) -> &'static str {
        match self {
            Self::Enum => "EnumBundleElement",
            Self::State => "StateBundleElement",
        }
    }

    /// The `$type` the bundle itself carries.
    ///
    /// Required on any *nested* bundle reference. `POST
    /// /api/admin/projects/{id}/customFields/{instance}` with a body of
    /// `{"bundle":{"id":"163-2"},"$type":"EnumProjectCustomField"}` answers
    /// **HTTP 500 `java.lang.InstantiationException`**: `Bundle` is abstract,
    /// and YouTrack will not infer the concrete subclass from the id — even
    /// though the id is globally unique and the outer `$type` already implies
    /// exactly one answer. Adding `"$type":"EnumBundle"` inside the nested
    /// object is the whole difference between a 500 and a 200.
    ///
    /// The endpoint is only reached on the clone-and-re-point path, which
    /// needs a bundle shared by two projects to trigger — so it stayed
    /// unexercised against a real server until an instance had a second
    /// project in it.
    #[must_use]
    pub fn bundle_type(self) -> &'static str {
        match self {
            Self::Enum => "EnumBundle",
            Self::State => "StateBundle",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BundleValue {
    pub id: String,
    pub name: String,
    pub ordinal: i64,
    pub color: serde_json::Value,
    pub is_resolved: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct BundleRef {
    pub id: String,
    pub name: String,
    pub kind: BundleKind,
    pub values: Vec<BundleValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingValue {
    pub name: String,
    /// `Some` for a state bundle, `None` for an enum bundle.
    pub is_resolved: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ProjectFieldBundle {
    pub instance_id: String,
    pub field_id: String,
    pub field_name: String,
    pub bundle: BundleRef,
}

/// The values `field_name`'s map needs that `bundle` does not have.
///
/// Comparison is by exact name, because YouTrack values are case-sensitive.
/// A case-insensitive match that is not also exact is refused rather than
/// treated as missing: creating `epic` beside an existing `Epic` would leave
/// the project with two near-identical values and no way to tell which one a
/// board meant.
///
/// # Errors
/// Returns `RemoteError::Config` on a case-only collision, and for a field
/// name no configured map covers.
pub fn plan_missing(
    cfg: &RemoteConfig,
    field_name: &str,
    bundle: &BundleRef,
) -> Result<Vec<PendingValue>, RemoteError> {
    let wanted: Vec<(&str, &str)> = match field_name {
        "Type" => cfg
            .type_map
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect(),
        "Priority" => cfg
            .priority_map
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect(),
        "State" => {
            let mut pairs: Vec<(&str, &str)> = cfg
                .status_map
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            pairs.push((DELETED_KEY, cfg.deleted_state.as_str()));
            pairs
        }
        other => {
            return Err(RemoteError::Config(format!(
                "no map is configured for field '{other}'"
            )));
        }
    };

    let mut missing = Vec::new();
    for (beads_key, remote_value) in wanted {
        if bundle.values.iter().any(|v| v.name == remote_value) {
            continue;
        }
        if let Some(existing) = bundle
            .values
            .iter()
            .find(|v| v.name.eq_ignore_ascii_case(remote_value))
        {
            return Err(RemoteError::Config(format!(
                "the map for '{field_name}' names '{remote_value}' but the bundle '{}' holds \
                 '{}' — the same value in a different case. Adding '{remote_value}' would \
                 create a near-identical twin, so this is refused: change the map to '{}' \
                 or rename the bundle value.",
                bundle.name, existing.name, existing.name
            )));
        }
        // Deduplicate: `deleted_state` may equal a status_map value.
        if missing
            .iter()
            .any(|p: &PendingValue| p.name == remote_value)
        {
            continue;
        }
        let is_resolved = (bundle.kind == BundleKind::State)
            .then(|| beads_key == "closed" || beads_key == DELETED_KEY);
        missing.push(PendingValue {
            name: remote_value.to_string(),
            is_resolved,
        });
    }
    Ok(missing)
}

/// The synthetic map key standing for `deleted_state`, which is a scalar in
/// the config rather than a `status_map` entry. It cannot collide with a real
/// beads status: `br` statuses are bare identifiers.
const DELETED_KEY: &str = "__deleted__";

const PROJECT_FIELDS_PATH_FIELDS: &str = "id,$type,canBeEmpty,field(id,name),\
bundle(id,name,$type,values(id,name,ordinal,color(id,background,foreground),isResolved))";

/// Page size for [`AdminClient::read_project_bundle_fields`].
const PROJECT_FIELDS_PAGE_SIZE: u32 = 100;

impl AdminClient {
    /// Read the project's bundle-backed fields and their bundles, paged.
    ///
    /// A project with more than `PROJECT_FIELDS_PAGE_SIZE` custom fields
    /// attached is unusual but not impossible — `br` itself adds five on
    /// `init` — and an unpaginated read has the same failure shape every
    /// other site in this module was fixed for: a field instance past the
    /// first page reads as though the project simply does not have that
    /// field, silently skipping whatever `init`/`plan_missing` would have
    /// done with it. Pages with `$skip`/`$top`, stopping on the first short
    /// page; carries no sort clause because a project's own field
    /// attachments are a small, effectively static collection for the life
    /// of one run, unlike the issue list `fetch::fetch_snapshot` pages.
    ///
    /// # Errors
    /// Returns `RemoteError` on transport or HTTP failure.
    pub fn read_project_bundle_fields(
        &self,
        project: &ProjectRef,
    ) -> Result<Vec<ProjectFieldBundle>, RemoteError> {
        let mut fields = Vec::new();
        let mut skip = 0_u32;
        loop {
            let path = format!(
                "/api/admin/projects/{}/customFields?fields={PROJECT_FIELDS_PATH_FIELDS}&$skip={skip}&$top={PROJECT_FIELDS_PAGE_SIZE}",
                project.id
            );
            let value = self.http().get_json(&path, "project custom fields")?;
            let page = json_array(Some(&value));
            let count = u32::try_from(page.len()).unwrap_or(u32::MAX);
            fields.extend(page.iter().filter_map(parse_project_field_bundle));
            if count < PROJECT_FIELDS_PAGE_SIZE {
                return Ok(fields);
            }
            skip += PROJECT_FIELDS_PAGE_SIZE;
        }
    }

    /// POST each missing value to `bundle`.
    ///
    /// One request per value: the values endpoint takes a single element.
    /// (The bulk form exists only when creating a whole bundle — see the
    /// clone in `crate::remote::youtrack::clone`.)
    ///
    /// # Errors
    /// Returns `RemoteError` on transport or HTTP failure.
    pub fn add_bundle_values(
        &self,
        bundle: &BundleRef,
        values: &[PendingValue],
    ) -> Result<(), RemoteError> {
        let path = format!(
            "/api/admin/customFieldSettings/bundles/{}/{}/values?fields=id,name",
            bundle.kind.segment(),
            bundle.id
        );
        for value in values {
            let mut body = json!({ "name": value.name, "$type": bundle.kind.element_type() });
            if let Some(resolved) = value.is_resolved {
                body["isResolved"] = json!(resolved);
            }
            self.http()
                .post_json(&path, &body, &format!("bundle value '{}'", value.name))?;
        }
        Ok(())
    }
}

fn parse_project_field_bundle(raw: &serde_json::Value) -> Option<ProjectFieldBundle> {
    let bundle_raw = raw.get("bundle")?;
    let kind = match bundle_raw
        .get("$type")
        .and_then(serde_json::Value::as_str)?
    {
        "EnumBundle" => BundleKind::Enum,
        "StateBundle" => BundleKind::State,
        // Version, Owned, Build and User bundles are not ours to touch.
        _ => return None,
    };
    Some(ProjectFieldBundle {
        instance_id: raw.get("id")?.as_str()?.to_string(),
        field_id: raw.get("field")?.get("id")?.as_str()?.to_string(),
        field_name: raw.get("field")?.get("name")?.as_str()?.to_string(),
        bundle: BundleRef {
            id: bundle_raw.get("id")?.as_str()?.to_string(),
            name: bundle_raw.get("name")?.as_str()?.to_string(),
            kind,
            values: parse_bundle_values(bundle_raw.get("values")),
        },
    })
}

/// Parse a `values(...)` array. Shared with the bundle re-read that follows a
/// clone, which asks the bundles endpoint directly rather than reaching the
/// same array through a project field.
#[must_use]
pub fn parse_bundle_values(raw: Option<&serde_json::Value>) -> Vec<BundleValue> {
    json_array(raw)
        .iter()
        .filter_map(|v| {
            Some(BundleValue {
                id: v.get("id")?.as_str()?.to_string(),
                name: v.get("name")?.as_str()?.to_string(),
                ordinal: v
                    .get("ordinal")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
                color: v.get("color").cloned().unwrap_or(serde_json::Value::Null),
                is_resolved: v.get("isResolved").and_then(serde_json::Value::as_bool),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::config::RemoteConfig;
    use crate::remote::http::{HttpClient, RetryPolicy, Token};
    use test_support::mock_http::MockServer;

    fn config() -> RemoteConfig {
        RemoteConfig::from_yaml_str(
            r#"
backend: youtrack
url: https://example.invalid
project: EM
status_map: { open: Open, in_progress: In Progress, blocked: Blocked,
              deferred: Deferred, draft: Draft, pinned: Pinned, closed: Fixed }
type_map: { task: Task, bug: Bug, feature: Feature, epic: Epic,
            chore: Chore, docs: Docs, question: Question }
priority_map: { 0: Show-stopper, 1: Critical, 2: Major, 3: Normal, 4: Minor }
deleted_state: "Won't fix"
"#,
        )
        .expect("valid config")
    }

    fn bundle(kind: BundleKind, names: &[&str]) -> BundleRef {
        BundleRef {
            id: "163-1".into(),
            name: "Types".into(),
            kind,
            values: names
                .iter()
                .enumerate()
                .map(|(i, n)| BundleValue {
                    id: format!("164-{i}"),
                    name: (*n).to_string(),
                    ordinal: i64::try_from(i).expect("test index fits"),
                    color: serde_json::Value::Null,
                    is_resolved: None,
                })
                .collect(),
        }
    }

    #[test]
    fn a_complete_bundle_needs_nothing() {
        let b = bundle(
            BundleKind::Enum,
            &[
                "Task", "Bug", "Feature", "Epic", "Chore", "Docs", "Question",
            ],
        );
        let missing = plan_missing(&config(), "Type", &b).expect("plan");
        assert!(
            missing.is_empty(),
            "an existing value must never be re-added: {missing:?}"
        );
    }

    #[test]
    fn the_reference_types_bundle_needs_exactly_three() {
        // The live EM instance's stock Types bundle.
        let b = bundle(
            BundleKind::Enum,
            &[
                "Bug",
                "Cosmetics",
                "Exception",
                "Feature",
                "Task",
                "Usability Problem",
                "Performance Problem",
                "Epic",
            ],
        );
        let missing = plan_missing(&config(), "Type", &b).expect("plan");
        let names: Vec<&str> = missing.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["Chore", "Docs", "Question"]);
    }

    #[test]
    fn the_reference_states_bundle_needs_exactly_four_with_correct_resolution() {
        let b = bundle(
            BundleKind::State,
            &[
                "Submitted",
                "Open",
                "In Progress",
                "To be discussed",
                "Reopened",
                "Can't Reproduce",
                "Duplicate",
                "Fixed",
                "Won't fix",
                "Incomplete",
                "Obsolete",
                "Verified",
            ],
        );
        let missing = plan_missing(&config(), "State", &b).expect("plan");
        let names: Vec<&str> = missing.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["Blocked", "Deferred", "Draft", "Pinned"]);
        for value in &missing {
            assert_eq!(
                value.is_resolved,
                Some(false),
                "{} is not a resolved state on the beads side",
                value.name
            );
        }
    }

    #[test]
    fn a_new_closed_state_is_resolved() {
        let b = bundle(BundleKind::State, &["Open"]);
        let missing = plan_missing(&config(), "State", &b).expect("plan");
        let fixed = missing
            .iter()
            .find(|v| v.name == "Fixed")
            .expect("Fixed is missing");
        assert_eq!(
            fixed.is_resolved,
            Some(true),
            "closed must map to a resolved state"
        );
        let deleted = missing
            .iter()
            .find(|v| v.name == "Won't fix")
            .expect("deleted_state is missing");
        assert_eq!(
            deleted.is_resolved,
            Some(true),
            "deleted_state must also be resolved"
        );
    }

    #[test]
    fn an_enum_bundle_never_carries_is_resolved() {
        let b = bundle(BundleKind::Enum, &["Task"]);
        let missing = plan_missing(&config(), "Type", &b).expect("plan");
        for value in &missing {
            assert_eq!(
                value.is_resolved, None,
                "isResolved is a state-bundle concept only: {value:?}"
            );
        }
    }

    #[test]
    fn a_case_only_difference_fails_naming_both_spellings() {
        // Map says `epic: Epic`; the bundle holds `epic`.
        let b = bundle(
            BundleKind::Enum,
            &[
                "Task", "Bug", "Feature", "epic", "Chore", "Docs", "Question",
            ],
        );
        let err = plan_missing(&config(), "Type", &b).expect_err("must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("Epic"),
            "must name the configured spelling: {msg}"
        );
        assert!(
            msg.contains("epic"),
            "must name the bundle's spelling: {msg}"
        );
    }

    #[test]
    fn a_field_with_no_configured_map_is_refused() {
        let b = bundle(BundleKind::Enum, &["Whatever"]);
        let err = plan_missing(&config(), "Subsystem", &b).expect_err("must refuse");
        assert!(err.to_string().contains("Subsystem"), "{err}");
    }

    #[test]
    fn only_enum_and_state_bundles_are_read_back() {
        let server = MockServer::start();
        let path = "/api/admin/projects/0-1/customFields?fields=id,$type,canBeEmpty,field(id,name),bundle(id,name,$type,values(id,name,ordinal,color(id,background,foreground),isResolved))&$skip=0&$top=100";
        server.on(
            "GET",
            path,
            200,
            &serde_json::json!([
                {"id":"189-8","field":{"id":"161-1","name":"Type"},
                 "bundle":{"id":"163-1","name":"Types","$type":"EnumBundle","values":[
                     {"id":"164-0","name":"Bug","ordinal":0,
                      "color":{"id":"18","background":"#e6f5cf","foreground":"#4da400"}}]}},
                {"id":"189-9","field":{"id":"161-2","name":"State"},
                 "bundle":{"id":"165-0","name":"States","$type":"StateBundle","values":[
                     {"id":"166-7","name":"Fixed","ordinal":7,"isResolved":true}]}},
                {"id":"189-11","field":{"id":"161-5","name":"Fix versions"},
                 "bundle":{"id":"168-2","name":"EasyMoney: Versions","$type":"VersionBundle","values":[]}},
                {"id":"189-30","field":{"id":"161-30","name":"Beads ID"}}
            ])
            .to_string(),
        );

        let admin = AdminClient::new(HttpClient::new(
            &server.base_url(),
            Token::new("t"),
            RetryPolicy::none(),
        ));
        let project = ProjectRef {
            id: "0-1".into(),
            short_name: "EM".into(),
            name: "EasyMoney".into(),
        };
        let fields = admin
            .read_project_bundle_fields(&project)
            .expect("read fields");

        let names: Vec<&str> = fields.iter().map(|f| f.field_name.as_str()).collect();
        assert_eq!(
            names,
            ["Type", "State"],
            "a VersionBundle and a bundle-less field are both skipped"
        );
        assert_eq!(fields[0].bundle.kind, BundleKind::Enum);
        assert_eq!(fields[1].bundle.kind, BundleKind::State);
        assert_eq!(fields[1].bundle.values[0].is_resolved, Some(true));
        assert!(fields[0].bundle.values[0].color.is_object());
        assert_eq!(fields[0].instance_id, "189-8");
    }

    #[test]
    fn read_project_bundle_fields_pages_past_the_first_hundred() {
        // The original bug: this read asked for `$top=100` with no `$skip`,
        // so a field instance past the first page read as though the
        // project simply did not have that field. This pins that the read
        // itself now pages, and that the first page's URL carries an
        // explicit `&$skip=0`.
        let server = MockServer::start();
        let path_fields = "id,$type,canBeEmpty,field(id,name),\
bundle(id,name,$type,values(id,name,ordinal,color(id,background,foreground),isResolved))";
        let first_page: Vec<_> = (0..100)
            .map(|n| {
                serde_json::json!({"id": format!("189-{n}"), "field": {"id": format!("161-{n}"), "name": format!("Custom {n}")}})
            })
            .collect();
        server.on(
            "GET",
            &format!("/api/admin/projects/0-1/customFields?fields={path_fields}&$skip=0&$top=100"),
            200,
            &serde_json::Value::Array(first_page).to_string(),
        );
        server.on(
            "GET",
            &format!(
                "/api/admin/projects/0-1/customFields?fields={path_fields}&$skip=100&$top=100"
            ),
            200,
            &serde_json::json!([
                {"id":"189-999","field":{"id":"161-2","name":"State"},
                 "bundle":{"id":"165-0","name":"States","$type":"StateBundle","values":[
                     {"id":"166-7","name":"Fixed","ordinal":7,"isResolved":true}]}}
            ])
            .to_string(),
        );

        let admin = AdminClient::new(HttpClient::new(
            &server.base_url(),
            Token::new("t"),
            RetryPolicy::none(),
        ));
        let project = ProjectRef {
            id: "0-1".into(),
            short_name: "EM".into(),
            name: "EasyMoney".into(),
        };
        let fields = admin
            .read_project_bundle_fields(&project)
            .expect("read must page past the boundary");

        assert!(
            fields.iter().any(|f| f.instance_id == "189-999"),
            "a field instance beyond the first page must be visible: {fields:?}"
        );
    }

    #[test]
    fn each_missing_value_is_one_post_carrying_the_element_type() {
        let server = MockServer::start();
        server.on(
            "POST",
            "/api/admin/customFieldSettings/bundles/state/165-0/values?fields=id,name",
            200,
            r#"{"id":"166-20","name":"Blocked"}"#,
        );

        let admin = AdminClient::new(HttpClient::new(
            &server.base_url(),
            Token::new("t"),
            RetryPolicy::none(),
        ));
        let target = BundleRef {
            id: "165-0".into(),
            name: "States".into(),
            kind: BundleKind::State,
            values: Vec::new(),
        };
        admin
            .add_bundle_values(
                &target,
                &[
                    PendingValue {
                        name: "Blocked".into(),
                        is_resolved: Some(false),
                    },
                    PendingValue {
                        name: "Fixed".into(),
                        is_resolved: Some(true),
                    },
                ],
            )
            .expect("add values");

        let posts = server.write_requests();
        assert_eq!(posts.len(), 2, "one request per value");
        assert!(
            posts[0].body.contains("StateBundleElement"),
            "{}",
            posts[0].body
        );
        assert!(
            posts[0].body.contains(r#""isResolved":false"#),
            "{}",
            posts[0].body
        );
        assert!(
            posts[1].body.contains(r#""isResolved":true"#),
            "{}",
            posts[1].body
        );
    }

    #[test]
    fn a_bundle_needing_nothing_issues_no_write_at_all() {
        let server = MockServer::start();
        let admin = AdminClient::new(HttpClient::new(
            &server.base_url(),
            Token::new("t"),
            RetryPolicy::none(),
        ));
        let target = BundleRef {
            id: "163-1".into(),
            name: "Types".into(),
            kind: BundleKind::Enum,
            values: Vec::new(),
        };
        admin.add_bundle_values(&target, &[]).expect("no-op");
        assert!(server.requests().is_empty(), "{:?}", server.requests());
    }
}
