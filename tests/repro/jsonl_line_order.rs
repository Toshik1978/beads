//! The JSONL's line order depended on which writer last touched it.
//!
//! A full export writes records in id order. The incremental flush that runs
//! after an ordinary `create` kept existing lines where they were and appended
//! new ids at the end, so after a few creates the file was no longer in id
//! order, and the next full export — forced by anything that sets
//! `needs_flush` — moved those lines back. The diff was pure reordering, and
//! whether a given sync produced one looked random.
//!
//! The incremental flush now places a new id where a full export would, so on
//! an id-ordered file the two writers produce identical bytes.

use crate::common;

use beads::sync::{ExportConfig, auto_flush, export_to_jsonl, finalize_export};
use common::{fixtures, test_db};
use tempfile::TempDir;

fn issue(id: &str) -> beads::model::Issue {
    let mut issue = fixtures::issue(id);
    issue.id = id.to_string();
    issue
}

#[test]
fn an_incremental_flush_writes_what_a_full_export_would() {
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    std::fs::create_dir_all(&beads_dir).unwrap();
    let path = beads_dir.join("issues.jsonl");

    let mut storage = test_db();
    for id in ["bd-a", "bd-c", "bd-e"] {
        storage.create_issue(&issue(id), "tester").unwrap();
    }
    let exported = export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
    finalize_export(&mut storage, &exported, Some(&exported.issue_hashes), &path).unwrap();

    for id in ["bd-b", "bd-d", "bd-f"] {
        storage.create_issue(&issue(id), "tester").unwrap();
    }
    let flushed = auto_flush(&mut storage, &beads_dir, &path, false).unwrap();
    assert!(flushed.flushed);

    let full = temp.path().join("full.jsonl");
    export_to_jsonl(
        &storage,
        &full,
        &ExportConfig {
            allow_external_jsonl: true,
            ..ExportConfig::default()
        },
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        std::fs::read_to_string(&full).unwrap(),
        "the incremental flush and a full export must agree on line order"
    );
}
