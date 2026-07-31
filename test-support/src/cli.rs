use assert_cmd::Command;
use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;

const SMOKE_PRESERVED_ENV_KEYS: &[&str] = &[
    "BEADS_DIR",
    "BEADS_JSONL",
    "BEADS_CACHE_DIR",
    "BR_OUTPUT_FORMAT",
    "TOON_DEFAULT_FORMAT",
    "TOON_STATS",
];

fn should_clear_inherited_br_env(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    key.starts_with("BD_")
        || key.starts_with("BEADS_")
        || matches!(
            key.as_ref(),
            "BR_DISABLE_READ_ONLY_FAST_OPEN"
                | "BR_OUTPUT_FORMAT"
                | "TOON_DEFAULT_FORMAT"
                | "TOON_STATS"
                // `create`/`update`/`close`/`reopen`'s --agent-name/--harness/--model
                // args are declared with clap's `env = "BR_..."` and no
                // `hide_env_values`, so `--help` prints the *current* value after
                // the `=` in `[env: BR_AGENT_NAME=...]`. Agent harnesses (this one
                // included) commonly set these by default, so an unfiltered
                // inherited value leaks into any `--help` snapshot -- the same
                // class of defect as a real username baked into a golden file.
                // Clear them here so run_br's own env override (set below via
                // `cmd.envs(env_vars)`, after this clearing loop) is what a
                // caller sees, not whatever the invoking shell happened to export.
                | "BR_AGENT_NAME"
                | "BR_HARNESS"
                | "BR_MODEL"
        )
}

fn should_preserve_smoke_env(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    SMOKE_PRESERVED_ENV_KEYS.contains(&key.as_ref())
}

fn clear_inherited_br_env(cmd: &mut Command) {
    clear_inherited_br_env_except(cmd, &[]);
}

fn clear_inherited_br_env_except(cmd: &mut Command, preserve: &[&str]) {
    for (key, _) in std::env::vars_os() {
        let key_str = key.to_string_lossy();
        let should_preserve = preserve.contains(&key_str.as_ref());
        if should_clear_inherited_br_env(&key) && !should_preserve {
            cmd.env_remove(&key);
        }
    }
}

#[derive(Debug)]
pub struct BrRun {
    pub stdout: String,
    /// What the child wrote to stderr -- except when it failed having written
    /// nothing at all, in which case this carries a synthetic
    /// `<no stderr; ...>` diagnosis instead (see `describe_exit`). See
    /// bds-nld: essentially every assertion in the e2e suite reports a failure
    /// as `"... failed: {}", run.stderr`, so a child killed by a signal
    /// produced a panic message that ended at the colon and said nothing.
    /// Substituting here makes all ~1900 of those sites diagnosable without
    /// editing them, and only in the case that is currently empty. `log_path`
    /// always holds the pristine capture.
    pub stderr: String,
    pub status: std::process::ExitStatus,
    /// `status.code()`, hoisted so a test can assert on it directly: `None`
    /// means the child died on a signal rather than exiting.
    pub code: Option<i32>,
    pub duration: Duration,
    pub log_path: PathBuf,
}

/// Renders why a child process ended, distinguishing a signal death from an
/// ordinary non-zero exit.
///
/// `ExitStatus`'s own `Display` already does this on Unix, but it is only ever
/// written to the log file; the assertion messages print `stderr` alone. This
/// is the text substituted into an otherwise-empty `stderr`.
pub fn describe_exit(status: std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("child exited with code {code} and wrote nothing to stderr");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!(
                "child was killed by signal {signal} (no exit code, no stderr) -- \
                 typically the OS reclaiming it under memory or CPU pressure, \
                 not a br failure path"
            );
        }
    }
    format!("child ended without an exit code and wrote nothing to stderr: {status}")
}

pub struct BrWorkspace {
    pub temp_dir: TempDir,
    pub root: PathBuf,
    pub log_dir: PathBuf,
}

impl BrWorkspace {
    // clippy::new_without_default asks for `impl Default` here. Deliberately
    // not added: `Default::default()` is called implicitly — by
    // `#[derive(Default)]` on any struct that comes to hold one of these, by
    // `..Default::default()` in struct update syntax, by
    // `Option::unwrap_or_default()` — and every one of those would silently
    // create a temp directory, canonicalize it and create a log dir, panicking
    // if any of the three fails. `Default` should be cheap and infallible;
    // this is neither, so it stays an explicit `new()`.
    #[allow(clippy::new_without_default)]
    #[must_use]
    pub fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp dir");
        // Canonicalize so recorded paths are stable (e.g. /private/var/... on
        // macOS instead of /var/...) rather than mixing symlinked and
        // resolved forms across assertions. `temp_dir` is kept alive in the
        // struct so the directory isn't dropped.
        let root = dunce::canonicalize(temp_dir.path()).expect("canonicalize temp dir");
        let log_dir = root.join("logs");
        fs::create_dir_all(&log_dir).expect("log dir");
        Self {
            temp_dir,
            root,
            log_dir,
        }
    }
}

pub fn run_br<I, S>(workspace: &BrWorkspace, args: I, label: &str) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // Reuse run_br_with_env with empty env vars
    run_br_with_env(
        workspace,
        args,
        std::iter::empty::<(String, String)>(),
        label,
    )
}

pub fn run_br_with_env<I, S, E, K, V>(
    workspace: &BrWorkspace,
    args: I,
    env_vars: E,
    label: &str,
) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    run_br_full(workspace, args, env_vars, None, label)
}

pub fn run_br_with_stdin<I, S>(workspace: &BrWorkspace, args: I, input: &str, label: &str) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_br_full(
        workspace,
        args,
        std::iter::empty::<(String, String)>(),
        Some(input),
        label,
    )
}

pub fn run_br_smoke_at_root_with_env<I, S, E, K, V>(
    root: &Path,
    args: I,
    env_vars: E,
    label: &str,
) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let log_dir = root.join("logs");
    run_br_full_in_root(root, &log_dir, args, env_vars, None, label, true)
}

fn run_br_full<I, S, E, K, V>(
    workspace: &BrWorkspace,
    args: I,
    env_vars: E,
    stdin_input: Option<&str>,
    label: &str,
) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    run_br_full_in_root(
        &workspace.root,
        &workspace.log_dir,
        args,
        env_vars,
        stdin_input,
        label,
        false,
    )
}

fn run_br_full_in_root<I, S, E, K, V>(
    root: &Path,
    log_dir: &Path,
    args: I,
    env_vars: E,
    stdin_input: Option<&str>,
    label: &str,
    preserve_smoke_env: bool,
) -> BrRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    fs::create_dir_all(log_dir).expect("log dir");

    let mut cmd = Command::new(
        crate::cargo_built_binary("br").expect("`br` must be built; run `cargo build` first"),
    );
    cmd.current_dir(root);
    cmd.args(args);
    if preserve_smoke_env {
        clear_inherited_br_env_except(&mut cmd, SMOKE_PRESERVED_ENV_KEYS);
    } else {
        clear_inherited_br_env(&mut cmd);
    }
    // Default e2e runs un-throttled so history-mechanics tests (backup
    // chronology, prune, restore) observe one `.br_history` snapshot per
    // mutation. Set before caller `env_vars` so a test can override this to
    // exercise the #313 snapshot throttle.
    cmd.env("BR_HISTORY_MIN_INTERVAL_SECS", "0");
    // Keep `warn!` (several tests assert on it) while dropping `debug!` noise
    // from stderr, which otherwise satisfies loose substring assertions for
    // the wrong reason. Set before caller `env_vars` so a test can still
    // override it (e.g. to opt back into debug output).
    cmd.env("RUST_LOG", "beads=warn");
    cmd.envs(env_vars);
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_BACKTRACE", "1");
    cmd.env("HOME", root);

    if let Some(input) = stdin_input {
        cmd.write_stdin(input);
    }

    let start = Instant::now();
    let output = cmd.output().expect("run br");
    let duration = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let log_path = log_dir.join(format!("{label}.log"));
    let timestamp = SystemTime::now();
    let log_body = format!(
        "label: {label}\nstarted: {:?}\nduration: {:?}\nstatus: {}\nargs: {:?}\ncwd: {}\n\nstdout:\n{}\n\nstderr:\n{}\n",
        timestamp,
        duration,
        output.status,
        cmd.get_args().collect::<Vec<_>>(),
        root.display(),
        stdout,
        stderr
    );
    fs::write(&log_path, log_body).expect("write log");

    // Substitute only when the run failed AND said nothing -- the one case
    // where every existing assertion in the tree prints an empty string. A
    // successful run keeps its (possibly empty) stderr byte-for-byte, and so
    // does any failure that actually wrote a message, so no substring
    // assertion changes meaning. The log written just above always holds the
    // raw capture alongside the full `status`.
    let stderr = if output.status.success() || !stderr.is_empty() {
        stderr
    } else {
        format!("<no stderr; {}>", describe_exit(output.status))
    };

    BrRun {
        stdout,
        stderr,
        status: output.status,
        code: output.status.code(),
        duration,
        log_path,
    }
}

/// Extract the issue ID from `br create` stdout.
///
/// Handles both formats: `"Created pfx-xxx: title"` and `"✓ Created pfx-xxx: title"`.
pub fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string()
}

pub fn extract_json_payload(stdout: &str) -> String {
    let lines: Vec<&str> = stdout.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') || trimmed.starts_with('{') {
            return lines[idx..].join("\n").trim().to_string();
        }
    }
    stdout.trim().to_string()
}

pub fn parse_json_value(stdout: &str) -> Value {
    let payload = extract_json_payload(stdout);
    serde_json::from_str(&payload).expect("valid JSON payload")
}

/// Extract an issue array from JSON stdout, handling both formats:
/// - Paginated: `{"issues": [...], "total": N, ...}` → returns the inner array
/// - Bare array: `[...]` → returns it directly
pub fn extract_issues_array(stdout: &str) -> Vec<Value> {
    let json = parse_json_value(stdout);
    if let Some(arr) = json.as_array() {
        return arr.clone();
    }
    if let Some(issues) = json.get("issues").and_then(Value::as_array) {
        return issues.clone();
    }
    panic!(
        "JSON output is neither a bare array nor an object with 'issues': {}",
        &stdout[..stdout.len().min(200)]
    );
}

pub fn parse_list_page(stdout: &str) -> Value {
    let json = parse_json_value(stdout);
    assert!(
        json.is_object(),
        "list JSON should be an object with pagination metadata"
    );
    assert!(
        json.get("issues").is_some(),
        "list JSON should contain an issues field"
    );
    json
}

pub fn parse_list_issues(stdout: &str) -> Vec<Value> {
    parse_list_page(stdout)
        .get("issues")
        .and_then(Value::as_array)
        .cloned()
        .expect("list JSON should contain an issues array")
}

#[cfg(test)]
mod tests {
    use super::{should_clear_inherited_br_env, should_preserve_smoke_env};
    use std::ffi::OsStr;

    #[test]
    fn inherited_beads_and_toon_env_are_cleared() {
        for key in [
            "BD_ACTOR",
            "BEADS_CACHE_DIR",
            "BEADS_JSONL",
            "BR_DISABLE_READ_ONLY_FAST_OPEN",
            "BR_OUTPUT_FORMAT",
            "TOON_DEFAULT_FORMAT",
            "TOON_STATS",
        ] {
            assert!(
                should_clear_inherited_br_env(OsStr::new(key)),
                "{key} should be cleared for hermetic br tests"
            );
        }
    }

    #[test]
    fn inherited_tier1_attribution_env_is_cleared() {
        // create/update/close/reopen's --agent-name/--harness/--model args use
        // clap's `env = "BR_..."` without `hide_env_values`, so `--help` prints
        // the live value. An agent harness commonly sets these, so an
        // unfiltered inherited value would leak into `--help` snapshots
        // (bds-04l.3.3 fix round 1).
        for key in ["BR_AGENT_NAME", "BR_HARNESS", "BR_MODEL"] {
            assert!(
                should_clear_inherited_br_env(OsStr::new(key)),
                "{key} should be cleared so --help snapshots don't depend on the invoking harness"
            );
        }
    }

    #[test]
    fn unrelated_env_are_preserved() {
        for key in ["HOME", "PATH", "RUST_LOG", "NO_COLOR"] {
            assert!(
                !should_clear_inherited_br_env(OsStr::new(key)),
                "{key} should not be blanket-cleared"
            );
        }
    }

    #[test]
    fn smoke_profile_preserves_selected_routing_and_output_env() {
        for key in [
            "BEADS_DIR",
            "BEADS_CACHE_DIR",
            "BEADS_JSONL",
            "BR_OUTPUT_FORMAT",
            "TOON_DEFAULT_FORMAT",
            "TOON_STATS",
        ] {
            assert!(
                should_preserve_smoke_env(OsStr::new(key)),
                "{key} should be preserved for non-hermetic smoke coverage"
            );
        }
    }

    #[test]
    fn smoke_profile_still_clears_unrelated_legacy_beads_env() {
        for key in ["BD_ACTOR", "BD_DB", "BD_CONFIG", "BEADS_DEBUG"] {
            assert!(
                !should_preserve_smoke_env(OsStr::new(key)),
                "{key} should still be scrubbed in smoke mode"
            );
        }
    }
}
