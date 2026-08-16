//! bds-n8d: `br sync --import-only` ignored fields deleted from the JSONL.
//!
//! A hand edit that removes a key from a record in `.beads/issues.jsonl` left
//! the old value in the database. The import reported the record as "up-to-date"
//! — the two sides plainly disagreed — and, because the skip left the record's
//! export hash unrecorded, set `needs_flush`, so the next write to any issue
//! exported the unedited row straight back over the file. The edit appeared to
//! work and then silently undid itself.
//!
//! The cause was `determine_action` deciding purely on `updated_at`: a hand
//! edit changes content without touching the timestamp, both sides read as the
//! same revision, and equal meant skip. At a tie the JSONL now wins, which is
//! what the two artefacts are — the file is committed and the `.db` beside it
//! is gitignored and rebuildable.
//!
//! `owner` and `source_repo` are both exercised on purpose: `owner` is part of
//! `content_hash` and `source_repo` is not, so a fix that leaned on the stored
//! hash alone would pass the first test and fail the second.

use assert_cmd::prelude::*;
use std::path::Path;
use std::process::Command;

fn br(workspace: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("br"));
    cmd.current_dir(workspace);
    cmd
}

fn init_workspace() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    br(temp.path()).arg("init").assert().success();
    temp
}

fn records(workspace: &Path) -> Vec<serde_json::Value> {
    let jsonl =
        std::fs::read_to_string(workspace.join(".beads/issues.jsonl")).expect("read issues.jsonl");
    jsonl
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("issue record is valid JSON"))
        .collect()
}

/// Delete `key` from every record, leaving `updated_at` untouched — the hand
/// edit the bug is about. Bumping the timestamp would make the import succeed
/// for the ordinary last-write-wins reason and test nothing.
fn strip_key(workspace: &Path, key: &str) {
    let stripped = records(workspace)
        .into_iter()
        .map(|mut record| {
            record
                .as_object_mut()
                .expect("record is a JSON object")
                .remove(key);
            serde_json::to_string(&record).expect("re-serialize record")
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        workspace.join(".beads/issues.jsonl"),
        format!("{stripped}\n"),
    )
    .expect("write issues.jsonl");
}

fn import_only(workspace: &Path) -> String {
    let output = br(workspace)
        .args(["sync", "--import-only"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).expect("import output is utf-8")
}

#[test]
fn a_field_deleted_from_the_jsonl_is_removed_from_the_database() {
    let temp = init_workspace();
    br(temp.path()).args(["create", "probe"]).assert().success();
    let id = records(temp.path())[0]["id"]
        .as_str()
        .expect("id is a string")
        .to_string();
    br(temp.path())
        .args(["update", &id, "--owner", "alice"])
        .assert()
        .success();

    strip_key(temp.path(), "owner");
    let report = import_only(temp.path());

    assert!(
        report.contains("Updated: 1 issues"),
        "the edit must be applied and counted as an update, got:\n{report}"
    );

    // And it must stay applied: the old behaviour left the record's export
    // hash unrecorded, which set needs_flush and wrote the stale row back.
    br(temp.path())
        .args(["create", "a later write that triggers a flush"])
        .assert()
        .success();
    let probe = records(temp.path())
        .into_iter()
        .find(|record| record["id"] == serde_json::json!(id))
        .expect("probe still present");
    assert!(
        probe.get("owner").is_none(),
        "a later flush restored the deleted field: {probe}"
    );
}

/// `source_repo` is outside `content_hash`, so this is the case a
/// hash-comparison fix would miss.
#[test]
fn a_field_outside_the_content_hash_is_removed_too() {
    let temp = init_workspace();
    br(temp.path()).args(["create", "probe"]).assert().success();
    assert!(
        records(temp.path())[0].get("source_repo").is_some(),
        "the test needs source_repo present to delete it"
    );

    strip_key(temp.path(), "source_repo");
    let report = import_only(temp.path());

    assert!(report.contains("Updated: 1 issues"), "got:\n{report}");
    br(temp.path())
        .args(["create", "a later write that triggers a flush"])
        .assert()
        .success();
    let probe = records(temp.path())
        .into_iter()
        .find(|record| record["title"] == serde_json::json!("probe"))
        .expect("probe still present");
    assert!(
        probe.get("source_repo").is_none(),
        "a later flush restored the deleted field: {probe}"
    );
}

/// The other half of the rule: a record that genuinely matches the database
/// must still be skipped, not rewritten. Without this the fix would turn every
/// import into a full upsert.
#[test]
fn an_unchanged_record_is_still_reported_as_up_to_date() {
    let temp = init_workspace();
    br(temp.path())
        .args(["create", "edited"])
        .assert()
        .success();
    br(temp.path())
        .args(["create", "untouched"])
        .assert()
        .success();

    // Edit exactly one record, so the file hash changes and the import does
    // per-record work on both.
    let edited = records(temp.path())
        .into_iter()
        .map(|mut record| {
            if record["title"] == serde_json::json!("edited") {
                record
                    .as_object_mut()
                    .expect("object")
                    .insert("owner".to_string(), serde_json::json!("bob"));
            }
            serde_json::to_string(&record).expect("re-serialize")
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        temp.path().join(".beads/issues.jsonl"),
        format!("{edited}\n"),
    )
    .expect("write issues.jsonl");

    let report = import_only(temp.path());
    assert!(report.contains("Updated: 1 issues"), "got:\n{report}");
    assert!(
        report.contains("Skipped: 1 issues"),
        "the untouched record must still skip, got:\n{report}"
    );
}
