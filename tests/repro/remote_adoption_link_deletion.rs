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
//! The `br remote pull`/`push` verbs are still stubs (a later task owns them),
//! so the pull half is `common::remote_harness::adopt_everything` — which
//! assembles the same library calls those verbs will — and the push half is
//! executed against the mock through `link_add`/`link_remove`, so the
//! assertion is still on the wire traffic a real push would produce.

use crate::common;

use beads::remote::link_diff::{LinkChange, mirrored_direction};
use beads::remote::plan::build_plan;
use beads::remote::youtrack::fetch::fetch_snapshot;
use beads::remote::youtrack::links::{LinkTypes, link_add, link_remove};
use common::cli::{BrWorkspace, run_br_with_env};
use common::mock_http::MockServer;
use common::remote_harness::{adopt_everything, client, hydrated_issues, open_storage};
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

/// The push half: rebuild the plan and execute its link changes for real.
fn push_link_changes(workspace: &BrWorkspace, server: &MockServer) {
    let cfg =
        beads::remote::config::RemoteConfig::load(&beads_dir(workspace)).expect("remote.yaml");
    let http = client(&server.base_url());
    let types = LinkTypes::resolve(&http).expect("link types");
    let snapshot = fetch_snapshot(&http, &cfg, &types).expect("fetch");

    let storage = open_storage(&beads_dir(workspace));
    let issues = hydrated_issues(&storage);
    let plan = build_plan(&cfg, &issues, snapshot, &types);

    for issue_plan in &plan.link_changes {
        for change in &issue_plan.changes {
            match change {
                LinkChange::Add {
                    kind,
                    target_readable,
                } => link_add(
                    &http,
                    &issue_plan.remote_id,
                    &types.link_id(*kind, mirrored_direction(*kind)),
                    target_readable,
                )
                .expect("link_add"),
                LinkChange::Remove {
                    kind,
                    target_internal_id,
                    ..
                } => link_remove(
                    &http,
                    &issue_plan.remote_id,
                    &types.link_id(*kind, mirrored_direction(*kind)),
                    target_internal_id,
                )
                .expect("link_remove"),
            }
        }
    }
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

    write_remote_config(&beads_dir(&workspace), &server.base_url());
    workspace
}

#[test]
fn adopting_a_subtask_then_pushing_does_not_delete_its_parent_link() {
    let _log = common::test_log("adopting_a_subtask_then_pushing_does_not_delete_its_parent_link");
    let server = MockServer::start();
    let workspace = scenario(&server);

    let run = adopt_everything(&beads_dir(&workspace), &server.base_url(), "em").expect("adoption");
    assert_eq!(run.adopted.len(), 2, "EM-11 and EM-12 are both adoptable");

    push_link_changes(&workspace, &server);

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
