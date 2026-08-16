//! The first-run gate, `--dry-run`, the push-side refusal, and `sync`'s order.
//!
//! **The gate is about irreversibility, not size.** `br remote` has no code
//! path that deletes a YouTrack issue, so a first run against the wrong
//! project leaves every issue it created to be deleted by hand. A run where no
//! bead is paired with the configured project is therefore refused unless it
//! says `--confirm-initial`; a workspace where even one bead is already paired
//! is not a first run, because the blast radius of a wrong project is already
//! visible to the operator.
//!
//! **`--dry-run` issues zero writes, not few.** `write_requests()` sees every
//! state-changing method the process actually sent, so this is the same proof
//! `status` uses rather than a promise about the code.

use crate::common;

use common::cli::{BrWorkspace, run_br_with_env};
use common::mock_http::MockServer;
use common::youtrack_fixtures::{
    LINK_TYPES, LINK_TYPES_PATH, PROJECTS, PROJECTS_PATH, issues_path, write_remote_config,
};

const TOKEN: [(&str, &str); 1] = [("BR_YOUTRACK_TOKEN", "t")];

fn beads_dir(workspace: &BrWorkspace) -> std::path::PathBuf {
    workspace.root.join(".beads")
}

/// One mirrored issue whose every stock field agrees with a `--priority 2`
/// task of the same title.
fn mirrored(id: u32, summary: &str, links: &str) -> String {
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

fn init(workspace: &BrWorkspace) {
    let run = run_br_with_env(workspace, ["init", "--prefix", "em"], TOKEN, "init");
    assert!(run.status.success(), "br init failed: {}", run.stderr);
}

fn create(workspace: &BrWorkspace, args: Vec<&str>) -> String {
    let run = run_br_with_env(workspace, args, TOKEN, "create");
    assert!(run.status.success(), "create failed: {}", run.stderr);
    common::cli::parse_created_id(&run.stdout)
}

/// Three unpaired beads and a mirror holding nothing at all — a first run.
fn first_run_workspace(server: &MockServer) -> BrWorkspace {
    let workspace = BrWorkspace::new();
    init(&workspace);
    for title in ["One", "Two", "Three"] {
        create(&workspace, vec!["create", title, "--priority", "2"]);
    }
    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on("GET", PROJECTS_PATH, 200, PROJECTS);
    server.on("GET", &issues_path(0), 200, "[]");
    write_remote_config(&beads_dir(&workspace), &server.base_url());
    workspace
}

#[test]
fn e2e_push_refuses_a_first_run_and_writes_nothing() {
    let _log = common::test_log("e2e_push_refuses_a_first_run_and_writes_nothing");
    let server = MockServer::start();
    let workspace = first_run_workspace(&server);

    let run = run_br_with_env(&workspace, ["remote", "push"], TOKEN, "push_gated");

    assert!(
        !run.status.success(),
        "must exit non-zero: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    let output = format!("{}{}", run.stdout, run.stderr);
    assert!(
        output.contains("--confirm-initial"),
        "must name the flag that lifts it: {output}"
    );
    assert!(
        output.contains('3'),
        "must say how many it would have created: {output}"
    );
    assert!(
        run.stdout.contains("create 3 issue(s)"),
        "and must print what it refused: {}",
        run.stdout
    );
    assert!(
        server.write_requests().is_empty(),
        "a gated run must write nothing: {:?}",
        server.write_requests()
    );
}

#[test]
fn e2e_sync_refuses_a_first_run_too() {
    let _log = common::test_log("e2e_sync_refuses_a_first_run_too");
    let server = MockServer::start();
    let workspace = first_run_workspace(&server);

    let run = run_br_with_env(&workspace, ["remote", "sync"], TOKEN, "sync_gated");

    assert!(!run.status.success(), "must exit non-zero: {}", run.stdout);
    let output = format!("{}{}", run.stdout, run.stderr);
    assert!(output.contains("--confirm-initial"), "{output}");
    assert!(
        server.write_requests().is_empty(),
        "sync's push half is gated exactly as push is: {:?}",
        server.write_requests()
    );
}

#[test]
fn e2e_one_existing_pairing_lifts_the_gate() {
    let _log = common::test_log("e2e_one_existing_pairing_lifts_the_gate");
    let server = MockServer::start();
    let workspace = BrWorkspace::new();
    init(&workspace);
    create(
        &workspace,
        vec![
            "create",
            "Mirrored",
            "--priority",
            "2",
            "--external-ref",
            "EM-1",
        ],
    );
    create(&workspace, vec!["create", "Brand new", "--priority", "2"]);

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on("GET", PROJECTS_PATH, 200, PROJECTS);
    server.on(
        "GET",
        &issues_path(0),
        200,
        &format!("[{}]", mirrored(1, "Mirrored", "")),
    );
    server.on(
        "POST",
        "/api/issues?fields=id,idReadable",
        200,
        r#"{"id":"3-2","idReadable":"EM-2"}"#,
    );
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    let run = run_br_with_env(&workspace, ["remote", "push"], TOKEN, "push_ungated");

    assert!(
        run.status.success(),
        "something is already mirrored, so this is not a first run: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("created EM-2"),
        "and it did the work: {}",
        run.stdout
    );
}

#[test]
fn e2e_dry_run_writes_nothing_on_every_mutating_verb() {
    let _log = common::test_log("e2e_dry_run_writes_nothing_on_every_mutating_verb");
    let server = MockServer::start();
    let workspace = BrWorkspace::new();
    init(&workspace);
    create(&workspace, vec!["create", "Brand new", "--priority", "2"]);

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on("GET", PROJECTS_PATH, 200, PROJECTS);
    // An unclaimed issue to adopt and an unpaired bead to create, so every
    // verb has real work it is declining to do.
    server.on(
        "GET",
        &issues_path(0),
        200,
        &format!("[{}]", mirrored(7, "Unclaimed", "")),
    );
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    for (label, args) in [
        ("pull_dry", vec!["remote", "pull", "--dry-run"]),
        (
            "push_dry",
            vec!["remote", "push", "--confirm-initial", "--dry-run"],
        ),
        (
            "sync_dry",
            vec!["remote", "sync", "--confirm-initial", "--dry-run"],
        ),
    ] {
        let run = run_br_with_env(&workspace, args, TOKEN, label);
        assert!(
            run.status.success(),
            "{label} failed: stdout={} stderr={}",
            run.stdout,
            run.stderr
        );
        assert!(
            run.stdout.contains("nothing was written"),
            "{label} must say so: {}",
            run.stdout
        );
        assert!(
            run.stdout.contains("create 1 issue(s)") && run.stdout.contains("adoption candidates"),
            "{label} must print the plan it declined to run: {}",
            run.stdout
        );
        assert!(
            server.write_requests().is_empty(),
            "{label} must issue zero writes, not few: {:?}",
            server.write_requests()
        );
    }

    // And nothing was adopted locally either.
    let listed = run_br_with_env(&workspace, ["--json", "list"], TOKEN, "list");
    let paired: Vec<_> = common::cli::parse_list_issues(&listed.stdout)
        .into_iter()
        .filter(|issue| issue["external_ref"] != serde_json::Value::Null)
        .collect();
    assert!(
        paired.is_empty(),
        "a dry run must not write locally either: {paired:?}"
    );
}

#[test]
fn e2e_a_push_meeting_an_unmapped_type_names_the_config_key() {
    let _log = common::test_log("e2e_a_push_meeting_an_unmapped_type_names_the_config_key");
    let server = MockServer::start();
    let workspace = BrWorkspace::new();
    init(&workspace);
    create(
        &workspace,
        vec![
            "create",
            "A spike",
            "--priority",
            "2",
            "--type",
            "spike",
            "--external-ref",
            "EM-1",
        ],
    );

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on("GET", PROJECTS_PATH, 200, PROJECTS);
    server.on(
        "GET",
        &issues_path(0),
        200,
        &format!("[{}]", mirrored(1, "A spike", "")),
    );
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    let run = run_br_with_env(&workspace, ["remote", "push"], TOKEN, "push_unmapped");

    assert!(!run.status.success(), "must refuse: {}", run.stdout);
    let output = format!("{}{}", run.stdout, run.stderr);
    assert!(
        output.contains("spike"),
        "must name the offending value: {output}"
    );
    assert!(
        output.contains("type_map"),
        "must name the map to extend: {output}"
    );
    assert!(
        server.write_requests().is_empty(),
        "the refusal comes before any write: {:?}",
        server.write_requests()
    );
}

/// `sync` is `pull` then `push`, in that order. Pull first so an adoption that
/// lands this run is already a bead by the time push computes its link diff —
/// otherwise the newly-adopted issue looks unpaired to the push half and the
/// link differ tries to remove the very link the pull imported.
#[test]
fn e2e_sync_pulls_before_it_pushes() {
    let _log = common::test_log("e2e_sync_pulls_before_it_pushes");
    let server = MockServer::start();
    let workspace = BrWorkspace::new();
    init(&workspace);
    create(
        &workspace,
        vec![
            "create",
            "Mirrored parent",
            "--priority",
            "2",
            "--external-ref",
            "EM-10",
        ],
    );

    let child_link = r#"{"id":"173-3t","direction":"INWARD","linkType":{"id":"173-3","name":"Subtask"},
                         "issues":[{"id":"3-10","idReadable":"EM-10"}]}"#;
    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on("GET", PROJECTS_PATH, 200, PROJECTS);
    server.on(
        "GET",
        &issues_path(0),
        200,
        &format!(
            "[{},{}]",
            mirrored(
                10,
                "Mirrored parent",
                r#"{"id":"173-3s","direction":"OUTWARD","linkType":{"id":"173-3","name":"Subtask"},
                    "issues":[{"id":"3-11","idReadable":"EM-11"}]}"#
            ),
            mirrored(11, "Typed in the web UI", child_link)
        ),
    );
    server.on(
        "POST",
        "/api/issues/EM-11?fields=idReadable",
        200,
        r#"{"idReadable":"EM-11"}"#,
    );
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    let run = run_br_with_env(&workspace, ["remote", "sync"], TOKEN, "sync");
    assert!(
        run.status.success(),
        "sync failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(run.stdout.contains("adopted EM-11"), "{}", run.stdout);

    let requests = server.requests();
    let first_write = requests
        .iter()
        .position(|request| request.method == "POST")
        .expect("the pull half adopted something and stamped it");
    let last_fetch = requests
        .iter()
        .rposition(|request| request.path == issues_path(0))
        .expect("the push half refetched");
    assert!(
        first_write < last_fetch,
        "the pull half must complete before the push half reads: {requests:?}"
    );

    // The consequence that ordering exists for: the push half saw a bead that
    // already carries its parent link, so it removed nothing.
    let deletes: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "DELETE")
        .collect();
    assert!(
        deletes.is_empty(),
        "push must not delete a link the pull just imported: {deletes:?}"
    );
}

/// Everything a first push of a parent, a child, a label and a comment can
/// legitimately ask for — including a *second* issue-list read, which is the
/// re-plan.
fn route_first_push(server: &MockServer) {
    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on("GET", PROJECTS_PATH, 200, PROJECTS);
    // Empty on the first read, and holding both new issues on the re-plan —
    // which is what a real mirror would report once the creates landed.
    server.on_sequence(
        "GET",
        &issues_path(0),
        vec![
            (200, "[]".to_string()),
            (
                200,
                format!(
                    "[{},{}]",
                    mirrored(1, "Parent", ""),
                    mirrored(2, "Child", "")
                ),
            ),
        ],
    );
    server.on_sequence(
        "POST",
        "/api/issues?fields=id,idReadable",
        vec![
            (200, r#"{"id":"3-1","idReadable":"EM-1"}"#.to_string()),
            (200, r#"{"id":"3-2","idReadable":"EM-2"}"#.to_string()),
        ],
    );
    server.on("GET", "/api/tags?fields=id,name&$top=500", 200, "[]");
    server.on(
        "POST",
        "/api/tags?fields=id,name",
        200,
        r#"{"id":"6-1","name":"mirror"}"#,
    );
    server.on(
        "GET",
        "/api/issues/EM-2/comments?fields=id,text,author(login),created&$top=500",
        200,
        "[]",
    );
    server.on(
        "POST",
        "/api/issues/EM-2/comments?fields=id",
        200,
        r#"{"id":"4-1"}"#,
    );
    server.on(
        "POST",
        "/api/issues/EM-2?fields=idReadable",
        200,
        r#"{"idReadable":"EM-2"}"#,
    );
    // The parent link is owned by the child and written through the `…t` id.
    server.on(
        "POST",
        "/api/issues/EM-2/links/173-3t/issues?fields=idReadable",
        200,
        r#"{"idReadable":"EM-1"}"#,
    );
}

/// A first push must mirror the issues it creates **whole**, in the same run.
///
/// The plan a push starts from is built before any issue exists, so every one
/// of its sections — `field_changes`, `link_changes`, and the comment work —
/// covers *paired* beads only, and `issue_create_body` carries no `tags`
/// either. Without a second planning pass a first
/// `br remote push --confirm-initial` mirrors every issue's fields correctly
/// and its links, comments and labels not at all, then exits 0 saying nothing
/// about the second run needed to finish. A half-mirrored project that reports
/// success is the outcome worth paying an extra fetch for.
#[test]
fn e2e_a_first_push_mirrors_links_comments_and_labels_in_the_same_run() {
    let _log =
        common::test_log("e2e_a_first_push_mirrors_links_comments_and_labels_in_the_same_run");
    let server = MockServer::start();
    let workspace = BrWorkspace::new();
    init(&workspace);
    let parent = create(&workspace, vec!["create", "Parent", "--priority", "2"]);
    let child = create(&workspace, vec!["create", "Child", "--priority", "2"]);
    for args in [
        vec!["dep", "add", &child, &parent, "--type", "parent-child"],
        vec!["label", "add", &child, "mirror"],
        vec!["comments", "add", &child, "ship it"],
    ] {
        let run = run_br_with_env(&workspace, args, TOKEN, "setup");
        assert!(run.status.success(), "setup failed: {}", run.stderr);
    }

    route_first_push(&server);
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    let run = run_br_with_env(
        &workspace,
        ["remote", "push", "--confirm-initial"],
        TOKEN,
        "first_push",
    );
    assert!(
        run.status.success(),
        "push failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("re-planned after 2 new pairing(s)"),
        "the second pass must be reported, not silent: {}",
        run.stdout
    );

    let writes = server.write_requests();
    let sent_to = |path: &str| writes.iter().find(|request| request.path == path);
    assert!(
        sent_to("/api/issues/EM-2/links/173-3t/issues?fields=idReadable").is_some(),
        "the parent link was never mirrored: {writes:?}"
    );
    let comment = sent_to("/api/issues/EM-2/comments?fields=id")
        .unwrap_or_else(|| panic!("the comment was never mirrored: {writes:?}"));
    assert!(comment.body.contains("ship it"), "{}", comment.body);
    let tagged = sent_to("/api/issues/EM-2?fields=idReadable")
        .unwrap_or_else(|| panic!("the label was never mirrored: {writes:?}"));
    assert!(
        tagged.body.contains("6-1"),
        "the resolved tag id must ride the issue update: {}",
        tagged.body
    );

    // And exactly two creates: the re-plan must not create anything.
    let creates = writes
        .iter()
        .filter(|request| request.path == "/api/issues?fields=id,idReadable")
        .count();
    assert_eq!(creates, 2, "the second pass has no create step: {writes:?}");
}

/// The push half doing real work: a field a local edit won, and a bead comment
/// that has never crossed.
#[test]
fn e2e_a_push_writes_the_changed_fields_and_the_new_comments() {
    let _log = common::test_log("e2e_a_push_writes_the_changed_fields_and_the_new_comments");
    let server = MockServer::start();
    let workspace = BrWorkspace::new();
    init(&workspace);
    let bead = create(
        &workspace,
        vec![
            "create",
            "Local title",
            "--priority",
            "2",
            "--external-ref",
            "EM-1",
        ],
    );
    let commented = run_br_with_env(
        &workspace,
        ["comments", "add", &bead, "ship it"],
        TOKEN,
        "comment",
    );
    assert!(
        commented.status.success(),
        "comment failed: {}",
        commented.stderr
    );

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on("GET", PROJECTS_PATH, 200, PROJECTS);
    server.on(
        "GET",
        &issues_path(0),
        200,
        &format!("[{}]", mirrored(1, "Remote title", "")),
    );
    server.on(
        "GET",
        "/api/issues/EM-1/comments?fields=id,text,author(login),created&$top=500",
        200,
        "[]",
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

    let run = run_br_with_env(&workspace, ["remote", "push"], TOKEN, "push");
    assert!(
        run.status.success(),
        "push failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );

    let writes = server.write_requests();
    let update = writes
        .iter()
        .find(|request| request.path == "/api/issues/EM-1?fields=idReadable")
        .unwrap_or_else(|| panic!("no field update was sent: {writes:?}"));
    assert!(
        update.body.contains("Local title"),
        "beads is authoritative for a title: {}",
        update.body
    );

    let comment = writes
        .iter()
        .find(|request| request.path == "/api/issues/EM-1/comments?fields=id")
        .unwrap_or_else(|| panic!("no comment was pushed: {writes:?}"));
    assert!(
        comment.body.contains("[br]") && comment.body.contains("ship it"),
        "a pushed comment carries the marker that stops it coming back: {}",
        comment.body
    );
}
