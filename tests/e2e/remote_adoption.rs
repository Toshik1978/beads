//! Adopting a web-UI issue, end to end through `br remote pull`.
//!
//! These claims are worth an integration test rather than a unit test because
//! each is about what the *workspace* looks like afterwards, not about what a
//! function returned.
//!
//! **A subtask typed into the web UI becomes a child bead, with no
//! tombstone.** beads mints hierarchical ids from parentage, so the only way
//! to get `em-4r2.1` is to create the bead as `em-4r2`'s child in the first
//! place. Creating it flat and reparenting it afterwards is a *rename* — a
//! tombstone at the old id, a `former_ids` entry and a forwarding pointer —
//! for every subtask anyone ever captures in the UI. The absence of a
//! tombstone is therefore the assertion, not a nicety.
//!
//! **An adoption interrupted between the local write and the `Beads ID`
//! stamp re-runs to exactly one bead.** The bead carries `external_ref` from
//! the instant it exists, so the next run pairs it like any other bead and
//! never offers it for adoption again.
//!
//! **An adoptee arrives whole**: description, all four prose fields, tags as
//! labels, and comments authored by the integration user — which is exactly
//! the marker that stops the next push sending them back.
//!
//! **An issue that cannot be adopted is named, and so is the issue its refusal
//! blocks.** A candidate whose parent is unreadable is deferred, and a
//! deferral nobody prints is a user watching the same issue not arrive, run
//! after run.
//!
//! These tests used to drive a hand-assembled copy of the pipeline in
//! `test-support`, because the verb was a stub. They now run the real
//! `br remote pull` as a subprocess, which is the only way to prove the CLI
//! assembles those calls in the right order rather than proving that a second
//! implementation of the order agrees with the first.
//!
//! **The one write an adoption makes is the `Beads ID` stamp**, and the tests
//! below pin that it is the *only* one: no issue is created, no field is
//! written, no link is touched. That stamp is a courtesy — the bead already
//! exists and is already paired — which is why the interruption test can kill
//! it and still expect the run to succeed.

use crate::common;

use beads::model::Status;
use common::cli::{BrWorkspace, run_br_with_env};
use common::mock_http::MockServer;
use common::remote_harness::open_storage;
use common::youtrack_fixtures::{LINK_TYPES, LINK_TYPES_PATH, issues_path, write_remote_config};

const TOKEN: [(&str, &str); 1] = [("BR_YOUTRACK_TOKEN", "t")];

/// One ordinary issue with every stock field set.
fn issue(id: u32, summary: &str, links: &str) -> String {
    format!(
        r#"{{"id":"3-{id}","idReadable":"EM-{id}","summary":"{summary}","updated":1000,
            "commentsCount":0,"tags":[],"links":[{links}],
            "customFields":[
              {{"name":"Type","value":{{"name":"Task"}}}},
              {{"name":"State","value":{{"name":"Open"}}}},
              {{"name":"Priority","value":{{"name":"Major"}}}},
              {{"name":"Assignee","value":null}},
              {{"name":"Fix versions","value":[]}}]}}"#
    )
}

/// The `Subtask` link a child carries naming its parent: written through the
/// `…t` id, reported as `INWARD`. That correspondence is pinned in
/// `src/remote/link_diff.rs`.
fn subtask_of(parent: u32) -> String {
    format!(
        r#"{{"id":"173-3t","direction":"INWARD","linkType":{{"id":"173-3","name":"Subtask"}},
            "issues":[{{"id":"3-{parent}","idReadable":"EM-{parent}"}}]}}"#
    )
}

/// The `Beads ID` stamp an adoption posts, per issue. Routing it keeps the
/// courtesy from failing for want of a canned response.
fn accept_stamps(server: &MockServer, ids: &[u32]) {
    for id in ids {
        server.on(
            "POST",
            &format!("/api/issues/EM-{id}?fields=idReadable"),
            200,
            &format!(r#"{{"idReadable":"EM-{id}"}}"#),
        );
    }
}

fn beads_dir(workspace: &BrWorkspace) -> std::path::PathBuf {
    workspace.root.join(".beads")
}

fn pull(workspace: &BrWorkspace, label: &str) -> common::cli::BrRun {
    let run = run_br_with_env(workspace, ["remote", "pull"], TOKEN, label);
    assert!(
        run.status.success(),
        "pull failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    run
}

/// Every write the mirror saw, which must be `Beads ID` stamps and nothing
/// else. An adoption creates no issue, writes no field and touches no link.
fn assert_only_stamps(server: &MockServer) {
    let unexpected: Vec<_> = server
        .write_requests()
        .into_iter()
        .filter(|request| !request.body.contains("Beads ID"))
        .collect();
    assert!(
        unexpected.is_empty(),
        "an adoption's only remote write is the Beads ID stamp; the mirror saw {unexpected:?}"
    );
}

/// Bead id → `external_ref`, for every bead `br list` reports.
fn by_external_ref(workspace: &BrWorkspace, reference: &str) -> Option<String> {
    let listed = run_br_with_env(workspace, ["--json", "list"], TOKEN, "list");
    assert!(listed.status.success(), "list failed: {}", listed.stderr);
    common::cli::parse_list_issues(&listed.stdout)
        .into_iter()
        .find(|issue| issue["external_ref"] == reference)
        .and_then(|issue| issue["id"].as_str().map(str::to_string))
}

/// A workspace holding one bead already paired with `EM-10`.
fn workspace_with_mirrored_parent() -> (BrWorkspace, String) {
    let workspace = BrWorkspace::new();
    let init = run_br_with_env(&workspace, ["init", "--prefix", "em"], TOKEN, "init");
    assert!(init.status.success(), "br init failed: {}", init.stderr);

    let created = run_br_with_env(
        &workspace,
        [
            "create",
            "Mirrored parent",
            "--priority",
            "2",
            "--external-ref",
            "EM-10",
        ],
        TOKEN,
        "create",
    );
    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr
    );
    let parent_bead = common::cli::parse_created_id(&created.stdout);
    (workspace, parent_bead)
}

#[test]
fn e2e_a_web_ui_subtask_is_adopted_as_a_child_with_no_tombstone() {
    let _log = common::test_log("e2e_a_web_ui_subtask_is_adopted_as_a_child_with_no_tombstone");
    let (workspace, parent_bead) = workspace_with_mirrored_parent();
    let server = MockServer::start();

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on(
        "GET",
        &issues_path(0),
        200,
        &format!(
            "[{},{}]",
            issue(10, "Mirrored parent", ""),
            issue(11, "Typed in the web UI", &subtask_of(10))
        ),
    );
    accept_stamps(&server, &[11]);
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    let run = pull(&workspace, "remote_pull");

    let expected = format!("{parent_bead}.1");
    assert!(
        run.stdout.contains(&format!("adopted EM-11 as {expected}")),
        "the adoptee must be minted AS the parent's child, not flat then reparented: {}",
        run.stdout
    );
    assert_only_stamps(&server);

    // The whole reason the id is minted this way: a flat id plus a reparent
    // is a rename, and a rename leaves a tombstone behind forever.
    let tombstones = run_br_with_env(
        &workspace,
        ["--json", "list", "--status", "tombstone"],
        TOKEN,
        "list_tombstones",
    );
    assert!(
        tombstones.status.success(),
        "list failed: {}",
        tombstones.stderr
    );
    assert!(
        common::cli::parse_list_issues(&tombstones.stdout).is_empty(),
        "a flat create followed by a reparent would have left one: {}",
        tombstones.stdout
    );

    let listed = run_br_with_env(&workspace, ["--json", "list"], TOKEN, "list");
    assert!(listed.status.success(), "list failed: {}", listed.stderr);
    let issues = common::cli::parse_list_issues(&listed.stdout);
    let adoptee = issues
        .iter()
        .find(|i| i["id"] == expected.as_str())
        .unwrap_or_else(|| panic!("adopted bead missing from list: {}", listed.stdout));
    assert_eq!(adoptee["external_ref"], "EM-11", "the bead is paired");
    assert_eq!(adoptee["title"], "Typed in the web UI");
}

#[test]
fn e2e_a_three_deep_web_ui_chain_is_adopted_parent_first_in_one_run() {
    let _log = common::test_log("e2e_a_three_deep_web_ui_chain_is_adopted_parent_first_in_one_run");
    let (workspace, parent_bead) = workspace_with_mirrored_parent();
    let server = MockServer::start();

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    // Deliberately deepest-first on the wire: the ordering must not depend on
    // the fetch happening to arrive conveniently.
    server.on(
        "GET",
        &issues_path(0),
        200,
        &format!(
            "[{},{},{},{}]",
            issue(13, "grandchild", &subtask_of(12)),
            issue(12, "child", &subtask_of(11)),
            issue(11, "under the mirrored bead", &subtask_of(10)),
            issue(10, "Mirrored parent", "")
        ),
    );
    accept_stamps(&server, &[11, 12, 13]);
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    pull(&workspace, "remote_pull");

    assert_eq!(
        by_external_ref(&workspace, "EM-11"),
        Some(format!("{parent_bead}.1"))
    );
    assert_eq!(
        by_external_ref(&workspace, "EM-12"),
        Some(format!("{parent_bead}.1.1"))
    );
    assert_eq!(
        by_external_ref(&workspace, "EM-13"),
        Some(format!("{parent_bead}.1.1.1")),
        "one run adopts the whole chain, parent-first"
    );
    assert_only_stamps(&server);
}

/// A refusal names its issue, and so must the deferral it causes: fixing one
/// `type_map` entry brings in two issues, and a user cannot know that from a
/// message about only one of them.
#[test]
fn e2e_a_refused_parent_defers_its_child_and_both_are_named() {
    let _log = common::test_log("e2e_a_refused_parent_defers_its_child_and_both_are_named");
    let (workspace, _parent_bead) = workspace_with_mirrored_parent();
    let server = MockServer::start();

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on(
        "GET",
        &issues_path(0),
        200,
        &format!(
            "[{},{},{}]",
            issue(10, "Mirrored parent", ""),
            // EM-99's Type has no beads preimage, so it is refused — and its
            // child cannot be created under a parent that does not exist.
            r#"{"id":"3-99","idReadable":"EM-99","summary":"A user story","updated":1000,
                "commentsCount":0,"tags":[],"links":[],
                "customFields":[{"name":"Type","value":{"name":"User Story"}},
                                {"name":"State","value":{"name":"Open"}},
                                {"name":"Priority","value":{"name":"Major"}}]}"#,
            issue(11, "under the unreadable one", &subtask_of(99))
        ),
    );
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    let run = pull(&workspace, "remote_pull");

    assert!(
        by_external_ref(&workspace, "EM-11").is_none(),
        "flattening EM-11 would cost a rename later: {}",
        run.stdout
    );

    // EM-99 is refused one layer earlier than `classify_adoption`:
    // `fetch_snapshot` diverts an issue it cannot read into
    // `snapshot.unmappable`, so it never reaches the pairing and never becomes
    // a candidate. That is the live refusal, and it is reported through the
    // plan's `refused adoptions` section.
    assert!(run.stdout.contains("refused adoptions"), "{}", run.stdout);
    assert!(run.stdout.contains("User Story"), "{}", run.stdout);
    assert!(run.stdout.contains("type_map"), "{}", run.stdout);
    // And exactly once. Two `RefusedAdoption` types exist for the two shapes,
    // and telling the user twice, in two wordings, to make one edit is worse
    // than telling them once. This asserts the *outcome* and not the mechanism:
    // `push_unreported_refusal`, which dedups them, is unreachable on this path
    // because the fetch diverts EM-99 before `classify_adoption` ever sees it,
    // so a single refusal here is also what "the second source never fired"
    // looks like. The dedup itself is unit-tested; this pins that the user sees
    // one line.
    assert_eq!(
        run.stdout.matches("EM-99:").count(),
        1,
        "one issue, one refusal: {}",
        run.stdout
    );

    // What this test exists for: the *consequence* of that refusal on another
    // issue is reported too. Nothing else would ever mention EM-11.
    assert!(
        run.stdout.contains("deferred: EM-11"),
        "a deferral nobody prints is an issue that never arrives, silently: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains("EM-99"),
        "the deferral must name the blocker: {}",
        run.stdout
    );
}

#[test]
fn e2e_an_adoptee_arrives_with_its_prose_labels_and_comments() {
    let _log = common::test_log("e2e_an_adoptee_arrives_with_its_prose_labels_and_comments");
    let (workspace, _parent_bead) = workspace_with_mirrored_parent();
    let server = MockServer::start();

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on(
        "GET",
        &issues_path(0),
        200,
        &format!(
            "[{},{}]",
            issue(10, "Mirrored parent", ""),
            r#"{"id":"3-11","idReadable":"EM-11","summary":"Typed in the web UI",
                "description":"body **markdown**","updated":1000,"commentsCount":3,
                "tags":[{"id":"6-1","name":"mirror"},{"id":"6-2","name":"ui"}],"links":[],
                "customFields":[
                  {"name":"Type","value":{"name":"Bug"}},
                  {"name":"State","value":{"name":"In Progress"}},
                  {"name":"Priority","value":{"name":"Critical"}},
                  {"name":"Design","value":{"text":"design para 1\n\ndesign para 2"}},
                  {"name":"Acceptance Criteria","value":{"text":"it works"}},
                  {"name":"Notes","value":{"text":"a note"}},
                  {"name":"Close Reason","value":{"text":"not yet"}}]}"#
        ),
    );
    server.on(
        "GET",
        "/api/issues/EM-11/comments?fields=id,text,author(login),created&$skip=0&$top=500",
        200,
        r#"[{"id":"4-1","text":"a human said this","author":{"login":"kate"},"created":1000},
            {"id":"4-2","text":"[br]\nbr's own echo","author":{"login":"integration"},"created":1001},
            {"id":"4-3","text":"and this","author":{"login":"kate"},"created":1002}]"#,
    );
    accept_stamps(&server, &[11]);
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    pull(&workspace, "remote_pull");

    let adopted = by_external_ref(&workspace, "EM-11").expect("EM-11 was adopted");
    let storage = open_storage(&beads_dir(&workspace));
    let bead = storage
        .get_issue(&adopted)
        .expect("get_issue")
        .expect("adopted bead");
    assert_eq!(bead.description.as_deref(), Some("body **markdown**"));
    assert_eq!(
        bead.design.as_deref(),
        Some("design para 1\n\ndesign para 2"),
        "a blank line inside a prose field must survive"
    );
    assert_eq!(bead.acceptance_criteria.as_deref(), Some("it works"));
    assert_eq!(bead.notes.as_deref(), Some("a note"));
    assert_eq!(bead.close_reason.as_deref(), Some("not yet"));
    assert_eq!(bead.issue_type, beads::model::IssueType::Bug);
    assert_eq!(bead.status, beads::model::Status::InProgress);
    assert_eq!(bead.priority, beads::model::Priority::HIGH);

    let mut labels = storage.get_labels_for_export().expect("labels")[&adopted].clone();
    labels.sort();
    assert_eq!(labels, ["mirror", "ui"], "tags arrive as labels");

    let local = storage.get_comments(&adopted).expect("comments");
    assert_eq!(
        local.len(),
        2,
        "br's own [br]-marked echo is not imported as human content: {local:?}"
    );
    for comment in &local {
        assert_eq!(
            comment.author,
            beads::remote::comments::YOUTRACK_AUTHOR,
            "the author is what stops the next push sending it back"
        );
    }

    assert_only_stamps(&server);

    // The claim that author actually buys, made where it matters: a second
    // pull, seeing the same three remote comments and the two local ones,
    // plans no push at all — so nothing goes back out.
    let second = pull(&workspace, "remote_pull_again");
    assert!(
        !second.stdout.contains("comment(s) [push]"),
        "an adopted comment must never be pushed back: {}",
        second.stdout
    );
}

#[test]
fn e2e_an_interrupted_adoption_reruns_to_exactly_one_bead() {
    let _log = common::test_log("e2e_an_interrupted_adoption_reruns_to_exactly_one_bead");
    let (workspace, _parent_bead) = workspace_with_mirrored_parent();
    let server = MockServer::start();

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on(
        "GET",
        &issues_path(0),
        200,
        &format!(
            "[{},{}]",
            issue(10, "Mirrored parent", ""),
            issue(11, "Typed in the web UI", &subtask_of(10))
        ),
    );
    // The `Beads ID` stamp is the one write an adoption attempts, and it is
    // the write this test kills: the connection is dropped without a response,
    // exactly as a crash or a severed link would look. The run must still
    // succeed — the bead already exists and is already paired, and rolling it
    // back over a cosmetic field would delete correct work.
    server.on_drop("POST", "/api/issues/EM-11?fields=idReadable");
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    let first = pull(&workspace, "remote_pull");
    let adopted = by_external_ref(&workspace, "EM-11").expect("EM-11 was adopted");
    assert!(
        first.stderr.contains("could not stamp"),
        "the failed stamp must be reported, not swallowed: {}",
        first.stderr
    );

    // The second run sees exactly the same fetch. The bead already carries
    // EM-11, so pairing claims it and adoption is offered nothing.
    let second = pull(&workspace, "remote_pull_again");
    assert!(
        !second.stdout.contains("adopted EM-11"),
        "a bead that already carries the ref is paired, not re-adopted: {}",
        second.stdout
    );

    let listed = run_br_with_env(&workspace, ["--json", "list"], TOKEN, "list");
    assert!(listed.status.success(), "list failed: {}", listed.stderr);
    let carriers: Vec<_> = common::cli::parse_list_issues(&listed.stdout)
        .into_iter()
        .filter(|i| i["external_ref"] == "EM-11")
        .collect();
    assert_eq!(
        carriers.len(),
        1,
        "exactly one bead may carry EM-11: {}",
        listed.stdout
    );
    assert_eq!(
        carriers[0]["id"], adopted,
        "and it is the one the first run created"
    );
    assert_ne!(
        carriers[0]["status"],
        Status::Tombstone.as_str(),
        "and it is live"
    );
}
