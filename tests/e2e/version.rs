//! E2E tests for the version command.
//!
//! Tests the `br version` command and its flags: --short, --json.
//! Part of beads-1hof.

// `common` is now the `test-support` crate; aliased so that the 753
// `common::` paths in this suite keep working unchanged.
use crate::common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

#[test]
fn e2e_version_short_flag() {
    let _log = common::test_log("e2e_version_short_flag");
    let workspace = BrWorkspace::new();

    // Test --short flag
    let version = run_br(&workspace, ["version", "--short"], "version_short");
    assert!(
        version.status.success(),
        "version --short failed: {}",
        version.stderr
    );

    let stdout = version.stdout.trim();
    // Should be just the version number, e.g. "0.1.7"
    assert!(
        stdout.chars().all(|c| c.is_numeric() || c == '.'),
        "version --short should contain only version number, got: '{}'",
        stdout
    );
    assert!(
        stdout.contains('.'),
        "version --short should look like semver, got: '{}'",
        stdout
    );
}

#[test]
fn e2e_version_json_flag() {
    let _log = common::test_log("e2e_version_json_flag");
    let workspace = BrWorkspace::new();

    // Test --json flag
    let version = run_br(&workspace, ["version", "--json"], "version_json");
    assert!(
        version.status.success(),
        "version --json failed: {}",
        version.stderr
    );

    let payload = extract_json_payload(&version.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON");

    // Verify fields
    assert!(json.get("version").is_some(), "missing version field");
    assert!(json.get("build").is_some(), "missing build field");
    // `commit` is asserted absent rather than conditionally present: the build
    // script that stamped it is gone, so a `commit` key here would mean one
    // came back. See `there_is_no_build_script_and_no_build_dependencies` in
    // tests/licensing.rs for why it went.
    assert!(
        json.get("commit").is_none(),
        "commit field should be absent: no build script stamps one"
    );
    // No cargo features exist anymore, so the features field is never populated.
    assert!(
        json.get("features").is_none(),
        "features field should be absent: no cargo feature can populate it"
    );
}
