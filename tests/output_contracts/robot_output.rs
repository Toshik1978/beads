use super::init_workspace;
use crate::common::cli::{BrWorkspace, run_br};
use insta::assert_snapshot;
use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::process::ExitStatus;

// Representative fixture for exact robot-output goldens.
//
// Golden update workflow:
// INSTA_UPDATE=always cargo test --test snapshots robot_golden
//
// Review the resulting tests/snapshots/snapshots/*.snap diffs before
// committing. The `br ready` fixture is fully deterministic and unmasked.
//
// bds-04l.11: there are deliberately NO goldens of bv's output here. Three
// used to exist, pinning bv v0.18.0's exact rendering -- its emoji reason
// strings, hint wording and graph-phase names -- none of which this repo
// controls, so a bv upgrade rewrote them with no change to br. Keeping them
// stable had already required masking wall-clock metadata, elapsed timings,
// the bv version, score fields, stale-day wording and individual usage hints:
// roughly ninety lines of regex whose only job was to absorb a downstream
// binary's drift, and which would have needed another entry per new bv field.
// What IS br's business is tested instead, by
// `bv_robot_modes_emit_br_commands_that_br_accepts` below: bv's rendering
// belongs to bv, but the br commands bv emits have to be commands br accepts.
const ROBOT_JSONL_FIXTURE: &str = r#"{"id":"bd-blocker","title":"00 Blocking Root","description":"Unblocks dependent work","status":"open","priority":0,"issue_type":"task","created_at":"2026-02-01T00:00:00Z","created_by":"fixture","updated_at":"2026-02-01T00:00:00Z","source_repo":".","labels":["core"],"compaction_level":0,"original_size":0}
{"id":"bd-ready-p0","title":"01 Ready Critical Unassigned","status":"open","priority":0,"issue_type":"bug","created_at":"2026-02-02T00:00:00Z","created_by":"fixture","updated_at":"2026-02-02T00:00:00Z","source_repo":".","labels":["ops","agent"],"compaction_level":0,"original_size":0}
{"id":"bd-ready-p1-assigned","title":"02 Ready Assigned Feature","status":"open","priority":1,"issue_type":"feature","assignee":"alice","owner":"owner@example.com","created_at":"2026-02-03T00:00:00Z","created_by":"fixture","updated_at":"2026-02-03T00:00:00Z","source_repo":".","labels":["frontend"],"compaction_level":0,"original_size":0}
{"id":"bd-ready-p2-label","title":"03 Ready Backend Task","status":"open","priority":2,"issue_type":"task","created_at":"2026-02-04T00:00:00Z","created_by":"fixture","updated_at":"2026-02-04T00:00:00Z","source_repo":".","labels":["backend"],"compaction_level":0,"original_size":0}
{"id":"bd-blocked","title":"04 Blocked By Root","status":"open","priority":1,"issue_type":"task","created_at":"2026-02-05T00:00:00Z","created_by":"fixture","updated_at":"2026-02-05T00:00:00Z","source_repo":".","labels":["blocked"],"dependencies":[{"issue_id":"bd-blocked","depends_on_id":"bd-blocker","type":"blocks","created_at":"2026-02-05T00:00:00Z","created_by":"fixture","metadata":"{}","thread_id":""}],"compaction_level":0,"original_size":0}
{"id":"bd-deferred","title":"05 Deferred Ready Later","status":"deferred","priority":1,"issue_type":"task","defer_until":"2026-09-01T00:00:00Z","created_at":"2026-02-06T00:00:00Z","created_by":"fixture","updated_at":"2026-02-06T00:00:00Z","source_repo":".","labels":["waiting"],"compaction_level":0,"original_size":0}
{"id":"bd-in-progress","title":"06 In Progress Assigned","status":"in_progress","priority":0,"issue_type":"task","assignee":"bob","created_at":"2026-02-07T00:00:00Z","created_by":"fixture","updated_at":"2026-02-07T00:00:00Z","source_repo":".","labels":["active"],"compaction_level":0,"original_size":0}
{"id":"bd-closed","title":"07 Closed Done","status":"closed","priority":2,"issue_type":"task","created_at":"2026-02-08T00:00:00Z","created_by":"fixture","updated_at":"2026-02-08T00:00:00Z","closed_at":"2026-02-08T01:00:00Z","close_reason":"done","source_repo":".","labels":["done"],"compaction_level":0,"original_size":0}
"#;

#[derive(Debug)]
struct BvRun {
    stdout: String,
    stderr: String,
    status: ExitStatus,
}

fn init_robot_golden_workspace() -> BrWorkspace {
    let workspace = init_workspace();
    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    fs::write(jsonl_path, ROBOT_JSONL_FIXTURE).expect("write robot JSONL fixture");

    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--json"],
        "robot_golden_import",
    );
    assert!(
        import.status.success(),
        "robot fixture import failed:\nstdout:\n{}\nstderr:\n{}",
        import.stdout,
        import.stderr
    );

    workspace
}

fn clear_inherited_br_env(command: &mut std::process::Command) {
    for (key, _) in std::env::vars_os() {
        let key_str = key.to_string_lossy();
        if key_str.starts_with("BD_")
            || key_str.starts_with("BEADS_")
            || matches!(
                key_str.as_ref(),
                "BR_OUTPUT_FORMAT" | "TOON_DEFAULT_FORMAT" | "TOON_STATS"
            )
        {
            command.env_remove(key);
        }
    }
}

/// Is a `bv` executable reachable through `PATH`?
///
/// Resolved by scanning `PATH` rather than by running `bv`, so the answer is
/// strictly about *presence*. A `bv` that exists but misbehaves must still fail
/// the test — see `bv_available_or_skip`.
fn bv_is_on_path() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("bv").is_file()))
}

/// Gate for the `bv` contract test: `true` to run, or `false` after announcing
/// a skip.
///
/// `bv` is a downstream consumer of `br`'s data, and keeping it working is a
/// stated goal of this fork, so the contract test earns its place wherever `bv`
/// is installed. But `br`'s own suite must not require a downstream binary to
/// be present — notably it is absent from the `task test:linux` container, where
/// it would otherwise look exactly like a Linux regression.
///
/// Two properties this deliberately has:
///
/// - It skips on **absence only**. If `bv` is on `PATH` and emits a `br` command
///   `br` rejects, that stays red. `run_bv`'s `.expect()` still catches a `bv`
///   that is present but cannot be executed.
/// - It is **loud**, unconditionally. libtest discards a passing test's captured
///   output, so a plain `eprintln!` would make the skip read identically to a
///   pass — the one failure mode this is meant to avoid. The capture is a
///   thread-local sink consulted by the print macros (`_eprint`), *not* by
///   `std::io::stderr()`, which writes to fd 2 directly. So writing through the
///   handle reaches the test log while `eprintln!` does not.
///
///   Note the handle, not the path: an earlier version opened `/dev/stderr` and
///   fell back to `eprintln!` if that failed. The fallback re-introduced exactly
///   the silent skip this requirement forbids — `/dev/stderr` is absent in some
///   sandboxes and unopenable when fd 2 is a socket, as some CI log collectors
///   arrange, and in those environments the skip would have gone invisible
///   again. `std::io::stderr()` never touches the filesystem and so has no such
///   failure mode to fall back from. Do not reintroduce a fallback here.
///
/// bds-04l.11 previously noted here that the goldens encoded `bv v0.18.0`'s
/// exact output, so a machine carrying any other `bv` failed rather than
/// skipped. Those goldens are gone; what remains asserts only the interface
/// between the two binaries, which does not move when `bv`'s renderer does.
fn bv_available_or_skip(test_name: &str) -> bool {
    if bv_is_on_path() {
        return true;
    }

    let message = format!(
        "SKIPPED {test_name}: no `bv` on PATH. This is a contract test against \
         the downstream bv viewer, not a test of br; install bv to run it. \
         (bds-04l.11)\n"
    );
    let _ = std::io::stderr().write_all(message.as_bytes());

    false
}

fn run_bv<I, S>(workspace: &BrWorkspace, args: I) -> BvRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = std::process::Command::new("bv");
    command.current_dir(&workspace.root);
    command.args(args);
    clear_inherited_br_env(&mut command);
    command.env("NO_COLOR", "1");
    command.env("CI", "1");

    let output = command
        .output()
        .expect("run bv; install bv to update robot goldens");
    BvRun {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        status: output.status,
    }
}

fn assert_valid_json(raw: &str, context: &str) {
    let error = serde_json::from_str::<Value>(raw)
        .err()
        .map(|err| format!("{context} did not emit valid JSON: {err}\n\n{raw}"));
    assert_eq!(None, error);
}

#[test]
fn robot_golden_ready_output() {
    let workspace = init_robot_golden_workspace();

    let output = run_br(
        &workspace,
        [
            "ready",
            "--json",
            "--include-deferred",
            "--sort",
            "priority",
            "--limit",
            "0",
        ],
        "robot_golden_ready",
    );
    assert!(
        output.status.success(),
        "ready --json failed: {}",
        output.stderr
    );
    assert_valid_json(&output.stdout, "ready --json");
    assert_snapshot!("robot_ready_output", output.stdout.trim_end());
}

/// Collects every `br` invocation `bv` emits anywhere in a JSON payload.
///
/// `bv` puts them in different places per mode -- `--robot-next` uses
/// `claim_command` / `show_command`, `--robot-triage` a `commands` object --
/// so this walks the whole tree rather than naming keys, and keeps working if
/// `bv` moves or adds one. A `CI=1 ` prefix is stripped: it is a shell
/// assignment, not an argument. Non-`br` commands (`bv --robot-triage`, which
/// triage emits to refresh itself) are skipped -- this repo does not own them.
fn br_commands_in(payload: &Value, found: &mut Vec<String>) {
    match payload {
        Value::Object(fields) => {
            for value in fields.values() {
                br_commands_in(value, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                br_commands_in(item, found);
            }
        }
        Value::String(text) => {
            let command = text.strip_prefix("CI=1 ").unwrap_or(text);
            if let Some(args) = command.strip_prefix("br ") {
                found.push(args.to_string());
            }
        }
        _ => {}
    }
}

/// bds-04l.11. The contract worth pinning is the interface, not the rendering.
///
/// `bv` reads `br`'s data and hands the operator `br` commands to run next. If
/// `br`'s CLI drifts -- a renamed flag, a removed subcommand, a changed status
/// spelling -- those commands stop working, and that IS this repo's
/// regression. Whether `bv` writes "🔓 Unblocks 1 item(s)" or something else
/// is not.
///
/// So each robot mode is run and every `br` command in its output is executed
/// against the same workspace, asserting `br` accepts it. Commands are
/// collected structurally rather than by key name, and the run is required to
/// find some, so a `bv` that stops emitting them cannot make this pass
/// vacuously.
#[test]
fn bv_robot_modes_emit_br_commands_that_br_accepts() {
    if !bv_available_or_skip("bv_robot_modes_emit_br_commands_that_br_accepts") {
        return;
    }
    let workspace = init_robot_golden_workspace();

    let mut commands = Vec::new();
    for mode in ["--robot-next", "--robot-triage", "--robot-plan"] {
        let output = run_bv(&workspace, [mode]);
        assert!(
            output.status.success(),
            "bv {mode} failed against br's data: {}",
            output.stderr
        );
        assert_valid_json(&output.stdout, &format!("bv {mode}"));

        let payload: Value = serde_json::from_str(&output.stdout)
            .unwrap_or_else(|error| panic!("bv {mode} emitted invalid json: {error}"));
        br_commands_in(&payload, &mut commands);
    }

    assert!(
        !commands.is_empty(),
        "bv emitted no `br` commands across --robot-next/--robot-triage/--robot-plan; \
         either bv changed shape or this test has stopped checking anything"
    );

    commands.sort_unstable();
    commands.dedup();
    for (index, args) in commands.iter().enumerate() {
        let argv: Vec<&str> = args.split_whitespace().collect();
        let run = run_br(&workspace, &argv, &format!("bv_suggested_command_{index}"));
        assert!(
            run.status.success(),
            "bv told the operator to run `br {args}`, which br rejected: {}",
            run.stderr
        );
    }
}
