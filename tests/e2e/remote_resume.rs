//! A first run that is interrupted partway, and the run that follows it.
//!
//! This is the one claim the whole batching design exists to make, and it is
//! a claim about *when* a local write happens rather than about what any
//! function returns — which is why it is an e2e against the real binary and
//! not a unit test.
//!
//! `br remote push` persists a bead's `external_ref` immediately after the
//! POST that created its issue, not at the end of the batch and not at the end
//! of the run. Batch it, and an interruption loses every pairing made since
//! the last flush: the next run re-creates all of them, and the mirror
//! silently doubles. Write it per create, and the worst case is one issue
//! created whose ref was not recorded — one duplicate, not N. The two tests
//! below pin exactly that: after a run cut off at the fourth create, precisely
//! three beads carry a ref; and the run after it creates the remaining seven,
//! not thirteen.

use crate::common;

use common::cli::{BrWorkspace, parse_list_issues, run_br_with_env};
use common::mock_http::MockServer;
use common::youtrack_fixtures::{
    LINK_TYPES, LINK_TYPES_PATH, PROJECTS, PROJECTS_PATH, issues_path, write_remote_config,
};
use serde_json::Value;

const TOKEN: [(&str, &str); 1] = [("BR_YOUTRACK_TOKEN", "t")];

/// The path `execute_creates` posts an issue to.
const CREATE_PATH: &str = "/api/issues?fields=id,idReadable";

/// How many beads the first run is asked to mirror.
const BEADS: u32 = 10;

/// How many creates the mock answers before it starts dropping connections.
const SERVED: usize = 3;

fn beads_dir(workspace: &BrWorkspace) -> std::path::PathBuf {
    workspace.root.join(".beads")
}

/// Ten canned create responses, `EM-1` … `EM-10`.
///
/// Distinct on purpose: `external_ref` carries a UNIQUE index, so a mock that
/// answered every create with the same id would fail the *second* pairing for
/// a reason that has nothing to do with what is under test.
fn create_responses(from: u32, count: u32) -> Vec<(u16, String)> {
    (from..from + count)
        .map(|n| (200, format!(r#"{{"id":"3-{n}","idReadable":"EM-{n}"}}"#)))
        .collect()
}

/// A workspace with `BEADS` unpaired beads and a `remote.yaml` pointed at
/// `server`, which answers the reconciliation reads and an empty project.
fn workspace_with_ten_unpaired_beads(server: &MockServer) -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br_with_env(&workspace, ["init", "--prefix", "em"], TOKEN, "init");
    assert!(init.status.success(), "br init failed: {}", init.stderr);

    for n in 1..=BEADS {
        let title = format!("Bead {n:02}");
        let created = run_br_with_env(
            &workspace,
            ["create", &title, "--priority", "2"],
            TOKEN,
            "create",
        );
        assert!(
            created.status.success(),
            "create failed: {}",
            created.stderr
        );
    }

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on("GET", PROJECTS_PATH, 200, PROJECTS);
    server.on("GET", &issues_path(0), 200, "[]");
    write_remote_config(&beads_dir(&workspace), &server.base_url());
    workspace
}

/// Every bead, as `br --json list` reports it.
fn listed(workspace: &BrWorkspace) -> Vec<Value> {
    let run = run_br_with_env(workspace, ["--json", "list"], TOKEN, "list");
    assert!(run.status.success(), "list failed: {}", run.stderr);
    parse_list_issues(&run.stdout)
}

/// `(external_ref, title)` for every bead that carries an in-scope ref.
fn pairings(workspace: &BrWorkspace) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = listed(workspace)
        .iter()
        .filter_map(|issue| {
            let reference = issue["external_ref"].as_str()?;
            let title = issue["title"].as_str()?;
            Some((reference.to_string(), title.to_string()))
        })
        .collect();
    pairs.sort();
    pairs
}

/// How many create POSTs `server` saw.
fn create_posts(server: &MockServer) -> usize {
    server
        .requests()
        .iter()
        .filter(|request| request.method == "POST" && request.path == CREATE_PATH)
        .count()
}

/// Run the interrupted first pass and return the workspace it left behind.
fn interrupted_first_run(server: &MockServer) -> BrWorkspace {
    let workspace = workspace_with_ten_unpaired_beads(server);
    server.on_sequence("POST", CREATE_PATH, create_responses(1, BEADS));
    // Three creates are answered; every request after them is accepted,
    // recorded, and then dropped without a reply — which is what a severed
    // connection or a killed process looks like from the client's side.
    server.on_drop_after("POST", CREATE_PATH, SERVED);

    let first = run_br_with_env(
        &workspace,
        ["remote", "push", "--confirm-initial"],
        TOKEN,
        "push_interrupted",
    );
    assert!(
        !first.status.success(),
        "the drop must surface as a failure: stdout={} stderr={}",
        first.stdout,
        first.stderr
    );
    workspace
}

#[test]
fn e2e_an_interrupted_first_run_resumes_without_duplicating() {
    let _log = common::test_log("e2e_an_interrupted_first_run_resumes_without_duplicating");
    let server = MockServer::start();
    let workspace = interrupted_first_run(&server);

    // Exactly three beads carry a ref: the ones whose POST returned. If the
    // pairing were written at the end of the batch or the end of the run this
    // would be 0, and the next run would create all ten again.
    let paired = pairings(&workspace);
    assert_eq!(
        paired.len(),
        SERVED,
        "external_ref must be persisted per create, not at the end of the run: {paired:?}"
    );
    let refs: Vec<&str> = paired.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(refs, ["EM-1", "EM-2", "EM-3"]);

    // And the run really did stop rather than hammering the remaining seven.
    assert_eq!(
        create_posts(&server),
        SERVED + 1,
        "the failure must end the pass, not be repeated once per remaining bead"
    );
}

#[test]
fn e2e_a_resume_creates_only_the_remainder() {
    let _log = common::test_log("e2e_a_resume_creates_only_the_remainder");
    let first_server = MockServer::start();
    let workspace = interrupted_first_run(&first_server);
    let already = pairings(&workspace);
    assert_eq!(already.len(), SERVED);

    // The second run's mirror holds the three issues the first run created,
    // reported exactly as they now look — so nothing but the remainder is
    // outstanding.
    let resumed = MockServer::start();
    let page: Vec<String> = already
        .iter()
        .map(|(reference, title)| mirrored_issue(reference, title))
        .collect();
    resumed.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    resumed.on("GET", PROJECTS_PATH, 200, PROJECTS);
    resumed.on(
        "GET",
        &issues_path(0),
        200,
        &format!("[{}]", page.join(",")),
    );
    resumed.on_sequence(
        "POST",
        CREATE_PATH,
        create_responses(u32::try_from(SERVED).expect("small") + 1, BEADS - 3),
    );
    write_remote_config(&beads_dir(&workspace), &resumed.base_url());

    let second = run_br_with_env(
        &workspace,
        ["remote", "push", "--confirm-initial"],
        TOKEN,
        "push_resumed",
    );
    assert!(
        second.status.success(),
        "the resume must complete: stdout={} stderr={}",
        second.stdout,
        second.stderr
    );

    assert_eq!(
        create_posts(&resumed),
        usize::try_from(BEADS).expect("small") - SERVED,
        "a resume creates the remainder and nothing else"
    );

    let paired = pairings(&workspace);
    assert_eq!(
        paired.len(),
        usize::try_from(BEADS).expect("small"),
        "the mirror ends with ten issues, not thirteen: {paired:?}"
    );
    let mut refs: Vec<&str> = paired.iter().map(|(id, _)| id.as_str()).collect();
    refs.sort_unstable();
    refs.dedup();
    assert_eq!(
        refs.len(),
        usize::try_from(BEADS).expect("small"),
        "and no issue is claimed twice"
    );
}

// ---------------------------------------------------------------------------
// The other half of the resume story: a create that *was* applied.
//
// The two tests above cover an interruption that left the server untouched.
// The dangerous case is the opposite one — the server applied the create and
// the answer never came back — because the bead has no ref, so the next run
// wants to create it again and the mirror doubles. `HttpClient` deliberately
// does not retry a POST past a 5xx for exactly that reason, so the recovery is
// a check for the write's own effect instead: `issue_create_body` stamps the
// bead id into the issue's `Beads ID` field, and the orphan comes back on the
// next fetch naming the bead it belongs to.
//
// Both verbs have to handle it, and for different reasons. `push` must pair it
// rather than create a second issue; `pull` must pair it rather than *adopt*
// it, which would mint a second bead for one issue.
// ---------------------------------------------------------------------------

/// The issue a create applied before its 503, as the next fetch reports it:
/// unpaired, and carrying `Beads ID`.
fn orphaned_issue(id_readable: &str, summary: &str, beads_id: &str) -> String {
    let internal = id_readable.rsplit('-').next().unwrap_or("0");
    format!(
        r#"{{"id":"3-{internal}","idReadable":"{id_readable}","summary":"{summary}",
             "updated":1000,"commentsCount":0,"tags":[],"links":[],
             "customFields":[
               {{"name":"Type","value":{{"name":"Task"}}}},
               {{"name":"State","value":{{"name":"Open"}}}},
               {{"name":"Priority","value":{{"name":"Major"}}}},
               {{"name":"Beads ID","value":"{beads_id}"}},
               {{"name":"Assignee","value":null}}]}}"#
    )
}

/// A workspace with one bead already paired (so the gate is lifted) and one
/// unpaired bead whose create is about to be applied-then-lost.
fn workspace_with_one_lost_create(server: &MockServer) -> (BrWorkspace, String) {
    let workspace = BrWorkspace::new();
    let init = run_br_with_env(&workspace, ["init", "--prefix", "em"], TOKEN, "init");
    assert!(init.status.success(), "br init failed: {}", init.stderr);
    for args in [
        vec![
            "create",
            "Already mirrored",
            "--priority",
            "2",
            "--external-ref",
            "EM-1",
        ],
        vec!["create", "Lost one", "--priority", "2"],
    ] {
        let created = run_br_with_env(&workspace, args, TOKEN, "create");
        assert!(
            created.status.success(),
            "create failed: {}",
            created.stderr
        );
    }

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on("GET", PROJECTS_PATH, 200, PROJECTS);
    server.on(
        "GET",
        &issues_path(0),
        200,
        &format!("[{}]", mirrored_issue("EM-1", "Already mirrored")),
    );
    // The server applied the create and then failed. A POST is never retried
    // past a 5xx, so this surfaces rather than duplicating.
    server.on(
        "POST",
        CREATE_PATH,
        503,
        r#"{"error":"Service Unavailable"}"#,
    );
    write_remote_config(&beads_dir(&workspace), &server.base_url());

    let first = run_br_with_env(
        &workspace,
        ["remote", "push"],
        TOKEN,
        "push_applied_then_503",
    );
    assert!(
        !first.status.success(),
        "the 503 must surface: stdout={} stderr={}",
        first.stdout,
        first.stderr
    );
    let posts = server
        .requests()
        .iter()
        .filter(|request| request.path == CREATE_PATH)
        .count();
    assert_eq!(posts, 1, "a POST that may have applied is never repeated");

    // The bead the server created an issue for, and never learned the id of.
    let orphan = listed(&workspace)
        .into_iter()
        .find(|issue| issue["external_ref"] == Value::Null)
        .and_then(|issue| issue["id"].as_str().map(str::to_string))
        .expect("the lost bead is still unpaired");
    (workspace, orphan)
}

/// The mock the run *after* the lost create sees: `EM-1` still paired, and the
/// issue the 503 hid, unpaired but stamped with the bead it belongs to.
fn server_holding_the_orphan(orphan: &str) -> MockServer {
    let server = MockServer::start();
    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on("GET", PROJECTS_PATH, 200, PROJECTS);
    server.on(
        "GET",
        &issues_path(0),
        200,
        &format!(
            "[{},{}]",
            mirrored_issue("EM-1", "Already mirrored"),
            orphaned_issue("EM-2", "Lost one", orphan)
        ),
    );
    // Routed so that a *duplicate* create succeeds and is observable, rather
    // than failing on an unrouted request for an unrelated reason.
    server.on(
        "POST",
        CREATE_PATH,
        200,
        r#"{"id":"3-9","idReadable":"EM-9"}"#,
    );
    server
}

#[test]
fn e2e_a_create_that_applied_before_a_503_is_recovered_by_the_next_push() {
    let _log =
        common::test_log("e2e_a_create_that_applied_before_a_503_is_recovered_by_the_next_push");
    let first_server = MockServer::start();
    let (workspace, orphan) = workspace_with_one_lost_create(&first_server);

    let resumed = server_holding_the_orphan(&orphan);
    write_remote_config(&beads_dir(&workspace), &resumed.base_url());

    let second = run_br_with_env(&workspace, ["remote", "push"], TOKEN, "push_recovering");
    assert!(
        second.status.success(),
        "the recovery run must complete: stdout={} stderr={}",
        second.stdout,
        second.stderr
    );

    assert_eq!(
        create_posts(&resumed),
        0,
        "the issue already exists; creating a second one is the duplicate this \
         whole mechanism exists to prevent"
    );
    assert!(
        second
            .stdout
            .contains(&format!("paired {orphan} with EM-2")),
        "the recovery must be reported: {}",
        second.stdout
    );
    // The one pending create in this plan recovers rather than posting, so
    // nothing here ever calls `issue_create_body` — the only place a project
    // id is used. `execute_creates` must not spend a project lookup a run
    // like this one will never need.
    assert!(
        !resumed
            .requests()
            .iter()
            .any(|request| request.path == PROJECTS_PATH),
        "an all-recoverable create must not resolve the project id: {:?}",
        resumed.requests()
    );

    let paired = pairings(&workspace);
    assert_eq!(
        paired,
        vec![
            ("EM-1".to_string(), "Already mirrored".to_string()),
            ("EM-2".to_string(), "Lost one".to_string()),
        ],
        "two beads, two issues, no duplicate"
    );
}

#[test]
fn e2e_a_lost_create_is_recovered_by_pull_rather_than_adopted_twice() {
    let _log = common::test_log("e2e_a_lost_create_is_recovered_by_pull_rather_than_adopted_twice");
    let first_server = MockServer::start();
    let (workspace, orphan) = workspace_with_one_lost_create(&first_server);
    let before = listed(&workspace).len();

    let resumed = server_holding_the_orphan(&orphan);
    write_remote_config(&beads_dir(&workspace), &resumed.base_url());

    let pulled = run_br_with_env(&workspace, ["remote", "pull"], TOKEN, "pull_recovering");
    assert!(
        pulled.status.success(),
        "the recovery run must complete: stdout={} stderr={}",
        pulled.stdout,
        pulled.stderr
    );

    assert!(
        !pulled.stdout.contains("adopted EM-2"),
        "EM-2 belongs to a bead that already exists; adopting it mints a second \
         bead for one issue: {}",
        pulled.stdout
    );
    assert!(
        pulled
            .stdout
            .contains(&format!("paired {orphan} with EM-2")),
        "the recovery must be reported: {}",
        pulled.stdout
    );
    assert_eq!(
        listed(&workspace).len(),
        before,
        "no bead was added: {}",
        pulled.stdout
    );
    assert_eq!(
        pairings(&workspace),
        vec![
            ("EM-1".to_string(), "Already mirrored".to_string()),
            ("EM-2".to_string(), "Lost one".to_string()),
        ]
    );
}

/// One mirrored issue whose every field already agrees with its bead, so the
/// resume has nothing but creates left to do.
fn mirrored_issue(id_readable: &str, summary: &str) -> String {
    let internal = id_readable.rsplit('-').next().unwrap_or("0");
    format!(
        r#"{{"id":"3-{internal}","idReadable":"{id_readable}","summary":"{summary}",
             "updated":1000,"commentsCount":0,"tags":[],"links":[],
             "customFields":[
               {{"name":"Type","value":{{"name":"Task"}}}},
               {{"name":"State","value":{{"name":"Open"}}}},
               {{"name":"Priority","value":{{"name":"Major"}}}},
               {{"name":"Assignee","value":null}},
               {{"name":"Fix versions","value":[]}}]}}"#
    )
}
