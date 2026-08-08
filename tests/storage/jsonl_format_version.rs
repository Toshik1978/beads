//! bds-ja3: the `issues.jsonl` interchange format's generation marker.
//!
//! The rules being pinned here are stated in full in `src/sync/jsonl_format.rs`;
//! this file is the evidence for each of them. In short: every record br writes
//! declares its generation, an older file is migrated forward and announced, a
//! newer file is refused rather than read best-effort, and a key this build does
//! not recognise is dropped — that last one being a decision with a reason
//! rather than the accident it used to be.

use crate::common;

use beads::error::BeadsError;
use beads::storage::SqliteStorage;
use beads::sync::jsonl_format::{
    CURRENT_JSONL_FORMAT_VERSION, FORMAT_VERSION_KEY, UNVERSIONED_JSONL_FORMAT,
};
use beads::sync::{ExportConfig, ImportConfig, export_to_jsonl, import_from_jsonl};
use common::{fixtures, test_db};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// A workspace with one issue in it, and the path its JSONL will be written to.
fn seeded(id: &str) -> (SqliteStorage, TempDir, PathBuf) {
    let mut storage = test_db();
    let mut issue = fixtures::issue("format probe");
    issue.id = id.to_string();
    storage.create_issue(&issue, "tester").unwrap();

    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    (storage, temp, path)
}

fn records(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .expect("read jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid JSON"))
        .collect()
}

fn write_records(path: &Path, records: &[serde_json::Value]) {
    let body = records
        .iter()
        .map(|record| serde_json::to_string(record).expect("serialize"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, format!("{body}\n")).expect("write jsonl");
}

#[test]
fn every_exported_record_declares_the_current_generation() {
    let (storage, _temp, path) = seeded("bd-gen");

    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();

    let exported = records(&path);
    assert_eq!(exported.len(), 1);
    assert_eq!(
        exported[0].get(FORMAT_VERSION_KEY),
        Some(&serde_json::Value::from(CURRENT_JSONL_FORMAT_VERSION)),
        "a file with no generation marker is one no future reader can date"
    );
}

/// Criterion: an import of a previous-generation file upgrades it, says what it
/// changed, and is idempotent on a second run.
#[test]
fn a_previous_generation_file_is_migrated_reported_and_then_left_alone() {
    let (mut storage, _temp, path) = seeded("bd-old");
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();

    // Strip the marker back off: this is exactly what every file written
    // before the marker existed looks like.
    let mut legacy = records(&path);
    for record in &mut legacy {
        record
            .as_object_mut()
            .expect("object")
            .remove(FORMAT_VERSION_KEY);
    }
    write_records(&path, &legacy);

    let first = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd-"))
        .expect("legacy file imports");
    assert_eq!(first.format_upgrades.upgraded, 1, "got {first:?}");
    assert_eq!(
        first.format_upgrades.oldest_seen,
        Some(UNVERSIONED_JSONL_FORMAT)
    );
    assert!(
        first.format_upgrades.needs_restamp(),
        "an upgraded file has to be rewritten before the upgrade is finished"
    );
    assert_eq!(
        storage.get_metadata("needs_flush").unwrap().as_deref(),
        Some("true"),
        "the restamp is carried out by the next flush, which is what stops the \
         upgrade being announced on every run"
    );

    // The flush is what actually upgrades the file on disk.
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
    assert_eq!(
        records(&path)[0].get(FORMAT_VERSION_KEY),
        Some(&serde_json::Value::from(CURRENT_JSONL_FORMAT_VERSION))
    );

    let second = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd-"))
        .expect("restamped file imports");
    assert_eq!(
        second.format_upgrades.upgraded, 0,
        "the second run must find nothing left to migrate; got {second:?}"
    );
    assert!(!second.format_upgrades.needs_restamp());
}

/// The migration moves no data today — generation 0 and 1 describe the same
/// field set — and that has to stay verified, not assumed, or the first real
/// migration will land on top of an untested mechanism.
#[test]
fn migrating_from_the_unversioned_generation_changes_no_issue_content() {
    let (mut storage, _temp, path) = seeded("bd-same");
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
    let before = storage.get_issue("bd-same").unwrap().expect("exists");

    let mut legacy = records(&path);
    for record in &mut legacy {
        record
            .as_object_mut()
            .expect("object")
            .remove(FORMAT_VERSION_KEY);
    }
    write_records(&path, &legacy);
    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd-")).unwrap();

    let after = storage.get_issue("bd-same").unwrap().expect("exists");
    assert_eq!(
        before, after,
        "0 -> 1 is a stamp; no field moved, was renamed, or changed meaning"
    );
}

/// Criterion: a newer generation is recognised as newer. It is refused rather
/// than read best-effort, because a newer generation may reinterpret a key this
/// build already knows — and a best-effort read would flush the misreading back
/// over a committed file.
#[test]
fn a_newer_generation_is_refused_and_leaves_the_database_untouched() {
    let (mut storage, _temp, path) = seeded("bd-future");
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();

    let mut future = records(&path);
    for record in &mut future {
        record.as_object_mut().expect("object").insert(
            FORMAT_VERSION_KEY.to_string(),
            serde_json::Value::from(CURRENT_JSONL_FORMAT_VERSION + 1),
        );
        record
            .as_object_mut()
            .expect("object")
            .insert("title".to_string(), serde_json::Value::from("rewritten"));
    }
    write_records(&path, &future);

    let error = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd-"))
        .expect_err("a newer generation must not import");
    assert!(
        matches!(error, BeadsError::JsonlFormatTooNew { found, supported }
            if found == CURRENT_JSONL_FORMAT_VERSION + 1
                && supported == CURRENT_JSONL_FORMAT_VERSION),
        "unexpected error: {error:?}"
    );

    assert_eq!(
        storage
            .get_issue("bd-future")
            .unwrap()
            .expect("exists")
            .title,
        "format probe",
        "the refused import must not have applied any part of the file"
    );
}

/// Criterion: the unknown-key rule is decided, documented, and pinned.
///
/// The decision is **drop**. Within a generation this build understands, an
/// unrecognised key is not a future field — future fields travel behind the
/// version marker, and the test above is what protects them. It is a foreign
/// field from a tracker this fork does not model, and carrying it would mean
/// storing meaningless data in the derived database and letting it vote in
/// `sync_equals`, `content_hash` and the three-way merge.
#[test]
fn an_unrecognised_key_is_dropped_rather_than_carried_through_a_round_trip() {
    let (mut storage, _temp, path) = seeded("bd-extra");
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();

    let mut foreign = records(&path);
    for record in &mut foreign {
        let object = record.as_object_mut().expect("object");
        object.insert(
            "lease_granted_node".to_string(),
            serde_json::Value::from("node-7"),
        );
        // Bump the timestamp so the record is unambiguously newer and the
        // importer applies it: the point is what survives an applied record,
        // not whether it was applied.
        object.insert(
            "updated_at".to_string(),
            serde_json::Value::from("2099-01-01T00:00:00Z"),
        );
    }
    write_records(&path, &foreign);

    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd-"))
        .expect("a record with a foreign key still imports");

    let round_tripped = path.parent().expect("dir").join("round-tripped.jsonl");
    export_to_jsonl(&storage, &round_tripped, &ExportConfig::default()).unwrap();

    let exported = records(&round_tripped);
    assert_eq!(exported.len(), 1);
    assert!(
        exported[0].get("lease_granted_node").is_none(),
        "an unrecognised key is dropped, deliberately -- see the module doc on \
         `sync::jsonl_format` for why forward compatibility is provided by the \
         version refusal instead: {}",
        exported[0]
    );
    assert_eq!(
        exported[0].get(FORMAT_VERSION_KEY),
        Some(&serde_json::Value::from(CURRENT_JSONL_FORMAT_VERSION)),
        "dropping a foreign key must not disturb the marker"
    );
}
