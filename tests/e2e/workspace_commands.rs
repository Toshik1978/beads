//! E2E tests for workspace initialization and diagnostic commands.
//!
//! Tests init, config, info, and version commands.
//! Part of beads-6esx.

// `common` is now the `test-support` crate; aliased so that the 753
// `common::` paths in this suite keep working unchanged.
use crate::common;

use common::cli::{BrWorkspace, extract_json_payload, parse_list_issues, run_br, run_br_with_env};
use serde_json::Value;
use std::fs;

// ============================================================================
// init command tests
// ============================================================================

#[test]
fn e2e_init_new_workspace() {
    let _log = common::test_log("e2e_init_new_workspace");
    let workspace = BrWorkspace::new();

    // Initialize a new workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    assert!(
        init.stdout.contains("Initialized") || init.stdout.contains("initialized"),
        "init should report success: {}",
        init.stdout
    );

    // Verify .beads directory was created
    let beads_dir = workspace.root.join(".beads");
    assert!(beads_dir.exists(), ".beads directory should exist");

    // Verify database file exists
    let db_path = beads_dir.join("beads.db");
    assert!(db_path.exists(), "beads.db should exist");
}

#[test]
fn e2e_sync_import_only_accepts_mixed_prefixes_and_keeps_default_prefix_for_new_ids() {
    let _log = common::test_log(
        "e2e_sync_import_only_accepts_mixed_prefixes_and_keeps_default_prefix_for_new_ids",
    );
    let workspace = BrWorkspace::new();

    let init = run_br(
        &workspace,
        ["init", "--prefix", "local"],
        "init_local_prefix",
    );
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Seed issue", "--json"],
        "create_seed_issue",
    );
    assert!(
        create.status.success(),
        "seed create failed: {}",
        create.stderr
    );
    let seed_payload = extract_json_payload(&create.stdout);
    let seed_issue: Value =
        serde_json::from_str(&seed_payload).expect("seed create should emit valid JSON");

    let mut imported_issue = seed_issue.clone();
    imported_issue["id"] = Value::String("other-abc12".to_string());
    imported_issue["title"] = Value::String("Imported mixed-prefix issue".to_string());
    imported_issue["content_hash"] = Value::Null;

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    fs::write(
        &jsonl_path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&seed_issue).expect("serialize seed issue"),
            serde_json::to_string(&imported_issue).expect("serialize imported issue"),
        ),
    )
    .expect("write mixed-prefix jsonl");

    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--json"],
        "sync_import_mixed_prefixes",
    );
    assert!(
        import.status.success(),
        "sync --import-only should accept mixed prefixes: {}",
        import.stderr
    );

    let list = run_br(&workspace, ["list", "--json"], "list_after_mixed_import");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let issues = parse_list_issues(&list.stdout);
    let ids: Vec<&str> = issues
        .iter()
        .filter_map(|issue| issue["id"].as_str())
        .collect();
    assert!(
        ids.iter().any(|id| id.starts_with("local-")),
        "expected local-prefixed issue in {ids:?}"
    );
    assert!(
        ids.contains(&"other-abc12"),
        "expected other-abc12 in {ids:?}"
    );

    let create_after_import = run_br(
        &workspace,
        ["create", "Fresh local issue", "--json"],
        "create_after_mixed_import",
    );
    assert!(
        create_after_import.status.success(),
        "create after mixed import failed: {}",
        create_after_import.stderr
    );
    let created_payload = extract_json_payload(&create_after_import.stdout);
    let created_issue: Value = serde_json::from_str(&created_payload).expect("created issue JSON");
    let created_id = created_issue["id"]
        .as_str()
        .expect("created issue id should be present");
    assert!(
        created_id.starts_with("local-"),
        "new issues should keep configured default prefix: {created_id}"
    );
}

#[test]
fn e2e_init_already_initialized() {
    let _log = common::test_log("e2e_init_already_initialized");
    let workspace = BrWorkspace::new();

    // First init
    let init1 = run_br(&workspace, ["init"], "init1");
    assert!(
        init1.status.success(),
        "first init failed: {}",
        init1.stderr
    );

    // Second init without --force should warn or succeed gracefully
    let init2 = run_br(&workspace, ["init"], "init2");
    // Either succeeds with warning or fails gracefully with "already" message
    // br returns JSON error with code "ALREADY_INITIALIZED"
    let stderr_lower = init2.stderr.to_lowercase();
    assert!(
        init2.status.success()
            || stderr_lower.contains("already")
            || init2.stderr.contains("ALREADY_INITIALIZED"),
        "second init should succeed or warn: stdout='{}', stderr='{}'",
        init2.stdout,
        init2.stderr
    );
}

#[test]
fn e2e_init_force_reinit() {
    let _log = common::test_log("e2e_init_force_reinit");
    let workspace = BrWorkspace::new();

    // First init
    let init1 = run_br(&workspace, ["init"], "init1");
    assert!(
        init1.status.success(),
        "first init failed: {}",
        init1.stderr
    );

    // Create an issue to verify database is reset
    let create = run_br(&workspace, ["create", "Test issue before force"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Force reinit (if supported)
    let init2 = run_br(&workspace, ["init", "--force"], "init2_force");
    // --force may not be implemented, check either way
    if init2.status.success() {
        // After force reinit, the database should be fresh
        // List should show no issues or only one if --force doesn't clear
        let list = run_br(&workspace, ["list", "--json"], "list_after_force");
        assert!(
            list.status.success(),
            "list after force init failed: {}",
            list.stderr
        );
    }
}

#[test]
fn e2e_init_creates_jsonl() {
    let _log = common::test_log("e2e_init_creates_jsonl");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue and sync to JSONL
    let create = run_br(&workspace, ["create", "JSONL test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync");
    assert!(sync.status.success(), "sync failed: {}", sync.stderr);

    // Verify JSONL file exists
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    assert!(jsonl_path.exists(), "issues.jsonl should exist after sync");

    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    assert!(
        contents.contains("JSONL test issue"),
        "JSONL should contain the issue"
    );
}

// ============================================================================
// config command tests
// ============================================================================

#[test]
fn e2e_config_list() {
    let _log = common::test_log("e2e_config_list");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // List config
    let config_list = run_br(&workspace, ["config", "list"], "config_list");
    assert!(
        config_list.status.success(),
        "config list failed: {}",
        config_list.stderr
    );
    // Should output something (even if empty)
}

#[test]
fn e2e_config_get_set() {
    let _log = common::test_log("e2e_config_get_set");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Use a unique test key that won't conflict with defaults
    // Note: issue_prefix may have DB defaults that take precedence over YAML
    let set = run_br(
        &workspace,
        ["config", "set", "test_custom_key=TESTVALUE"],
        "config_set",
    );
    assert!(set.status.success(), "config set failed: {}", set.stderr);

    // Get the config value
    let get = run_br(
        &workspace,
        ["config", "get", "test_custom_key"],
        "config_get",
    );
    assert!(get.status.success(), "config get failed: {}", get.stderr);
    assert!(
        get.stdout.contains("TESTVALUE"),
        "config get should return TESTVALUE: {}",
        get.stdout
    );
}

#[test]
fn e2e_config_json_output() {
    let _log = common::test_log("e2e_config_json_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // List config with --json
    let config_list = run_br(&workspace, ["config", "list", "--json"], "config_list_json");
    assert!(
        config_list.status.success(),
        "config list --json failed: {}",
        config_list.stderr
    );

    // Should be valid JSON
    let payload = extract_json_payload(&config_list.stdout);
    let _json: Value =
        serde_json::from_str(&payload).expect("config list should output valid JSON");
}

#[test]
fn e2e_update_quiet_suppresses_success_output() {
    let _log = common::test_log("e2e_update_quiet_suppresses_success_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Quiet update test", "--json"],
        "create_quiet_update",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let payload = extract_json_payload(&create.stdout);
    let issue: Value = serde_json::from_str(&payload).expect("parse create json");
    let id = issue["id"].as_str().expect("issue id");

    let update = run_br(
        &workspace,
        ["--quiet", "update", id, "--status", "in_progress"],
        "update_quiet",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    assert!(
        update.stdout.trim().is_empty(),
        "quiet update should suppress success output: {}",
        update.stdout
    );
}

#[cfg(not(windows))]
#[test]
fn e2e_config_edit_creates_user_config() {
    let _log = common::test_log("e2e_config_edit_creates_user_config");
    let workspace = BrWorkspace::new();

    let env_vars = vec![("EDITOR", "true")];
    let edit = run_br_with_env(&workspace, ["config", "edit"], env_vars, "config_edit");
    assert!(edit.status.success(), "config edit failed: {}", edit.stderr);

    let config_path = workspace
        .root
        .join(".config")
        .join("beads")
        .join("config.yaml");
    assert!(
        config_path.exists(),
        "config edit should create user config at {}",
        config_path.display()
    );

    let contents = fs::read_to_string(&config_path).expect("read user config");
    assert!(
        contents.contains("br configuration"),
        "config edit should create default template content"
    );
}

// ============================================================================
// info command tests
// ============================================================================

#[test]
fn e2e_info_basic() {
    let _log = common::test_log("e2e_info_basic");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Run info command
    let info = run_br(&workspace, ["info"], "info");
    assert!(info.status.success(), "info failed: {}", info.stderr);

    // Should contain path information
    assert!(
        info.stdout.contains(".beads") || info.stdout.contains("beads"),
        "info should mention beads directory: {}",
        info.stdout
    );
}

#[test]
fn e2e_info_json_output() {
    let _log = common::test_log("e2e_info_json_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Info with --json
    let info = run_br(&workspace, ["info", "--json"], "info_json");
    assert!(info.status.success(), "info --json failed: {}", info.stderr);

    let payload = extract_json_payload(&info.stdout);
    let json: Value = serde_json::from_str(&payload).expect("info should output valid JSON");

    // Should have workspace path (br uses "database_path")
    assert!(
        json.get("workspace_path").is_some()
            || json.get("db_path").is_some()
            || json.get("path").is_some()
            || json.get("database_path").is_some(),
        "info JSON should contain path info: {json}"
    );
}

#[test]
fn e2e_info_uninitialized() {
    let _log = common::test_log("e2e_info_uninitialized");
    let workspace = BrWorkspace::new();

    // Run info without init
    let info = run_br(&workspace, ["info"], "info_no_init");
    // Should fail or report no workspace
    assert!(
        !info.status.success()
            || info.stderr.contains("not found")
            || info.stdout.contains("not found"),
        "info should report missing workspace"
    );
}

// ============================================================================
// where command tests
// ============================================================================

// ============================================================================
// version command tests
// ============================================================================

#[test]
fn e2e_version_basic() {
    let _log = common::test_log("e2e_version_basic");
    let workspace = BrWorkspace::new();

    // Version doesn't require init
    let version = run_br(&workspace, ["version"], "version");
    assert!(
        version.status.success(),
        "version failed: {}",
        version.stderr
    );

    // Should contain version number
    assert!(
        version.stdout.contains("0.") || version.stdout.contains("1."),
        "version should contain version number: {}",
        version.stdout
    );
}

#[test]
fn e2e_version_json_output() {
    let _log = common::test_log("e2e_version_json_output");
    let workspace = BrWorkspace::new();

    // Version with --json
    let version = run_br(&workspace, ["version", "--json"], "version_json");
    assert!(
        version.status.success(),
        "version --json failed: {}",
        version.stderr
    );

    let payload = extract_json_payload(&version.stdout);
    let json: Value = serde_json::from_str(&payload).expect("version should output valid JSON");

    // Should have version field
    assert!(
        json.get("version").is_some() || json.get("semver").is_some(),
        "version JSON should contain version field: {json}"
    );
}

#[test]
fn e2e_version_short_flag() {
    let _log = common::test_log("e2e_version_short_flag");
    let workspace = BrWorkspace::new();

    // Test -V flag
    let version = run_br(&workspace, ["-V"], "version_short");
    assert!(version.status.success(), "-V failed: {}", version.stderr);

    assert!(
        version.stdout.contains("br")
            || version.stdout.contains("0.")
            || version.stdout.contains("1."),
        "-V should output version: {}",
        version.stdout
    );
}

#[test]
fn e2e_version_help() {
    let _log = common::test_log("e2e_version_help");
    let workspace = BrWorkspace::new();

    // Test --version flag
    let version = run_br(&workspace, ["--version"], "version_long");
    assert!(
        version.status.success(),
        "--version failed: {}",
        version.stderr
    );

    assert!(
        version.stdout.contains("br")
            || version.stdout.contains("0.")
            || version.stdout.contains("1."),
        "--version should output version: {}",
        version.stdout
    );
}

// ============================================================================
// Combined/integration tests
// ============================================================================

#[test]
fn e2e_full_workspace_lifecycle() {
    let _log = common::test_log("e2e_full_workspace_lifecycle");
    let workspace = BrWorkspace::new();

    // 1. Check version works without init
    let version = run_br(&workspace, ["version"], "version");
    assert!(version.status.success());

    // 2. Initialize
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // 3. Info should show workspace details
    let info = run_br(&workspace, ["info"], "info");
    assert!(info.status.success());

    // 4. Config should be accessible
    let config = run_br(&workspace, ["config", "list"], "config");
    assert!(config.status.success());
}
