//! `br remote init` end to end, driving the real binary against the loopback
//! mock rather than a live tracker.
//!
//! Three claims are worth making at this level, and none of them can be made
//! from a unit test:
//!
//! * the vocabulary pre-flight runs **before the process opens a socket**, so
//!   a knowable config gap costs zero requests rather than dying partway
//!   through a first run;
//! * a project that already matches the maps issues **zero write requests**,
//!   which is what makes `init` safe to re-run;
//! * the reference project's missing values are added with exactly seven
//!   POSTs and nothing else.

use crate::common;

use common::cli::{BrWorkspace, run_br_with_env};
use common::mock_http::MockServer;
use rusqlite::Connection;

const TOKEN: [(&str, &str); 1] = [("BR_YOUTRACK_TOKEN", "t")];

/// Write a `.beads/remote.yaml` pointing at `base_url`.
fn write_remote_config(workspace: &BrWorkspace, base_url: &str) {
    let template = include_str!("../fixtures/remote_em.yaml");
    let yaml = template.replace("https://example.invalid", base_url);
    let path = workspace.root.join(".beads").join("remote.yaml");
    std::fs::write(&path, yaml).expect("write remote.yaml");
}

fn initialised_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br_with_env(&workspace, ["init", "--prefix", "em"], TOKEN, "init");
    assert!(init.status.success(), "br init failed: {}", init.stderr);
    workspace
}

/// `missing_field_prototypes` and `ensure_field_prototypes` →
/// `list_field_prototypes` share one implementation, so they hit the same
/// query and need only one mock.
const FIVE_PROTOTYPES: &str = r#"[{"id":"161-11","name":"Beads ID","fieldType":{"id":"string"}},
        {"id":"161-12","name":"Design","fieldType":{"id":"text"}},
        {"id":"161-13","name":"Acceptance Criteria","fieldType":{"id":"text"}},
        {"id":"161-14","name":"Notes","fieldType":{"id":"text"}},
        {"id":"161-15","name":"Close Reason","fieldType":{"id":"text"}}]"#;

/// The routes a fully-provisioned reference project answers.
///
/// Every paged admin read in this subsystem carries an explicit `&$skip=0`
/// on its first page.
fn provisioned_server() -> MockServer {
    let server = MockServer::start();
    server.on(
        "GET",
        "/api/admin/projects?fields=id,name,shortName&$skip=0&$top=500",
        200,
        r#"[{"id":"0-1","name":"EasyMoney","shortName":"EM"}]"#,
    );
    server.on(
        "GET",
        "/api/admin/customFieldSettings/customFields?fields=id,name,fieldType(id)&$skip=0&$top=500",
        200,
        FIVE_PROTOTYPES,
    );
    server.on(
        "GET",
        "/api/admin/projects/0-1/customFields?fields=id,field(name),defaultValues(name)&$skip=0&$top=200",
        200,
        r#"[{"id":"189-8","field":{"name":"Type"},"defaultValues":[{"name":"Task"}]},
            {"id":"189-9","field":{"name":"State"},"defaultValues":[{"name":"Open"}]},
            {"id":"189-10","field":{"name":"Priority"},"defaultValues":[{"name":"Major"}]},
            {"id":"189-30","field":{"name":"Beads ID"},"defaultValues":[]},
            {"id":"189-31","field":{"name":"Design"},"defaultValues":[]},
            {"id":"189-32","field":{"name":"Acceptance Criteria"},"defaultValues":[]},
            {"id":"189-33","field":{"name":"Notes"},"defaultValues":[]},
            {"id":"189-34","field":{"name":"Close Reason"},"defaultValues":[]}]"#,
    );
    server.on(
        "GET",
        "/api/admin/customFieldSettings/customFields?fields=id,name,instances(id,project(shortName),bundle(id,name))&$skip=0&$top=500",
        200,
        r#"[{"id":"161-1","name":"Type","instances":[
                {"id":"189-8","project":{"shortName":"EM"},"bundle":{"id":"163-1","name":"Types"}}]},
            {"id":"161-2","name":"State","instances":[
                {"id":"189-9","project":{"shortName":"EM"},"bundle":{"id":"165-0","name":"States"}}]},
            {"id":"161-3","name":"Priority","instances":[
                {"id":"189-10","project":{"shortName":"EM"},"bundle":{"id":"167-0","name":"Priorities"}}]}]"#,
    );
    server
}

fn bundle_fields_route(server: &MockServer, body: &str) {
    server.on(
        "GET",
        "/api/admin/projects/0-1/customFields?fields=id,$type,canBeEmpty,field(id,name),bundle(id,name,$type,values(id,name,ordinal,color(id,background,foreground),isResolved))&$skip=0&$top=100",
        200,
        body,
    );
}

/// Bundles already holding every mapped value.
const COMPLETE_BUNDLES: &str = r#"[
    {"id":"189-8","field":{"id":"161-1","name":"Type"},
     "bundle":{"id":"163-1","name":"Types","$type":"EnumBundle","values":[
        {"id":"164-0","name":"Task","ordinal":0},{"id":"164-1","name":"Bug","ordinal":1},
        {"id":"164-2","name":"Feature","ordinal":2},{"id":"164-3","name":"Epic","ordinal":3},
        {"id":"164-4","name":"Chore","ordinal":4},{"id":"164-5","name":"Docs","ordinal":5},
        {"id":"164-6","name":"Question","ordinal":6}]}},
    {"id":"189-9","field":{"id":"161-2","name":"State"},
     "bundle":{"id":"165-0","name":"States","$type":"StateBundle","values":[
        {"id":"166-0","name":"Open","ordinal":0,"isResolved":false},
        {"id":"166-1","name":"In Progress","ordinal":1,"isResolved":false},
        {"id":"166-2","name":"Blocked","ordinal":2,"isResolved":false},
        {"id":"166-3","name":"Deferred","ordinal":3,"isResolved":false},
        {"id":"166-4","name":"Draft","ordinal":4,"isResolved":false},
        {"id":"166-5","name":"Pinned","ordinal":5,"isResolved":false},
        {"id":"166-6","name":"Fixed","ordinal":6,"isResolved":true},
        {"id":"166-7","name":"Won't fix","ordinal":7,"isResolved":true}]}},
    {"id":"189-10","field":{"id":"161-3","name":"Priority"},
     "bundle":{"id":"167-0","name":"Priorities","$type":"EnumBundle","values":[
        {"id":"168-0","name":"Show-stopper","ordinal":0},{"id":"168-1","name":"Critical","ordinal":1},
        {"id":"168-2","name":"Major","ordinal":2},{"id":"168-3","name":"Normal","ordinal":3},
        {"id":"168-4","name":"Minor","ordinal":4}]}}]"#;

/// The stock reference bundles, missing three types and four states.
const REFERENCE_BUNDLES: &str = r#"[
    {"id":"189-8","field":{"id":"161-1","name":"Type"},
     "bundle":{"id":"163-1","name":"Types","$type":"EnumBundle","values":[
        {"id":"164-0","name":"Bug","ordinal":0},{"id":"164-1","name":"Cosmetics","ordinal":1},
        {"id":"164-2","name":"Exception","ordinal":2},{"id":"164-3","name":"Feature","ordinal":3},
        {"id":"164-4","name":"Task","ordinal":4},{"id":"164-5","name":"Usability Problem","ordinal":5},
        {"id":"164-6","name":"Performance Problem","ordinal":6},{"id":"164-7","name":"Epic","ordinal":7}]}},
    {"id":"189-9","field":{"id":"161-2","name":"State"},
     "bundle":{"id":"165-0","name":"States","$type":"StateBundle","values":[
        {"id":"166-0","name":"Submitted","ordinal":0,"isResolved":false},
        {"id":"166-1","name":"Open","ordinal":1,"isResolved":false},
        {"id":"166-2","name":"In Progress","ordinal":2,"isResolved":false},
        {"id":"166-3","name":"Fixed","ordinal":3,"isResolved":true},
        {"id":"166-4","name":"Won't fix","ordinal":4,"isResolved":true}]}},
    {"id":"189-10","field":{"id":"161-3","name":"Priority"},
     "bundle":{"id":"167-0","name":"Priorities","$type":"EnumBundle","values":[
        {"id":"168-0","name":"Show-stopper","ordinal":0},{"id":"168-1","name":"Critical","ordinal":1},
        {"id":"168-2","name":"Major","ordinal":2},{"id":"168-3","name":"Normal","ordinal":3},
        {"id":"168-4","name":"Minor","ordinal":4}]}}]"#;

#[test]
fn e2e_remote_init_refuses_an_unmapped_type_before_any_write() {
    let _log = common::test_log("e2e_remote_init_refuses_an_unmapped_type_before_any_write");
    let workspace = initialised_workspace();
    let server = MockServer::start();

    let created = run_br_with_env(
        &workspace,
        ["create", "Spiky", "--type", "spike"],
        TOKEN,
        "create",
    );
    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr
    );
    write_remote_config(&workspace, &server.base_url());

    let run = run_br_with_env(&workspace, ["remote", "init"], TOKEN, "remote_init");

    assert!(!run.status.success(), "must refuse: {}", run.stdout);
    assert!(
        run.stderr.contains("spike") || run.stdout.contains("spike"),
        "must name the offending value; stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        server.requests().is_empty(),
        "the pre-flight must run before any network access, got {:?}",
        server.requests()
    );
}

/// `priority_map` must be total over 0..4, exactly like `type_map`/
/// `status_map` must be total over their own built-in vocabularies — a
/// priority with nowhere to go cannot be pushed and cannot be recognised
/// coming back. Unlike a type or status, the gap here is caught even before
/// `init`'s own pre-flight runs: `RemoteConfig::load` refuses to parse an
/// incomplete `priority_map` at all, so the run never gets far enough to
/// open a socket.
#[test]
fn e2e_remote_init_refuses_an_incomplete_priority_map_before_any_write() {
    let _log =
        common::test_log("e2e_remote_init_refuses_an_incomplete_priority_map_before_any_write");
    let workspace = initialised_workspace();
    let server = MockServer::start();

    let template = include_str!("../fixtures/remote_em.yaml");
    let yaml = template
        .replace("https://example.invalid", &server.base_url())
        .replace("  4: Minor\n", "");
    let path = workspace.root.join(".beads").join("remote.yaml");
    std::fs::write(&path, yaml).expect("write remote.yaml");

    let run = run_br_with_env(&workspace, ["remote", "init"], TOKEN, "remote_init");

    assert!(!run.status.success(), "must refuse: {}", run.stdout);
    assert!(
        run.stderr.contains("priority_map") || run.stdout.contains("priority_map"),
        "must name the config key; stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        server.requests().is_empty(),
        "an incomplete priority_map must refuse before any network access, got {:?}",
        server.requests()
    );
}

/// The pre-flight is only worth as much as the rows it reads.
///
/// A `git pull` lands new issues in `issues.jsonl` and leaves SQLite behind.
/// Rewriting the exported line here is that situation exactly: the database
/// still says `task`, the file says `spike`. If `br remote` were excluded from
/// the auto-import gate the pre-flight would read the stale database, pass,
/// and provision a project whose maps do not cover the workspace — which is
/// the one failure the pre-flight exists to prevent.
#[test]
fn e2e_remote_init_preflight_sees_types_that_arrived_only_in_the_jsonl() {
    let _log =
        common::test_log("e2e_remote_init_preflight_sees_types_that_arrived_only_in_the_jsonl");
    let workspace = initialised_workspace();
    let server = MockServer::start();

    let created = run_br_with_env(&workspace, ["create", "Ordinary"], TOKEN, "create");
    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr
    );

    let jsonl = workspace.root.join(".beads").join("issues.jsonl");
    let original = std::fs::read_to_string(&jsonl).expect("read issues.jsonl");
    assert!(
        original.contains(r#""issue_type":"task""#),
        "the export should carry the default type: {original}"
    );
    let pulled = original.replace(r#""issue_type":"task""#, r#""issue_type":"spike""#);
    std::fs::write(&jsonl, pulled).expect("write issues.jsonl");
    write_remote_config(&workspace, &server.base_url());

    let run = run_br_with_env(&workspace, ["remote", "init"], TOKEN, "remote_init_stale");

    assert!(
        !run.status.success(),
        "a type present only in the JSONL must still trip the pre-flight: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr.contains("spike") || run.stdout.contains("spike"),
        "must name the value that arrived with the pull; stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        server.requests().is_empty(),
        "still before any network access, got {:?}",
        server.requests()
    );
}

/// `Priority` derives `#[serde(transparent)] Deserialize`, so nothing
/// stops a value outside 0..=4 from sitting in `.beads/beads.db` — `FromStr`
/// only guards `br create`/`br update`, and the SQLite `CHECK` constraint
/// only guards a write, not a row already on disk (a schema migration bug,
/// or a hand edit made with `PRAGMA ignore_check_constraints=ON`, could get
/// one there without ever going through either guard). Reproduce that
/// directly against the database rather than the JSONL: editing the JSONL
/// line instead would go through `IssueValidator` on the next auto-import
/// and be refused before `remote init` ever ran, which would prove nothing
/// about this pre-flight.
///
/// `remote.yaml`'s `priority_map` here is total over 0..=4, so
/// `RemoteConfig::load`'s own `check_total` gate passes clean — this test is
/// worthless if that earlier gate is what actually refuses. The one thing
/// left standing between an out-of-range priority already on disk and a
/// write to YouTrack is `preflight_vocabulary`'s loop over
/// `WorkspaceVocabulary.priorities`, so this is the only test that exercises
/// it end to end through `workspace_vocabulary()`.
#[test]
fn e2e_remote_init_refuses_an_out_of_range_priority_before_any_write() {
    let _log =
        common::test_log("e2e_remote_init_refuses_an_out_of_range_priority_before_any_write");
    let workspace = initialised_workspace();
    let server = MockServer::start();

    let created = run_br_with_env(&workspace, ["create", "Ordinary"], TOKEN, "create");
    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr
    );

    let db_path = workspace.root.join(".beads").join("beads.db");
    let conn = Connection::open(&db_path).expect("open beads.db");
    conn.execute_batch(
        "PRAGMA ignore_check_constraints = ON; \
         UPDATE issues SET priority = 9;",
    )
    .expect("corrupt the stored priority past the CHECK constraint");
    drop(conn);

    write_remote_config(&workspace, &server.base_url());

    let run = run_br_with_env(&workspace, ["remote", "init"], TOKEN, "remote_init");

    assert!(
        !run.status.success(),
        "an out-of-range stored priority must refuse: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr.contains("priority '9'") || run.stdout.contains("priority '9'"),
        "must name the offending value and that it is a priority; stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr.contains("priority_map") || run.stdout.contains("priority_map"),
        "must name the config key that would cover it; stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        server.requests().is_empty(),
        "the pre-flight must run before any network access, got {:?}",
        server.requests()
    );
}

#[test]
fn e2e_remote_init_is_a_no_op_on_a_reconciled_project() {
    let _log = common::test_log("e2e_remote_init_is_a_no_op_on_a_reconciled_project");
    let workspace = initialised_workspace();
    let server = provisioned_server();
    bundle_fields_route(&server, COMPLETE_BUNDLES);
    write_remote_config(&workspace, &server.base_url());

    let run = run_br_with_env(&workspace, ["remote", "init"], TOKEN, "remote_init");

    assert!(
        run.status.success(),
        "init failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        server.write_requests().is_empty(),
        "a reconciled project must be re-runnable for free, got {:?}",
        server.write_requests()
    );
    assert!(
        run.stdout.contains("nothing to do"),
        "must say so plainly: {}",
        run.stdout
    );
}

#[test]
fn e2e_remote_init_adds_exactly_the_seven_missing_values() {
    let _log = common::test_log("e2e_remote_init_adds_exactly_the_seven_missing_values");
    let workspace = initialised_workspace();
    let server = provisioned_server();
    bundle_fields_route(&server, REFERENCE_BUNDLES);
    server.on(
        "POST",
        "/api/admin/customFieldSettings/bundles/enum/163-1/values?fields=id,name",
        200,
        r#"{"id":"164-20","name":"Chore"}"#,
    );
    server.on(
        "POST",
        "/api/admin/customFieldSettings/bundles/state/165-0/values?fields=id,name",
        200,
        r#"{"id":"166-20","name":"Blocked"}"#,
    );
    write_remote_config(&workspace, &server.base_url());

    let run = run_br_with_env(&workspace, ["remote", "init"], TOKEN, "remote_init");

    assert!(
        run.status.success(),
        "init failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    let writes = server.write_requests();
    assert_eq!(
        writes.len(),
        7,
        "three types and four states, nothing else: {writes:?}"
    );
    let names: Vec<String> = writes
        .iter()
        .filter_map(|r| serde_json::from_str::<serde_json::Value>(&r.body).ok())
        .filter_map(|body| body.get("name")?.as_str().map(str::to_string))
        .collect();
    assert_eq!(
        names,
        [
            "Chore", "Docs", "Question", "Blocked", "Deferred", "Draft", "Pinned"
        ]
    );
    for request in &writes {
        if request.path.contains("/state/") {
            assert!(
                request.body.contains(r#""isResolved":false"#),
                "a new open state must not read as resolved: {}",
                request.body
            );
        }
    }
}

#[test]
fn e2e_remote_init_dry_run_writes_nothing() {
    let _log = common::test_log("e2e_remote_init_dry_run_writes_nothing");
    let workspace = initialised_workspace();
    let server = provisioned_server();
    bundle_fields_route(&server, REFERENCE_BUNDLES);
    write_remote_config(&workspace, &server.base_url());

    let run = run_br_with_env(
        &workspace,
        ["remote", "init", "--dry-run"],
        TOKEN,
        "remote_init_dry_run",
    );

    assert!(
        run.status.success(),
        "dry run failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        server.write_requests().is_empty(),
        "a dry run must write nothing, got {:?}",
        server.write_requests()
    );
    assert!(
        run.stdout.contains("Chore"),
        "it should still say what it would add: {}",
        run.stdout
    );
}

#[test]
fn e2e_remote_init_needs_the_token_and_never_prints_it() {
    let _log = common::test_log("e2e_remote_init_needs_the_token_and_never_prints_it");
    let workspace = initialised_workspace();
    let server = provisioned_server();
    bundle_fields_route(&server, COMPLETE_BUNDLES);
    write_remote_config(&workspace, &server.base_url());

    let missing = run_br_with_env(
        &workspace,
        ["remote", "init"],
        [("BR_YOUTRACK_TOKEN", "")],
        "remote_init_no_token",
    );
    assert!(!missing.status.success(), "must refuse: {}", missing.stdout);
    assert!(
        missing.stderr.contains("BR_YOUTRACK_TOKEN")
            || missing.stdout.contains("BR_YOUTRACK_TOKEN"),
        "must name the variable; stdout={} stderr={}",
        missing.stdout,
        missing.stderr
    );

    let run = run_br_with_env(
        &workspace,
        ["remote", "init"],
        [("BR_YOUTRACK_TOKEN", "super-secret-value")],
        "remote_init_token",
    );
    assert!(run.status.success(), "init failed: {}", run.stderr);
    assert!(
        !run.stdout.contains("super-secret-value") && !run.stderr.contains("super-secret-value"),
        "the credential must never reach any output stream"
    );
    let recorded = server.requests();
    assert!(
        recorded
            .iter()
            .all(|r| r.auth.as_deref() == Some("Bearer super-secret-value")),
        "every request must still carry it"
    );
}

/// The success path above proves the report and the progress lines are clean.
/// It cannot prove the *error* path is, because with a real token set nothing
/// fails. A 401 is the failure most likely to tempt an implementation into
/// quoting the credential back, so it is the one worth exercising.
#[test]
fn e2e_remote_init_never_prints_the_token_when_a_request_is_rejected() {
    let _log =
        common::test_log("e2e_remote_init_never_prints_the_token_when_a_request_is_rejected");
    let workspace = initialised_workspace();
    let server = provisioned_server();
    bundle_fields_route(&server, COMPLETE_BUNDLES);
    // Reject the very first call, so the run dies inside the HTTP layer with a
    // live credential in hand.
    server.on(
        "GET",
        "/api/admin/projects?fields=id,name,shortName&$skip=0&$top=500",
        401,
        r#"{"error":"Unauthorized","error_description":"Token is invalid or expired"}"#,
    );
    write_remote_config(&workspace, &server.base_url());

    let run = run_br_with_env(
        &workspace,
        ["remote", "init"],
        [("BR_YOUTRACK_TOKEN", "super-secret-value")],
        "remote_init_401",
    );

    assert!(
        !run.status.success(),
        "a 401 must fail the run: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    let combined = format!("{}{}", run.stdout, run.stderr);
    assert!(
        combined.contains("401"),
        "the refusal must say what went wrong: {combined}"
    );
    assert!(
        !combined.contains("super-secret-value"),
        "the credential must never reach an error message either: {combined}"
    );
    assert!(
        !combined.contains("Bearer "),
        "nor the header built from it: {combined}"
    );
    assert_eq!(
        server.requests().len(),
        1,
        "a 401 is not retryable, so it must not be re-sent: {:?}",
        server.requests()
    );
}
