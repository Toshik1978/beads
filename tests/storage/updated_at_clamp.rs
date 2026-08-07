//! bds-6nz: label, dependency and comment writes must not move `updated_at`
//! backwards.
//!
//! `sync_equals` compares labels and dependencies, and both travel in the
//! JSONL, but `determine_action` gates purely on `updated_at`. So a write
//! that changes synced content while *lowering* the timestamp is invisible
//! to the next import: the peer's still-future copy reads as newer, the
//! local edit is skipped, and the following flush exports the stale row back
//! over the JSONL. That is the erasure path bds-a23.6 fixed for whole-issue
//! updates and bds-a23.16 for renames; these are the remaining triggers.
//!
//! Every test here follows the same shape as
//! `crud::update_issue_after_a_future_dated_import_still_advances_updated_at`:
//! seed a row through a real JSONL import stamped hours ahead of this
//! machine's clock, then assert the mutation still advances the timestamp
//! *strictly* — `determine_action`'s `Equal` arm is `Skip`, so a tie is as
//! invisible as a decrease.
//!
//! The ordering matters and is easy to get wrong: `push_updated_at_forward`
//! runs *after* whatever setup the operation under test needs, never before.
//! A setup step that itself bumps `updated_at` (adding the label that is
//! about to be removed, say) would otherwise pull the row back to the real
//! clock and leave the test asserting nothing — four of these passed against
//! the unfixed code when written the other way round.

use crate::common;

use beads::storage::SqliteStorage;
use beads::sync::{ImportConfig, import_from_jsonl};
use chrono::{DateTime, Duration, Utc};
use common::{fixtures, test_db};
use tempfile::TempDir;

fn create(storage: &mut SqliteStorage, id: &str) {
    let mut issue = fixtures::issue("clamp-subject");
    issue.id = id.to_string();
    storage.create_issue(&issue, "tester").unwrap();
}

/// Re-import `id`'s current content stamped six hours into the future, which
/// is how a row acquires an `updated_at` ahead of this machine's clock: a
/// clock-skewed peer wrote it and the JSONL carried it over verbatim.
///
/// Content other than the timestamp is round-tripped from the row as it
/// stands, so this can be dropped in after any amount of setup without
/// undoing it.
fn push_updated_at_forward(storage: &mut SqliteStorage, id: &str) -> DateTime<Utc> {
    let mut future = storage.get_issue(id).unwrap().expect("exists");
    // `get_issue` returns the `issues` row alone; labels and dependencies
    // live in their own tables and have to be hydrated by hand. Without
    // this the record below would say "no labels, no dependencies" and the
    // import — being newer — would obediently strip whatever the setup step
    // just added, leaving the operation under test with nothing to remove.
    future.labels = storage.get_labels(id).unwrap();
    future.dependencies = storage.get_dependencies_full(id).unwrap();
    future.updated_at = Utc::now() + Duration::hours(6);

    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&future).unwrap()),
    )
    .unwrap();
    import_from_jsonl(storage, &path, &ImportConfig::default(), Some("bd-")).unwrap();

    let after_import = storage.get_issue(id).unwrap().expect("exists");
    assert_eq!(
        after_import.updated_at, future.updated_at,
        "import must carry the future timestamp verbatim -- that is the \
         premise of these tests, not something they are checking"
    );
    after_import.updated_at
}

fn assert_advanced(storage: &SqliteStorage, id: &str, before: DateTime<Utc>, what: &str) {
    let after = storage.get_issue(id).unwrap().expect("exists").updated_at;
    assert!(
        after > before,
        "{what} must move updated_at strictly forward even when the row's \
         existing value is already ahead of the real clock; before = \
         {before:?}, after = {after:?}"
    );
}

#[test]
fn add_label_after_a_future_dated_import_still_advances_updated_at() {
    let mut storage = test_db();
    create(&mut storage, "bd-lbladd");
    let before = push_updated_at_forward(&mut storage, "bd-lbladd");

    assert!(storage.add_label("bd-lbladd", "urgent").unwrap());

    assert_advanced(&storage, "bd-lbladd", before, "add_label");
}

#[test]
fn remove_label_after_a_future_dated_import_still_advances_updated_at() {
    let mut storage = test_db();
    create(&mut storage, "bd-lblrm");
    storage.add_label("bd-lblrm", "urgent").unwrap();
    let before = push_updated_at_forward(&mut storage, "bd-lblrm");

    assert!(storage.remove_label("bd-lblrm", "urgent").unwrap());

    assert_advanced(&storage, "bd-lblrm", before, "remove_label");
}

#[test]
fn set_labels_after_a_future_dated_import_still_advances_updated_at() {
    let mut storage = test_db();
    create(&mut storage, "bd-lblset");
    let before = push_updated_at_forward(&mut storage, "bd-lblset");

    storage
        .set_labels("bd-lblset", &["alpha".to_string(), "beta".to_string()])
        .unwrap();

    assert_advanced(&storage, "bd-lblset", before, "set_labels");
}

/// The bulk paths are the ones the bead flagged as needing a design answer:
/// they shared a single `Utc::now()` across every row in one statement. The
/// clamp is per row, so a batch mixing a future-dated row with an ordinary
/// one must advance *both*.
#[test]
fn bulk_label_add_advances_updated_at_for_future_dated_and_ordinary_rows() {
    let mut storage = test_db();
    create(&mut storage, "bd-bulkfuture");
    create(&mut storage, "bd-bulkordinary");

    let future_before = push_updated_at_forward(&mut storage, "bd-bulkfuture");
    let ordinary_before = storage
        .get_issue("bd-bulkordinary")
        .unwrap()
        .expect("exists")
        .updated_at;

    let ids = vec!["bd-bulkfuture".to_string(), "bd-bulkordinary".to_string()];
    let changed = storage.add_label_to_issues_bulk(&ids, "sweep").unwrap();
    assert_eq!(changed.len(), 2);

    assert_advanced(
        &storage,
        "bd-bulkfuture",
        future_before,
        "add_label_to_issues_bulk (future-dated row)",
    );
    assert_advanced(
        &storage,
        "bd-bulkordinary",
        ordinary_before,
        "add_label_to_issues_bulk (ordinary row)",
    );
}

#[test]
fn bulk_label_remove_after_a_future_dated_import_still_advances_updated_at() {
    let mut storage = test_db();
    create(&mut storage, "bd-bulkrm");
    let ids = vec!["bd-bulkrm".to_string()];
    storage.add_label_to_issues_bulk(&ids, "sweep").unwrap();
    let before = push_updated_at_forward(&mut storage, "bd-bulkrm");

    let changed = storage
        .remove_label_from_issues_bulk(&ids, "sweep")
        .unwrap();
    assert_eq!(changed.len(), 1);

    assert_advanced(
        &storage,
        "bd-bulkrm",
        before,
        "remove_label_from_issues_bulk",
    );
}

#[test]
fn rename_label_after_a_future_dated_import_still_advances_updated_at() {
    let mut storage = test_db();
    create(&mut storage, "bd-lblrename");
    storage.add_label("bd-lblrename", "old-name").unwrap();
    let before = push_updated_at_forward(&mut storage, "bd-lblrename");

    assert!(storage.rename_label("old-name", "new-name").unwrap() > 0);

    assert_advanced(&storage, "bd-lblrename", before, "rename_label");
}

#[test]
fn add_dependency_after_a_future_dated_import_still_advances_updated_at() {
    let mut storage = test_db();
    create(&mut storage, "bd-depfrom");
    create(&mut storage, "bd-depto");
    let before = push_updated_at_forward(&mut storage, "bd-depfrom");

    assert!(
        storage
            .add_dependency("bd-depfrom", "bd-depto", "blocks", "tester")
            .unwrap()
    );

    assert_advanced(&storage, "bd-depfrom", before, "add_dependency");
}

#[test]
fn remove_dependency_after_a_future_dated_import_still_advances_updated_at() {
    let mut storage = test_db();
    create(&mut storage, "bd-deprmfrom");
    create(&mut storage, "bd-deprmto");
    storage
        .add_dependency("bd-deprmfrom", "bd-deprmto", "blocks", "tester")
        .unwrap();
    let before = push_updated_at_forward(&mut storage, "bd-deprmfrom");

    assert!(
        storage
            .remove_dependency("bd-deprmfrom", "bd-deprmto")
            .unwrap()
    );

    assert_advanced(&storage, "bd-deprmfrom", before, "remove_dependency");
}

/// `remove_all_dependencies` touches the issue *and* every issue that
/// depended on it, so the dependent is the row this checks: it is the one
/// reached through the bulk path rather than by name.
#[test]
fn remove_all_dependencies_advances_updated_at_on_the_dependent_too() {
    let mut storage = test_db();
    create(&mut storage, "bd-depall");
    create(&mut storage, "bd-depdependent");
    storage
        .add_dependency("bd-depdependent", "bd-depall", "blocks", "tester")
        .unwrap();
    let before = push_updated_at_forward(&mut storage, "bd-depdependent");

    assert_eq!(storage.remove_all_dependencies("bd-depall").unwrap(), 1);

    assert_advanced(
        &storage,
        "bd-depdependent",
        before,
        "remove_all_dependencies",
    );
}

#[test]
fn set_parent_after_a_future_dated_import_still_advances_updated_at() {
    let mut storage = test_db();
    create(&mut storage, "bd-child");
    create(&mut storage, "bd-parent");
    let before = push_updated_at_forward(&mut storage, "bd-child");

    storage
        .set_parent("bd-child", Some("bd-parent"), "tester")
        .unwrap();

    assert_advanced(&storage, "bd-child", before, "set_parent");
}

#[test]
fn add_comment_after_a_future_dated_import_still_advances_updated_at() {
    let mut storage = test_db();
    create(&mut storage, "bd-cmt");
    let before = push_updated_at_forward(&mut storage, "bd-cmt");

    storage
        .add_comment("bd-cmt", "tester", "a comment")
        .unwrap();

    assert_advanced(&storage, "bd-cmt", before, "add_comment");
}

/// Two label writes in a row against the same future-dated issue must each
/// advance, not merely the first: the clamp reads the row's *current* stored
/// value every time, so the second write clears the timestamp the first one
/// just wrote rather than the one the import left.
#[test]
fn successive_label_writes_each_advance_updated_at() {
    let mut storage = test_db();
    create(&mut storage, "bd-lbltwice");
    let seeded = push_updated_at_forward(&mut storage, "bd-lbltwice");

    storage.add_label("bd-lbltwice", "first").unwrap();
    let after_first = storage
        .get_issue("bd-lbltwice")
        .unwrap()
        .expect("exists")
        .updated_at;
    assert!(after_first > seeded);

    storage.add_label("bd-lbltwice", "second").unwrap();
    assert_advanced(&storage, "bd-lbltwice", after_first, "the second add_label");
}
