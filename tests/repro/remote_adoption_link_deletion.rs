//! Adopting a web-UI subtask must not lead the next push to delete its parent
//! link. The differ is local-wins, so an adopted bead that arrives without its
//! links looks — correctly, and disastrously — like a bead whose links were
//! deleted locally.
//!
//! The failure this pins is quiet and one sync late:
//!
//! 1. a human creates a subtask under a mirrored issue in the web UI,
//! 2. adoption imports the issue but not its links, so the bead has no parent,
//! 3. the differ sees a `Subtask` link present remotely and absent locally,
//! 4. and the next push **deletes the parent link that was just created**.
//!
//! Nothing errors. The bead is there, the issue is there, and the relationship
//! between them is gone — destroyed by the mirror, one run after the mirror
//! imported it. `import_links` is what stops that, and this module is what
//! stops it coming back.
//!
//! The parent link is only half of it, and the cheaper half: `adopt_one` mints
//! the bead *as its parent's child*, so that link exists before `import_links`
//! runs and this test would still see no `DELETE` for it if `import_links` did
//! nothing at all. The fixture therefore also carries the links nothing but
//! `import_links` can write:
//!
//! - a `Relates` and a `Depend` **between two adoptees**, and
//! - a `Depend` from the **already-paired** EM-10 to the adoptee EM-12, which
//!   is the case that nearly escaped: a `Depend` is owned by the blocker, so
//!   that link's mirrored `OUTWARD` half lives on EM-10, an issue no adoption
//!   ever walks. It is imported from EM-12's `INWARD` half instead. "A new UI
//!   issue depends on an existing mirrored issue" is an ordinary way to file
//!   work, so this is not a corner.
//!
//! With `import_links` stubbed out the test fails with four deletions.
//!
//! Both halves are the real verbs: `br remote pull` and then `br remote push`,
//! driven as subprocesses against the loopback mock. The assertion is on the
//! wire traffic the push actually produced.

use crate::common;

use common::cli::{BrWorkspace, run_br_with_env};
use common::mock_http::MockServer;
use common::remote_harness::open_storage;
use common::youtrack_fixtures::{LINK_TYPES, LINK_TYPES_PATH, issues_path, write_remote_config};

const TOKEN: [(&str, &str); 1] = [("BR_YOUTRACK_TOKEN", "t")];

/// The web UI, one afternoon: EM-10 is already paired to a bead, EM-11 is a
/// subtask someone typed under it, EM-12 is a sibling, and while they were at
/// it they wired EM-11 to EM-12 and made EM-12 depend on the mirrored EM-10.
///
/// Every link appears on both of its ends, exactly as YouTrack reports them.
const ISSUES: &str = r#"[
  {"id":"3-10","idReadable":"EM-10","summary":"Mirrored parent","updated":1000,
   "commentsCount":0,"tags":[],
   "links":[{"id":"173-3s","direction":"OUTWARD","linkType":{"id":"173-3","name":"Subtask"},
             "issues":[{"id":"3-11","idReadable":"EM-11"}]},
            {"id":"173-1s","direction":"OUTWARD","linkType":{"id":"173-1","name":"Depend"},
             "issues":[{"id":"3-12","idReadable":"EM-12"}]}],
   "customFields":[{"name":"Type","value":{"name":"Task"}},
                   {"name":"State","value":{"name":"Open"}},
                   {"name":"Priority","value":{"name":"Major"}}]},
  {"id":"3-11","idReadable":"EM-11","summary":"Typed in the web UI","updated":1000,
   "commentsCount":0,"tags":[],
   "links":[{"id":"173-3t","direction":"INWARD","linkType":{"id":"173-3","name":"Subtask"},
             "issues":[{"id":"3-10","idReadable":"EM-10"}]},
            {"id":"173-1s","direction":"OUTWARD","linkType":{"id":"173-1","name":"Depend"},
             "issues":[{"id":"3-12","idReadable":"EM-12"}]},
            {"id":"173-0","direction":"BOTH","linkType":{"id":"173-0","name":"Relates"},
             "issues":[{"id":"3-12","idReadable":"EM-12"}]}],
   "customFields":[{"name":"Type","value":{"name":"Task"}},
                   {"name":"State","value":{"name":"Open"}},
                   {"name":"Priority","value":{"name":"Major"}}]},
  {"id":"3-12","idReadable":"EM-12","summary":"Its sibling","updated":1000,
   "commentsCount":0,"tags":[],
   "links":[{"id":"173-1t","direction":"INWARD","linkType":{"id":"173-1","name":"Depend"},
             "issues":[{"id":"3-11","idReadable":"EM-11"},{"id":"3-10","idReadable":"EM-10"}]},
            {"id":"173-0","direction":"BOTH","linkType":{"id":"173-0","name":"Relates"},
             "issues":[{"id":"3-11","idReadable":"EM-11"}]}],
   "customFields":[{"name":"Type","value":{"name":"Task"}},
                   {"name":"State","value":{"name":"Open"}},
                   {"name":"Priority","value":{"name":"Major"}}]}
]"#;

fn beads_dir(workspace: &BrWorkspace) -> std::path::PathBuf {
    workspace.root.join(".beads")
}

fn run_verb(workspace: &BrWorkspace, verb: &str) -> common::cli::BrRun {
    let run = run_br_with_env(workspace, ["remote", verb], TOKEN, verb);
    assert!(
        run.status.success(),
        "br remote {verb} failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    run
}

/// A workspace holding one bead paired with EM-10, and a mock that answers
/// the fetch plus **every link write a push could attempt**.
///
/// Registering the writes is the point: a push that wants to delete must
/// succeed and be recorded, not fail on an unrouted request. The bug has to be
/// observable to be asserted against.
fn scenario(server: &MockServer) -> BrWorkspace {
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

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on("GET", &issues_path(0), 200, ISSUES);
    for (issue, link_id, target) in [
        ("EM-10", "173-1s", "3-12"),
        ("EM-11", "173-3t", "3-10"),
        ("EM-11", "173-1s", "3-12"),
        ("EM-11", "173-0", "3-12"),
        ("EM-12", "173-1s", "3-11"),
        ("EM-12", "173-0", "3-11"),
    ] {
        server.on(
            "DELETE",
            &format!("/api/issues/{issue}/links/{link_id}/issues/{target}"),
            200,
            "",
        );
        server.on(
            "POST",
            &format!("/api/issues/{issue}/links/{link_id}/issues?fields=idReadable"),
            200,
            r#"{"idReadable":"EM-0"}"#,
        );
    }
    // The `Beads ID` stamp each adoption posts. Routing it keeps a courtesy
    // from failing for want of a canned response.
    for issue in ["EM-11", "EM-12"] {
        server.on(
            "POST",
            &format!("/api/issues/{issue}?fields=idReadable"),
            200,
            &format!(r#"{{"idReadable":"{issue}"}}"#),
        );
    }

    write_remote_config(&beads_dir(&workspace), &server.base_url());
    workspace
}

#[test]
fn adopting_a_subtask_then_pushing_does_not_delete_its_parent_link() {
    let _log = common::test_log("adopting_a_subtask_then_pushing_does_not_delete_its_parent_link");
    let server = MockServer::start();
    let workspace = scenario(&server);

    let pulled = run_verb(&workspace, "pull");
    assert!(
        pulled.stdout.contains("adopted EM-11") && pulled.stdout.contains("adopted EM-12"),
        "EM-11 and EM-12 are both adoptable: {}",
        pulled.stdout
    );

    run_verb(&workspace, "push");

    let deletes: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.method == "DELETE" && r.path.contains("/links/"))
        .collect();
    assert!(
        deletes.is_empty(),
        "the push deleted a link the pull had just imported: {deletes:?}"
    );

    // And the positive half. Without it, an adoption that imported nothing at
    // all — no bead, no links, no push to make — would sail through the
    // assertion above: zero deletes is also what doing nothing looks like.
    let listed = run_br_with_env(&workspace, ["--json", "list"], TOKEN, "list");
    assert!(listed.status.success(), "list failed: {}", listed.stderr);
    let issues = common::cli::parse_list_issues(&listed.stdout);
    let by_ref = |reference: &str| -> String {
        issues
            .iter()
            .find(|i| i["external_ref"] == reference)
            .and_then(|i| i["id"].as_str())
            .unwrap_or_else(|| panic!("{reference} was never adopted: {}", listed.stdout))
            .to_string()
    };
    let parent = by_ref("EM-10");
    let child = by_ref("EM-11");
    let sibling = by_ref("EM-12");
    assert_eq!(
        child,
        format!("{parent}.1"),
        "the adoptee must be its parent's child, not a flat bead"
    );

    let storage = open_storage(&beads_dir(&workspace));
    let rows = storage
        .get_all_dependency_records()
        .expect("dependency records");
    let edges: Vec<(String, String, String)> = rows
        .values()
        .flatten()
        .map(|d| {
            (
                d.issue_id.clone(),
                d.dep_type.to_string(),
                d.depends_on_id.clone(),
            )
        })
        .collect();
    for expected in [
        // Written by adopt_one, at creation.
        (child.clone(), "parent-child".to_string(), parent.clone()),
        // Written by import_links from EM-11's OUTWARD half: EM-11 blocks
        // EM-12, which beads records as EM-12 depending on EM-11.
        (sibling.clone(), "blocks".to_string(), child.clone()),
        (child, "related".to_string(), sibling.clone()),
        // Written by import_links from EM-12's INWARD half — the one that
        // would otherwise escape, because its mirrored half lives on the
        // already-paired EM-10 and nothing walks a paired issue's links.
        (sibling, "blocks".to_string(), parent),
    ] {
        assert!(
            edges.contains(&expected),
            "missing imported relation {expected:?} in {edges:?}"
        );
    }
}

/// The same bug in the other direction, and the one `.10` deliberately left
/// open: an adoptee that becomes the **parent** of an already-paired bead.
///
/// Someone drags an existing mirrored issue under a brand-new one in the web
/// UI. The new issue is adopted; the link appears on it as `Subtask` OUTWARD
/// and on the paired issue as `INWARD`. Reading only the `INWARD` half of a
/// `Subtask` — which is what "a parent link is owned by the child" means
/// everywhere else in this epic — imports nothing at all, because the adoptee
/// only carries the `OUTWARD` half and nothing walks a paired issue's links.
/// The paired bead then reports no parent, the local-wins differ sees a
/// `Subtask` link present remotely and absent locally, and the next push
/// deletes the link the human just made.
///
/// `import_links` therefore writes the `parent-child` row from that half too,
/// and writes **only** the row: renaming the existing bead to sit under its new
/// parent's id would leave a tombstone and `former_ids` churn behind, forever,
/// for a board rearrangement. The bead's id no longer describes its parentage,
/// which `br dep add … parent-child` already produces and `br reparent` can
/// resolve on request. The alternative was letting the mirror delete an
/// inbound edit.
const REPARENT_ISSUES: &str = r#"[
  {"id":"3-10","idReadable":"EM-10","summary":"Mirrored bead","updated":1000,
   "commentsCount":0,"tags":[],
   "links":[{"id":"173-3t","direction":"INWARD","linkType":{"id":"173-3","name":"Subtask"},
             "issues":[{"id":"3-20","idReadable":"EM-20"}]}],
   "customFields":[{"name":"Type","value":{"name":"Task"}},
                   {"name":"State","value":{"name":"Open"}},
                   {"name":"Priority","value":{"name":"Major"}}]},
  {"id":"3-20","idReadable":"EM-20","summary":"A new home for it","updated":1000,
   "commentsCount":0,"tags":[],
   "links":[{"id":"173-3s","direction":"OUTWARD","linkType":{"id":"173-3","name":"Subtask"},
             "issues":[{"id":"3-10","idReadable":"EM-10"}]}],
   "customFields":[{"name":"Type","value":{"name":"Task"}},
                   {"name":"State","value":{"name":"Open"}},
                   {"name":"Priority","value":{"name":"Major"}}]}
]"#;

/// A workspace with one bead paired to `EM-10`, and a mock that answers the
/// fetch, the adoption stamp, and **both** link writes a push could attempt —
/// the removal has to succeed and be recorded, or the bug is unobservable.
fn reparent_scenario(server: &MockServer) -> (BrWorkspace, String) {
    let workspace = BrWorkspace::new();
    let init = run_br_with_env(&workspace, ["init", "--prefix", "em"], TOKEN, "init");
    assert!(init.status.success(), "br init failed: {}", init.stderr);
    let created = run_br_with_env(
        &workspace,
        [
            "create",
            "Mirrored bead",
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
    let existing = common::cli::parse_created_id(&created.stdout);

    server.on("GET", LINK_TYPES_PATH, 200, LINK_TYPES);
    server.on("GET", &issues_path(0), 200, REPARENT_ISSUES);
    server.on(
        "POST",
        "/api/issues/EM-20?fields=idReadable",
        200,
        r#"{"idReadable":"EM-20"}"#,
    );
    server.on(
        "DELETE",
        "/api/issues/EM-10/links/173-3t/issues/3-20",
        200,
        "",
    );
    server.on(
        "POST",
        "/api/issues/EM-10/links/173-3t/issues?fields=idReadable",
        200,
        r#"{"idReadable":"EM-20"}"#,
    );
    write_remote_config(&beads_dir(&workspace), &server.base_url());
    (workspace, existing)
}

/// Every `(issue, dep_type, depends_on)` row the workspace holds.
fn dependency_edges(workspace: &BrWorkspace) -> Vec<(String, String, String)> {
    let storage = open_storage(&beads_dir(workspace));
    storage
        .get_all_dependency_records()
        .expect("dependency records")
        .values()
        .flatten()
        .map(|d| {
            (
                d.issue_id.clone(),
                d.dep_type.to_string(),
                d.depends_on_id.clone(),
            )
        })
        .collect()
}

#[test]
fn adopting_a_new_parent_over_a_paired_bead_does_not_delete_the_link() {
    let _log =
        common::test_log("adopting_a_new_parent_over_a_paired_bead_does_not_delete_the_link");
    let server = MockServer::start();
    let (workspace, existing) = reparent_scenario(&server);

    run_verb(&workspace, "pull");
    run_verb(&workspace, "push");

    let deletes: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.method == "DELETE")
        .collect();
    assert!(
        deletes.is_empty(),
        "the push deleted the parent link the pull had just imported: {deletes:?}"
    );

    let listed = run_br_with_env(&workspace, ["--json", "list"], TOKEN, "list");
    assert!(listed.status.success(), "list failed: {}", listed.stderr);
    let issues = common::cli::parse_list_issues(&listed.stdout);
    let adoptee = issues
        .iter()
        .find(|i| i["external_ref"] == "EM-20")
        .and_then(|i| i["id"].as_str())
        .unwrap_or_else(|| panic!("EM-20 was never adopted: {}", listed.stdout))
        .to_string();

    let edges = dependency_edges(&workspace);
    assert!(
        edges.contains(&(existing.clone(), "parent-child".to_string(), adoptee)),
        "the OUTWARD half of the adoptee's Subtask link must be imported: {edges:?}"
    );

    // And the existing bead was not renamed onto its new parent's id. A rename
    // is a tombstone plus a forwarding pointer, permanently, and inflicting one
    // because somebody rearranged a board is not a trade.
    assert!(
        issues.iter().any(|i| i["id"] == existing.as_str()),
        "the paired bead must keep its id: {}",
        listed.stdout
    );
    let tombstones = run_br_with_env(
        &workspace,
        ["--json", "list", "--status", "tombstone"],
        TOKEN,
        "list_tombstones",
    );
    assert!(
        common::cli::parse_list_issues(&tombstones.stdout).is_empty(),
        "no rename, so no tombstone: {}",
        tombstones.stdout
    );
}
