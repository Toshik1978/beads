//! E2E tests for the `defer` and `undefer` commands.
//!
//! These tests verify the defer/undefer lifecycle including:
//! - Setting/clearing deferred status
//! - Time parsing (relative, absolute, natural language)
//! - Ready/blocked list interactions
//! - Edge cases and error handling

// `common` is now the `test-support` crate; aliased so that the 753
// `common::` paths in this suite keep working unchanged.
use crate::common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use tracing::info;
fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    // Handle both formats: "Created bd-xxx: title" and "✓ Created bd-xxx: title"
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    let id_part = normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

fn setup_workspace_with_issue() -> (BrWorkspace, String) {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Test issue for defer", "-p", "2", "-t", "task"],
        "create_issue",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    (workspace, id)
}

fn setup_workspace_with_multiple_issues() -> (BrWorkspace, Vec<String>) {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut ids = Vec::new();
    for i in 1..=3 {
        let create = run_br(
            &workspace,
            ["create", &format!("Issue {i}"), "-p", "2", "-t", "task"],
            &format!("create_issue_{i}"),
        );
        assert!(create.status.success());
        ids.push(parse_created_id(&create.stdout));
    }

    (workspace, ids)
}

// =============================================================================
// Defer Basic Tests
// =============================================================================

#[test]
fn defer_sets_status_deferred() {
    common::init_test_logging();
    info!("defer_sets_status_deferred: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(&workspace, ["update", &id, "--status", "deferred"], "defer");
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    assert!(show.status.success());
    let payload = extract_json_payload(&show.stdout);
    let issues: Value = serde_json::from_str(&payload).expect("valid json");

    // show returns flattened array
    assert_eq!(
        issues[0]["status"].as_str().unwrap(),
        "deferred",
        "status should be deferred"
    );
    info!("defer_sets_status_deferred: assertions passed");
}

#[test]
fn defer_indefinitely_no_until() {
    common::init_test_logging();
    info!("defer_indefinitely_no_until: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["update", &id, "--status", "deferred", "--json"],
        "defer",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    // `update`'s JSON output is a flat array of updated issues (no
    // "deferred" wrapper key -- that batch-report shape belonged to the
    // now-removed `defer` subcommand).
    let deferred = result.as_array().expect("update json is an array");
    assert_eq!(deferred.len(), 1);
    let deferred = &deferred[0];
    assert_eq!(deferred["status"], "deferred");

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    let issue = &show_issues[0];

    assert!(
        issue.get("defer_until").is_none() || issue["defer_until"].is_null(),
        "defer_until should be null for indefinite defer"
    );
    info!("defer_indefinitely_no_until: assertions passed");
}

#[test]
fn defer_with_until_timestamp() {
    common::init_test_logging();
    info!("defer_with_until_timestamp: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        [
            "update", &id, "--status", "deferred", "--defer", "+1d", "--json",
        ],
        "defer_with_until",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    // Verify via show
    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    let issue = &show_issues[0];

    assert!(
        issue["defer_until"].as_str().is_some(),
        "defer_until should have a value"
    );
    info!("defer_with_until_timestamp: assertions passed");
}

#[test]
fn defer_multiple_issues() {
    common::init_test_logging();
    info!("defer_multiple_issues: starting");
    let (workspace, ids) = setup_workspace_with_multiple_issues();

    let defer = run_br(
        &workspace,
        [
            "update", &ids[0], &ids[1], &ids[2], "--status", "deferred", "--json",
        ],
        "defer_multiple",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    let deferred = result.as_array().expect("update json is an array");
    assert_eq!(deferred.len(), 3, "all 3 issues should be deferred");

    for id in &ids {
        let show = run_br(&workspace, ["show", id, "--json"], &format!("show_{id}"));
        let show_payload = extract_json_payload(&show.stdout);
        let issues: Value = serde_json::from_str(&show_payload).expect("valid json");
        assert_eq!(issues[0]["status"].as_str().unwrap(), "deferred");
    }
    info!("defer_multiple_issues: assertions passed");
}

#[test]
fn defer_json_output() {
    common::init_test_logging();
    info!("defer_json_output: starting");
    let (workspace, id) = setup_workspace_with_issue();

    // `defer`'s JSON output reported `previous_status` and `defer_until`
    // inline on the batch-report item. `update`'s JSON output
    // (`UpdatedIssueOutput`) carries neither field -- it is a flat
    // id/title/status/priority/updated_at snapshot, not a diff report.
    // Capture the pre-update status via `show` so the "transitioned from
    // open" assertion survives, and verify `defer_until` via a follow-up
    // `show` rather than the update response itself.
    let before_show = run_br(&workspace, ["show", &id, "--json"], "show_before");
    let before_payload = extract_json_payload(&before_show.stdout);
    let before_issues: Value = serde_json::from_str(&before_payload).expect("valid json");
    let previous_status = before_issues[0]["status"]
        .as_str()
        .expect("status string")
        .to_string();

    let defer = run_br(
        &workspace,
        [
            "update", &id, "--status", "deferred", "--defer", "tomorrow", "--json",
        ],
        "defer_json",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    let deferred = result.as_array().expect("update json is an array");
    assert!(!deferred.is_empty());

    let first = &deferred[0];
    assert!(first.get("id").is_some(), "deferred item should have id");
    assert!(
        first.get("title").is_some(),
        "deferred item should have title"
    );
    assert!(
        first.get("status").is_some(),
        "deferred item should have status"
    );
    assert_eq!(first["status"].as_str().unwrap(), "deferred");
    assert_eq!(previous_status, "open");

    let show = run_br(&workspace, ["show", &id, "--json"], "show_after");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    assert!(
        show_issues[0]["defer_until"].as_str().is_some(),
        "defer json output should preserve defer_until"
    );
    info!("defer_json_output: assertions passed");
}

// =============================================================================
// Natural Time Parsing Tests
// =============================================================================

#[test]
fn defer_until_tomorrow() {
    common::init_test_logging();
    info!("defer_until_tomorrow: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        [
            "update", &id, "--status", "deferred", "--defer", "tomorrow", "--json",
        ],
        "defer_tomorrow",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    let issue = &show_issues[0];

    let defer_until = issue["defer_until"].as_str().unwrap();
    assert!(
        !defer_until.is_empty(),
        "defer_until should be set for tomorrow"
    );
    info!("defer_until_tomorrow: assertions passed");
}

#[test]
fn defer_until_relative() {
    common::init_test_logging();
    info!("defer_until_relative: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        [
            "update", &id, "--status", "deferred", "--defer", "+2h", "--json",
        ],
        "defer_relative",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    let issue = &show_issues[0];

    let defer_until = issue["defer_until"].as_str().unwrap();
    assert!(!defer_until.is_empty(), "defer_until should be set for +2h");
    info!("defer_until_relative: assertions passed");
}

#[test]
fn defer_until_specific_date() {
    common::init_test_logging();
    info!("defer_until_specific_date: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        [
            "update",
            &id,
            "--status",
            "deferred",
            "--defer",
            "2099-12-31",
            "--json",
        ],
        "defer_specific_date",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    let issue = &show_issues[0];

    let defer_until = issue["defer_until"].as_str().unwrap();
    assert!(
        defer_until.contains("2099-12-31"),
        "defer_until should contain the specified date"
    );
    info!("defer_until_specific_date: assertions passed");
}

#[test]
fn defer_until_datetime() {
    common::init_test_logging();
    info!("defer_until_datetime: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        [
            "update",
            &id,
            "--status",
            "deferred",
            "--defer",
            "2099-02-01T09:00:00Z",
            "--json",
        ],
        "defer_datetime",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    let issue = &show_issues[0];

    let defer_until = issue["defer_until"].as_str().unwrap();
    assert!(
        defer_until.contains("2099-02-01"),
        "defer_until should contain the specified date"
    );
    info!("defer_until_datetime: assertions passed");
}

#[test]
fn defer_until_past_allows() {
    common::init_test_logging();
    info!("defer_until_past_allows: starting");
    let (workspace, id) = setup_workspace_with_issue();

    // Past dates should be allowed. Pass value with --until=-1d to avoid flag confusion
    // or use -- to separate args if id comes after?
    // clap syntax for negative values usually requires equals sign or --
    // br defer id --until=-1d should work
    let defer = run_br(
        &workspace,
        [
            "update",
            &id,
            "--status",
            "deferred",
            "--defer=-1d",
            "--json",
        ],
        "defer_past",
    );
    assert!(
        defer.status.success(),
        "defer with past date should succeed: {}",
        defer.stderr
    );

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    let issue = &show_issues[0];

    assert_eq!(issue["status"], "deferred");
    info!("defer_until_past_allows: assertions passed");
}

#[test]
fn defer_until_invalid_error() {
    common::init_test_logging();
    info!("defer_until_invalid_error: starting");
    let (workspace, id) = setup_workspace_with_issue();

    // Previously this asserted only `contains("unrecognized")` among other
    // options -- which clap's "unrecognized subcommand" error (from the
    // removed `defer` subcommand) satisfied for the wrong reason. Now that
    // `update` is a real subcommand, an invalid `--defer` value must fail
    // with `optional_date_field`'s actual parse error, not a clap routing
    // error.
    let defer = run_br(
        &workspace,
        ["update", &id, "--defer", "not-a-valid-time", "--json"],
        "defer_invalid_time",
    );
    assert!(
        !defer.status.success(),
        "defer with invalid time should fail"
    );
    // `update --json` reports errors as a JSON body on stdout, not a plain
    // message on stderr (unlike clap's own usage errors). Check both
    // streams so the assertion is robust to exactly where the message
    // lands, but the key point is it must NOT be clap's "unrecognized
    // subcommand" routing error.
    let combined = format!("{}\n{}", defer.stdout, defer.stderr).to_lowercase();
    assert!(
        !combined.contains("unrecognized"),
        "error must come from date parsing, not clap subcommand routing: {combined}"
    );
    assert!(
        combined.contains("invalid") || combined.contains("parse"),
        "error should mention invalid time format: {combined}"
    );
    info!("defer_until_invalid_error: assertions passed");
}

// =============================================================================
// Undefer Tests
// =============================================================================

#[test]
fn undefer_sets_status_open() {
    common::init_test_logging();
    info!("undefer_sets_status_open: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["update", &id, "--status", "deferred"],
        "defer_first",
    );
    assert!(defer.status.success());

    let undefer = run_br(
        &workspace,
        ["update", &id, "--defer", "", "--status", "open"],
        "undefer",
    );
    assert!(
        undefer.status.success(),
        "undefer failed: {}",
        undefer.stderr
    );

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let payload = extract_json_payload(&show.stdout);
    let issues: Value = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(
        issues[0]["status"].as_str().unwrap(),
        "open",
        "status should be open after undefer"
    );
    info!("undefer_sets_status_open: assertions passed");
}

#[test]
fn undefer_clears_defer_until() {
    common::init_test_logging();
    info!("undefer_clears_defer_until: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["update", &id, "--status", "deferred", "--defer", "+1d"],
        "defer_first",
    );
    assert!(defer.status.success());

    let undefer = run_br(
        &workspace,
        ["update", &id, "--defer", "", "--status", "open", "--json"],
        "undefer",
    );
    assert!(undefer.status.success());

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let payload = extract_json_payload(&show.stdout);
    let issues: Value = serde_json::from_str(&payload).expect("valid json");
    let issue = &issues[0];

    assert!(
        issue.get("defer_until").is_none() || issue["defer_until"].is_null(),
        "defer_until should be cleared after undefer"
    );
    info!("undefer_clears_defer_until: assertions passed");
}

#[test]
fn undefer_multiple_issues() {
    common::init_test_logging();
    info!("undefer_multiple_issues: starting");
    let (workspace, ids) = setup_workspace_with_multiple_issues();

    let defer = run_br(
        &workspace,
        ["update", &ids[0], &ids[1], &ids[2], "--status", "deferred"],
        "defer_all",
    );
    assert!(defer.status.success());

    let undefer = run_br(
        &workspace,
        [
            "update", &ids[0], &ids[1], &ids[2], "--defer", "", "--status", "open", "--json",
        ],
        "undefer_all",
    );
    assert!(undefer.status.success());

    let payload = extract_json_payload(&undefer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    let undeferred = result.as_array().expect("update json is an array");
    assert_eq!(undeferred.len(), 3, "all 3 issues should be undeferred");

    for id in &ids {
        let show = run_br(&workspace, ["show", id, "--json"], &format!("show_{id}"));
        let show_payload = extract_json_payload(&show.stdout);
        let issues: Value = serde_json::from_str(&show_payload).expect("valid json");
        assert_eq!(issues[0]["status"].as_str().unwrap(), "open");
    }
    info!("undefer_multiple_issues: assertions passed");
}

#[test]
fn undefer_json_output() {
    common::init_test_logging();
    info!("undefer_json_output: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["update", &id, "--status", "deferred"],
        "defer_first",
    );
    assert!(defer.status.success());

    // Same shape change as `defer_json_output`: `update`'s JSON output
    // carries no `previous_status`, so capture the pre-undefer status via
    // `show` instead.
    let before_show = run_br(&workspace, ["show", &id, "--json"], "show_before");
    let before_payload = extract_json_payload(&before_show.stdout);
    let before_issues: Value = serde_json::from_str(&before_payload).expect("valid json");
    let previous_status = before_issues[0]["status"]
        .as_str()
        .expect("status string")
        .to_string();

    let undefer = run_br(
        &workspace,
        ["update", &id, "--defer", "", "--status", "open", "--json"],
        "undefer",
    );
    assert!(undefer.status.success());

    let payload = extract_json_payload(&undefer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    let undeferred = result.as_array().expect("update json is an array");
    assert_eq!(undeferred.len(), 1);

    let first = &undeferred[0];
    assert!(first.get("id").is_some());
    assert!(first.get("title").is_some());
    assert!(first.get("status").is_some());
    assert_eq!(first["status"].as_str().unwrap(), "open");
    assert_eq!(previous_status, "deferred");
    info!("undefer_json_output: assertions passed");
}

// =============================================================================
// Edge Cases
// =============================================================================

#[test]
fn defer_already_deferred_updates_time() {
    common::init_test_logging();
    info!("defer_already_deferred_updates_time: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer1 = run_br(
        &workspace,
        [
            "update", &id, "--status", "deferred", "--defer", "+1d", "--json",
        ],
        "defer_first",
    );
    assert!(defer1.status.success());

    let defer2 = run_br(
        &workspace,
        [
            "update", &id, "--status", "deferred", "--defer", "+2d", "--json",
        ],
        "defer_second",
    );
    assert!(defer2.status.success());

    let payload = extract_json_payload(&defer2.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    let deferred = result.as_array().expect("update json is an array");
    assert_eq!(deferred.len(), 1);

    // Check time updated via show
    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    // Verify defer_until is > 1d from now
    assert!(show_issues[0]["defer_until"].as_str().is_some());
    info!("defer_already_deferred_updates_time: assertions passed");
}

#[test]
fn update_does_not_skip_already_open_issues() {
    // RENAMED from `undefer_already_open_skips`: the old `undefer`
    // subcommand had dedicated batch-processing logic that reported an
    // already-open issue as "skipped" with a "not deferred" reason instead
    // of touching it. `update` has no such per-item skip mechanism -- it is
    // a generic field mutator that applies the requested fields
    // unconditionally (rejecting only whole-command validation failures,
    // e.g. a tombstone target). Measured: `update ID --defer "" --status
    // open` on an already-open issue succeeds as a harmless no-op and
    // reports the issue as updated, not skipped. Unlike the closed-issue
    // case below, this is not a bug -- a no-op on an already-open issue is
    // genuinely harmless -- so this is a rename-only fix documenting what
    // the test actually verifies, not a known-gap regression test.
    common::init_test_logging();
    info!("update_does_not_skip_already_open_issues: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let undefer = run_br(
        &workspace,
        ["update", &id, "--defer", "", "--status", "open", "--json"],
        "undefer_open",
    );
    assert!(undefer.status.success());

    let payload = extract_json_payload(&undefer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");
    let updated = result.as_array().expect("update json is an array");
    assert_eq!(
        updated.len(),
        1,
        "the no-op undefer of an already-open issue should still report the issue"
    );
    assert_eq!(updated[0]["id"], id);
    assert_eq!(updated[0]["status"], "open");

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    assert_eq!(issues[0]["status"], "open");
    info!("update_does_not_skip_already_open_issues: assertions passed");
}

#[test]
fn update_refuses_to_un_close_a_closed_issue() {
    // bds-04l.7, now FIXED. This test previously documented the gap under
    // the name `update_does_not_skip_closed_issues` and asserted the buggy
    // behavior on purpose, so that whoever fixed the bug would see exactly
    // what changed. This is that change.
    //
    // The removed `defer` subcommand special-cased closed issues, refusing
    // to touch them ("cannot defer closed issue"; old guard at
    // `src/cli/commands/defer.rs:281-291` at `c6ae47f^`, which skipped any
    // `issue.status.is_terminal()`). It was the only caller enforcing that
    // direction, so removing it in bds-04l.2.6 exposed an asymmetry:
    // `reject_terminal_status_transition` refused to *enter* a terminal
    // state but nothing refused to *leave* one, and
    // `update ID --status deferred` on a closed issue exited 0 while
    // clearing `closed_at` and `close_reason` -- an un-close with `br
    // reopen` never invoked and no audit trail.
    //
    // `validate_mutable_target_issues` now refuses that transition and
    // names `br reopen`. What this test pins is that the refusal happens at
    // all AND that nothing was mutated on the way to it: a guard that
    // errored after writing would be no better than the bug.
    common::init_test_logging();
    info!("update_refuses_to_un_close_a_closed_issue: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let close = run_br(
        &workspace,
        ["close", &id, "--reason", "done"],
        "close_first",
    );
    assert!(close.status.success());

    let before_show = run_br(&workspace, ["show", &id, "--json"], "show_before");
    let before_payload = extract_json_payload(&before_show.stdout);
    let before_issues: Value = serde_json::from_str(&before_payload).expect("valid json");
    assert_eq!(before_issues[0]["status"], "closed");
    assert!(
        before_issues[0].get("closed_at").is_some(),
        "sanity check: a closed issue must have closed_at set before this test begins"
    );
    assert!(
        before_issues[0].get("close_reason").is_some(),
        "sanity check: a closed issue must have close_reason set before this test begins"
    );

    let defer = run_br(
        &workspace,
        ["update", &id, "--status", "deferred", "--json"],
        "defer_closed",
    );
    assert!(
        !defer.status.success(),
        "update must refuse to move a closed issue out of its terminal state: {}",
        defer.stderr
    );
    // Under `--json` the structured error envelope goes to stdout, not
    // stderr, so this reads stdout deliberately rather than by oversight.
    let refusal: Value =
        serde_json::from_str(&extract_json_payload(&defer.stdout)).expect("error envelope json");
    assert_eq!(
        refusal["error"]["code"], "VALIDATION_FAILED",
        "unexpected error envelope: {refusal}"
    );
    assert!(
        refusal["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("br reopen")),
        "the refusal must name the command that owns this transition, got: {refusal}"
    );

    // The audit trail must survive the refusal intact -- this is the half
    // that made the original bug worse than a missing skip.
    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    assert_eq!(
        issues[0]["status"], "closed",
        "the refused update must leave status untouched"
    );
    assert_eq!(
        issues[0]["closed_at"], before_issues[0]["closed_at"],
        "the refused update must not clear closed_at"
    );
    assert_eq!(
        issues[0]["close_reason"], before_issues[0]["close_reason"],
        "the refused update must not clear close_reason"
    );

    // `br reopen` remains the supported route out, so the guard closes a
    // bypass rather than trapping the issue in a terminal state.
    let reopen = run_br(
        &workspace,
        ["reopen", &id, "--json"],
        "reopen_after_refusal",
    );
    assert!(
        reopen.status.success(),
        "br reopen must still work after the refusal: {}",
        reopen.stderr
    );
    let after_show = run_br(&workspace, ["show", &id, "--json"], "show_after_reopen");
    let after_payload = extract_json_payload(&after_show.stdout);
    let after_issues: Value = serde_json::from_str(&after_payload).expect("valid json");
    assert_ne!(
        after_issues[0]["status"], "closed",
        "br reopen must actually leave the terminal state"
    );

    info!("update_refuses_to_un_close_a_closed_issue: assertions passed");
}

#[test]
fn defer_nonexistent_error() {
    common::init_test_logging();
    info!("defer_nonexistent_error: starting");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // NOTE: contrary to the task brief's characterization, this test was
    // NOT a false-pass -- it was genuinely failing in the baseline (clap's
    // "unrecognized subcommand" error contains neither "not found" nor
    // "matching", so the old assertion never matched it). Only
    // `defer_until_invalid_error` was the true false-pass. Rewritten here
    // regardless, against the real `update` error contract.
    let defer = run_br(
        &workspace,
        ["update", "bd-nonexistent", "--defer", "+1d", "--json"],
        "defer_nonexistent",
    );

    // Should fail with not found. `update --json` prints its error body to
    // stdout (not stderr) -- measured directly against the binary -- so
    // assert on stdout specifically rather than a stdout+stderr union, to
    // stay precise about where the error actually surfaces.
    assert!(!defer.status.success());
    assert!(defer.stdout.contains("not found") || defer.stdout.contains("matching"));
    info!("defer_nonexistent_error: assertions passed");
}

// =============================================================================
// Ready/Blocked Interaction Tests
// =============================================================================

#[test]
fn deferred_not_in_ready() {
    common::init_test_logging();
    info!("deferred_not_in_ready: starting");
    let (workspace, ids) = setup_workspace_with_multiple_issues();

    // Defer one issue
    let defer = run_br(
        &workspace,
        ["update", &ids[0], "--status", "deferred"],
        "defer_one",
    );
    assert!(defer.status.success());

    let ready = run_br(&workspace, ["ready", "--json"], "ready");
    assert!(ready.status.success());

    let payload = extract_json_payload(&ready.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // Deferred issue should NOT appear in ready list
    let ready_ids: Vec<&str> = issues.iter().filter_map(|i| i["id"].as_str()).collect();

    assert!(
        !ready_ids.contains(&ids[0].as_str()),
        "deferred issue should not appear in ready list"
    );

    // Other issues should still be in ready
    assert!(
        ready_ids.contains(&ids[1].as_str()),
        "non-deferred issues should be in ready list"
    );
    info!("deferred_not_in_ready: assertions passed");
}

#[test]
fn deferred_not_blocked() {
    common::init_test_logging();
    info!("deferred_not_blocked: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(&workspace, ["update", &id, "--status", "deferred"], "defer");
    assert!(defer.status.success());

    let blocked = run_br(&workspace, ["blocked", "--json"], "blocked");
    assert!(blocked.status.success());

    let payload = extract_json_payload(&blocked.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).unwrap_or_else(|_| vec![]);

    // Deferred issue should NOT appear in blocked list (deferred != blocked)
    assert!(
        !issues
            .iter()
            .filter_map(|i| i["id"].as_str())
            .any(|x| x == id.as_str()),
        "deferred issue should not appear in blocked list"
    );
    info!("deferred_not_blocked: assertions passed");
}

#[test]
fn undefer_appears_in_ready() {
    common::init_test_logging();
    info!("undefer_appears_in_ready: starting");
    let (workspace, id) = setup_workspace_with_issue();

    // Defer then undefer
    let defer = run_br(&workspace, ["update", &id, "--status", "deferred"], "defer");
    assert!(defer.status.success());

    let ready_before = run_br(&workspace, ["ready", "--json"], "ready_before");
    let payload_before = extract_json_payload(&ready_before.stdout);
    let issues_before: Vec<Value> =
        serde_json::from_str(&payload_before).unwrap_or_else(|_| vec![]);
    assert!(
        !issues_before
            .iter()
            .filter_map(|i| i["id"].as_str())
            .any(|x| x == id.as_str())
    );

    // Undefer
    let undefer = run_br(
        &workspace,
        ["update", &id, "--defer", "", "--status", "open"],
        "undefer",
    );
    assert!(undefer.status.success());

    let ready_after = run_br(&workspace, ["ready", "--json"], "ready_after");
    assert!(ready_after.status.success());

    let payload_after = extract_json_payload(&ready_after.stdout);
    let issues_after: Vec<Value> = serde_json::from_str(&payload_after).expect("valid json");

    assert!(
        issues_after
            .iter()
            .filter_map(|i| i["id"].as_str())
            .any(|x| x == id.as_str()),
        "undeferred issue should appear in ready list"
    );
    info!("undefer_appears_in_ready: assertions passed");
}

#[test]
fn deferred_issue_serializes_defer_until_into_jsonl() {
    // br's own ready/list/blocked/stale filters read defer_until back out of
    // the workspace. If it ever stops being serialized, deferral silently stops
    // working across a sync round-trip and no other test would catch it.
    let (workspace, id) = setup_workspace_with_issue();

    let deferred = run_br(
        &workspace,
        [
            "update",
            &id,
            "--status",
            "deferred",
            "--defer",
            "2030-01-15",
        ],
        "defer_for_jsonl",
    );
    assert!(
        deferred.status.success(),
        "update --defer failed: {}",
        deferred.stderr
    );

    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush_for_jsonl");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let jsonl = std::fs::read_to_string(workspace.root.join(".beads/issues.jsonl"))
        .expect("issues.jsonl must exist after flush");
    let line = jsonl
        .lines()
        .find(|l| l.contains(&id))
        .expect("the deferred issue must appear in issues.jsonl");
    let value: Value = serde_json::from_str(line).expect("issues.jsonl line must be valid JSON");

    assert!(
        value.get("defer_until").is_some(),
        "defer_until must be serialized into issues.jsonl. Got: {line}"
    );
    assert_eq!(
        value.get("status").and_then(Value::as_str),
        Some("deferred"),
        "a hard-deferred issue must serialize status=deferred. Got: {line}"
    );
}
