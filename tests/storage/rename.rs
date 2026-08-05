//! Tests for the `rename_issue` storage primitive and its subtree cascade.

use crate::common;

use beads::model::{DependencyType, Status};
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
    // dedicated coverage. bds-a23.10: every descendant carried along by the
    // cascade gets the same treatment, not just the directly renamed node —
    // see `every_vacated_descendant_id_gets_a_tombstone` for that coverage.
    let stone = storage.get_issue("bd-p.1").unwrap();
    assert!(
        stone.is_some_and(|issue| issue.status == Status::Tombstone),
        "the vacated top-level ID keeps a tombstone row"
    );
    let descendant_stone = storage.get_issue("bd-p.1.1").unwrap();
    assert!(
        descendant_stone.is_some_and(|issue| issue.status == Status::Tombstone),
        "the vacated descendant ID keeps a tombstone row too"
    );
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

/// bds-a23.6: `sync_equals` used to ignore `former_ids`, so `merge_issue`'s
/// 3-way merge (the engine behind `br sync`'s merge path) would find the
/// local row and the incoming JSONL row "equal" and short-circuit to
/// `Keep(local)` before ever checking which side actually changed — silently
/// dropping the new `former_ids` entry a rename recorded elsewhere.
///
/// A fresh-database import can't see this bug: with no existing row, the
/// collision detector inserts unconditionally and `former_ids` rides along
/// for free (see `rename_tombstone_and_former_ids_survive_a_jsonl_export_import_round_trip`
/// above). This test instead imports over an *existing* row — the base and
/// left (local) states both lack the rename, and only right (incoming
/// JSONL) carries the new `former_ids` hop — which is exactly the shape
/// `sync_equals` has to get right for the merge to keep the incoming side.
#[test]
fn merging_a_former_ids_only_change_over_an_existing_row_propagates_it() {
    let mut base = fixtures::issue("bd-f");
    base.id = "bd-f".to_string();

    let left = base.clone(); // local DB: unchanged since base

    let mut right = base.clone(); // incoming JSONL: another clone renamed it
    right.former_ids = vec!["bd-old".to_string()];

    let mut base_map = std::collections::HashMap::new();
    base_map.insert(base.id.clone(), base.clone());
    let mut left_map = std::collections::HashMap::new();
    left_map.insert(left.id.clone(), left.clone());
    let mut right_map = std::collections::HashMap::new();
    right_map.insert(right.id.clone(), right.clone());

    let context = beads::sync::MergeContext::new(base_map, left_map, right_map);
    let report =
        beads::sync::three_way_merge(&context, beads::sync::ConflictResolution::PreferNewer, None);

    assert!(
        report.conflicts.is_empty(),
        "a former_ids-only delta must not read as a conflict; got {:?}",
        report.conflicts
    );
    let kept = report
        .kept
        .iter()
        .find(|issue| issue.id == "bd-f")
        .expect("bd-f must be kept by the merge");
    assert_eq!(
        kept.former_ids,
        vec!["bd-old".to_string()],
        "the incoming former_ids entry must survive the merge over the \
         existing row, not be silently dropped as 'no change'"
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

fn create_at(storage: &mut SqliteStorage, id: &str) {
    let mut issue = fixtures::issue(id);
    issue.id = id.to_string();
    storage.create_issue(&issue, "tester").unwrap();
}

/// Regression for review finding A on bds-a23.4.1: nothing previously bumped
/// `child_counters` for the *target* parent on a reparent (only `create_issue`
/// did, and only for the parent an issue was created directly under), so a
/// second, independent `attach_to_parent` into the same parent recomputed the
/// exact slot the first one had just taken and collided with it.
#[test]
fn attach_to_parent_bumps_the_target_parents_child_counter() {
    let mut storage = test_db();
    for id in ["bd-e1", "bd-c1", "bd-c2"] {
        create_at(&mut storage, id);
    }

    let first = storage
        .attach_to_parent("bd-c1", "bd-e1", "tester")
        .unwrap();
    assert_eq!(first, "bd-e1.1");

    let second = storage
        .attach_to_parent("bd-c2", "bd-e1", "tester")
        .unwrap();
    assert_eq!(
        second, "bd-e1.2",
        "a second, independent attach into the same parent must not recompute \
         the slot the first attach just took"
    );
}

/// Regression for review finding B on bds-a23.4.1: setting the parent-child
/// dependency and renaming used to be two separately committed operations
/// (`set_parent_with_options` then `rename_issue`). If the rename failed
/// after the dependency insert had already committed, the dependency pointed
/// at the new parent while the ID still named the old one -- the exact
/// divergence this epic exists to make unrepresentable. `attach_to_parent`
/// now runs both inside one transaction.
#[test]
fn attach_to_parent_rolls_back_the_dependency_when_rename_fails() {
    let mut storage = test_db();
    create_at(&mut storage, "bd-e1");
    create_at(&mut storage, "bd-e2");

    // A real first child directly under `bd-e2` so `child_counters["bd-e2"]`
    // exists and `next_child_number` takes the `child_counters` fast path
    // instead of the pattern-scan fallback. The fallback scans for *any* id
    // matching `bd-e2.%` -- including the squatter planted below, two levels
    // down -- and would report a different (collision-free) number than the
    // real attach ends up computing, defeating the setup.
    create_at(&mut storage, "bd-e2.1");

    // The subtree being moved: a child with its own child, so the rename
    // cascades through more than one write (deepest-first).
    create_at(&mut storage, "bd-e1.1");
    storage
        .add_dependency("bd-e1.1", "bd-e1", "parent-child", "tester")
        .unwrap();
    create_at(&mut storage, "bd-e1.1.1");
    storage
        .add_dependency("bd-e1.1.1", "bd-e1.1", "parent-child", "tester")
        .unwrap();

    // `next_child_number("bd-e2")` will be 2 (the sibling above holds slot
    // 1), so the real move is `bd-e1.1` -> `bd-e2.2`, cascading the
    // grandchild `bd-e1.1.1` -> `bd-e2.2.1`. Plant a squatter at exactly that
    // grandchild slot: descendants rename deepest-first, so this is the
    // first write inside the transaction to fail -- *after*
    // `set_parent_in_tx`'s dependency INSERT has already run in the same
    // transaction, which is what makes this a genuine atomicity test rather
    // than an up-front guard test.
    create_at(&mut storage, "bd-e2.2.1");

    storage
        .attach_to_parent("bd-e1.1", "bd-e2", "tester")
        .expect_err("bd-e2.2.1 is already occupied, so the rename must fail");

    let deps = storage.get_dependencies_full("bd-e1.1").unwrap();
    assert!(
        !deps.iter().any(|dep| dep.depends_on_id == "bd-e2"
            && matches!(dep.dep_type, DependencyType::ParentChild)),
        "the parent-child dependency must not survive a rename failure inside \
         the same attach; got: {deps:?}"
    );
    assert!(
        deps.iter().any(|dep| dep.depends_on_id == "bd-e1"
            && matches!(dep.dep_type, DependencyType::ParentChild)),
        "the original dependency must still be in place; got: {deps:?}"
    );

    assert!(storage.get_issue("bd-e1.1").unwrap().is_some());
    assert!(storage.get_issue("bd-e1.1.1").unwrap().is_some());
    assert!(storage.get_issue("bd-e2.2").unwrap().is_none());
    assert!(
        storage.get_issue("bd-e2.2.1").unwrap().is_some(),
        "the pre-existing squatter must be untouched"
    );
}

/// Regression for review minor 1 on bds-a23.4.1: `set_parent_with_options`
/// already no-ops when the dependency edge is unchanged, but `attach_to_parent`
/// used to rename unconditionally regardless -- reattaching to the current
/// parent minted a gratuitous new ID and left a tombstone behind for no
/// actual change.
#[test]
fn attach_to_parent_is_a_no_op_when_already_a_child_of_the_target() {
    let mut storage = test_db();
    create_at(&mut storage, "bd-parent");
    create_at(&mut storage, "bd-child");

    let attached = storage
        .attach_to_parent("bd-child", "bd-parent", "tester")
        .unwrap();
    assert_eq!(attached, "bd-parent.1");

    let reattached = storage
        .attach_to_parent("bd-parent.1", "bd-parent", "tester")
        .unwrap();
    assert_eq!(
        reattached, "bd-parent.1",
        "reattaching to the current parent must be a no-op, not mint a new slot"
    );

    assert!(
        storage.get_issue("bd-parent.2").unwrap().is_none(),
        "a true no-op must not consume another slot"
    );
    let issue = storage.get_issue("bd-parent.1").unwrap().unwrap();
    assert_eq!(
        issue.former_ids,
        vec!["bd-child".to_string()],
        "the first, real attach recorded `bd-child` as expected, but the \
         second, no-op reattach must not append another hop"
    );
}

/// Regression for bds-a23.8: the no-op guard tested
/// `issue_id.starts_with(parent_id + ".")`, which also matches a *deeper*
/// descendant, not just a direct child. `bd-p.1.1` created directly (not via
/// a real two-level attach) whose `parent-child` dependency names `bd-p`
/// itself is exactly the diverged shape this epic exists to repair -- the ID
/// claims `bd-p.1` as its parent while the dependency says `bd-p` -- and the
/// depth-blind guard would treat it as "already attached" and silently no-op
/// instead of renumbering it to a real direct child of `bd-p`.
#[test]
fn attach_to_parent_renumbers_a_two_level_id_whose_dep_names_the_grandparent_directly() {
    let mut storage = test_db();
    create_at(&mut storage, "bd-p");
    create_at(&mut storage, "bd-p.1.1");
    storage
        .add_dependency("bd-p.1.1", "bd-p", "parent-child", "tester")
        .unwrap();

    let result = storage
        .attach_to_parent("bd-p.1.1", "bd-p", "tester")
        .unwrap();

    assert_ne!(
        result, "bd-p.1.1",
        "a depth-blind guard treats this as already attached and no-ops; it \
         must renumber to a direct child of bd-p instead"
    );
    let suffix = result
        .strip_prefix("bd-p.")
        .expect("the repaired id must be a dotted child of bd-p");
    assert!(
        !suffix.is_empty() && !suffix.contains('.'),
        "the repaired id must be a *direct* child of bd-p (exactly one dotted \
         segment past the parent), not another multi-level id; got {result}"
    );
}

/// Regression for bds-a23.9: `attach_to_parent` calls `rename_issue_in_tx`
/// directly, bypassing `rename_issue`'s pre-transaction `id_exists(new_id)`
/// guard. `child_counters` is only ever bumped by `create_issue` (for the
/// exact id created) and by a full JSONL import's trailing
/// `rebuild_child_counters_in_tx` sweep -- neither of which runs for a row
/// written the way a JSONL import actually lands one row at a time,
/// `SqliteStorage::upsert_issue_for_import` (`src/storage/sqlite.rs:12213`,
/// the same primitive `process_import_action` calls per line while streaming
/// an import). Writing a sibling through it, the way an import applies each
/// line before its own trailing rebuild catches up, leaves
/// `child_counters["bd-e1"]` at the real first child's count of 1 even
/// though `bd-e1.2` already exists -- so a later, independent attach into
/// `bd-e1` computes slot 2 and collides with it. Before the fix that
/// collision fell through to a raw SQLite PRIMARY KEY constraint error
/// instead of the same clear `"<id> already exists"` validation error
/// `rename_issue` would have produced.
#[test]
fn attach_to_parent_reports_a_clear_error_when_a_jsonl_imported_child_collides() {
    let mut storage = test_db();
    create_at(&mut storage, "bd-e1");
    // A real first child so `child_counters["bd-e1"]` exists at 1 and
    // `next_child_number` takes the `child_counters` fast path rather than
    // the live-row scan fallback (which would see the imported sibling below
    // and not reproduce the staleness this test targets).
    create_at(&mut storage, "bd-e1.1");
    storage
        .add_dependency("bd-e1.1", "bd-e1", "parent-child", "tester")
        .unwrap();

    // A second child, `bd-e1.2`, arriving via JSONL import: written through
    // the same row-level primitive a real import uses, which does not touch
    // `child_counters` -- the counter stays at 1 even though slot 2 is now
    // taken, the exact staleness the bug depends on.
    let mut imported_child = fixtures::issue("Imported child");
    imported_child.id = "bd-e1.2".to_string();
    storage.upsert_issue_for_import(&imported_child).unwrap();
    storage
        .add_dependency("bd-e1.2", "bd-e1", "parent-child", "tester")
        .unwrap();
    assert_eq!(
        storage.next_child_number("bd-e1").unwrap(),
        2,
        "precondition: the counter must still say 2 is free, even though \
         bd-e1.2 already exists -- that mismatch is the bug"
    );

    create_at(&mut storage, "bd-movable");
    let err = storage
        .attach_to_parent("bd-movable", "bd-e1", "tester")
        .expect_err("bd-e1.2 is already occupied, so the attach must fail");

    let message = err.to_string();
    assert!(
        message.contains("bd-e1.2") && message.contains("already exists"),
        "the error must be the same clear validation error rename_issue gives, \
         not a raw SQLite constraint violation; got: {message}"
    );
    assert!(
        !message.to_lowercase().contains("constraint"),
        "must not leak a raw SQLite constraint error; got: {message}"
    );
}

/// bds-a23.10: every renamed node, at any depth, gets the same provenance as
/// the top-level node -- not just the directly renamed one.
#[test]
fn every_renamed_descendant_records_its_own_former_id() {
    let mut storage = tree();

    storage.rename_issue("bd-p.1", "bd-q.7", "tester").unwrap();

    let grandchild = storage
        .get_issue("bd-q.7.1")
        .unwrap()
        .expect("bd-p.1.1 moved to bd-q.7.1");
    assert_eq!(
        grandchild.former_ids,
        vec!["bd-p.1.1"],
        "a descendant carried by the cascade must record where it came from"
    );
}

#[test]
fn every_vacated_descendant_id_gets_a_tombstone() {
    let mut storage = tree();

    storage.rename_issue("bd-p.1", "bd-q.7", "tester").unwrap();

    let stone = storage
        .get_issue("bd-p.1.1")
        .unwrap()
        .expect("the vacated descendant ID keeps a tombstone");
    assert_eq!(stone.status, Status::Tombstone);
    assert!(
        stone
            .delete_reason
            .as_deref()
            .unwrap_or_default()
            .contains("bd-q.7.1"),
        "the tombstone must name where the descendant went; got {:?}",
        stone.delete_reason
    );
}

#[test]
fn a_descendant_old_id_resolves_to_its_new_home() {
    let mut storage = tree();
    storage.rename_issue("bd-p.1", "bd-q.7", "tester").unwrap();

    let found = storage.find_id_by_former_id("bd-p.1.1").unwrap();

    assert_eq!(found.as_deref(), Some("bd-q.7.1"));
}

#[test]
fn descendant_tombstones_do_not_block_the_old_parent_closing() {
    let mut storage = tree();

    storage.rename_issue("bd-p.1", "bd-q.7", "tester").unwrap();
    storage.rename_issue("bd-p.2", "bd-q.8", "tester").unwrap();

    let blockers = storage.get_open_dot_notation_children("bd-p").unwrap();

    assert!(
        blockers.is_empty(),
        "tombstones are terminal and must not count as open children; got {blockers:?}"
    );
}

#[test]
fn a_descendant_carrying_an_external_ref_can_be_renamed() {
    let mut storage = test_db();
    for id in ["bd-x", "bd-x.1", "bd-x.1.1"] {
        let mut issue = fixtures::issue(id);
        issue.id = id.to_string();
        if id == "bd-x.1.1" {
            issue.external_ref = Some("JIRA-777".to_string());
        }
        storage.create_issue(&issue, "tester").unwrap();
    }

    storage
        .rename_issue("bd-x.1", "bd-y.4", "tester")
        .expect("a descendant's external_ref must not collide with its own tombstone");

    let moved = storage.get_issue("bd-y.4.1").unwrap().expect("moved");
    assert_eq!(moved.external_ref.as_deref(), Some("JIRA-777"));
    let stone = storage.get_issue("bd-x.1.1").unwrap().expect("tombstone");
    assert_eq!(
        stone.external_ref, None,
        "the tombstone must not hold the reference"
    );
}

/// bds-a23.10 fix round 1: `descendant_ids` has no status filter, so a
/// subtree already containing a tombstone (left behind by an earlier direct
/// rename of one of its members) gets that tombstone carried along by every
/// later ancestor rename too. `record_rename_provenance` must not run again
/// for a descendant that is already a tombstone -- doing so mints a second,
/// semantically empty tombstone every hop, and the row count compounds
/// instead of growing by a fixed amount per rename.
#[test]
fn repeated_ancestor_renames_do_not_compound_an_already_tombstoned_descendant() {
    let mut storage = test_db();
    create_at(&mut storage, "bd-p");
    create_at(&mut storage, "bd-p.1");

    // A direct rename of the child leaves exactly one tombstone behind, at
    // bd-p.1, alongside the live row now at bd-p.5.
    storage.rename_issue("bd-p.1", "bd-p.5", "tester").unwrap();
    let after_direct_rename = storage.get_all_issues_metadata().unwrap().len();

    // Ancestor rename #1: bd-p's subtree carries both the live bd-p.5 and
    // the already-dead bd-p.1 tombstone.
    storage.rename_issue("bd-p", "bd-q", "tester").unwrap();
    let after_first_ancestor_rename = storage.get_all_issues_metadata().unwrap().len();

    // Ancestor rename #2: the same shape, one hop further out. If the
    // already-tombstoned descendant were being re-tombstoned each time, this
    // hop's growth would be larger than the first hop's, not equal to it.
    storage.rename_issue("bd-q", "bd-r", "tester").unwrap();
    let after_second_ancestor_rename = storage.get_all_issues_metadata().unwrap().len();

    let first_hop_growth = after_first_ancestor_rename - after_direct_rename;
    let second_hop_growth = after_second_ancestor_rename - after_first_ancestor_rename;

    // Fixed shape: each ancestor rename mints exactly one tombstone for the
    // top-level node and one for the live descendant (bd-p.5); the already-
    // tombstoned descendant (bd-p.1, carried along) contributes zero new
    // rows per hop, no matter how many further ancestor renames it rides.
    assert_eq!(
        first_hop_growth, 2,
        "an ancestor rename over a subtree with one live and one already- \
         tombstoned descendant must add exactly 2 rows (one tombstone for \
         the renamed ancestor, one for the live descendant) -- got {first_hop_growth}"
    );
    assert_eq!(
        second_hop_growth, first_hop_growth,
        "growth per ancestor rename must stay constant, not compound -- \
         hop 1 added {first_hop_growth} rows, hop 2 added {second_hop_growth}; \
         a growing delta means the already-tombstoned descendant is being \
         re-tombstoned on every further ancestor rename"
    );

    // The already-tombstoned descendant's lineage still resolves, moved but
    // never duplicated: it occupies exactly one row, now at bd-r.1.
    let carried_tombstone = storage
        .get_issue("bd-r.1")
        .unwrap()
        .expect("the pre-existing tombstone must have moved with the subtree");
    assert_eq!(carried_tombstone.status, Status::Tombstone);
    assert!(
        storage.get_issue("bd-q.1").unwrap().is_none(),
        "the tombstone must have moved on, not been left behind a second time"
    );
}
