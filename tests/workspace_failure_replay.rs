// `common` is now the `test-support` crate; aliased so that the 753
// `common::` paths in this suite keep working unchanged.
extern crate test_support as common;

use common::cli::{BrRun, BrWorkspace, extract_json_payload, run_br};
use common::{
    WorkspaceFailureCommandOutcome, WorkspaceFailureFixtureMetadata,
    isolated_workspace_failure_fixture, list_workspace_failure_fixtures,
};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Outcome of one expectation check: `Ok(())` when it held, `Err(message)` when
/// it did not. Checks report rather than panic so a single run can surface every
/// disagreement across every fixture instead of stopping at the first one.
type Check = Result<(), String>;

/// `assert!`, but it reports the failure to the caller instead of unwinding.
macro_rules! check {
    ($cond:expr, $($arg:tt)+) => {
        if !$cond {
            return Err(format!($($arg)+));
        }
    };
}

struct FixtureWorkspace {
    metadata: WorkspaceFailureFixtureMetadata,
    beads_dir: PathBuf,
    workspace: BrWorkspace,
}

fn fixture_workspace(name: &str) -> FixtureWorkspace {
    let isolated = isolated_workspace_failure_fixture(name).expect("isolated fixture");
    let metadata = isolated.fixture.metadata.clone();
    let root = isolated.root.clone();
    let beads_dir = isolated.beads_dir.clone();
    let log_dir = root.join("logs");
    fs::create_dir_all(&log_dir).expect("log dir");

    FixtureWorkspace {
        metadata,
        beads_dir,
        workspace: BrWorkspace {
            temp_dir: isolated.temp_dir,
            root,
            log_dir,
        },
    }
}

fn parse_stdout_json(run: &BrRun, context: &str) -> Result<Value, String> {
    let payload = extract_json_payload(&run.stdout);
    serde_json::from_str(&payload).map_err(|err| {
        format!(
            "{context} should emit valid JSON on stdout: {err}\nstdout={}\nstderr={}",
            run.stdout, run.stderr
        )
    })
}

fn surface_label(name: &str, surface: &str) -> String {
    let slug: String = surface
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect();
    format!("{name}_{slug}")
}

fn run_surface(fixture: &FixtureWorkspace, surface: &str) -> Result<BrRun, String> {
    let label = surface_label(&fixture.metadata.name, surface);
    let run = match surface {
        "startup/open" => run_br(&fixture.workspace, ["list", "--json"], &label),
        "create" => run_br(
            &fixture.workspace,
            ["create", "Replay harness probe", "--json"],
            &label,
        ),
        "sync --status" => run_br(&fixture.workspace, ["sync", "--status", "--json"], &label),
        "sync --import-only" => run_br(
            &fixture.workspace,
            ["sync", "--import-only", "--json"],
            &label,
        ),
        "list --no-db" => run_br(&fixture.workspace, ["--no-db", "list", "--json"], &label),
        "config get" => run_br(
            &fixture.workspace,
            ["config", "get", "issue_prefix", "--json"],
            &label,
        ),
        "config list" => run_br(&fixture.workspace, ["config", "list", "--json"], &label),
        "history" => run_br(&fixture.workspace, ["history", "list", "--json"], &label),
        "info" => run_br(&fixture.workspace, ["info", "--json"], &label),
        other => return Err(format!("unsupported replay surface '{other}'")),
    };
    Ok(run)
}

fn check_sqlite_header(db_path: &Path, context: &str) -> Check {
    let bytes = fs::read(db_path).map_err(|err| {
        format!(
            "{context} should leave a readable SQLite database at {}: {err}",
            db_path.display()
        )
    })?;
    check!(
        bytes.starts_with(b"SQLite format 3\0"),
        "{context} should leave a SQLite database header at {}",
        db_path.display()
    );
    Ok(())
}

fn resolved_database_path(fixture: &FixtureWorkspace, surface: &str) -> Result<PathBuf, String> {
    let info_run = run_br(
        &fixture.workspace,
        ["info", "--json"],
        &surface_label(&fixture.metadata.name, surface),
    );
    check!(
        info_run.status.success(),
        "{} {surface} failed: {}",
        fixture.metadata.name,
        info_run.stderr
    );
    let info_json = parse_stdout_json(&info_run, &format!("{} {surface}", fixture.metadata.name))?;
    info_json["database_path"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| {
            format!(
                "{} {surface} info output should include database_path: {info_json}",
                fixture.metadata.name
            )
        })
}

fn check_config_error(run: &BrRun, needle: &str, context: &str) -> Check {
    check!(
        !run.status.success(),
        "{context} should fail\nstdout={}\nstderr={}",
        run.stdout,
        run.stderr
    );
    let error_json = parse_stdout_json(run, context)?;
    check!(
        error_json["error"]["code"].as_str() == Some("CONFIG_ERROR"),
        "{context} should surface CONFIG_ERROR: {error_json}"
    );
    check!(
        error_json["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains(needle)),
        "{context} should mention '{needle}': {error_json}"
    );
    Ok(())
}

fn first_issue_id(list_json: &Value) -> Result<String, String> {
    list_json["issues"]
        .as_array()
        .and_then(|issues| issues.first())
        .and_then(|issue| issue["id"].as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("list output should contain at least one issue id: {list_json}"))
}

fn first_issue_id_from_jsonl(jsonl_path: &Path) -> Result<String, String> {
    let contents = fs::read_to_string(jsonl_path)
        .map_err(|err| format!("read jsonl at {}: {err}", jsonl_path.display()))?;
    contents
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .and_then(|issue| issue["id"].as_str().map(str::to_string))
        .ok_or_else(|| {
            format!(
                "fixture jsonl at {} should contain at least one valid issue id",
                jsonl_path.display()
            )
        })
}

fn create_issue_id(create_json: &Value) -> Result<String, String> {
    if let Some(created) = create_json["created"]
        .as_array()
        .and_then(|created| created.first())
    {
        return created["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("created entry should contain id: {created}"));
    }
    create_json["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("create output should contain id: {create_json}"))
}

fn check_custom_path_resolution(fixture: &FixtureWorkspace, surface: &str, json: &Value) -> Check {
    if fixture.metadata.name != "metadata_custom_paths" {
        return Ok(());
    }

    let expected_db_path = fixture.beads_dir.join("custom.db");
    let expected_jsonl_path = fixture.beads_dir.join("custom.jsonl");
    let surface_name = match surface {
        "info" => "info",
        other => return Err(format!("unsupported custom-path surface '{other}'")),
    };

    check!(
        json["database_path"]
            .as_str()
            .is_some_and(|path| path == expected_db_path.display().to_string()),
        "{surface_name} should resolve custom database path: {json}"
    );
    check!(
        json["jsonl_path"]
            .as_str()
            .is_some_and(|path| path == expected_jsonl_path.display().to_string()),
        "{surface_name} should resolve custom JSONL path: {json}"
    );
    Ok(())
}

fn check_status_surface(
    context: &str,
    json: &Value,
    expected_jsonl_newer: bool,
    expected_db_newer: bool,
) -> Check {
    check!(
        json["jsonl_newer"] == Value::Bool(expected_jsonl_newer),
        "{context} reported unexpected jsonl_newer: {json}"
    );
    check!(
        json["db_newer"] == Value::Bool(expected_db_newer),
        "{context} reported unexpected db_newer: {json}"
    );
    Ok(())
}

fn check_surface_outcome(
    fixture: &FixtureWorkspace,
    surface: &str,
    outcome: WorkspaceFailureCommandOutcome,
) -> Check {
    let run = run_surface(fixture, surface)?;
    let context = format!("{} {surface}", fixture.metadata.name);

    match outcome {
        WorkspaceFailureCommandOutcome::Success => {
            check!(run.status.success(), "{context} failed: {}", run.stderr);
            let json = parse_stdout_json(&run, &context)?;
            if surface == "info" {
                check_custom_path_resolution(fixture, surface, &json)?;
            }
        }
        WorkspaceFailureCommandOutcome::SuccessWithAutoRecovery => {
            check!(run.status.success(), "{context} failed: {}", run.stderr);
            let _json = parse_stdout_json(&run, &context)?;
            check_sqlite_header(&resolved_database_path(fixture, "resolved_db")?, &context)?;
        }
        WorkspaceFailureCommandOutcome::StatusInSync => {
            check!(run.status.success(), "{context} failed: {}", run.stderr);
            let json = parse_stdout_json(&run, &context)?;
            check_status_surface(&context, &json, false, false)?;
        }
        WorkspaceFailureCommandOutcome::StatusJsonlNewer => {
            check!(run.status.success(), "{context} failed: {}", run.stderr);
            let json = parse_stdout_json(&run, &context)?;
            check_status_surface(&context, &json, true, false)?;
        }
        WorkspaceFailureCommandOutcome::StatusDiverged => {
            check!(run.status.success(), "{context} failed: {}", run.stderr);
            let json = parse_stdout_json(&run, &context)?;
            check_status_surface(&context, &json, true, true)?;
        }
        WorkspaceFailureCommandOutcome::StatusDbNewer => {
            check!(run.status.success(), "{context} failed: {}", run.stderr);
            let json = parse_stdout_json(&run, &context)?;
            check_status_surface(&context, &json, false, true)?;
        }
        WorkspaceFailureCommandOutcome::FailsPrefixMismatch => {
            check_config_error(&run, "Prefix mismatch", &context)?;
        }
        WorkspaceFailureCommandOutcome::FailsConflictMarkers => {
            check_config_error(&run, "conflict marker", &context)?;
        }
        WorkspaceFailureCommandOutcome::FailsInvalidJson => {
            check_config_error(&run, "invalid issue record", &context)?;
        }
    }
    Ok(())
}

fn check_core_read_success(fixture: &FixtureWorkspace) -> Check {
    let list_workspace = fixture_workspace(&fixture.metadata.name);
    let list = run_br(
        &list_workspace.workspace,
        ["list", "--json"],
        &surface_label(&fixture.metadata.name, "core_list"),
    );
    check!(
        list.status.success(),
        "{} list --json failed: {}",
        fixture.metadata.name,
        list.stderr
    );
    let list_json = parse_stdout_json(&list, &format!("{} core list", fixture.metadata.name))?;
    let issue_id = first_issue_id(&list_json)?;

    let ready_workspace = fixture_workspace(&fixture.metadata.name);
    let ready = run_br(
        &ready_workspace.workspace,
        ["ready", "--json"],
        &surface_label(&fixture.metadata.name, "core_ready"),
    );
    check!(
        ready.status.success(),
        "{} ready --json failed: {}",
        fixture.metadata.name,
        ready.stderr
    );
    let _ready_json = parse_stdout_json(&ready, &format!("{} core ready", fixture.metadata.name))?;

    let show_workspace = fixture_workspace(&fixture.metadata.name);
    let show = run_br(
        &show_workspace.workspace,
        ["show", &issue_id, "--json"],
        &surface_label(&fixture.metadata.name, "core_show"),
    );
    check!(
        show.status.success(),
        "{} show --json failed: {}",
        fixture.metadata.name,
        show.stderr
    );
    let _show_json = parse_stdout_json(&show, &format!("{} core show", fixture.metadata.name))?;

    if fixture
        .metadata
        .outcome_for("startup/open")
        .is_some_and(|outcome| outcome == WorkspaceFailureCommandOutcome::SuccessWithAutoRecovery)
    {
        let context = format!("{} core show", fixture.metadata.name);
        check_sqlite_header(
            &resolved_database_path(&show_workspace, "core_resolved_db")?,
            &context,
        )?;
    }
    Ok(())
}

fn check_core_read_failure(
    fixture: &FixtureWorkspace,
    probe_json: &Value,
    failure: WorkspaceFailureCommandOutcome,
) -> Check {
    let list_workspace = fixture_workspace(&fixture.metadata.name);
    check_surface_outcome(&list_workspace, "startup/open", failure)?;

    let ready_workspace = fixture_workspace(&fixture.metadata.name);
    let ready = run_br(
        &ready_workspace.workspace,
        ["ready", "--json"],
        &surface_label(&fixture.metadata.name, "core_ready_fail"),
    );
    let needle = match failure {
        WorkspaceFailureCommandOutcome::FailsPrefixMismatch => "Prefix mismatch",
        WorkspaceFailureCommandOutcome::FailsConflictMarkers => "conflict marker",
        other => {
            return Err(format!(
                "{} has unsupported failure outcome for core read replay: {other:?}",
                fixture.metadata.name
            ));
        }
    };
    check_config_error(
        &ready,
        needle,
        &format!("{} core ready", fixture.metadata.name),
    )?;

    let jsonl_path = probe_json["jsonl_path"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| format!("info output should include jsonl_path: {probe_json}"))?;
    let issue_id = first_issue_id_from_jsonl(&jsonl_path)?;
    let show_workspace = fixture_workspace(&fixture.metadata.name);
    let show = run_br(
        &show_workspace.workspace,
        ["show", &issue_id, "--json"],
        &surface_label(&fixture.metadata.name, "core_show_fail"),
    );
    check_config_error(
        &show,
        needle,
        &format!("{} core show", fixture.metadata.name),
    )
}

fn check_core_write_success(
    fixture: &FixtureWorkspace,
    create: &BrRun,
    expected_create: WorkspaceFailureCommandOutcome,
) -> Check {
    let create_json = parse_stdout_json(create, &format!("{} core create", fixture.metadata.name))?;
    let issue_id = create_issue_id(&create_json)?;
    if expected_create == WorkspaceFailureCommandOutcome::SuccessWithAutoRecovery {
        let context = format!("{} core create", fixture.metadata.name);
        check_sqlite_header(
            &resolved_database_path(fixture, "core_create_resolved_db")?,
            &context,
        )?;
    }

    let show = run_br(
        &fixture.workspace,
        ["show", &issue_id, "--json"],
        &surface_label(&fixture.metadata.name, "core_show_created"),
    );
    check!(
        show.status.success(),
        "{} show after create failed: {}",
        fixture.metadata.name,
        show.stderr
    );
    let _show_json = parse_stdout_json(
        &show,
        &format!("{} core show after create", fixture.metadata.name),
    )?;

    let update = run_br(
        &fixture.workspace,
        ["update", &issue_id, "--status", "in_progress", "--json"],
        &surface_label(&fixture.metadata.name, "core_update"),
    );
    check!(
        update.status.success(),
        "{} update failed: {}",
        fixture.metadata.name,
        update.stderr
    );

    let label_add = run_br(
        &fixture.workspace,
        ["label", "add", &issue_id, "replay-probe", "--json"],
        &surface_label(&fixture.metadata.name, "core_label"),
    );
    check!(
        label_add.status.success(),
        "{} label add failed: {}",
        fixture.metadata.name,
        label_add.stderr
    );

    let comment = run_br(
        &fixture.workspace,
        ["comments", "add", &issue_id, "Replay note", "--json"],
        &surface_label(&fixture.metadata.name, "core_comment"),
    );
    check!(
        comment.status.success(),
        "{} comments add failed: {}",
        fixture.metadata.name,
        comment.stderr
    );

    let close = run_br(
        &fixture.workspace,
        ["close", &issue_id, "--reason", "Replay close", "--json"],
        &surface_label(&fixture.metadata.name, "core_close"),
    );
    check!(
        close.status.success(),
        "{} close failed: {}",
        fixture.metadata.name,
        close.stderr
    );

    let reopen = run_br(
        &fixture.workspace,
        ["reopen", &issue_id, "--json"],
        &surface_label(&fixture.metadata.name, "core_reopen"),
    );
    check!(
        reopen.status.success(),
        "{} reopen failed: {}",
        fixture.metadata.name,
        reopen.stderr
    );

    let delete = run_br(
        &fixture.workspace,
        ["delete", &issue_id, "--json"],
        &surface_label(&fixture.metadata.name, "core_delete"),
    );
    check!(
        delete.status.success(),
        "{} delete failed: {}",
        fixture.metadata.name,
        delete.stderr
    );
    Ok(())
}

fn check_core_write_failure(
    fixture: &FixtureWorkspace,
    create: &BrRun,
    expected_create: WorkspaceFailureCommandOutcome,
) -> Check {
    let needle = match expected_create {
        WorkspaceFailureCommandOutcome::FailsPrefixMismatch => "Prefix mismatch",
        WorkspaceFailureCommandOutcome::FailsConflictMarkers => "conflict marker",
        other => {
            return Err(format!(
                "{} has unsupported create outcome for core write replay: {other:?}",
                fixture.metadata.name
            ));
        }
    };
    check_config_error(
        create,
        needle,
        &format!("{} core create", fixture.metadata.name),
    )
}

fn check_core_read_surfaces(metadata: &WorkspaceFailureFixtureMetadata) -> Check {
    let probe_workspace = fixture_workspace(&metadata.name);
    let probe_run = run_br(
        &probe_workspace.workspace,
        ["info", "--json"],
        &surface_label(&metadata.name, "core_probe"),
    );
    check!(
        probe_run.status.success(),
        "{} info --json failed: {}",
        metadata.name,
        probe_run.stderr
    );
    let probe_json = parse_stdout_json(&probe_run, &format!("{} core probe", metadata.name))?;

    let info_workspace = fixture_workspace(&metadata.name);
    let info = run_br(
        &info_workspace.workspace,
        ["info", "--json"],
        &surface_label(&metadata.name, "core_info"),
    );
    check!(
        info.status.success(),
        "{} info --json failed: {}",
        metadata.name,
        info.stderr
    );
    let _info_json = parse_stdout_json(&info, &format!("{} core info", metadata.name))?;

    let startup = metadata
        .outcome_for("startup/open")
        .ok_or_else(|| format!("{} has no startup/open expectation", metadata.name))?;

    match startup {
        WorkspaceFailureCommandOutcome::Success
        | WorkspaceFailureCommandOutcome::SuccessWithAutoRecovery => {
            check_core_read_success(&probe_workspace)
        }
        WorkspaceFailureCommandOutcome::FailsPrefixMismatch
        | WorkspaceFailureCommandOutcome::FailsConflictMarkers => {
            check_core_read_failure(&probe_workspace, &probe_json, startup)
        }
        other => Err(format!(
            "{} has unsupported startup/open outcome for core read replay: {other:?}",
            metadata.name
        )),
    }
}

fn check_core_write_surfaces(metadata: &WorkspaceFailureFixtureMetadata) -> Check {
    let expected_create = metadata
        .outcome_for("create")
        .ok_or_else(|| format!("{} has no create expectation", metadata.name))?;
    let workspace = fixture_workspace(&metadata.name);
    let create = run_br(
        &workspace.workspace,
        ["create", "Replay write probe", "--json"],
        &surface_label(&metadata.name, "core_create"),
    );

    match expected_create {
        WorkspaceFailureCommandOutcome::Success
        | WorkspaceFailureCommandOutcome::SuccessWithAutoRecovery => {
            check!(
                create.status.success(),
                "{} create failed: {}",
                metadata.name,
                create.stderr
            );
            check_core_write_success(&workspace, &create, expected_create)
        }
        WorkspaceFailureCommandOutcome::FailsPrefixMismatch
        | WorkspaceFailureCommandOutcome::FailsConflictMarkers => {
            check_core_write_failure(&workspace, &create, expected_create)
        }
        other => Err(format!(
            "{} has unsupported create outcome for core write replay: {other:?}",
            metadata.name
        )),
    }
}

/// Fail once with every collected mismatch, so one run reports every
/// disagreement instead of hiding all but the first behind a panic.
fn assert_no_mismatches(mismatches: &[String], what: &str) {
    assert!(
        mismatches.is_empty(),
        "{} {what} did not hold:\n\n{}",
        mismatches.len(),
        mismatches.join("\n\n")
    );
}

#[test]
fn workspace_failure_replay_manifest_expectations_hold_on_fresh_copies() {
    let _guard = common::workspace_replay_test_guard();
    let _log =
        common::test_log("workspace_failure_replay_manifest_expectations_hold_on_fresh_copies");
    let fixtures = list_workspace_failure_fixtures().expect("fixture catalog");

    let mut mismatches = Vec::new();
    for fixture in fixtures {
        for expectation in &fixture.metadata.expected_command_outcomes {
            let workspace = fixture_workspace(&fixture.metadata.name);
            if let Err(mismatch) =
                check_surface_outcome(&workspace, &expectation.surface, expectation.outcome)
            {
                mismatches.push(format!(
                    "[{} | {} | expected {:?}]\n{mismatch}",
                    fixture.metadata.name, expectation.surface, expectation.outcome
                ));
            }
        }
    }

    assert_no_mismatches(&mismatches, "manifest surface expectation(s)");
}

#[test]
fn workspace_failure_replay_core_read_surfaces_match_expected_posture() {
    let _guard = common::workspace_replay_test_guard();
    let _log =
        common::test_log("workspace_failure_replay_core_read_surfaces_match_expected_posture");
    let fixtures = list_workspace_failure_fixtures().expect("fixture catalog");

    let mut mismatches = Vec::new();
    for fixture in fixtures {
        if let Err(mismatch) = check_core_read_surfaces(&fixture.metadata) {
            mismatches.push(format!("[{}]\n{mismatch}", fixture.metadata.name));
        }
    }

    assert_no_mismatches(&mismatches, "core read expectation(s)");
}

#[test]
fn workspace_failure_replay_core_write_surfaces_match_expected_posture() {
    let _guard = common::workspace_replay_test_guard();
    let _log =
        common::test_log("workspace_failure_replay_core_write_surfaces_match_expected_posture");
    let fixtures = list_workspace_failure_fixtures().expect("fixture catalog");

    let mut mismatches = Vec::new();
    for fixture in fixtures {
        if let Err(mismatch) = check_core_write_surfaces(&fixture.metadata) {
            mismatches.push(format!("[{}]\n{mismatch}", fixture.metadata.name));
        }
    }

    assert_no_mismatches(&mismatches, "core write expectation(s)");
}

fn infer_classification(metadata: &WorkspaceFailureFixtureMetadata) -> &'static str {
    let startup = metadata.outcome_for("startup/open");
    let create = metadata.outcome_for("create");
    let sync_status = metadata.outcome_for("sync --status");

    let startup_fails = matches!(
        startup,
        Some(
            WorkspaceFailureCommandOutcome::FailsPrefixMismatch
                | WorkspaceFailureCommandOutcome::FailsConflictMarkers
                | WorkspaceFailureCommandOutcome::FailsInvalidJson
        )
    );
    let startup_needs_recovery = matches!(
        startup,
        Some(WorkspaceFailureCommandOutcome::SuccessWithAutoRecovery)
    );
    let sync_shows_drift = matches!(
        sync_status,
        Some(
            WorkspaceFailureCommandOutcome::StatusJsonlNewer
                | WorkspaceFailureCommandOutcome::StatusDiverged
        )
    );

    if startup_fails {
        return "unsafe";
    }
    if startup_needs_recovery {
        return "recoverable";
    }
    if sync_shows_drift {
        return "degraded";
    }
    match (startup, create) {
        (
            Some(WorkspaceFailureCommandOutcome::Success),
            Some(WorkspaceFailureCommandOutcome::Success),
        ) => "usable",
        _ => "unknown",
    }
}

fn check_classification(
    metadata: &WorkspaceFailureFixtureMetadata,
    valid_classifications: &[&str],
) -> Check {
    let declared = &metadata.expected_classification;
    check!(
        valid_classifications.contains(&declared.as_str()),
        "{}: declared classification '{declared}' is not in the valid set {valid_classifications:?}",
        metadata.name
    );

    let inferred = infer_classification(metadata);
    check!(
        declared.as_str() == inferred,
        "{}: declared classification '{declared}' does not match inferred '{inferred}' from \
         surface outcomes (startup/open={:?}, create={:?})",
        metadata.name,
        metadata.outcome_for("startup/open"),
        metadata.outcome_for("create"),
    );
    Ok(())
}

#[test]
fn workspace_failure_replay_classification_coherence() {
    let _guard = common::workspace_replay_test_guard();
    let _log = common::test_log("workspace_failure_replay_classification_coherence");
    let fixtures = list_workspace_failure_fixtures().expect("fixture catalog");

    assert!(
        !fixtures.is_empty(),
        "fixture catalog should contain at least one fixture"
    );

    let valid_classifications = ["healthy", "usable", "degraded", "recoverable", "unsafe"];

    let mut mismatches = Vec::new();
    for fixture in &fixtures {
        if let Err(mismatch) = check_classification(&fixture.metadata, &valid_classifications) {
            mismatches.push(mismatch);
        }
    }

    let families: std::collections::HashSet<&str> = fixtures
        .iter()
        .map(|f| f.metadata.expected_classification.as_str())
        .collect();
    if families.len() < 3 {
        mismatches.push(format!(
            "fixture corpus should cover at least 3 distinct classification levels, got: {families:?}"
        ));
    }

    assert_no_mismatches(&mismatches, "classification expectation(s)");
}
