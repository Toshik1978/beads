//! bds-31i: `br create` used to stamp the workspace's absolute path onto every
//! issue via `source_repo_path`. The value reached `.beads/issues.jsonl`, which
//! projects commit, so every issue published the author's directory layout.
//! Nothing consumed the field — no command filtered or routed on it — and
//! `Issue::sync_equals` compared it, so two clones of one repo at different
//! paths disagreed on every record.
//!
//! `source_repo_path` itself was removed in bds-b4f.2.4 (never populated in
//! any real workspace; `source_repo`, the basename, is what survives). But the
//! defect this file pins was never really about that one field — it is that
//! `issues.jsonl` is a file two machines share, and no value in it may name a
//! local absolute path. That invariant outlives the field that first exposed
//! it, so these tests now assert it directly over the whole exported record
//! rather than by naming a column that no longer exists.
//!
//! `source_repo` (the basename) is unaffected and still stamped: it identifies
//! the repo without naming the machine.

use assert_cmd::prelude::*;
use std::process::Command;

/// Every issue record in a workspace's JSONL export, as parsed JSON.
fn issue_records(workspace: &std::path::Path) -> Vec<serde_json::Value> {
    let jsonl =
        std::fs::read_to_string(workspace.join(".beads/issues.jsonl")).expect("read issues.jsonl");
    jsonl
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("issue record is valid JSON"))
        .collect()
}

/// Recursively collect every string value's (JSON-pointer-ish) path within
/// `value`, so a leak can be reported with where it was found rather than
/// just that one exists somewhere in the record.
fn find_strings_containing<'a>(
    value: &'a serde_json::Value,
    needle: &str,
    path: String,
    hits: &mut Vec<(String, &'a str)>,
) {
    match value {
        serde_json::Value::String(s) => {
            if s.contains(needle) {
                hits.push((path, s.as_str()));
            }
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                find_strings_containing(item, needle, format!("{path}[{index}]"), hits);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                find_strings_containing(item, needle, format!("{path}.{key}"), hits);
            }
        }
        _ => {}
    }
}

/// Assert that no string value anywhere in `record` contains `absolute_path`
/// — the general invariant this file exists to guard: a committed JSONL
/// record must never name a local filesystem path, regardless of which field
/// might carry one.
fn assert_no_absolute_path_leak(record: &serde_json::Value, absolute_path: &str) {
    let mut hits = Vec::new();
    find_strings_containing(record, absolute_path, "$".to_string(), &mut hits);
    assert!(
        hits.is_empty(),
        "record leaked this machine's absolute path {absolute_path:?} at {hits:?}: {record}"
    );
}

fn init_workspace() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .current_dir(temp.path())
        .arg("init")
        .assert()
        .success();
    temp
}

#[test]
fn create_does_not_stamp_an_absolute_path_into_the_jsonl() {
    let temp = init_workspace();
    let canonical = temp.path().canonicalize().expect("canonicalize workspace");

    Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .current_dir(temp.path())
        .args(["create", "An issue that must not name this machine"])
        .assert()
        .success();

    let records = issue_records(temp.path());
    assert_eq!(records.len(), 1, "expected exactly one issue");

    assert_no_absolute_path_leak(&records[0], &canonical.to_string_lossy());

    // The basename survives — it is the part that identifies the repo without
    // naming the machine it lives on.
    assert_eq!(
        records[0]
            .get("source_repo")
            .and_then(serde_json::Value::as_str),
        canonical.file_name().and_then(std::ffi::OsStr::to_str),
        "source_repo must still carry the repo basename",
    );
}

#[test]
fn markdown_import_does_not_stamp_an_absolute_path_either() {
    let temp = init_workspace();
    let canonical = temp.path().canonicalize().expect("canonicalize workspace");
    let markdown = temp.path().join("import.md");
    std::fs::write(&markdown, "## Imported issue\n\nA body.\n").expect("write markdown");

    Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .current_dir(temp.path())
        .args(["create", "-f"])
        .arg(&markdown)
        .assert()
        .success();

    let records = issue_records(temp.path());
    assert!(!records.is_empty(), "import produced no issues");
    for record in &records {
        assert_no_absolute_path_leak(record, &canonical.to_string_lossy());
    }
}
