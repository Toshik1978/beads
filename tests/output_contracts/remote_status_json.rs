//! Output contract for `br remote status --json`.
//!
//! `br remote status --json` is a consumer surface — the only way to see the
//! reconciliation plan without executing it — and had no output-contract
//! test at all before this one. It drives the real binary against the
//! loopback mock (`test_support::mock_http::MockServer`), never a live
//! instance, and every id and count in the fixture below is fixed by hand so
//! the snapshot cannot drift for reasons other than a real change to the
//! JSON shape. Nothing in the payload carries a timestamp, so there is
//! nothing here for `normalize_json` to do.
//!
//! The scenario is deliberately small — one field change, one create
//! candidate, one adoption candidate — but every other top-level array in
//! `ReconcilePlan` stays present, just empty, which is exactly the point: a
//! consumer parsing this must be able to rely on every key existing whether
//! or not it holds anything.

use crate::common::cli::{BrWorkspace, run_br_with_env};
use crate::common::mock_http::MockServer;
use crate::common::youtrack_fixtures::{
    LINK_TYPES, LINK_TYPES_PATH, issues_path, write_remote_config,
};
use insta::assert_json_snapshot;
use serde_json::Value;
use std::fs;

const TOKEN: [(&str, &str); 1] = [("BR_YOUTRACK_TOKEN", "t")];

// One paired bead whose title disagrees with its mirror (always local-wins,
// title is not returnable), and one unpaired bead — a create candidate. Fixed
// ids throughout, so nothing here depends on the hash-based id generator.
const FIXTURE: &str = r#"{"id":"em-golden-paired","title":"Golden paired issue (local)","status":"open","priority":2,"issue_type":"task","external_ref":"EM-1","created_at":"2026-01-01T00:00:00Z","created_by":"fixture","updated_at":"2026-01-01T00:00:00Z","source_repo":".","compaction_level":0,"original_size":0}
{"id":"em-golden-create","title":"Golden bead to create","status":"open","priority":2,"issue_type":"task","created_at":"2026-01-02T00:00:00Z","created_by":"fixture","updated_at":"2026-01-02T00:00:00Z","source_repo":".","compaction_level":0,"original_size":0}
"#;

// EM-1 pairs with the bead above (title differs on purpose); EM-2 is claimed
// by no bead at all, so it becomes an adoption candidate. Both otherwise
// agree with the workspace's `type_map`/`status_map`/`priority_map`, so
// nothing here also becomes a field change or a refused adoption.
const ISSUES_PAGE: &str = r#"[
    {"id":"3-1","idReadable":"EM-1","summary":"Golden paired issue (remote)","updated":1000,
     "commentsCount":0,"tags":[],"links":[],
     "customFields":[
       {"name":"Type","value":{"name":"Task"}},
       {"name":"State","value":{"name":"Open"}},
       {"name":"Priority","value":{"name":"Major"}}]},
    {"id":"3-2","idReadable":"EM-2","summary":"Golden adoption candidate","updated":1000,
     "commentsCount":0,"tags":[],"links":[],
     "customFields":[
       {"name":"Type","value":{"name":"Task"}},
       {"name":"State","value":{"name":"Open"}},
       {"name":"Priority","value":{"name":"Major"}}]}
]"#;

#[test]
fn snapshot_remote_status_json() {
    let server = MockServer::start();
    let workspace = BrWorkspace::new();

    let init = run_br_with_env(&workspace, ["init", "--prefix", "em"], TOKEN, "init");
    assert!(init.status.success(), "br init failed: {}", init.stderr);

    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    fs::write(&jsonl_path, FIXTURE).expect("write remote status fixture");
    let import = run_br_with_env(
        &workspace,
        ["sync", "--import-only", "--json"],
        TOKEN,
        "remote_status_json_import",
    );
    assert!(
        import.status.success(),
        "fixture import failed:\nstdout:\n{}\nstderr:\n{}",
        import.stdout,
        import.stderr
    );

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on("GET", &issues_path(0), 200, ISSUES_PAGE);
    write_remote_config(&workspace.root.join(".beads"), &server.base_url());

    let run = run_br_with_env(
        &workspace,
        ["--json", "remote", "status"],
        TOKEN,
        "remote_status_json",
    );
    assert!(
        run.status.success(),
        "remote status --json failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        server.write_requests().is_empty(),
        "status is read-only; it issued {:?}",
        server.write_requests()
    );

    let json: Value =
        serde_json::from_str(run.stdout.trim()).unwrap_or_else(|e| panic!("{e}: {}", run.stdout));
    assert_json_snapshot!("remote_status_json_output", json);
}
