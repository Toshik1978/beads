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
fn rename_front_door_collision_leaves_the_subtree_untouched() {
    // NB: this only exercises the up-front `id_exists(new_id)` guard, which
    // returns *before* the write transaction ever opens — no rollback is
    // involved. It's still worth keeping (the guard must not touch
    // descendants either), but `rename_rolls_back_earlier_cascade_writes_on_a_later_failure`
    // below is the test that actually exercises transactional atomicity.
    let mut storage = tree();

    let _ = storage.rename_issue("bd-p.1", "bd-p.2", "tester");

    assert!(storage.get_issue("bd-p.1.1").unwrap().is_some());
}

#[test]
fn rename_rolls_back_earlier_cascade_writes_on_a_later_failure() {
    // `bd-q.1` is occupied by an unrelated issue; `bd-q` and `bd-q.1.1` are
    // free, so the up-front `id_exists("bd-q")` guard passes and the cascade
    // actually begins inside `mutate()`'s transaction. Descendants rename
    // deepest-first (`descendant_ids_returns_every_depth_deepest_first`
    // above), so for a rename of `bd-p` -> `bd-q` the order is `bd-p.1.1`,
    // then `bd-p.1`, then `bd-p.2`. The first `UPDATE issues SET id =
    // 'bd-q.1.1'` succeeds *inside* the transaction; the second then hits a
    // real `PRIMARY KEY` collision renaming `bd-p.1` to the already-occupied
    // `bd-q.1`. A genuinely transactional `rename_issue` must undo that
    // already-applied first write along with everything else — which is what
    // distinguishes this from a non-transactional implementation that simply
    // stops on the first failure and leaves prior renames in place.
    let mut storage = tree();
    let mut occupied = fixtures::issue("bd-q.1");
    occupied.id = "bd-q.1".to_string();
    storage.create_issue(&occupied, "tester").unwrap();

    storage
        .rename_issue("bd-p", "bd-q", "tester")
        .expect_err("bd-q.1 is already occupied by an unrelated issue");

    assert!(
        storage.get_issue("bd-p.1.1").unwrap().is_some(),
        "the descendant renamed earlier in the cascade must have been rolled back"
    );
    assert!(
        storage.get_issue("bd-q.1.1").unwrap().is_none(),
        "the earlier, already-applied rename must not have survived the rollback"
    );
    assert!(storage.get_issue("bd-p").unwrap().is_some());
    assert!(storage.get_issue("bd-p.1").unwrap().is_some());
    assert!(storage.get_issue("bd-p.2").unwrap().is_some());
    assert!(
        storage.get_issue("bd-q").unwrap().is_none(),
        "the top-level rename must not have taken effect either"
    );
    assert!(
        storage.get_issue("bd-q.1").unwrap().is_some(),
        "the pre-existing occupant of the colliding id must be untouched"
    );
}

#[test]
fn rename_invalidates_the_blocked_cache_for_inbound_blockers() {
    // `blocked_issues_cache.blocked_by` is a JSON blob of blocker refs, opaque
    // to the plain-SQL rewrite in `rewrite_issue_id`: renaming a blocker moves
    // its own `blocked_issues_cache` row but cannot reach into some *other*
    // issue's blob and edit the id embedded there. The only way to purge the
    // old id out of those blobs is a cache rebuild, which is why
    // `rename_issue` must call `ctx.invalidate_cache()`.
    let mut storage = tree();
    let mut other = fixtures::issue("bd-other");
    other.id = "bd-other".to_string();
    storage.create_issue(&other, "tester").unwrap();
    storage
        .add_dependency("bd-other", "bd-p.2", "blocks", "tester")
        .unwrap();

    // Force the cache onto disk (non-stale) *before* the rename. `get_blockers`
    // falls back to an in-memory recompute from `dependencies`/`issues`
    // whenever the cache is merely marked stale, and that fallback would
    // report the right answer even if `rename_issue` never invalidated
    // anything — so without this, the test would not actually exercise the
    // bug.
    storage.rebuild_blocked_cache(true).unwrap();
    assert_eq!(
        storage.get_blockers("bd-other").unwrap(),
        vec!["bd-p.2".to_string()],
        "sanity check: the cache must be populated with the pre-rename blocker"
    );

    storage.rename_issue("bd-p.2", "bd-new", "tester").unwrap();

    assert_eq!(
        storage.get_blockers("bd-other").unwrap(),
        vec!["bd-new".to_string()],
        "blocked_issues_cache still names the vacated id — rename_issue must \
         invalidate the cache"
    );
}

#[test]
fn former_ids_defaults_to_empty_and_round_trips_through_storage() {
    let mut storage = test_db();
    let mut issue = fixtures::issue("bd-f");
    issue.id = "bd-f".to_string();
    storage.create_issue(&issue, "tester").unwrap();

    let fresh = storage.get_issue("bd-f").unwrap().expect("exists");
    assert!(fresh.former_ids.is_empty(), "a new issue has no former IDs");

    let mut with_history = fresh.clone();
    with_history.former_ids = vec!["bd-old".to_string(), "bd-older".to_string()];
    // NOTE: the brief named a nonexistent `update_issue_full`. The real
    // full-issue-replace primitive on `SqliteStorage` is
    // `upsert_issue_for_import`, used by JSONL import/sync to write every
    // column of an `Issue` in one shot — exactly the "replace the whole row"
    // operation this round-trip needs.
    storage.upsert_issue_for_import(&with_history).unwrap();

    let reloaded = storage.get_issue("bd-f").unwrap().expect("exists");
    assert_eq!(reloaded.former_ids, vec!["bd-old", "bd-older"]);
}

#[test]
fn former_ids_survives_a_jsonl_round_trip() {
    let mut issue = fixtures::issue("bd-f");
    issue.id = "bd-f".to_string();
    issue.former_ids = vec!["bd-old".to_string()];

    let line = serde_json::to_string(&issue).unwrap();
    let back: beads::model::Issue = serde_json::from_str(&line).unwrap();

    assert_eq!(back.former_ids, vec!["bd-old"]);

    // And an issue with no former IDs must not emit the key at all — the JSONL
    // is a tracked file and every issue gaining a `"former_ids":[]` would be a
    // whole-file diff for nothing.
    let mut plain = fixtures::issue("bd-g");
    plain.id = "bd-g".to_string();
    let plain_line = serde_json::to_string(&plain).unwrap();
    assert!(
        !plain_line.contains("former_ids"),
        "empty former_ids must be skipped in serialization; got {plain_line}"
    );
}
