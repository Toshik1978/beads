//! A rename from another clone never landed on a clone that still held the
//! old id.
//!
//! Clone A reparented an issue: the row moved to a new hierarchical id, the old
//! id went into its `former_ids`, and a tombstone was left under the old id.
//! Clone B, which still had the old id as a live row, pulled that JSONL and
//! imported it. The renamed record matched B's old row — by `external_ref` if
//! it had one, by content hash otherwise, since the hash leaves the id out — so
//! the import folded the new id into the old row instead of inserting it. The
//! old id's own tombstone then lost to that just-written row, the import
//! certified the result, and every later `--import-only` reported the file as
//! current.
//!
//! The next write on B finished the job: the import had left `needs_flush`
//! set for the rows it could not certify, and `needs_flush` made the flush a
//! full export with the stale-database guard switched off, so B's stale state
//! was written over the pulled file and the new ids disappeared from it.
//!
//! Two rules now hold. A record may match a local issue by `external_ref` or
//! content hash only when the file does not carry that issue under its own id
//! — a file cannot hold two records for one issue. And a local row the import
//! keeps over the file's copy is marked dirty, so it is flushed like any other
//! local edit instead of licensing an unguarded full export.

use crate::common;

use beads::model::Status;
use beads::storage::SqliteStorage;
use beads::sync::{ExportConfig, ImportConfig, auto_flush, export_to_jsonl, import_from_jsonl};
use common::{fixtures, test_db};
use std::path::Path;
use tempfile::TempDir;

fn import(storage: &mut SqliteStorage, path: &Path) {
    import_from_jsonl(storage, path, &ImportConfig::default(), Some("bd-")).expect("import");
}

fn export(storage: &SqliteStorage, path: &Path) {
    export_to_jsonl(storage, path, &ExportConfig::default()).expect("export");
}

fn closed_issue(id: &str, external_ref: Option<&str>) -> beads::model::Issue {
    let mut issue = fixtures::issue(id);
    issue.id = id.to_string();
    issue.status = Status::Closed;
    issue.closed_at = Some(issue.updated_at);
    issue.close_reason = Some("done".to_string());
    issue.external_ref = external_ref.map(str::to_string);
    issue
}

/// Clone A holds `issue`, clone B has imported A's export of it, and then A
/// renames `old_id` to `new_id` and exports again. Returns B and the path of
/// the post-rename file, not yet imported by B.
fn two_clones_after_a_rename(
    issue: &beads::model::Issue,
    new_id: &str,
    after_rename: impl FnOnce(&mut SqliteStorage),
) -> (SqliteStorage, TempDir, std::path::PathBuf) {
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    std::fs::create_dir_all(&beads_dir).unwrap();
    let path = beads_dir.join("issues.jsonl");

    let mut a = test_db();
    a.create_issue(issue, "tester").unwrap();
    export(&a, &path);

    let mut b = test_db();
    import(&mut b, &path);
    assert!(
        b.get_issue(&issue.id).unwrap().is_some(),
        "B holds the old id"
    );

    a.rename_issue(&issue.id, new_id, "tester").unwrap();
    after_rename(&mut a);
    export(&a, &path);

    (b, temp, path)
}

fn assert_rename_landed(b: &SqliteStorage, old_id: &str, new_id: &str) {
    let moved = b
        .get_issue(new_id)
        .unwrap()
        .unwrap_or_else(|| panic!("{new_id} must exist after importing the rename"));
    assert_eq!(moved.former_ids, vec![old_id.to_string()]);

    let vacated = b.get_issue(old_id).unwrap().expect("tombstone row");
    assert_eq!(
        vacated.status,
        Status::Tombstone,
        "{old_id} must become the tombstone the file carries, not stay live"
    );
}

/// No `external_ref`: the renamed record used to match the old row by content
/// hash, which leaves the id out.
#[test]
fn a_rename_lands_on_a_clone_that_holds_the_old_id() {
    let issue = closed_issue("bd-old", None);
    let (mut b, _temp, path) = two_clones_after_a_rename(&issue, "bd-new", |_| {});

    import(&mut b, &path);

    assert_rename_landed(&b, "bd-old", "bd-new");
}

/// With an `external_ref` — a remote-mirrored issue — the match went through
/// Phase 1 instead, and did so even when the renamed row had been edited since,
/// so its content no longer hashed like the old row's.
#[test]
fn a_rename_with_an_external_ref_lands_and_carries_the_ref() {
    let issue = closed_issue("bd-old", Some("YT-1"));
    let (mut b, _temp, path) = two_clones_after_a_rename(&issue, "bd-new", |a| {
        a.update_issue(
            "bd-new",
            &beads::storage::IssueUpdate {
                notes: Some(Some("edited after the rename".to_string())),
                ..Default::default()
            },
            "tester",
        )
        .unwrap();
    });

    import(&mut b, &path);

    assert_rename_landed(&b, "bd-old", "bd-new");
    assert_eq!(
        b.get_issue("bd-new")
            .unwrap()
            .unwrap()
            .external_ref
            .as_deref(),
        Some("YT-1")
    );
    assert_eq!(b.get_issue("bd-old").unwrap().unwrap().external_ref, None);
}

/// The export is in id order, so when the new id sorts first its record is
/// imported while the old row still holds the `external_ref` — which a unique
/// index guards. The file has moved the ref, so the old row must yield it.
#[test]
fn a_rename_to_an_earlier_sorting_id_hands_the_external_ref_over() {
    let issue = closed_issue("bd-zzz", Some("YT-1"));
    let (mut b, _temp, path) = two_clones_after_a_rename(&issue, "bd-aaa", |_| {});

    import(&mut b, &path);

    assert_rename_landed(&b, "bd-zzz", "bd-aaa");
    assert_eq!(
        b.get_issue("bd-aaa")
            .unwrap()
            .unwrap()
            .external_ref
            .as_deref(),
        Some("YT-1")
    );
}

/// A row the import keeps over the file's copy is an unflushed local edit and
/// is flushed as one: marked dirty, not `needs_flush`, whose full export skips
/// the guard that refuses to drop ids the file has and the database lacks.
#[test]
fn a_local_win_on_import_is_flushed_without_dropping_file_ids() {
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    std::fs::create_dir_all(&beads_dir).unwrap();
    let path = beads_dir.join("issues.jsonl");

    let mut storage = test_db();
    let mut local = fixtures::issue("bd-local");
    local.id = "bd-local".to_string();
    local.title = "Newer local title".to_string();
    local.updated_at += chrono::Duration::hours(1);
    storage.create_issue(&local, "tester").unwrap();
    storage
        .clear_dirty_flags(&["bd-local".to_string()])
        .unwrap();

    let mut older = local.clone();
    older.title = "Older file title".to_string();
    older.updated_at -= chrono::Duration::hours(2);
    let line = beads::sync::jsonl_format::to_line(&older).unwrap();
    std::fs::write(&path, format!("{line}\n")).unwrap();

    import(&mut storage, &path);

    assert_eq!(
        storage.get_issue("bd-local").unwrap().unwrap().title,
        "Newer local title",
        "last-writer-wins keeps the newer local row"
    );
    assert_ne!(
        storage.get_metadata("needs_flush").unwrap().as_deref(),
        Some("true")
    );
    assert_eq!(storage.get_dirty_issue_ids().unwrap(), vec!["bd-local"]);

    // The file gains a record the database has not imported yet — a pull
    // landing between the import and the next write.
    let mut pulled = fixtures::issue("bd-pulled");
    pulled.id = "bd-pulled".to_string();
    let pulled_line = beads::sync::jsonl_format::to_line(&pulled).unwrap();
    std::fs::write(&path, format!("{line}\n{pulled_line}\n")).unwrap();

    auto_flush(&mut storage, &beads_dir, &path, false).unwrap();

    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.contains("\"id\":\"bd-pulled\""),
        "the flush dropped a record the database had not imported:\n{written}"
    );
    assert!(written.contains("Newer local title"), "{written}");
}
