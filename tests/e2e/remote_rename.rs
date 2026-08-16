//! A reparenting rename, through to `br remote status`.
//!
//! `br rename` itself refuses to move an issue within the hierarchy — that
//! is `br update --parent`'s job, and it renumbers by driving the same
//! `rename_issue` cascade underneath (see `src/cli/commands/rename.rs`'s
//! doc comment). That cascade rewrites the live row's id in place and keeps
//! every other column, `external_ref` included, so the renamed child stays
//! paired with the same mirrored issue automatically; the tombstone left at
//! the vacated id carries no `external_ref` of its own (see
//! `src/remote/tombstone.rs`'s module doc), so it never becomes a
//! `TombstonedPair` and needs no forwarding.
//!
//! What *does* need to happen is an ordinary field/link reconciliation: the
//! child's real parent changed, the mirror still shows the old one, and
//! `diff_links` (bds-4r2.4.4) is what is supposed to notice and report that
//! — with no re-creation of the issue and, as everywhere in `br remote`, no
//! deletion of anything.

use crate::common;

use common::cli::{BrWorkspace, parse_created_id, run_br_with_env};
use common::mock_http::MockServer;
use serde_json::Value;

const TOKEN: [(&str, &str); 1] = [("BR_YOUTRACK_TOKEN", "t")];

use common::youtrack_fixtures::{LINK_TYPES, LINK_TYPES_PATH, issues_path};

fn write_remote_config(workspace: &BrWorkspace, base_url: &str) {
    common::youtrack_fixtures::write_remote_config(&workspace.root.join(".beads"), base_url);
}

fn issue_with_no_links(id_readable: &str, summary: &str) -> Value {
    serde_json::json!({
        "id": format!("3-{id_readable}"),
        "idReadable": id_readable,
        "summary": summary,
        "updated": 1000,
        "commentsCount": 0,
        "tags": [],
        "links": [],
        "customFields": [
            {"name": "Type", "value": {"name": "Epic"}},
            {"name": "State", "value": {"name": "Open"}},
            {"name": "Priority", "value": {"name": "Major"}},
            {"name": "Assignee", "value": null},
            {"name": "Fix versions", "value": []}
        ]
    })
}

/// The child's mirror, still carrying the *pre-reparent* parent link — the
/// state a stale mirror would be in the moment after a local-only reparent.
fn child_with_parent_link(id_readable: &str, summary: &str, parent_readable: &str) -> Value {
    serde_json::json!({
        "id": format!("3-{id_readable}"),
        "idReadable": id_readable,
        "summary": summary,
        "updated": 1000,
        "commentsCount": 0,
        "tags": [],
        "links": [
            {
                "id": "173-3t",
                "direction": "INWARD",
                "linkType": {"id": "173-3", "name": "Subtask"},
                "issues": [{"id": format!("3-{parent_readable}"), "idReadable": parent_readable}]
            }
        ],
        "customFields": [
            {"name": "Type", "value": {"name": "Task"}},
            {"name": "State", "value": {"name": "Open"}},
            {"name": "Priority", "value": {"name": "Major"}},
            {"name": "Assignee", "value": null},
            {"name": "Fix versions", "value": []}
        ]
    })
}

// One scenario stated end to end: build the workspace, rename, reparent,
// then assert on every request the mock saw. Splitting it into helpers would
// scatter the setup this test's assertions are read against.
#[allow(clippy::too_many_lines)]
#[test]
fn e2e_a_reparenting_rename_produces_link_changes_and_no_create_or_delete() {
    let _log =
        common::test_log("e2e_a_reparenting_rename_produces_link_changes_and_no_create_or_delete");
    let server = MockServer::start();
    let workspace = BrWorkspace::new();

    assert!(
        run_br_with_env(&workspace, ["init", "--prefix", "em"], TOKEN, "init")
            .status
            .success()
    );

    let epic1 = run_br_with_env(
        &workspace,
        [
            "create",
            "Epic one",
            "--type",
            "epic",
            "--priority",
            "2",
            "--external-ref",
            "EM-1",
        ],
        TOKEN,
        "create_epic1",
    );
    assert!(epic1.status.success(), "create epic1: {}", epic1.stderr);
    let epic1_id = parse_created_id(&epic1.stdout);

    let epic2 = run_br_with_env(
        &workspace,
        [
            "create",
            "Epic two",
            "--type",
            "epic",
            "--priority",
            "2",
            "--external-ref",
            "EM-2",
        ],
        TOKEN,
        "create_epic2",
    );
    assert!(epic2.status.success(), "create epic2: {}", epic2.stderr);
    let epic2_id = parse_created_id(&epic2.stdout);

    let child = run_br_with_env(
        &workspace,
        [
            "create",
            "Child",
            "--type",
            "task",
            "--priority",
            "2",
            "--parent",
            &epic1_id,
            "--external-ref",
            "EM-3",
        ],
        TOKEN,
        "create_child",
    );
    assert!(child.status.success(), "create child: {}", child.stderr);
    let child_id = parse_created_id(&child.stdout);
    assert!(
        child_id.contains('.'),
        "precondition: {child_id} should be a dotted ID"
    );

    // The reparenting rename: renumbers the child under epic2, tombstones
    // the vacated dotted id, and (per `rename_issue_in_tx`) leaves
    // `external_ref` on the surviving row untouched.
    let reparent = run_br_with_env(
        &workspace,
        ["update", &child_id, "--parent", &epic2_id],
        TOKEN,
        "reparent",
    );
    assert!(
        reparent.status.success(),
        "reparent failed: {}",
        reparent.stderr
    );

    // The mirror has not moved: it still shows the child's parent link
    // pointing at EM-1, which is now stale.
    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on(
        "GET",
        &issues_path(0),
        200,
        &serde_json::Value::Array(vec![
            issue_with_no_links("EM-1", "Epic one"),
            issue_with_no_links("EM-2", "Epic two"),
            child_with_parent_link("EM-3", "Child", "EM-1"),
        ])
        .to_string(),
    );
    write_remote_config(&workspace, &server.base_url());

    let run = run_br_with_env(
        &workspace,
        ["--json", "remote", "status"],
        TOKEN,
        "remote_status",
    );
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

    let payload: Value =
        serde_json::from_str(run.stdout.trim()).unwrap_or_else(|e| panic!("{e}: {}", run.stdout));

    assert!(
        payload["plan"]["creates"]
            .as_array()
            .expect("creates")
            .is_empty(),
        "external_ref survives the rename automatically; nothing should need creating: {}",
        run.stdout
    );
    assert!(
        payload["plan"]["dangling"]
            .as_array()
            .expect("dangling")
            .is_empty(),
        "the child's ref still resolves; nothing should be dangling: {}",
        run.stdout
    );
    assert!(
        payload["plan"]["tombstoned"]
            .as_array()
            .expect("tombstoned")
            .is_empty(),
        "the rename's own tombstone carries no external_ref, so it must not \
         be reported as a mirrored tombstone: {}",
        run.stdout
    );
    let link_changes = payload["plan"]["link_changes"]
        .as_array()
        .expect("link_changes");
    assert!(
        !link_changes.is_empty(),
        "the reparent must surface as a link change: {}",
        run.stdout
    );

    assert!(
        run.stdout.contains("EM-1") && run.stdout.contains("EM-2"),
        "the plan must name both the old and the new parent link: {}",
        run.stdout
    );
}
