//! bds-b4h: `br statuses`, `br types`, and `br stale --limit`.
//!
//! The interesting claim is that `br statuses` answers for **this** project
//! rather than reciting a constant. `src/cli/commands/vocabulary.rs` unit-tests
//! the merge against a hand-built `Workflow`; what is left for here is the thing
//! only a real workspace can show — that the command reads the `policy.yaml` on
//! disk, and that what it says is allowed is what `br update` actually accepts.
//!
//! That last pairing is the point of the command. bds-npo deleted an error whose
//! hint hard-coded the built-in vocabulary because in a strict workspace that was
//! the wrong list. If `br statuses` and `br update` could disagree, this would be
//! the same bug with a nicer interface.

use crate::common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::fs;

fn workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "vc"], "vocab init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    workspace
}

fn write_policy(workspace: &BrWorkspace, body: &str) {
    fs::write(workspace.root.join(".beads").join("policy.yaml"), body).expect("write policy");
}

fn statuses_json(workspace: &BrWorkspace, label: &str) -> Value {
    let run = run_br(workspace, ["statuses", "--json"], label);
    assert!(run.status.success(), "statuses: {}", run.stderr);
    serde_json::from_str(&extract_json_payload(&run.stdout)).expect("parse statuses json")
}

fn allowed_names(payload: &Value) -> Vec<String> {
    payload["statuses"]
        .as_array()
        .expect("statuses array")
        .iter()
        .filter(|entry| entry["allowed"].as_bool() == Some(true))
        .filter_map(|entry| entry["name"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn statuses_reports_an_unconfigured_project_as_accepting_anything() {
    let workspace = workspace();
    let payload = statuses_json(&workspace, "vocab statuses default");

    assert_eq!(payload["strict"].as_bool(), Some(false));
    assert_eq!(payload["enforced"].as_bool(), Some(false));
    assert_eq!(payload["any_value_accepted"].as_bool(), Some(true));
    assert_eq!(
        payload["ready_group"].as_array().map(Vec::len),
        Some(1),
        "the unconfigured ready group is [open]: {payload}"
    );
    assert!(
        allowed_names(&payload).contains(&"in_progress".to_string()),
        "{payload}"
    );

    let text = run_br(&workspace, ["statuses"], "vocab statuses text");
    assert!(text.status.success(), "{}", text.stderr);
    assert!(
        text.stdout.contains("any status value is accepted"),
        "the unconfigured case has to say so in words too: {}",
        text.stdout
    );
}

/// The command reads the policy on disk, and what it reports as allowed is what
/// `br update` accepts. Both halves in one test, because either alone would let
/// the pair drift.
#[test]
fn statuses_reports_the_projects_own_vocabulary_and_agrees_with_update() {
    let workspace = workspace();
    write_policy(
        &workspace,
        "workflow:\n  strict: true\n  statuses: [open, rework, closed]\n  \
         status_groups:\n    ready: [open, rework]\n",
    );

    let payload = statuses_json(&workspace, "vocab statuses strict");
    assert_eq!(payload["enforced"].as_bool(), Some(true));
    assert_eq!(payload["any_value_accepted"].as_bool(), Some(false));
    assert_eq!(
        allowed_names(&payload),
        vec![
            "open".to_string(),
            "closed".to_string(),
            "rework".to_string()
        ],
        "built-ins in declared order, then the policy-only value: {payload}"
    );
    assert_eq!(
        payload["ready_group"],
        serde_json::json!(["open", "rework"]),
        "{payload}"
    );

    let rework = payload["statuses"]
        .as_array()
        .expect("array")
        .iter()
        .find(|entry| entry["name"].as_str() == Some("rework"))
        .expect("policy-only status is listed");
    assert_eq!(
        rework["builtin"].as_bool(),
        Some(false),
        "a value that exists only in policy.yaml must not be labelled built-in"
    );

    // And now the pairing. `br statuses` said `rework` is allowed and `blocked`
    // is not; `br update` has to agree, or the report is decoration.
    let created = run_br(&workspace, ["create", "subject"], "vocab create");
    assert!(created.status.success(), "create: {}", created.stderr);
    let line = created.stdout.lines().next().unwrap_or("");
    let id = line
        .strip_prefix("✓ ")
        .unwrap_or(line)
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or_default()
        .trim()
        .to_string();

    let to_rework = run_br(
        &workspace,
        ["update", &id, "--status", "rework"],
        "vocab to rework",
    );
    assert!(
        to_rework.status.success(),
        "`br statuses` reported rework as allowed, so update must accept it: {}",
        to_rework.stderr
    );

    let to_blocked = run_br(
        &workspace,
        ["update", &id, "--status", "blocked"],
        "vocab to blocked",
    );
    assert!(
        !to_blocked.status.success(),
        "`br statuses` reported blocked as NOT allowed, so update must refuse it: {}",
        to_blocked.stdout
    );
}

/// `strict: true` with no status list enforces nothing. Reported as its own state
/// because a project that set `strict` and expected enforcement has a real
/// problem, and this is the command that can show it.
#[test]
fn statuses_distinguishes_strict_but_empty_from_enforcing() {
    let workspace = workspace();
    write_policy(&workspace, "workflow:\n  strict: true\n");

    let payload = statuses_json(&workspace, "vocab statuses strict empty");
    assert_eq!(payload["strict"].as_bool(), Some(true));
    assert_eq!(payload["enforced"].as_bool(), Some(false));
    assert_eq!(payload["any_value_accepted"].as_bool(), Some(true));

    let text = run_br(&workspace, ["statuses"], "vocab statuses strict empty text");
    assert!(
        text.stdout.contains("workflow.statuses is empty"),
        "the trap has to be named, not implied: {}",
        text.stdout
    );
}

#[test]
fn types_lists_the_builtins_and_says_any_other_value_is_accepted() {
    let workspace = workspace();
    let run = run_br(&workspace, ["types", "--json"], "vocab types json");
    assert!(run.status.success(), "types: {}", run.stderr);
    let payload: Value =
        serde_json::from_str(&extract_json_payload(&run.stdout)).expect("parse types json");

    assert_eq!(payload["any_value_accepted"].as_bool(), Some(true));
    let names: Vec<&str> = payload["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|entry| entry["name"].as_str())
        .collect();
    assert!(
        names.contains(&"task") && names.contains(&"epic"),
        "{payload}"
    );

    let text = run_br(&workspace, ["types"], "vocab types text");
    assert!(
        text.stdout.contains("Any other value is also accepted"),
        "IssueType::Custom is the answer, and it has to be stated: {}",
        text.stdout
    );

    // And it is true: a type nobody shipped is accepted and stored as given.
    let created = run_br(
        &workspace,
        ["create", "odd one", "--type", "spike"],
        "vocab custom type",
    );
    assert!(
        created.status.success(),
        "the claim `br types` makes has to hold: {}",
        created.stderr
    );
}

/// `br stale --limit` keeps the *stalest* N, not an arbitrary N: the limit is
/// applied alongside the stalest-first ordering rather than before it.
#[test]
fn stale_limit_keeps_the_stalest_rows() {
    let workspace = workspace();
    for title in ["first", "second", "third"] {
        let created = run_br(&workspace, ["create", title], "vocab stale create");
        assert!(created.status.success(), "create: {}", created.stderr);
    }

    // `--days 0` makes everything stale, so the limit is the only thing narrowing.
    let unlimited = run_br(
        &workspace,
        ["stale", "--days", "0", "--json"],
        "vocab stale all",
    );
    assert!(unlimited.status.success(), "{}", unlimited.stderr);
    let all: Value =
        serde_json::from_str(&extract_json_payload(&unlimited.stdout)).expect("parse stale json");
    let all_rows = all.as_array().cloned().unwrap_or_default();
    assert_eq!(all_rows.len(), 3, "{}", unlimited.stdout);

    let limited = run_br(
        &workspace,
        ["stale", "--days", "0", "--limit", "2", "--json"],
        "vocab stale limited",
    );
    assert!(limited.status.success(), "{}", limited.stderr);
    let capped: Value =
        serde_json::from_str(&extract_json_payload(&limited.stdout)).expect("parse stale json");
    let capped_rows = capped.as_array().cloned().unwrap_or_default();
    assert_eq!(capped_rows.len(), 2, "{}", limited.stdout);
    assert_eq!(
        capped_rows.iter().map(|row| &row["id"]).collect::<Vec<_>>(),
        all_rows
            .iter()
            .take(2)
            .map(|row| &row["id"])
            .collect::<Vec<_>>(),
        "the limit has to keep the head of the stalest-first ordering, not an \
         arbitrary subset"
    );
}
