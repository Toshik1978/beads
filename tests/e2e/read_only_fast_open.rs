//! E2E coverage for CLI read-only fast-open behavior.
//!
//! These tests compare the optimized current-schema read-only path against the
//! conservative locked path, then prove representative read commands still run
//! while another process holds `.beads/.write.lock`.

// `common` is now the `test-support` crate; aliased so that the 753
// `common::` paths in this suite keep working unchanged.
use crate::common;

use common::cli::{BrRun, BrWorkspace, parse_created_id, run_br, run_br_with_env};
use serde_json::json;
use std::fs::OpenOptions;
use std::time::{Duration, Instant};

const DISABLE_FAST_OPEN_ENV: (&str, &str) = ("BR_DISABLE_READ_ONLY_FAST_OPEN", "1");

struct SeededWorkspace {
    workspace: BrWorkspace,
    blocker_id: String,
    blocked_id: String,
}

struct MatrixCommand {
    label: &'static str,
    args: Vec<String>,
}

fn assert_success(run: &BrRun, label: &str) {
    assert!(
        run.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
}

fn run_success(workspace: &BrWorkspace, args: &[&str], label: &str) -> BrRun {
    let run = run_br(workspace, args.iter().copied(), label);
    assert_success(&run, label);
    run
}

fn create_issue(workspace: &BrWorkspace, args: &[&str], label: &str) -> String {
    parse_created_id(&run_success(workspace, args, label).stdout)
}

fn seed_workspace() -> SeededWorkspace {
    let workspace = BrWorkspace::new();

    run_success(&workspace, &["init"], "init");
    let epic_id = create_issue(
        &workspace,
        &[
            "create",
            "Fast-open roadmap epic",
            "-p",
            "0",
            "--type",
            "epic",
            "-l",
            "roadmap,fast-open",
        ],
        "create_epic",
    );
    let blocker_id = create_issue(
        &workspace,
        &[
            "create",
            "Fast-open blocker issue",
            "-p",
            "1",
            "--type",
            "bug",
            "-l",
            "backend,fast-open",
        ],
        "create_blocker",
    );
    let blocked_id = create_issue(
        &workspace,
        &[
            "create",
            "Fast-open blocked issue",
            "-p",
            "2",
            "--type",
            "task",
            "-l",
            "backend",
            "--parent",
            &epic_id,
        ],
        "create_blocked",
    );
    create_issue(
        &workspace,
        &[
            "create",
            "Fast-open ready issue",
            "-p",
            "0",
            "--type",
            "feature",
            "-l",
            "ready,fast-open",
            "--parent",
            &epic_id,
        ],
        "create_ready",
    );
    run_success(
        &workspace,
        &[
            "comments",
            "add",
            &blocker_id,
            "--author",
            "fast-open-test",
            "Snapshot matrix comment",
        ],
        "add_comment",
    );
    run_success(
        &workspace,
        &["dep", "add", &blocked_id, &blocker_id],
        "dep_add",
    );
    run_success(
        &workspace,
        &["sync", "--flush-only", "--json"],
        "sync_flush",
    );

    SeededWorkspace {
        workspace,
        blocker_id,
        blocked_id,
    }
}

fn matrix_commands(seed: &SeededWorkspace) -> Vec<MatrixCommand> {
    let mut commands = Vec::new();
    commands.extend(core_read_commands(seed));
    commands.extend(status_and_report_commands());
    commands.extend(relation_commands(seed));
    commands
}

fn core_read_commands(seed: &SeededWorkspace) -> Vec<MatrixCommand> {
    vec![
        exact_command("list_json", strings(["list", "--json", "--limit", "5"])),
        exact_command(
            "show_json",
            vec![
                "show".into(),
                seed.blocker_id.clone(),
                "--format".into(),
                "json".into(),
            ],
        ),
        exact_command(
            "search_json",
            strings(["search", "Fast-open", "--format", "json", "--limit", "5"]),
        ),
        exact_command("ready_json", strings(["ready", "--json", "--limit", "5"])),
        exact_command(
            "blocked_json",
            strings(["blocked", "--json", "--limit", "5"]),
        ),
    ]
}

fn status_and_report_commands() -> Vec<MatrixCommand> {
    vec![
        exact_command("stale_json", strings(["stale", "--days", "0", "--json"])),
        exact_command("sync_status_json", strings(["sync", "--status", "--json"])),
        exact_command(
            "stats_no_activity_json",
            strings(["stats", "--no-activity", "--json"]),
        ),
        exact_command(
            "status_no_activity_json",
            strings(["status", "--no-activity", "--json"]),
        ),
    ]
}

fn relation_commands(seed: &SeededWorkspace) -> Vec<MatrixCommand> {
    vec![
        exact_command(
            "comments_json",
            vec![
                "comments".into(),
                "list".into(),
                seed.blocker_id.clone(),
                "--json".into(),
            ],
        ),
        exact_command(
            "comments_shorthand_json",
            vec!["comments".into(), seed.blocker_id.clone(), "--json".into()],
        ),
        exact_command("epic_status_json", strings(["epic", "status", "--json"])),
        exact_command("label_list_unique", strings(["label", "list"])),
        exact_command(
            "label_list_all_json",
            strings(["label", "list-all", "--json"]),
        ),
        exact_command(
            "dep_list_json",
            vec![
                "dep".into(),
                "list".into(),
                seed.blocked_id.clone(),
                "--format".into(),
                "json".into(),
            ],
        ),
        exact_command(
            "dep_tree_json",
            vec![
                "dep".into(),
                "tree".into(),
                seed.blocked_id.clone(),
                "--json".into(),
            ],
        ),
        exact_command("dep_cycles_json", strings(["dep", "cycles", "--json"])),
    ]
}

fn exact_command(label: &'static str, args: Vec<String>) -> MatrixCommand {
    MatrixCommand { label, args }
}

fn strings<const N: usize>(values: [&str; N]) -> Vec<String> {
    values.into_iter().map(str::to_string).collect()
}

/// Global flags every matrix command needs for read-only fast-open to engage.
///
/// `build_cli_overrides` in src/main.rs gates fast-open on
/// `cli.no_auto_import && cli.no_auto_flush` (pinned by the unit test
/// `read_only_fast_open_requires_explicit_stale_and_flush_opt_out`): auto-import
/// and auto-flush are writes, so a read command that might trigger either still
/// takes the conservative locked path. Without these flags the matrix runs the
/// conservative path on *both* sides, which makes
/// `cli_read_only_fast_open_matrix_matches_conservative_outputs` vacuous and
/// makes the held-write-lock test time out.
const FAST_OPEN_PRECONDITION_FLAGS: [&str; 2] = ["--no-auto-import", "--no-auto-flush"];

fn run_command(workspace: &BrWorkspace, command: &MatrixCommand, disable_fast_open: bool) -> BrRun {
    let args = FAST_OPEN_PRECONDITION_FLAGS
        .into_iter()
        .chain(command.args.iter().map(String::as_str));
    if disable_fast_open {
        run_br_with_env(
            workspace,
            args,
            [DISABLE_FAST_OPEN_ENV],
            &format!("{}_conservative", command.label),
        )
    } else {
        run_br(workspace, args, &format!("{}_fast", command.label))
    }
}

fn assert_outputs_match(command: &MatrixCommand, fast: &BrRun, conservative: &BrRun) {
    assert_eq!(
        fast.stdout, conservative.stdout,
        "{} stdout changed between read-only fast-open and conservative locked path",
        command.label
    );
}

#[test]
fn cli_read_only_fast_open_matrix_matches_conservative_outputs() {
    let _log = common::test_log("cli_read_only_fast_open_matrix_matches_conservative_outputs");
    let seed = seed_workspace();

    for command in matrix_commands(&seed) {
        let conservative = run_command(&seed.workspace, &command, true);
        assert_success(&conservative, command.label);

        let fast = run_command(&seed.workspace, &command, false);
        assert_success(&fast, command.label);

        assert_outputs_match(&command, &fast, &conservative);
    }
}

#[test]
fn cli_read_only_fast_open_matrix_bypasses_held_write_lock() {
    let _log = common::test_log("cli_read_only_fast_open_matrix_bypasses_held_write_lock");
    let seed = seed_workspace();
    let lock_path = seed.workspace.root.join(".beads/.write.lock");
    let write_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open write lock");
    write_lock.lock().expect("hold write lock");

    for command in matrix_commands(&seed) {
        let fast = run_command(&seed.workspace, &command, false);
        assert_success(&fast, command.label);
    }

    let blocked_conservative = run_command(
        &seed.workspace,
        &exact_command(
            "list_json_locked_conservative",
            strings(["--lock-timeout", "50", "list", "--json", "--limit", "1"]),
        ),
        true,
    );
    assert!(
        !blocked_conservative.status.success(),
        "disabled fast-open should wait for the held write lock and time out"
    );
    let combined = format!(
        "{} {}",
        blocked_conservative.stdout, blocked_conservative.stderr
    )
    .to_ascii_lowercase();
    assert!(
        combined.contains("lock") || combined.contains("timed out"),
        "conservative failure should mention lock contention, got: {combined}"
    );
}

fn run_matrix_round(workspace: &BrWorkspace, commands: &[MatrixCommand], disable_fast_open: bool) {
    for command in commands {
        let run = run_command(workspace, command, disable_fast_open);
        assert_success(&run, command.label);
    }
}

fn duration_ns_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[test]
#[ignore = "perf probe for CLI read-only fast-open matrix evidence"]
fn cli_read_only_fast_open_matrix_perf_probe() {
    let seed = seed_workspace();
    let commands = matrix_commands(&seed);
    let rounds = 5_u32;

    let conservative_start = Instant::now();
    for _ in 0..rounds {
        run_matrix_round(&seed.workspace, &commands, true);
    }
    let conservative = conservative_start.elapsed();

    let fast_start = Instant::now();
    for _ in 0..rounds {
        run_matrix_round(&seed.workspace, &commands, false);
    }
    let fast = fast_start.elapsed();

    let conservative_ns = duration_ns_u64(conservative);
    let fast_ns = duration_ns_u64(fast);
    println!(
        "{}",
        json!({
            "commands": commands.iter().map(|command| command.label).collect::<Vec<_>>(),
            "rounds": rounds,
            "conservative_total_ns": conservative_ns,
            "fast_open_total_ns": fast_ns,
            "speedup_milli": conservative_ns.saturating_mul(1000) / fast_ns.max(1),
            "equality": "routine matrix test asserts byte-identical stdout per command",
        })
    );
}
