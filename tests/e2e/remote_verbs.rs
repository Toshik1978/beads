//! What `pull` and `push` actually do to each side, end to end.
//!
//! Three claims, each of which is only meaningful as an assertion about the
//! workspace or the wire rather than about a return value:
//!
//! **A locally deleted bead is recorded on the mirror, never deleted from it.**
//! YouTrack deletion is permanent and permission-gated, and a mirror that can
//! permanently destroy the thing it mirrors is a worse tool than one that
//! cannot. The record is a `State` change to `deleted_state` plus a
//! `[br]`-marked comment, and the absence of any `DELETE` is the assertion.
//!
//! **A remote `State` edit wins and lands locally.** `State`, `Priority` and
//! comments are the only three things a human changes in the web UI that flow
//! back; everything else is local-wins unconditionally.
//!
//! **A comment typed in the web UI arrives authored by the integration user**,
//! which is exactly the marker that stops the next push sending it straight
//! back out.

use crate::common;

use common::cli::{BrWorkspace, run_br_with_env};
use common::mock_http::MockServer;
use common::remote_harness::open_storage;
use common::youtrack_fixtures::{LINK_TYPES, LINK_TYPES_PATH, issues_path, write_remote_config};

const TOKEN: [(&str, &str); 1] = [("BR_YOUTRACK_TOKEN", "t")];

/// Far enough in the future to beat any bead's `updated_at`, so the direction
/// rule resolves a difference the remote's way. Epoch **milliseconds**.
const REMOTE_IS_NEWER: i64 = 4_000_000_000_000;

fn beads_dir(workspace: &BrWorkspace) -> std::path::PathBuf {
    workspace.root.join(".beads")
}

/// One mirrored `EM-1`, with the state, comment count and `updated` a test
/// needs to drive a particular direction.
fn mirrored(summary: &str, state: &str, comments_count: u32, updated: i64) -> String {
    format!(
        r#"[{{"id":"3-1","idReadable":"EM-1","summary":"{summary}","updated":{updated},
             "commentsCount":{comments_count},"tags":[],"links":[],
             "customFields":[
               {{"name":"Type","value":{{"name":"Task"}}}},
               {{"name":"State","value":{{"name":"{state}"}}}},
               {{"name":"Priority","value":{{"name":"Major"}}}},
               {{"name":"Assignee","value":null}},
               {{"name":"Fix versions","value":[]}}]}}]"#
    )
}

/// A workspace whose single bead is paired with `EM-1`.
fn paired_workspace(title: &str) -> (BrWorkspace, String) {
    let workspace = BrWorkspace::new();
    let init = run_br_with_env(&workspace, ["init", "--prefix", "em"], TOKEN, "init");
    assert!(init.status.success(), "br init failed: {}", init.stderr);
    let created = run_br_with_env(
        &workspace,
        ["create", title, "--priority", "2", "--external-ref", "EM-1"],
        TOKEN,
        "create",
    );
    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr
    );
    let bead = common::cli::parse_created_id(&created.stdout);
    (workspace, bead)
}

fn run_verb(workspace: &BrWorkspace, verb: &str, label: &str) -> common::cli::BrRun {
    let run = run_br_with_env(workspace, ["remote", verb], TOKEN, label);
    assert!(
        run.status.success(),
        "br remote {verb} failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    run
}

#[test]
fn e2e_a_deleted_bead_is_recorded_on_the_mirror_and_never_deleted() {
    let _log = common::test_log("e2e_a_deleted_bead_is_recorded_on_the_mirror_and_never_deleted");
    let server = MockServer::start();
    let (workspace, bead) = paired_workspace("Obsolete");

    let deleted = run_br_with_env(
        &workspace,
        ["delete", &bead, "--reason", "superseded by the new engine"],
        TOKEN,
        "delete",
    );
    assert!(
        deleted.status.success(),
        "delete failed: {}",
        deleted.stderr
    );

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on(
        "GET",
        &issues_path(0),
        200,
        &mirrored("Obsolete", "Open", 0, 1000),
    );
    server.on(
        "POST",
        "/api/issues/EM-1?fields=idReadable",
        200,
        r#"{"idReadable":"EM-1"}"#,
    );
    server.on(
        "POST",
        "/api/issues/EM-1/comments?fields=id",
        200,
        r#"{"id":"4-1"}"#,
    );
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    // Not a first run: the tombstone still carries its pairing, which is
    // exactly how the deletion reaches the mirror at all.
    let run = run_verb(&workspace, "push", "push_tombstone");
    assert!(
        run.stdout.contains("deleted beads still mirrored"),
        "the plan must name it: {}",
        run.stdout
    );

    let writes = server.write_requests();
    let state = writes
        .iter()
        .find(|request| request.path == "/api/issues/EM-1?fields=idReadable")
        .unwrap_or_else(|| panic!("no state change was sent: {writes:?}"));
    assert!(
        state.body.contains("Won't fix"),
        "the mirror's record of a deletion is deleted_state: {}",
        state.body
    );
    let comment = writes
        .iter()
        .find(|request| request.path == "/api/issues/EM-1/comments?fields=id")
        .unwrap_or_else(|| panic!("no deletion comment was sent: {writes:?}"));
    assert!(
        comment.body.contains("deleted in beads")
            && comment.body.contains("superseded by the new engine"),
        "the comment carries the tombstone's own reason: {}",
        comment.body
    );
    assert!(
        comment.body.contains("[br]"),
        "and the marker that stops it being read back as human content: {}",
        comment.body
    );

    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.method != "DELETE"),
        "br remote must never delete a mirrored issue: {:?}",
        server.requests()
    );
}

#[test]
fn e2e_a_remote_state_edit_wins_and_is_applied_locally() {
    let _log = common::test_log("e2e_a_remote_state_edit_wins_and_is_applied_locally");
    let server = MockServer::start();
    let (workspace, bead) = paired_workspace("Mirrored");

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    // Someone closed it in the web UI, after the bead was last touched.
    server.on(
        "GET",
        &issues_path(0),
        200,
        &mirrored("Mirrored", "Fixed", 0, REMOTE_IS_NEWER),
    );
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    let run = run_verb(&workspace, "pull", "pull_state");
    assert!(
        run.stdout.contains("YouTrack wins"),
        "a remote win is never silent: {}",
        run.stdout
    );

    let storage = open_storage(&beads_dir(&workspace));
    let updated = storage
        .get_issue(&bead)
        .expect("get_issue")
        .expect("the bead");
    assert_eq!(
        updated.status,
        beads::model::Status::Closed,
        "the remote's State must reach the workspace, not just the printed plan"
    );

    assert!(
        server.write_requests().is_empty(),
        "a pull that only applies returnable fields writes nothing remotely: {:?}",
        server.write_requests()
    );
}

#[test]
fn e2e_a_web_ui_comment_arrives_authored_by_the_integration_user() {
    let _log = common::test_log("e2e_a_web_ui_comment_arrives_authored_by_the_integration_user");
    let server = MockServer::start();
    let (workspace, bead) = paired_workspace("Mirrored");

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on(
        "GET",
        &issues_path(0),
        200,
        &mirrored("Mirrored", "Open", 1, 1000),
    );
    server.on(
        "GET",
        "/api/issues/EM-1/comments?fields=id,text,author(login),created&$top=500",
        200,
        r#"[{"id":"4-1","text":"typed in the web UI","author":{"login":"kate"},"created":1000}]"#,
    );
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    let run = run_verb(&workspace, "pull", "pull_comment");
    assert!(
        run.stdout.contains("imported 1 comment(s)"),
        "{}",
        run.stdout
    );

    let storage = open_storage(&beads_dir(&workspace));
    let comments = storage.get_comments(&bead).expect("comments");
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert_eq!(comments[0].body, "typed in the web UI");
    assert_eq!(
        comments[0].author,
        beads::remote::comments::YOUTRACK_AUTHOR,
        "the author is what stops the next push sending it back"
    );
    drop(storage);

    // And it does not come back out. The counts now agree, so the second run
    // does not even ask for the comment list.
    let second = run_verb(&workspace, "push", "push_after_pull");
    assert!(
        server.write_requests().is_empty(),
        "a pulled comment must never be pushed back: {:?}",
        server.write_requests()
    );
    assert!(second.stdout.contains("nothing to do"), "{}", second.stdout);
}
