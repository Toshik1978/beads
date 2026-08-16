//! `br remote status` end to end, driving the real binary against the
//! loopback mock rather than a live tracker.
//!
//! The claim worth making here and nowhere else is the cheapest one to state
//! and the most expensive one to get wrong: a read-only verb must issue
//! **zero** writes. Not few — zero. `MockServer::write_requests()` sees every
//! state-changing method the process actually sent, so an accidental POST or
//! DELETE anywhere under `status` shows up as a failure with the offending
//! request printed.

use crate::common;

use beads::remote::youtrack::fetch::ISSUE_FIELDS;
use common::cli::{BrWorkspace, run_br_with_env};
use common::mock_http::MockServer;

const TOKEN: [(&str, &str); 1] = [("BR_YOUTRACK_TOKEN", "t")];

const LINK_TYPES_PATH: &str =
    "/api/issueLinkTypes?fields=id,name,sourceToTarget,targetToSource,directed&$top=100";

const LINK_TYPES: &str = r#"[
    {"id":"173-0","name":"Relates","sourceToTarget":"relates to","directed":false},
    {"id":"173-1","name":"Depend","sourceToTarget":"is required for","targetToSource":"depends on","directed":true},
    {"id":"173-3","name":"Subtask","sourceToTarget":"parent for","targetToSource":"subtask of","directed":true}
]"#;

fn write_remote_config(workspace: &BrWorkspace, base_url: &str) {
    let template = include_str!("../fixtures/remote_em.yaml");
    let yaml = template.replace("https://example.invalid", base_url);
    let path = workspace.root.join(".beads").join("remote.yaml");
    std::fs::write(&path, yaml).expect("write remote.yaml");
}

/// The wire format, spelled out rather than imported: paging over an
/// unordered collection is not a partition, so the sort clause is part of the
/// contract and an e2e is the right place to pin it literally.
fn issues_path(skip: u32) -> String {
    format!(
        "/api/issues?query=project:%20EM%20sort%20by:%20created%20asc\
         &fields={ISSUE_FIELDS}&$skip={skip}&$top=100"
    )
}

/// One mirrored issue whose every field already matches the bead below.
fn agreeing_page(summary: &str) -> String {
    format!(
        r#"[{{"id":"3-1","idReadable":"EM-1","summary":"{summary}","updated":1000,
             "commentsCount":0,"tags":[],"links":[],
             "customFields":[
               {{"name":"Type","value":{{"name":"Task"}}}},
               {{"name":"State","value":{{"name":"Open"}}}},
               {{"name":"Priority","value":{{"name":"Major"}}}},
               {{"name":"Assignee","value":null}},
               {{"name":"Fix versions","value":[]}}]}}]"#
    )
}

fn reconciled_workspace(server: &MockServer, summary: &str) -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br_with_env(&workspace, ["init", "--prefix", "em"], TOKEN, "init");
    assert!(init.status.success(), "br init failed: {}", init.stderr);

    let created = run_br_with_env(
        &workspace,
        [
            "create",
            summary,
            "--priority",
            "2",
            "--external-ref",
            "EM-1",
        ],
        TOKEN,
        "create",
    );
    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr
    );

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on("GET", &issues_path(0), 200, &agreeing_page(summary));
    write_remote_config(&workspace, &server.base_url());
    workspace
}

#[test]
fn e2e_remote_status_on_a_reconciled_workspace_issues_zero_writes() {
    let _log = common::test_log("e2e_remote_status_on_a_reconciled_workspace_issues_zero_writes");
    let server = MockServer::start();
    let workspace = reconciled_workspace(&server, "Mirrored");

    let run = run_br_with_env(&workspace, ["remote", "status"], TOKEN, "remote_status");

    assert!(
        run.status.success(),
        "status failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        server.write_requests().is_empty(),
        "status is read-only; it issued {:?}",
        server.write_requests()
    );
    assert!(
        run.stdout.contains("nothing to do"),
        "a reconciled workspace has no work: {}",
        run.stdout
    );
}

#[test]
fn e2e_remote_status_names_every_kind_of_pending_work() {
    let _log = common::test_log("e2e_remote_status_names_every_kind_of_pending_work");
    let server = MockServer::start();
    let workspace = BrWorkspace::new();
    let init = run_br_with_env(&workspace, ["init", "--prefix", "em"], TOKEN, "init");
    assert!(init.status.success(), "br init failed: {}", init.stderr);

    // A bead paired with a mirror whose title differs, a bead with no ref at
    // all, a bead whose ref names an issue that is gone, and a bead pointing
    // at some other tracker entirely.
    for args in [
        vec![
            "create",
            "Local title",
            "--priority",
            "2",
            "--external-ref",
            "EM-1",
        ],
        vec!["create", "Brand new", "--priority", "2"],
        vec![
            "create",
            "Gone",
            "--priority",
            "2",
            "--external-ref",
            "EM-404",
        ],
        vec![
            "create",
            "Elsewhere",
            "--priority",
            "2",
            "--external-ref",
            "JIRA-9",
        ],
    ] {
        let created = run_br_with_env(&workspace, args, TOKEN, "create");
        assert!(
            created.status.success(),
            "create failed: {}",
            created.stderr
        );
    }

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on(
        "GET",
        &issues_path(0),
        200,
        &format!(
            "[{},{}]",
            agreeing_page("Remote title").trim_matches(|c| c == '[' || c == ']'),
            r#"{"id":"3-7","idReadable":"EM-7","summary":"Unclaimed","updated":1000,
                "commentsCount":0,"tags":[],"links":[],
                "customFields":[
                  {"name":"Type","value":{"name":"Task"}},
                  {"name":"State","value":{"name":"Open"}},
                  {"name":"Priority","value":{"name":"Major"}}]}"#
        ),
    );
    write_remote_config(&workspace, &server.base_url());

    let run = run_br_with_env(&workspace, ["remote", "status"], TOKEN, "remote_status");

    assert!(
        run.status.success(),
        "status failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        server.write_requests().is_empty(),
        "status is read-only; it issued {:?}",
        server.write_requests()
    );
    for expected in [
        "create 1 issue(s)",
        "update 1 issue(s)",
        "adoption candidates",
        "EM-7",
        "dangling local refs",
        "EM-404",
        "out of scope",
    ] {
        assert!(
            run.stdout.contains(expected),
            "missing {expected:?} in:\n{}",
            run.stdout
        );
    }
}

#[test]
fn e2e_remote_status_json_reports_the_plan_and_a_zero_write_count() {
    let _log = common::test_log("e2e_remote_status_json_reports_the_plan_and_a_zero_write_count");
    let server = MockServer::start();
    let workspace = reconciled_workspace(&server, "Mirrored");

    let run = run_br_with_env(
        &workspace,
        ["--json", "remote", "status"],
        TOKEN,
        "remote_status_json",
    );

    assert!(
        run.status.success(),
        "status failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    let payload: serde_json::Value =
        serde_json::from_str(run.stdout.trim()).unwrap_or_else(|e| panic!("{e}: {}", run.stdout));
    assert_eq!(payload["project"], "EM");
    assert_eq!(
        payload["writes"], 0,
        "the read-only verb must report no writes: {}",
        run.stdout
    );
    assert!(
        payload["plan"]["creates"]
            .as_array()
            .expect("creates")
            .is_empty()
    );
    assert!(
        payload["plan"]["field_changes"]
            .as_array()
            .expect("field_changes")
            .is_empty()
    );
    let notes = payload["plan"]["notes"].as_array().expect("notes");
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("comments")),
        "a machine consumer must also learn comments are unreconciled: {}",
        run.stdout
    );
    assert!(server.write_requests().is_empty());
}

/// The command whose job is to report what cannot be mapped must not be the
/// command an unmappable value disables.
#[test]
fn e2e_remote_status_reports_an_unmappable_issue_instead_of_refusing() {
    let _log =
        common::test_log("e2e_remote_status_reports_an_unmappable_issue_instead_of_refusing");
    let server = MockServer::start();
    let workspace = BrWorkspace::new();
    let init = run_br_with_env(&workspace, ["init", "--prefix", "em"], TOKEN, "init");
    assert!(init.status.success(), "br init failed: {}", init.stderr);

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on(
        "GET",
        &issues_path(0),
        200,
        r#"[{"id":"3-5","idReadable":"EM-5","summary":"Reviewed","updated":1000,
             "commentsCount":0,"tags":[],"links":[],
             "customFields":[
               {"name":"Type","value":{"name":"Task"}},
               {"name":"State","value":{"name":"In Review"}},
               {"name":"Priority","value":{"name":"Major"}}]}]"#,
    );
    write_remote_config(&workspace, &server.base_url());

    let run = run_br_with_env(&workspace, ["remote", "status"], TOKEN, "remote_status");

    assert!(
        run.status.success(),
        "status must still run: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    for expected in ["refused adoptions", "EM-5", "In Review", "status_map"] {
        assert!(
            run.stdout.contains(expected),
            "missing {expected:?} in:\n{}",
            run.stdout
        );
    }
    assert!(server.write_requests().is_empty());
}
