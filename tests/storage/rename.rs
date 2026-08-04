//! Tests for the `rename_issue` storage primitive and its subtree cascade.

use crate::common;

use beads::model::Status;
use beads::storage::{IssueUpdate, SqliteStorage};
use common::{fixtures, test_db};

/// Build `parent`, `parent.1`, `parent.1.1` and `parent.2`, returning storage.
fn tree() -> SqliteStorage {
    let mut storage = test_db();
    for id in ["bd-p", "bd-p.1", "bd-p.1.1", "bd-p.2"] {
        let mut issue = fixtures::issue(id);
        issue.id = id.to_string();
        storage.create_issue(&issue, "tester").unwrap();
    }
    storage
}

#[test]
fn descendant_ids_returns_every_depth_deepest_first() {
    let storage = tree();

    let ids = storage.descendant_ids("bd-p").unwrap();

    assert_eq!(ids, vec!["bd-p.1.1", "bd-p.1", "bd-p.2"]);
}

#[test]
fn descendant_ids_includes_closed_and_tombstoned_descendants() {
    let mut storage = tree();
    storage
        .update_issue(
            "bd-p.2",
            &IssueUpdate {
                status: Some(Status::Closed),
                ..Default::default()
            },
            "tester",
        )
        .unwrap();

    let ids = storage.descendant_ids("bd-p").unwrap();

    assert!(
        ids.contains(&"bd-p.2".to_string()),
        "a closed descendant still occupies its ID and must move with the subtree; got {ids:?}"
    );
}

#[test]
fn descendant_ids_excludes_the_node_itself_and_unrelated_prefixes() {
    let mut storage = tree();
    let mut sibling = fixtures::issue("bd-p2");
    sibling.id = "bd-p2".to_string();
    storage.create_issue(&sibling, "tester").unwrap();

    let ids = storage.descendant_ids("bd-p").unwrap();

    assert!(
        !ids.contains(&"bd-p".to_string()),
        "must exclude the node itself"
    );
    assert!(
        !ids.contains(&"bd-p2".to_string()),
        "bd-p2 shares a textual prefix but is not a descendant; got {ids:?}"
    );
}

#[test]
fn rename_moves_inbound_dependency_edges() {
    let mut storage = tree();
    let mut other = fixtures::issue("bd-other");
    other.id = "bd-other".to_string();
    storage.create_issue(&other, "tester").unwrap();
    // An *inbound* edge: stored in the unconstrained depends_on_id column.
    storage
        .add_dependency("bd-other", "bd-p.2", "blocks", "tester")
        .unwrap();

    storage.rename_issue("bd-p.2", "bd-new", "tester").unwrap();

    let deps = storage.get_dependencies_full("bd-other").unwrap();
    assert_eq!(
        deps.iter()
            .map(|d| d.depends_on_id.as_str())
            .collect::<Vec<_>>(),
        vec!["bd-new"],
        "inbound edge still points at the old ID — depends_on_id has no FK, so \
         nothing would have faulted"
    );
}

#[test]
fn rename_moves_labels_and_comments_off_the_vacated_id() {
    let mut storage = tree();
    storage.add_label("bd-p.2", "urgent").unwrap();
    storage.add_comment("bd-p.2", "tester", "a note").unwrap();

    storage.rename_issue("bd-p.2", "bd-new", "tester").unwrap();

    assert!(
        storage.get_issue("bd-new").unwrap().is_some(),
        "renamed issue exists"
    );
    // `get_issue` alone does not populate relations (see
    // `get_issue_for_export`), so labels are checked via `get_labels`
    // directly rather than through the returned `Issue`.
    assert!(
        storage
            .get_labels("bd-new")
            .unwrap()
            .contains(&"urgent".to_string())
    );
    assert_eq!(storage.get_comments("bd-new").unwrap().len(), 1);
    assert_eq!(
        storage.get_comments("bd-p.2").unwrap().len(),
        0,
        "comments must not remain attached to the vacated ID"
    );
}

#[test]
fn rename_carries_the_whole_subtree() {
    let mut storage = tree();

    storage.rename_issue("bd-p.1", "bd-q.7", "tester").unwrap();

    assert!(storage.get_issue("bd-q.7").unwrap().is_some());
    assert!(
        storage.get_issue("bd-q.7.1").unwrap().is_some(),
        "the grandchild bd-p.1.1 must have followed its parent to bd-q.7.1"
    );
    assert!(storage.get_issue("bd-p.1").unwrap().is_none());
    assert!(storage.get_issue("bd-p.1.1").unwrap().is_none());
}

#[test]
fn rename_rejects_a_new_id_that_is_already_taken() {
    let mut storage = tree();

    let err = storage
        .rename_issue("bd-p.1", "bd-p.2", "tester")
        .expect_err("bd-p.2 already exists");

    assert!(
        format!("{err}").contains("bd-p.2"),
        "the error must name the colliding ID; got: {err}"
    );
    assert!(
        storage.get_issue("bd-p.1").unwrap().is_some(),
        "a rejected rename must leave the original in place"
    );
}

#[test]
fn rename_leaves_nothing_behind_on_failure() {
    let mut storage = tree();

    let _ = storage.rename_issue("bd-p.1", "bd-p.2", "tester");

    // The whole thing is one transaction, so the subtree must be untouched too.
    assert!(storage.get_issue("bd-p.1.1").unwrap().is_some());
}
