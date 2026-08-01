//! bds-31i: `br create` stamped the workspace's absolute path onto every issue.
//!
//! The value reached `.beads/issues.jsonl`, which projects commit, so every
//! issue published the author's directory layout. Nothing consumed the field —
//! no command filtered or routed on it — and `Issue::sync_equals` compares it,
//! so two clones of one repo at different paths disagreed on every record.
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

    Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .current_dir(temp.path())
        .args(["create", "An issue that must not name this machine"])
        .assert()
        .success();

    let records = issue_records(temp.path());
    assert_eq!(records.len(), 1, "expected exactly one issue");

    // Absent, not null or empty: `skip_serializing_if` must drop the key
    // entirely so the field costs nothing in a file that is committed.
    assert!(
        records[0].get("source_repo_path").is_none(),
        "create wrote source_repo_path into the JSONL: {}",
        records[0]
    );
    // The basename survives — it is the part that identifies the repo without
    // naming the machine it lives on.
    assert_eq!(
        records[0]
            .get("source_repo")
            .and_then(serde_json::Value::as_str),
        temp.path()
            .canonicalize()
            .expect("canonicalize workspace")
            .file_name()
            .and_then(std::ffi::OsStr::to_str),
        "source_repo must still carry the repo basename",
    );
}

#[test]
fn markdown_import_does_not_stamp_an_absolute_path_either() {
    let temp = init_workspace();
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
        assert!(
            record.get("source_repo_path").is_none(),
            "import wrote source_repo_path into the JSONL: {record}",
        );
    }
}

/// The field itself is not removed — only its automatic population. A caller
/// who genuinely wants the beads#289 disambiguation opts in, and then it must
/// round-trip.
#[test]
fn update_can_still_set_source_repo_path_explicitly() {
    let temp = init_workspace();

    let created = Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .current_dir(temp.path())
        .args(["create", "Explicit opt-in", "--json"])
        .output()
        .expect("create issue");
    assert!(created.status.success());
    let id = serde_json::from_slice::<serde_json::Value>(&created.stdout)
        .expect("valid json")
        .get("id")
        .and_then(|id| id.as_str())
        .expect("created issue has an id")
        .to_owned();

    Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .current_dir(temp.path())
        .args(["update", &id, "--source-repo-path", "/repos/widget_engine"])
        .assert()
        .success();

    let records = issue_records(temp.path());
    assert_eq!(
        records[0]
            .get("source_repo_path")
            .and_then(serde_json::Value::as_str),
        Some("/repos/widget_engine"),
        "an explicitly set source_repo_path must survive the JSONL round-trip",
    );
}
