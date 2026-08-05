use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

#[test]
fn test_create_parent_standin() {
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");

    // Initialize the beads directory
    let mut cmd = Command::cargo_bin("br").unwrap();
    cmd.arg("init")
        .env("BEADS_DIR", &beads_dir)
        .assert()
        .success();

    let file_path = temp.path().join("issues.md");
    std::fs::write(
        &file_path,
        r"
## My Epic
### ID
epic1
### Type
epic

## My Task
### Parent
epic1
",
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("br").unwrap();
    let imported = cmd
        .arg("create")
        .arg("-f")
        .arg(&file_path)
        .arg("--json")
        .env("BEADS_DIR", &beads_dir)
        .output()
        .unwrap();
    assert!(
        imported.status.success(),
        "markdown import failed: {}",
        String::from_utf8_lossy(&imported.stderr)
    );

    let issues: Vec<Value> = serde_json::from_slice(&imported.stdout).expect("import json");
    assert_eq!(issues.len(), 2);

    let epic = issues
        .iter()
        .find(|issue| issue["title"].as_str() == Some("My Epic"))
        .expect("imported epic");
    assert_eq!(epic["issue_type"].as_str(), Some("epic"));
    let epic_id = epic["id"].as_str().expect("epic id");

    let task = issues
        .iter()
        .find(|issue| issue["title"].as_str() == Some("My Task"))
        .expect("imported task");
    let dependencies = task["dependencies"].as_array().expect("dependencies array");
    assert!(
        dependencies.iter().any(|dep| {
            dep["depends_on_id"].as_str() == Some(epic_id)
                && dep["type"].as_str() == Some("parent-child")
        }),
        "parent stand-in should resolve to epic {epic_id}, got {dependencies:?}"
    );
}

#[test]
fn test_create_parent_standin_forward_reference() {
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");

    let mut cmd = Command::cargo_bin("br").unwrap();
    cmd.arg("init")
        .env("BEADS_DIR", &beads_dir)
        .assert()
        .success();

    let file_path = temp.path().join("issues.md");
    std::fs::write(
        &file_path,
        r"
## My Task
### Parent
epic1

## My Epic
### ID
epic1
### Type
epic
",
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("br").unwrap();
    let imported = cmd
        .arg("create")
        .arg("-f")
        .arg(&file_path)
        .arg("--json")
        .env("BEADS_DIR", &beads_dir)
        .output()
        .unwrap();
    assert!(
        imported.status.success(),
        "markdown import failed: {}",
        String::from_utf8_lossy(&imported.stderr)
    );

    let issues: Vec<Value> = serde_json::from_slice(&imported.stdout).expect("import json");
    assert_eq!(issues.len(), 2);

    let epic = issues
        .iter()
        .find(|issue| issue["title"].as_str() == Some("My Epic"))
        .expect("imported epic");
    assert_eq!(epic["issue_type"].as_str(), Some("epic"));
    let epic_id = epic["id"].as_str().expect("epic id");

    let task = issues
        .iter()
        .find(|issue| issue["title"].as_str() == Some("My Task"))
        .expect("imported task");
    let dependencies = task["dependencies"].as_array().expect("dependencies array");
    assert!(
        dependencies.iter().any(|dep| {
            dep["depends_on_id"].as_str() == Some(epic_id)
                && dep["type"].as_str() == Some("parent-child")
        }),
        "forward parent stand-in should resolve to epic {epic_id}, got {dependencies:?}"
    );

    // bds-a23.12: a forward `Parent:` reference is resolved through
    // `execute_import`'s `deferred_parent_deps` path -- the seventh
    // chokepoint the hierarchical-id invariant closes. Before that fix, this
    // edge was inserted through the unmodified bulk dependency path, which
    // left the task's id flat while still recording a `parent-child` edge:
    // exactly the divergence the invariant forbids. The task must instead be
    // renumbered under its resolved parent, the same way `--parent` and
    // `dep add --type parent-child` renumber theirs.
    let task_id = task["id"].as_str().expect("task id");
    assert!(
        task_id.starts_with(&format!("{epic_id}.")),
        "a forward Parent: reference must renumber the child to a dotted id \
         under its parent, not leave it flat with a parent-child edge; got \
         task id {task_id} for epic {epic_id}"
    );

    let mut projections_cmd = Command::cargo_bin("br").unwrap();
    let projections = projections_cmd
        .arg("info")
        .arg("--projections")
        .arg("--json")
        .env("BEADS_DIR", &beads_dir)
        .output()
        .unwrap();
    assert!(
        projections.status.success(),
        "br info --projections failed: {}",
        String::from_utf8_lossy(&projections.stderr)
    );
    let info: Value = serde_json::from_slice(&projections.stdout).expect("info json");
    assert_eq!(
        info["projections"]["parity_status"].as_str(),
        Some("matches"),
        "br info --projections must report no divergence after a forward \
         Parent: import; got {info}"
    );
}
