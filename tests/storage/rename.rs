//! Tests for the `rename_issue` storage primitive and its subtree cascade.

use crate::common;

use beads::model::Status;
use beads::storage::{IssueUpdate, SqliteStorage};
use beads::sync::{ExportConfig, ImportConfig, export_to_jsonl, import_from_jsonl};
use beads::util::id::{IdExistence, IdResolver, ResolverConfig};
use common::{fixtures, test_db};
use tempfile::TempDir;

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
    // The top-level node leaves a tombstone behind rather than vanishing —
    // see `rename_leaves_a_tombstone_at_the_vacated_id` below for the
    // dedicated coverage. Descendants moved only as a consequence of their
    // ancestor's move get no tombstone of their own and are simply gone.
    let stone = storage.get_issue("bd-p.1").unwrap();
    assert!(
        stone.is_some_and(|issue| issue.status == Status::Tombstone),
        "the vacated top-level ID keeps a tombstone row"
    );
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

#[test]
fn rename_records_the_old_id_in_former_ids() {
    let mut storage = tree();

    storage.rename_issue("bd-p.2", "bd-new", "tester").unwrap();

    let moved = storage.get_issue("bd-new").unwrap().expect("exists");
    assert_eq!(moved.former_ids, vec!["bd-p.2"]);
}

#[test]
fn former_ids_accumulate_oldest_first_across_repeated_renames() {
    let mut storage = tree();

    storage.rename_issue("bd-p.2", "bd-mid", "tester").unwrap();
    storage
        .rename_issue("bd-mid", "bd-final", "tester")
        .unwrap();

    let moved = storage.get_issue("bd-final").unwrap().expect("exists");
    assert_eq!(
        moved.former_ids,
        vec!["bd-p.2", "bd-mid"],
        "both hops must be recorded, oldest first"
    );
}

#[test]
fn rename_leaves_a_tombstone_at_the_vacated_id() {
    let mut storage = tree();

    storage.rename_issue("bd-p.2", "bd-new", "tester").unwrap();

    let stone = storage
        .get_issue("bd-p.2")
        .unwrap()
        .expect("the vacated ID keeps a tombstone row");
    assert_eq!(stone.status, Status::Tombstone);
    assert!(
        stone
            .delete_reason
            .as_deref()
            .unwrap_or_default()
            .contains("bd-new"),
        "the tombstone must name where the issue went; got {:?}",
        stone.delete_reason
    );
}

#[test]
fn rename_of_an_issue_with_an_external_ref_succeeds() {
    // idx_issues_external_ref_unique (schema.rs:90) is a unique partial index
    // over `issues(external_ref) WHERE external_ref IS NOT NULL` with no
    // status predicate, so a tombstone that copied the live row's
    // external_ref verbatim would collide with the row rewrite_issue_id had
    // already moved it to, and the whole rename transaction would roll back.
    let mut storage = test_db();
    let mut issue = fixtures::issue("bd-ref");
    issue.id = "bd-ref".to_string();
    issue.external_ref = Some("JIRA-123".to_string());
    storage.create_issue(&issue, "tester").unwrap();

    storage
        .rename_issue("bd-ref", "bd-ref2", "tester")
        .expect("a rename must not fail just because the issue has an external_ref");

    let moved = storage
        .get_issue("bd-ref2")
        .unwrap()
        .expect("renamed issue exists");
    assert_eq!(
        moved.external_ref,
        Some("JIRA-123".to_string()),
        "external_ref must follow the issue to its new id"
    );

    let stone = storage
        .get_issue("bd-ref")
        .unwrap()
        .expect("tombstone exists");
    assert_eq!(
        stone.external_ref, None,
        "the tombstone must not keep the external_ref -- the live row at the \
         new id owns it now, and a unique partial index has no status \
         predicate to tell the two rows apart"
    );
}

#[test]
fn the_tombstone_does_not_block_closing_the_old_parent() {
    let mut storage = tree();
    storage
        .rename_issue("bd-p.1", "bd-gone-1", "tester")
        .unwrap();
    storage
        .rename_issue("bd-p.2", "bd-gone-2", "tester")
        .unwrap();

    let blockers = storage.get_open_dot_notation_children("bd-p").unwrap();

    assert!(
        blockers.is_empty(),
        "tombstones are not open children and must not block the parent's close; got {blockers:?}"
    );
}

#[test]
fn a_detached_id_is_never_reissued() {
    let mut storage = tree();
    storage.rename_issue("bd-p.2", "bd-new", "tester").unwrap();

    // bd-p's counter is at 2; the next child must be .3, not a reuse of .2.
    // This exercises the `child_counters` path, not the live-row scan
    // fallback: `create_issue` already populates a `child_counters` row for
    // "bd-p" (`last_child = 2`) the moment `tree()` creates `bd-p.2`, via the
    // `update_child_counter_in_tx` call at src/storage/sqlite.rs:2853.
    // Confirmed empirically — `next_child_number("bd-p")` already returns 3
    // immediately after `tree()`, before any rename runs. `rename_issue`
    // never decrements that counter, so it stays the same read after the
    // rename below; `next_child_number` (sqlite.rs:9191) only falls back to
    // scanning live `issues` rows when no `child_counters` row exists at all,
    // which is not this case.
    let next = storage.next_child_number("bd-p").unwrap();

    assert!(
        next > 2,
        "reissuing bd-p.2 would silently redirect every stale reference to the \
         original occupant onto a different issue; got next = {next}"
    );
}

#[test]
fn rename_tombstone_and_former_ids_survive_a_jsonl_export_import_round_trip() {
    let mut storage = tree();
    storage.rename_issue("bd-p.2", "bd-new", "tester").unwrap();

    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();

    let mut imported = test_db();
    import_from_jsonl(&mut imported, &path, &ImportConfig::default(), Some("bd-")).unwrap();

    let moved = imported
        .get_issue("bd-new")
        .unwrap()
        .expect("the renamed issue survives the round trip");
    assert_eq!(
        moved.former_ids,
        vec!["bd-p.2"],
        "former_ids must survive a JSONL export/import round trip"
    );

    let stone = imported
        .get_issue("bd-p.2")
        .unwrap()
        .expect("the tombstone survives the round trip — this is the whole reason it exists");
    assert_eq!(stone.status, Status::Tombstone);
    assert!(
        stone
            .delete_reason
            .as_deref()
            .unwrap_or_default()
            .contains("bd-new"),
        "the imported tombstone must still name its destination; got {:?}",
        stone.delete_reason
    );
}

#[test]
fn find_id_by_former_id_locates_the_renamed_issue() {
    let mut storage = tree();
    storage.rename_issue("bd-p.2", "bd-new", "tester").unwrap();

    let found = storage.find_id_by_former_id("bd-p.2").unwrap();

    assert_eq!(found.as_deref(), Some("bd-new"));
}

#[test]
fn find_id_by_former_id_returns_none_for_an_unknown_id() {
    let storage = tree();

    assert_eq!(storage.find_id_by_former_id("bd-never").unwrap(), None);
}

#[test]
fn live_id_exists_ignores_tombstones() {
    let mut storage = tree();
    storage.rename_issue("bd-p.2", "bd-new", "tester").unwrap();

    assert!(!storage.live_id_exists("bd-p.2").unwrap());
    assert!(
        storage.id_exists("bd-p.2").unwrap(),
        "the tombstone row is still there"
    );
    assert!(storage.live_id_exists("bd-new").unwrap());
}

/// Wires `IdResolver` up against real storage the same way
/// `resolve_issue_id` (`src/cli/commands/mod.rs`) and `br show`
/// (`src/cli/commands/show.rs`) do, since neither `rename` nor `detach` has
/// a CLI surface yet to drive this as a subprocess end to end.
fn resolve_via_storage(storage: &SqliteStorage, input: &str) -> beads::Result<String> {
    let resolver = IdResolver::new(ResolverConfig::with_prefix("bd"));
    resolver
        .resolve_fallible(
            input,
            |id| {
                if storage.live_id_exists(id)? {
                    Ok(IdExistence::Live)
                } else if storage.id_exists(id)? {
                    Ok(IdExistence::Tombstone)
                } else {
                    Ok(IdExistence::Missing)
                }
            },
            |hash| storage.find_ids_by_hash(hash),
            |former| storage.find_id_by_former_id(former),
        )
        .map(|resolved| resolved.id)
}

#[test]
fn resolving_an_old_id_after_rename_finds_the_issue_that_now_holds_it() {
    let mut storage = tree();
    storage.rename_issue("bd-p.2", "bd-new", "tester").unwrap();

    let resolved = resolve_via_storage(&storage, "bd-p.2").expect("the old ID must still resolve");

    assert_eq!(
        resolved, "bd-new",
        "a reference to the pre-rename ID must land on the issue that now holds it"
    );
}

#[test]
fn resolving_a_genuinely_deleted_id_still_returns_its_tombstone() {
    let mut storage = tree();
    storage
        .delete_issue("bd-p.2", "tester", "no longer needed", None)
        .unwrap();

    let resolved = resolve_via_storage(&storage, "bd-p.2")
        .expect("a deleted issue's tombstone must still resolve, not IssueNotFound");

    assert_eq!(resolved, "bd-p.2");
}
