//! Adopting a web-UI issue, end to end against the loopback mock.
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
//! **An issue that cannot be adopted is named.** A candidate whose parent is
//! unreadable is deferred, and a deferral nobody prints is a user watching
//! the same issue not arrive, run after run.
//!
//! The verbs `br remote pull`/`sync` that will drive this in production
//! belong to a later task and are still stubs, so these tests call
//! `common::remote_harness::adopt_everything`, which assembles the pipeline
//! in the order the CLI will. What is under test is the library, and the mock
//! server is what proves the claim this feature rests on: **zero writes reach
//! the mirror during an adoption.**

use crate::common;

use beads::remote::adopt::{
    AdoptContext, DeferralReason, adopt_one, classify_adoption, import_comments, stamp_adoption,
};
use beads::remote::comments::{LocalComment, YOUTRACK_AUTHOR, fetch_comments, plan_comment_sync};
use beads::remote::config::RemoteConfig;
use beads::remote::youtrack::fetch::fetch_snapshot;
use beads::remote::youtrack::links::LinkTypes;
use common::cli::{BrWorkspace, run_br_with_env};
use common::mock_http::MockServer;
use common::remote_harness::{AdoptionRun, adopt_everything, client, id_config, open_storage};
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

fn beads_dir(workspace: &BrWorkspace) -> std::path::PathBuf {
    workspace.root.join(".beads")
}

fn run_adoption(workspace: &BrWorkspace, server: &MockServer) -> AdoptionRun {
    adopt_everything(&beads_dir(workspace), &server.base_url(), "em").expect("adoption")
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
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    let run = run_adoption(&workspace, &server);

    assert_eq!(run.adopted.len(), 1, "only EM-11 is unpaired");
    assert!(run.deferred.is_empty());
    assert!(run.refused.is_empty());
    let adopted = &run.adopted[0].bead_id;
    assert_eq!(
        adopted,
        &format!("{parent_bead}.1"),
        "the adoptee must be minted AS the parent's child, not flat then reparented"
    );

    assert!(
        server.write_requests().is_empty(),
        "adoption is local-only; the mirror saw {:?}",
        server.write_requests()
    );

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
        .find(|i| i["id"] == adopted.as_str())
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
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    let run = run_adoption(&workspace, &server);

    assert_eq!(run.adopted.len(), 3, "one run adopts the whole chain");
    assert_eq!(run.id_map["EM-11"], format!("{parent_bead}.1"));
    assert_eq!(run.id_map["EM-12"], format!("{parent_bead}.1.1"));
    assert_eq!(run.id_map["EM-13"], format!("{parent_bead}.1.1.1"));
    assert!(
        server.write_requests().is_empty(),
        "adoption is local-only; the mirror saw {:?}",
        server.write_requests()
    );
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

    let run = run_adoption(&workspace, &server);

    assert!(
        run.adopted.is_empty(),
        "flattening EM-11 would cost a rename later: {:?}",
        run.adopted
    );
    // EM-99 itself is refused one layer earlier than `classify_adoption`:
    // `fetch_snapshot` diverts an issue it cannot read into
    // `snapshot.unmappable`, so it never reaches the pairing at all and never
    // becomes a candidate. That refusal is reported through
    // `plan.refused_adoptions` — see `remote_status.rs` — and
    // `classify_adoption`'s own refusal is defence in depth against a future
    // caller that skips that fetch, unit-tested in `remote::adopt`.
    assert!(
        run.refused.is_empty(),
        "the fetch already diverted EM-99, so no candidate is left to refuse: {:?}",
        run.refused
    );

    // What this test exists for: the *consequence* of that refusal on another
    // issue is reported too. Nothing else would ever mention EM-11.
    assert_eq!(
        run.deferred.len(),
        1,
        "a deferral nobody prints is an issue that never arrives, silently"
    );
    assert_eq!(run.deferred[0].id_readable, "EM-11");
    assert_eq!(run.deferred[0].parent_id, "EM-99");
    assert_eq!(run.deferred[0].reason, DeferralReason::ParentNotAdoptable);
    let rendered = run.deferred[0].render();
    assert!(rendered.contains("EM-11"), "{rendered}");
    assert!(
        rendered.contains("EM-99"),
        "must name the blocker: {rendered}"
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
        r#"[{"id":"3-11","idReadable":"EM-11","summary":"Typed in the web UI",
             "description":"body **markdown**","updated":1000,"commentsCount":3,
             "tags":[{"id":"6-1","name":"mirror"},{"id":"6-2","name":"ui"}],"links":[],
             "customFields":[
               {"name":"Type","value":{"name":"Bug"}},
               {"name":"State","value":{"name":"In Progress"}},
               {"name":"Priority","value":{"name":"Critical"}},
               {"name":"Design","value":{"text":"design para 1\n\ndesign para 2"}},
               {"name":"Acceptance Criteria","value":{"text":"it works"}},
               {"name":"Notes","value":{"text":"a note"}},
               {"name":"Close Reason","value":{"text":"not yet"}}]}]"#,
    );
    server.on(
        "GET",
        "/api/issues/EM-11/comments?fields=id,text,author(login),created&$top=500",
        200,
        r#"[{"id":"4-1","text":"a human said this","author":{"login":"kate"},"created":1000},
            {"id":"4-2","text":"[br]\nbr's own echo","author":{"login":"integration"},"created":1001},
            {"id":"4-3","text":"and this","author":{"login":"kate"},"created":1002}]"#,
    );
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    let cfg = RemoteConfig::load(&beads_dir(&workspace)).expect("remote.yaml");
    let http = client(&server.base_url());
    let types = LinkTypes::resolve(&http).expect("link types");
    let snapshot = fetch_snapshot(&http, &cfg, &types).expect("fetch");
    let remote = snapshot
        .issues
        .iter()
        .find(|i| i.id_readable == "EM-11")
        .expect("EM-11");
    let candidate = classify_adoption(&cfg, remote).expect("mapped");

    let mut storage = open_storage(&beads_dir(&workspace));
    let ctx = AdoptContext {
        id_config: &id_config("em"),
        actor: "youtrack",
    };
    let outcome = adopt_one(&mut storage, &ctx, &candidate, None).expect("adopt_one");
    let comments = fetch_comments(&http, "EM-11").expect("comments");
    import_comments(&mut storage, &outcome.bead_id, &comments).expect("import_comments");

    let bead = storage
        .get_issue(&outcome.bead_id)
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

    let mut labels = storage.get_labels_for_export().expect("labels")[&outcome.bead_id].clone();
    labels.sort();
    assert_eq!(labels, ["mirror", "ui"], "tags arrive as labels");

    let local = storage.get_comments(&outcome.bead_id).expect("comments");
    assert_eq!(
        local.len(),
        2,
        "br's own [br]-marked echo is not imported as human content: {local:?}"
    );
    for comment in &local {
        assert_eq!(
            comment.author, YOUTRACK_AUTHOR,
            "the author is what stops the next push sending it back"
        );
    }

    // The claim that author actually buys: the existing echo suppression sees
    // these as already-remote and plans no push at all.
    let planned = plan_comment_sync(
        &local
            .iter()
            .map(|c| LocalComment {
                author: c.author.clone(),
                text: c.body.clone(),
            })
            .collect::<Vec<_>>(),
        &comments,
    );
    assert!(
        planned.to_push.is_empty(),
        "an adopted comment must never be pushed back: {:?}",
        planned.to_push
    );

    assert!(
        server.write_requests().is_empty(),
        "adoption is local-only; the mirror saw {:?}",
        server.write_requests()
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
    // The `Beads ID` stamp is the one write an adoption would ever attempt,
    // and it is the write this test kills: the connection is dropped without
    // a response, exactly as a crash or a severed link would look.
    server.on_drop("POST", "/api/issues/EM-11?fields=idReadable");
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    let mut run = run_adoption(&workspace, &server);
    assert_eq!(run.adopted.len(), 1);

    let http = client(&server.base_url());
    stamp_adoption(&mut run.adopted[0], "EM-11", || {
        http.post_json(
            "/api/issues/EM-11?fields=idReadable",
            &serde_json::json!({"customFields": []}),
            "issue",
        )
        .map(|_| ())
    });
    assert!(
        run.adopted[0].stamp_failed,
        "the stamp must be recorded as failed, not swallowed"
    );

    // The second run sees exactly the same fetch. The bead already carries
    // EM-11, so pairing claims it and adoption is offered nothing.
    let second = run_adoption(&workspace, &server);
    assert!(
        second.adopted.is_empty(),
        "a bead that already carries the ref is paired, not re-adopted: {:?}",
        second.adopted
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
        carriers[0]["id"], run.adopted[0].bead_id,
        "and it is the one the first run created"
    );
}
