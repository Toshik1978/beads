//! A tombstone's `closed_at` was written differently depending on which clone
//! exported it.
//!
//! `br delete` and a rename both tombstone an open issue without setting
//! `closed_at`, so the clone that made the tombstone exported `null`. Every
//! other clone filled it in on import — a terminal record without `closed_at`
//! is repaired to its `updated_at` — and exported that. Import compared the
//! two after the same repair, called them equal, and recorded the file as
//! applied, so the disagreement surfaced only as a full export rewriting the
//! lines for no semantic change.
//!
//! Export now applies the repair import already applies, so every clone
//! writes the same record whatever its database holds.

use crate::common;

use beads::sync::{ExportConfig, ImportConfig, export_to_jsonl, import_from_jsonl};
use common::{fixtures, test_db};
use tempfile::TempDir;

#[test]
fn a_tombstone_exports_the_same_from_every_clone() {
    let temp = TempDir::new().unwrap();
    let origin_path = temp.path().join("origin.jsonl");
    let clone_path = temp.path().join("clone.jsonl");

    let mut origin = test_db();
    let mut issue = fixtures::issue("bd-gone");
    issue.id = "bd-gone".to_string();
    origin.create_issue(&issue, "tester").unwrap();
    origin
        .delete_issue("bd-gone", "tester", "no longer needed", None)
        .unwrap();
    export_to_jsonl(&origin, &origin_path, &ExportConfig::default()).unwrap();

    let mut clone = test_db();
    import_from_jsonl(
        &mut clone,
        &origin_path,
        &ImportConfig::default(),
        Some("bd-"),
    )
    .unwrap();
    export_to_jsonl(&clone, &clone_path, &ExportConfig::default()).unwrap();

    let origin_line = std::fs::read_to_string(&origin_path).unwrap();
    assert_eq!(
        origin_line,
        std::fs::read_to_string(&clone_path).unwrap(),
        "the clone that made the tombstone and a clone that imported it must \
         export the same record"
    );
    let record: serde_json::Value = serde_json::from_str(origin_line.trim()).unwrap();
    assert_eq!(
        record["closed_at"], record["updated_at"],
        "a tombstone carries closed_at, repaired to updated_at as import does"
    );
}
