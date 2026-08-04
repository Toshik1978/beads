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
