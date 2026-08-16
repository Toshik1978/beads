//! Bead ↔ YouTrack issue field mapping, by name, in both directions.
//!
//! Issue-level bundle values (`Type`, `State`, `Priority`) are set BY NAME on
//! create and update — verified live against a real instance — so the
//! several-hundred-write hot path needs no id lookup; only project
//! `defaultValues` need ids, and that is a different task's concern. The four
//! prose fields ride `TextIssueCustomField`'s `value.text`; the Beads ID
//! stamp is a bare string on a `SimpleIssueCustomField`.
//!
//! The push direction refuses on an unmapped bead value (`Custom` issue
//! types and statuses are the reachable case — `RemoteConfig::validate` only
//! guarantees every *built-in* is mapped, and cannot see a value that does
//! not exist yet), symmetric with `reverse_fields` refusing on an unmapped
//! remote value. `init`/`status` are expected to pre-flight a workspace so a
//! push rarely reaches this, but the mapping layer does not trust that and
//! refuses anyway rather than sending YouTrack a value it cannot resolve.

use crate::model::{Issue, IssueType, Priority, Status};
use crate::remote::config::RemoteConfig;
use crate::remote::error::RemoteError;
use serde_json::{Value, json};

/// Which fields an update body should carry.
///
/// A plain struct of `bool`s rather than a bitflags type, per the task
/// brief's "bitflags-free `FieldSet`".
#[derive(Debug, Clone, Copy, Default)]
pub struct FieldSet {
    pub title: bool,
    pub description: bool,
    pub status: bool,
    pub issue_type: bool,
    pub priority: bool,
    pub design: bool,
    pub acceptance_criteria: bool,
    pub notes: bool,
    pub close_reason: bool,
    pub beads_id: bool,
}

/// A remote bundle value with no beads preimage, before it becomes prose.
///
/// [`reverse_fields`] flattens this into a `RemoteError::Config` message,
/// which is what every existing caller wants. Adoption wants the parts —
/// it reports the issue, the field, the offending value and the map to
/// extend as four separate strings — so the structured form is produced
/// first and the message is derived from it. One definition, two shapes:
/// see [`reverse_fields_structured`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmappedValue {
    /// The YouTrack field name, e.g. `Type`.
    pub field: String,
    /// The value that has no preimage, e.g. `User Story`.
    pub value: String,
    /// The `remote.yaml` key that would cover it, e.g. `type_map`.
    pub config_key: String,
}

impl UnmappedValue {
    /// The pull-direction refusal, as one sentence.
    #[must_use]
    pub fn message(&self) -> String {
        format!(
            "issue {} '{}' has no beads mapping; add an entry to {} in remote.yaml",
            self.field, self.value, self.config_key
        )
    }
}

/// A bead's fields as read back from a YouTrack issue body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteIssueFields {
    pub title: String,
    pub description: Option<String>,
    pub status: Status,
    pub issue_type: IssueType,
    pub priority: Priority,
    pub design: Option<String>,
    pub acceptance_criteria: Option<String>,
    pub notes: Option<String>,
    pub close_reason: Option<String>,
    pub beads_id: Option<String>,
}

/// Look up a `customFields` entry by its YouTrack field name.
///
/// The issue GET returns *every* field the project defines, so callers must
/// always select by name, never by array position.
#[must_use]
pub fn custom_field_by_name<'a>(raw: &'a Value, name: &str) -> Option<&'a Value> {
    raw.get("customFields")?
        .as_array()?
        .iter()
        .find(|field| field.get("name").and_then(Value::as_str) == Some(name))
}

/// The body of a create POST for `issue`, carrying every mapped field.
///
/// A prose field with no bead value is omitted entirely — there is nothing
/// to clear on a brand-new issue. Compare [`issue_update_body`], where an
/// absent-but-gated prose field must send an explicit clear instead.
///
/// # Errors
/// Returns `RemoteError::Config` when `issue`'s type, status or priority has
/// no preimage in `cfg`'s maps, naming the field and the offending value.
pub fn issue_create_body(
    cfg: &RemoteConfig,
    project_id: &str,
    issue: &Issue,
) -> Result<Value, RemoteError> {
    let mut custom_fields = Vec::new();
    push_bundle_field(
        &mut custom_fields,
        "Type",
        "SingleEnumIssueCustomField",
        &mapped_type(cfg, &issue.issue_type)?,
    );
    push_bundle_field(
        &mut custom_fields,
        "State",
        "StateIssueCustomField",
        &mapped_status(cfg, &issue.status)?,
    );
    push_bundle_field(
        &mut custom_fields,
        "Priority",
        "SingleEnumIssueCustomField",
        &mapped_priority(cfg, issue.priority)?,
    );
    push_prose_field_if_present(&mut custom_fields, "Design", issue.design.as_deref());
    push_prose_field_if_present(
        &mut custom_fields,
        "Acceptance Criteria",
        issue.acceptance_criteria.as_deref(),
    );
    push_prose_field_if_present(&mut custom_fields, "Notes", issue.notes.as_deref());
    push_prose_field_if_present(
        &mut custom_fields,
        "Close Reason",
        issue.close_reason.as_deref(),
    );
    custom_fields.push(json!({
        "name": "Beads ID",
        "$type": "SimpleIssueCustomField",
        "value": issue.id,
    }));

    let mut body = json!({
        "project": {"id": project_id},
        "summary": issue.title,
        "customFields": custom_fields,
    });
    if let Some(description) = &issue.description {
        body["description"] = json!(description);
    }
    Ok(body)
}

/// The body of an update POST for `issue`, carrying only the fields named
/// in `fields`.
///
/// A gated prose field that is `None` still appears in the body, with an
/// explicit `null` value — omitting it would leave stale remote text in
/// place instead of clearing it, since a caller only gates a field here
/// because it changed. Compare [`issue_create_body`], where an absent prose
/// field is omitted because there is nothing yet to clear.
///
/// # Errors
/// Returns `RemoteError::Config` when a *gated* type, status or priority
/// change has no preimage in `cfg`'s maps, naming the field and the
/// offending value.
pub fn issue_update_body(
    cfg: &RemoteConfig,
    issue: &Issue,
    fields: FieldSet,
) -> Result<Value, RemoteError> {
    let mut custom_fields = Vec::new();
    if fields.issue_type {
        push_bundle_field(
            &mut custom_fields,
            "Type",
            "SingleEnumIssueCustomField",
            &mapped_type(cfg, &issue.issue_type)?,
        );
    }
    if fields.status {
        push_bundle_field(
            &mut custom_fields,
            "State",
            "StateIssueCustomField",
            &mapped_status(cfg, &issue.status)?,
        );
    }
    if fields.priority {
        push_bundle_field(
            &mut custom_fields,
            "Priority",
            "SingleEnumIssueCustomField",
            &mapped_priority(cfg, issue.priority)?,
        );
    }
    if fields.design {
        push_prose_field_or_clear(&mut custom_fields, "Design", issue.design.as_deref());
    }
    if fields.acceptance_criteria {
        push_prose_field_or_clear(
            &mut custom_fields,
            "Acceptance Criteria",
            issue.acceptance_criteria.as_deref(),
        );
    }
    if fields.notes {
        push_prose_field_or_clear(&mut custom_fields, "Notes", issue.notes.as_deref());
    }
    if fields.close_reason {
        push_prose_field_or_clear(
            &mut custom_fields,
            "Close Reason",
            issue.close_reason.as_deref(),
        );
    }
    if fields.beads_id {
        custom_fields.push(json!({
            "name": "Beads ID",
            "$type": "SimpleIssueCustomField",
            "value": issue.id,
        }));
    }

    let mut body = json!({});
    if fields.title {
        body["summary"] = json!(issue.title);
    }
    if fields.description {
        body["description"] = json!(issue.description);
    }
    if !custom_fields.is_empty() {
        body["customFields"] = json!(custom_fields);
    }
    Ok(body)
}

/// The body of the one update a genuine deletion writes: `State` →
/// `cfg.deleted_state`.
///
/// Deliberately not [`issue_update_body`]. `tombstone` has no `status_map`
/// entry by design — the tombstone rule owns it, and mapping it would make a
/// forwarding pointer look like an ordinary status change — so that function
/// would refuse the very issue this exists for. `deleted_state` is a YouTrack
/// state name read straight out of `remote.yaml`, not a beads status put
/// through a map, which is why nothing here can fail.
///
/// `br remote` never deletes a remote issue
/// ([`crate::remote::tombstone`]); this update plus a `[br]`-marked comment
/// *is* the mirror's record of a deletion.
#[must_use]
pub fn deleted_state_body(cfg: &RemoteConfig) -> Value {
    let mut custom_fields = Vec::new();
    push_bundle_field(
        &mut custom_fields,
        "State",
        "StateIssueCustomField",
        &cfg.deleted_state,
    );
    json!({ "customFields": custom_fields })
}

/// Read every mapped field back out of a fetched YouTrack issue.
///
/// # Errors
/// Returns `RemoteError::Config` when a bundle field (`Type`/`State`/
/// `Priority`) carries a value with no preimage in `cfg`'s maps — a value
/// present but unmapped is a hard error, distinct from an absent value,
/// which resolves to the beads default.
pub fn reverse_fields(cfg: &RemoteConfig, raw: &Value) -> Result<RemoteIssueFields, RemoteError> {
    reverse_fields_structured(cfg, raw).map_err(|unmapped| RemoteError::Config(unmapped.message()))
}

/// [`reverse_fields`], with the refusal still in parts.
///
/// Adoption reports the issue, the field, the value and the map key
/// separately, and re-parsing them back out of a formatted sentence would
/// make the message format load-bearing. This is the same resolution with
/// the refusal left structured; `reverse_fields` is the flattening wrapper.
///
/// **`raw` is not always a whole fetched issue.**
/// [`crate::remote::adopt::classify_adoption`] rebuilds it from the three
/// keys read below — `summary`, `description` and `customFields` — because a
/// parsed `RemoteIssue` no longer holds the body it came from. Reading a
/// *fourth* top-level key here would silently resolve to `None` on that path
/// with nothing failing, so add the key to that reconstruction in the same
/// change. `adoption_resolves_the_same_fields_as_the_whole_body` is the test
/// that notices.
///
/// # Errors
/// Returns [`UnmappedValue`] when a bundle field (`Type`/`State`/`Priority`)
/// carries a value with no preimage in `cfg`'s maps — a value present but
/// unmapped is a hard error, distinct from an absent value, which resolves to
/// the beads default.
pub fn reverse_fields_structured(
    cfg: &RemoteConfig,
    raw: &Value,
) -> Result<RemoteIssueFields, UnmappedValue> {
    Ok(RemoteIssueFields {
        title: raw
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        description: raw
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        status: reverse_status(cfg, raw)?,
        issue_type: reverse_type(cfg, raw)?,
        priority: reverse_priority(cfg, raw)?,
        design: prose_field(raw, "Design"),
        acceptance_criteria: prose_field(raw, "Acceptance Criteria"),
        notes: prose_field(raw, "Notes"),
        close_reason: prose_field(raw, "Close Reason"),
        beads_id: beads_id_field(raw),
    })
}

/// A mapped bundle field's `value.name`, or `None` when the field or its
/// value is absent — never distinguished from a `null` value, since both
/// mean "nothing set".
fn bundle_value_name<'a>(raw: &'a Value, field_name: &str) -> Option<&'a str> {
    custom_field_by_name(raw, field_name)?
        .get("value")?
        .get("name")?
        .as_str()
}

fn reverse_type(cfg: &RemoteConfig, raw: &Value) -> Result<IssueType, UnmappedValue> {
    match bundle_value_name(raw, "Type") {
        None => Ok(IssueType::default()),
        Some(name) => cfg
            .reverse_type(name)
            .and_then(IssueType::known_value)
            .ok_or_else(|| unmapped_remote_value("Type", name, "type_map")),
    }
}

fn reverse_status(cfg: &RemoteConfig, raw: &Value) -> Result<Status, UnmappedValue> {
    match bundle_value_name(raw, "State") {
        None => Ok(Status::default()),
        Some(name) => cfg
            .reverse_status(name)
            .and_then(Status::known_value)
            .ok_or_else(|| unmapped_remote_value("State", name, "status_map")),
    }
}

fn reverse_priority(cfg: &RemoteConfig, raw: &Value) -> Result<Priority, UnmappedValue> {
    match bundle_value_name(raw, "Priority") {
        None => Ok(Priority::default()),
        Some(name) => cfg
            .reverse_priority(name)
            .map(|value| Priority(i32::from(value)))
            .ok_or_else(|| unmapped_remote_value("Priority", name, "priority_map")),
    }
}

/// A remote value with no beads preimage — the pull-direction refusal.
fn unmapped_remote_value(field: &str, value: &str, map_key: &str) -> UnmappedValue {
    UnmappedValue {
        field: field.to_string(),
        value: value.to_string(),
        config_key: map_key.to_string(),
    }
}

/// A bead value with no remote preimage — the push-direction refusal.
/// Symmetric with [`unmapped_remote_error`].
fn unmapped_local_error(field: &str, value: &str, map_key: &str) -> RemoteError {
    RemoteError::Config(format!(
        "beads {field} '{value}' has no YouTrack mapping; add an entry to {map_key} in remote.yaml"
    ))
}

/// A prose field's `value.text`, or `None` when the field or its value is
/// absent.
fn prose_field(raw: &Value, field_name: &str) -> Option<String> {
    custom_field_by_name(raw, field_name)?
        .get("value")?
        .get("text")?
        .as_str()
        .map(str::to_string)
}

fn beads_id_field(raw: &Value) -> Option<String> {
    custom_field_by_name(raw, "Beads ID")?
        .get("value")?
        .as_str()
        .map(str::to_string)
}

fn mapped_type(cfg: &RemoteConfig, issue_type: &IssueType) -> Result<String, RemoteError> {
    cfg.type_map
        .get(issue_type.as_str())
        .cloned()
        .ok_or_else(|| unmapped_local_error("issue type", issue_type.as_str(), "type_map"))
}

fn mapped_status(cfg: &RemoteConfig, status: &Status) -> Result<String, RemoteError> {
    cfg.status_map
        .get(status.as_str())
        .cloned()
        .ok_or_else(|| unmapped_local_error("status", status.as_str(), "status_map"))
}

fn mapped_priority(cfg: &RemoteConfig, priority: Priority) -> Result<String, RemoteError> {
    cfg.priority_map
        .get(&priority.0.to_string())
        .cloned()
        .ok_or_else(|| unmapped_local_error("priority", &priority.to_string(), "priority_map"))
}

fn push_bundle_field(fields: &mut Vec<Value>, name: &str, type_name: &str, mapped: &str) {
    fields.push(json!({
        "name": name,
        "$type": type_name,
        "value": {"name": mapped},
    }));
}

/// Create-path prose helper: a field with nothing to say is omitted, never
/// sent as `null` — there is no stale remote value to clear on a brand-new
/// issue.
fn push_prose_field_if_present(fields: &mut Vec<Value>, name: &str, prose: Option<&str>) {
    if let Some(text) = prose {
        fields.push(json!({
            "name": name,
            "$type": "TextIssueCustomField",
            "value": {"text": text},
        }));
    }
}

/// Update-path prose helper: always emits the field once gated. `None`
/// becomes an explicit `null` value rather than an omitted entry, so a bead
/// field cleared to `None` actually clears the remote text instead of
/// leaving it stale.
fn push_prose_field_or_clear(fields: &mut Vec<Value>, name: &str, prose: Option<&str>) {
    let value = prose.map_or(Value::Null, |text| json!({"text": text}));
    fields.push(json!({
        "name": name,
        "$type": "TextIssueCustomField",
        "value": value,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::config::RemoteConfig;

    fn config() -> RemoteConfig {
        RemoteConfig::from_yaml_str(include_str!("../../../tests/fixtures/remote_em.yaml"))
            .expect("valid config")
    }

    fn new_issue(id: &str, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            ..Issue::default()
        }
    }

    #[test]
    fn one_create_body_carries_every_field() {
        let mut issue = new_issue("bds-4r2", "br remote: a YouTrack mirror");
        issue.description = Some("body **markdown**".into());
        issue.design = Some("line1\n\nline2".into());
        issue.issue_type = crate::model::IssueType::Epic;
        issue.status = crate::model::Status::InProgress;
        issue.priority = crate::model::Priority::HIGH;

        let body = issue_create_body(&config(), "0-1", &issue).expect("create body");

        assert_eq!(body["project"]["id"], "0-1");
        assert_eq!(body["summary"], "br remote: a YouTrack mirror");
        assert_eq!(body["description"], "body **markdown**");

        let fields = body["customFields"].as_array().expect("customFields");
        let by_name = |name: &str| {
            fields
                .iter()
                .find(|f| f["name"] == name)
                .unwrap_or_else(|| panic!("missing {name}"))
        };

        assert_eq!(by_name("Type")["value"]["name"], "Epic");
        assert_eq!(by_name("Type")["$type"], "SingleEnumIssueCustomField");
        assert_eq!(by_name("State")["value"]["name"], "In Progress");
        assert_eq!(by_name("State")["$type"], "StateIssueCustomField");
        assert_eq!(by_name("Priority")["value"]["name"], "Critical");
        assert_eq!(by_name("Design")["value"]["text"], "line1\n\nline2");
        assert_eq!(by_name("Design")["$type"], "TextIssueCustomField");
        assert_eq!(by_name("Beads ID")["value"], "bds-4r2");
        assert_eq!(by_name("Beads ID")["$type"], "SimpleIssueCustomField");
    }

    #[test]
    fn no_issue_level_bundle_value_is_referenced_by_id() {
        let issue = new_issue("bds-1", "t");
        let body = issue_create_body(&config(), "0-1", &issue).expect("create body");
        let text = body.to_string();
        // Cheap but exact: element ids on this instance look like `164-9`.
        // If any appears in an issue body, someone resolved a name that did
        // not need resolving and added a lookup to every write. The project
        // reference is addressed by id on purpose (that id comes from a
        // separate provisioning lookup, not a bundle value), so it is
        // excluded from the scan rather than read as a false positive.
        let scoped = body["customFields"].to_string();
        assert!(
            !regex_lite_contains_element_id(&scoped),
            "issue bodies must reference bundle values by name: {text}"
        );
    }

    /// `NNN-NNN` inside a `"id"` position. Hand-rolled to avoid a regex dep.
    fn regex_lite_contains_element_id(text: &str) -> bool {
        text.split("\"id\":\"").skip(1).any(|rest| {
            let candidate = rest.split('"').next().unwrap_or_default();
            let mut parts = candidate.split('-');
            matches!(
                (parts.next(), parts.next(), parts.next()),
                (Some(a), Some(b), None)
                    if !a.is_empty() && a.chars().all(|c| c.is_ascii_digit())
                    && !b.is_empty() && b.chars().all(|c| c.is_ascii_digit())
            )
        })
    }

    fn remote_issue(type_value: Option<&str>) -> Value {
        let mut fields = vec![
            json!({"name":"State","value":{"name":"Open"}}),
            json!({"name":"Priority","value":{"name":"Major"}}),
            json!({"name":"Assignee","value":null}),
            json!({"name":"Fix versions","value":[]}),
        ];
        fields.push(match type_value {
            Some(v) => json!({"name":"Type","value":{"name":v}}),
            None => json!({"name":"Type","value":null}),
        });
        json!({"idReadable":"EM-1","summary":"captured","customFields":fields})
    }

    #[test]
    fn an_unmapped_type_is_a_hard_error_naming_the_value() {
        let err =
            reverse_fields(&config(), &remote_issue(Some("User Story"))).expect_err("must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("User Story"),
            "must name the offending value: {msg}"
        );
        assert!(msg.contains("Type"), "must name the field: {msg}");
    }

    #[test]
    fn an_absent_type_resolves_to_the_beads_default() {
        let fields = reverse_fields(&config(), &remote_issue(None)).expect("reverse");
        assert_eq!(fields.issue_type, crate::model::IssueType::Task);
        assert_eq!(fields.status, crate::model::Status::Open);
        assert_eq!(fields.priority, crate::model::Priority::MEDIUM);
    }

    #[test]
    fn unrelated_project_fields_are_ignored_not_mistaken_for_ours() {
        let fields = reverse_fields(&config(), &remote_issue(Some("Task"))).expect("reverse");
        assert_eq!(fields.issue_type, crate::model::IssueType::Task);
        assert!(
            fields.design.is_none(),
            "an absent prose field must stay absent"
        );
    }

    #[test]
    fn prose_fields_round_trip_through_the_text_shape() {
        let raw = json!({
            "idReadable": "EM-1",
            "summary": "captured",
            "customFields": [
                {"name": "State", "value": {"name": "Open"}},
                {"name": "Priority", "value": {"name": "Major"}},
                {"name": "Type", "value": {"name": "Task"}},
                {"name": "Design", "value": {"text": "line1\n\nline2\n\nline3, with a blank line above."}},
                {"name": "Notes", "value": {"text": "note para 1\n\nnote para 2"}},
            ]
        });

        let fields = reverse_fields(&config(), &raw).expect("reverse");

        assert_eq!(
            fields.design.as_deref(),
            Some("line1\n\nline2\n\nline3, with a blank line above.")
        );
        assert_eq!(fields.notes.as_deref(), Some("note para 1\n\nnote para 2"));
    }

    #[test]
    fn an_empty_field_set_produces_an_empty_update_body() {
        let issue = new_issue("bds-1", "t");
        let body = issue_update_body(&config(), &issue, FieldSet::default()).expect("update body");
        assert!(body.get("summary").is_none());
        assert!(body.get("description").is_none());
        assert!(body.get("customFields").is_none());
    }

    #[test]
    fn field_set_title_only_updates_only_summary() {
        let mut issue = new_issue("bds-1", "orig");
        issue.title = "new title".into();
        let body = issue_update_body(
            &config(),
            &issue,
            FieldSet {
                title: true,
                ..FieldSet::default()
            },
        )
        .expect("update body");
        assert_eq!(body["summary"], "new title");
        assert!(body.get("customFields").is_none());
        assert!(body.get("description").is_none());
    }

    #[test]
    fn field_set_description_only_updates_only_description() {
        let mut issue = new_issue("bds-1", "t");
        issue.description = Some("d".into());
        let body = issue_update_body(
            &config(),
            &issue,
            FieldSet {
                description: true,
                ..FieldSet::default()
            },
        )
        .expect("update body");
        assert_eq!(body["description"], "d");
        assert!(body.get("summary").is_none());
        assert!(body.get("customFields").is_none());
    }

    #[test]
    fn each_gated_bundle_or_prose_or_id_field_produces_exactly_itself() {
        let mut issue = new_issue("bds-1", "t");
        issue.issue_type = crate::model::IssueType::Bug;
        issue.status = crate::model::Status::Blocked;
        issue.priority = crate::model::Priority::CRITICAL;
        issue.design = Some("d".into());
        issue.acceptance_criteria = Some("ac".into());
        issue.notes = Some("n".into());
        issue.close_reason = Some("cr".into());

        let cases: Vec<(FieldSet, &str, Value)> = vec![
            (
                FieldSet {
                    issue_type: true,
                    ..FieldSet::default()
                },
                "Type",
                json!({"name": "Bug"}),
            ),
            (
                FieldSet {
                    status: true,
                    ..FieldSet::default()
                },
                "State",
                json!({"name": "Blocked"}),
            ),
            (
                FieldSet {
                    priority: true,
                    ..FieldSet::default()
                },
                "Priority",
                json!({"name": "Show-stopper"}),
            ),
            (
                FieldSet {
                    design: true,
                    ..FieldSet::default()
                },
                "Design",
                json!({"text": "d"}),
            ),
            (
                FieldSet {
                    acceptance_criteria: true,
                    ..FieldSet::default()
                },
                "Acceptance Criteria",
                json!({"text": "ac"}),
            ),
            (
                FieldSet {
                    notes: true,
                    ..FieldSet::default()
                },
                "Notes",
                json!({"text": "n"}),
            ),
            (
                FieldSet {
                    close_reason: true,
                    ..FieldSet::default()
                },
                "Close Reason",
                json!({"text": "cr"}),
            ),
            (
                FieldSet {
                    beads_id: true,
                    ..FieldSet::default()
                },
                "Beads ID",
                json!("bds-1"),
            ),
        ];

        for (fields, name, expected_value) in cases {
            let body = issue_update_body(&config(), &issue, fields).expect("update body");
            let custom_fields = body["customFields"].as_array().expect("customFields");
            assert_eq!(
                custom_fields.len(),
                1,
                "gating {name} must produce exactly one field"
            );
            assert_eq!(custom_fields[0]["name"], name);
            assert_eq!(custom_fields[0]["value"], expected_value);
            assert!(body.get("summary").is_none());
            assert!(body.get("description").is_none());
        }
    }

    #[test]
    fn issue_update_body_sets_a_prose_field_when_present() {
        let mut issue = new_issue("bds-1", "t");
        issue.design = Some("new design".into());
        let body = issue_update_body(
            &config(),
            &issue,
            FieldSet {
                design: true,
                ..FieldSet::default()
            },
        )
        .expect("update body");
        let custom_fields = body["customFields"].as_array().expect("customFields");
        assert_eq!(custom_fields.len(), 1);
        assert_eq!(custom_fields[0]["name"], "Design");
        assert_eq!(custom_fields[0]["value"]["text"], "new design");
    }

    #[test]
    fn issue_update_body_clears_a_prose_field_gated_but_absent() {
        let issue = new_issue("bds-1", "t"); // design stays None
        let body = issue_update_body(
            &config(),
            &issue,
            FieldSet {
                design: true,
                ..FieldSet::default()
            },
        )
        .expect("update body");
        let custom_fields = body["customFields"].as_array().expect("customFields");
        assert_eq!(custom_fields.len(), 1, "the field must still appear");
        assert_eq!(custom_fields[0]["name"], "Design");
        assert!(
            custom_fields[0]["value"].is_null(),
            "clearing must send an explicit null, not omit the field: {custom_fields:?}"
        );
    }

    #[test]
    fn create_omits_an_absent_prose_field_rather_than_clearing_it() {
        let issue = new_issue("bds-1", "t"); // design stays None
        let body = issue_create_body(&config(), "0-1", &issue).expect("create body");
        let custom_fields = body["customFields"].as_array().expect("customFields");
        assert!(
            !custom_fields.iter().any(|f| f["name"] == "Design"),
            "a brand-new issue has nothing to clear, so the field must be absent entirely: {custom_fields:?}"
        );
    }

    #[test]
    fn pushing_an_unmapped_custom_issue_type_refuses_naming_the_value() {
        let mut issue = new_issue("bds-1", "t");
        issue.issue_type = crate::model::IssueType::Custom("spike".into());
        let err = issue_create_body(&config(), "0-1", &issue).expect_err("must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("spike"),
            "must name the offending value: {msg}"
        );
        assert!(
            msg.contains("type_map"),
            "must name the map to extend: {msg}"
        );
    }

    #[test]
    fn pushing_an_unmapped_custom_status_refuses_on_update_too() {
        let mut issue = new_issue("bds-1", "t");
        issue.status = crate::model::Status::Custom("triaging".into());
        let err = issue_update_body(
            &config(),
            &issue,
            FieldSet {
                status: true,
                ..FieldSet::default()
            },
        )
        .expect_err("must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("triaging"),
            "must name the offending value: {msg}"
        );
        assert!(
            msg.contains("status_map"),
            "must name the map to extend: {msg}"
        );
    }
}
