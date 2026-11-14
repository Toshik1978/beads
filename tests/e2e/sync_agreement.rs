//! The write precondition, end to end: a command never writes over a JSONL the
//! database has not agreed with. A changed JSONL is imported, merged with the
//! local changes, or the command refuses — it is never silently skipped or
//! overwritten.

use crate::common;
use common::cli::{BrRun, BrWorkspace, parse_created_id, run_br, run_br_with_env};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::sync::Barrier;

/// `RUST_LOG=off` silences every tracing event, so anything a test still sees
/// on stderr was printed for the user rather than logged.
const NO_LOGS: [(&str, &str); 1] = [("RUST_LOG", "off")];

fn jsonl_path(ws: &BrWorkspace) -> PathBuf {
    ws.root.join(".beads").join("issues.jsonl")
}

fn init(ws: &BrWorkspace) {
    let init = run_br(ws, ["init", "--prefix", "sa"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
}

fn create(ws: &BrWorkspace, title: &str, label: &str) -> String {
    let run = run_br(ws, ["create", title], label);
    assert!(
        run.status.success(),
        "create {title} failed: {}",
        run.stderr
    );
    parse_created_id(&run.stdout)
}

fn jsonl_records(ws: &BrWorkspace) -> Vec<Value> {
    fs::read_to_string(jsonl_path(ws))
        .expect("read jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("jsonl line"))
        .collect()
}

fn jsonl_title(ws: &BrWorkspace, id: &str) -> Option<String> {
    jsonl_records(ws)
        .into_iter()
        .find(|record| record["id"] == id)
        .and_then(|record| record["title"].as_str().map(str::to_string))
}

/// Edit one record's title in the JSONL by hand, the way a `git pull` would.
fn retitle_in_jsonl(ws: &BrWorkspace, id: &str, title: &str) {
    let updated_at = (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339();
    let lines: Vec<String> = jsonl_records(ws)
        .into_iter()
        .map(|mut record| {
            if record["id"] == id {
                record["title"] = Value::String(title.to_string());
                record["updated_at"] = Value::String(updated_at.clone());
            }
            serde_json::to_string(&record).expect("serialize record")
        })
        .collect();
    fs::write(jsonl_path(ws), lines.join("\n") + "\n").expect("write jsonl");
}

fn db_titles(ws: &BrWorkspace, label: &str) -> Vec<(String, String)> {
    // `--allow-stale` reads the database as it is, without reconciling.
    let run = run_br(ws, ["--allow-stale", "list", "--json", "--all"], label);
    assert!(run.status.success(), "list failed: {}", run.stderr);
    issue_titles(&run)
}

fn issue_titles(run: &BrRun) -> Vec<(String, String)> {
    let mut titles: Vec<(String, String)> = common::cli::parse_list_issues(&run.stdout)
        .into_iter()
        .map(|issue| {
            (
                issue["id"].as_str().unwrap_or_default().to_string(),
                issue["title"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    titles.sort();
    titles
}

#[test]
fn create_over_a_conflicting_jsonl_refuses() {
    let ws = BrWorkspace::new();
    init(&ws);
    let a = create(&ws, "a", "create_a");
    let flush = run_br(&ws, ["sync", "--flush-only"], "flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let update = run_br(
        &ws,
        ["--no-auto-flush", "update", &a, "--title", "a from db"],
        "update_a",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    retitle_in_jsonl(&ws, &a, "a from jsonl");
    let before = db_titles(&ws, "db_before");

    let run = run_br_with_env(&ws, ["create", "b"], NO_LOGS, "create_b");

    assert!(
        !run.status.success(),
        "create should refuse: {}",
        run.stdout
    );
    assert!(run.stderr.contains("br sync --merge"), "{}", run.stderr);
    assert!(run.stderr.contains("--force-db"), "{}", run.stderr);
    assert_eq!(
        db_titles(&ws, "db_after"),
        before,
        "a refusal leaves the database unchanged"
    );
    assert_eq!(jsonl_title(&ws, &a).as_deref(), Some("a from jsonl"));
}

#[test]
fn auto_flush_never_writes_over_an_unseen_jsonl() {
    let ws = BrWorkspace::new();
    init(&ws);
    let a = create(&ws, "a", "create_a");
    retitle_in_jsonl(&ws, &a, "a from jsonl");
    let before = fs::read_to_string(jsonl_path(&ws)).expect("read jsonl");

    let run = run_br_with_env(
        &ws,
        ["--no-auto-import", "create", "b"],
        NO_LOGS,
        "create_b",
    );

    assert_eq!(
        fs::read_to_string(jsonl_path(&ws)).expect("read jsonl"),
        before,
        "auto-flush must not write over a JSONL the database never saw"
    );
    assert!(
        !run.status.success() || run.stderr.contains("br sync --merge"),
        "the skipped flush must be reported: status={} stderr={}",
        run.status,
        run.stderr
    );
    let status = run_br(&ws, ["sync", "--status"], "status");
    assert!(status.status.success(), "status failed: {}", status.stderr);
    assert!(!status.stdout.contains("In sync"), "{}", status.stdout);
}

#[test]
fn allow_stale_never_records_agreement() {
    let ws = BrWorkspace::new();
    init(&ws);
    let a = create(&ws, "a", "create_a");
    retitle_in_jsonl(&ws, &a, "a from jsonl");

    let stale = run_br(&ws, ["--allow-stale", "create", "b"], "create_b_stale");
    assert!(stale.status.success(), "create failed: {}", stale.stderr);
    let b = parse_created_id(&stale.stdout);

    let list = run_br(&ws, ["list", "--json"], "list_after");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let mut expected = vec![(a, "a from jsonl".to_string()), (b, "b".to_string())];
    expected.sort();
    assert_eq!(
        issue_titles(&list),
        expected,
        "the next plain command reconciles the JSONL change the stale write never saw"
    );
}

#[test]
fn no_db_mode_is_unaffected() {
    let ws = BrWorkspace::new();
    init(&ws);
    let a = create(&ws, "a", "create_a");
    retitle_in_jsonl(&ws, &a, "a from jsonl");

    let run = run_br(&ws, ["--no-db", "list", "--json"], "list_no_db");

    assert!(run.status.success(), "--no-db list failed: {}", run.stderr);
    assert_eq!(issue_titles(&run), vec![(a, "a from jsonl".to_string())]);
}

#[test]
fn concurrent_writers_wait_instead_of_refusing() {
    let ws = BrWorkspace::new();
    init(&ws);
    let titles = ["writer one", "writer two", "writer three"];
    let barrier = Barrier::new(titles.len());

    let runs: Vec<BrRun> = std::thread::scope(|scope| {
        let handles: Vec<_> = titles
            .iter()
            .enumerate()
            .map(|(n, title)| {
                let (ws, barrier) = (&ws, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    run_br(ws, ["create", title], &format!("writer_{n}"))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("writer thread"))
            .collect()
    });

    for run in &runs {
        assert!(run.status.success(), "a writer failed: {}", run.stderr);
        assert!(
            !run.stderr.contains("auto-flush") && !run.stderr.contains("AUTO_FLUSH"),
            "a writer skipped its flush: {}",
            run.stderr
        );
    }
    let mut want: Vec<String> = titles.iter().map(ToString::to_string).collect();
    want.sort();
    let mut in_db: Vec<String> = db_titles(&ws, "db")
        .into_iter()
        .map(|(_, title)| title)
        .collect();
    in_db.sort();
    assert_eq!(in_db, want, "all writers' issues are in the database");
    let mut in_jsonl: Vec<String> = jsonl_records(&ws)
        .into_iter()
        .filter_map(|record| record["title"].as_str().map(str::to_string))
        .collect();
    in_jsonl.sort();
    assert_eq!(in_jsonl, want, "all writers' issues are in the JSONL");
}

#[test]
fn flush_only_reconciles_before_writing() {
    let ws = BrWorkspace::new();
    init(&ws);
    let a = create(&ws, "a", "create_a");
    let b = create(&ws, "b", "create_b");
    let update = run_br(
        &ws,
        ["--no-auto-flush", "update", &a, "--title", "a from db"],
        "update_a",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    retitle_in_jsonl(&ws, &b, "b from jsonl");

    let flush = run_br(&ws, ["sync", "--flush-only"], "flush");

    assert!(flush.status.success(), "flush failed: {}", flush.stderr);
    assert_eq!(jsonl_title(&ws, &a).as_deref(), Some("a from db"));
    assert_eq!(jsonl_title(&ws, &b).as_deref(), Some("b from jsonl"));
    let mut expected = vec![
        (a, "a from db".to_string()),
        (b, "b from jsonl".to_string()),
    ];
    expected.sort();
    assert_eq!(db_titles(&ws, "db_after"), expected);
}

#[test]
fn a_fresh_workspace_flushes_without_a_prior_agreement() {
    let ws = BrWorkspace::new();
    init(&ws);

    // No command has reconciled the empty JSONL `br init` wrote, so there is
    // no agreement yet; there is also nothing in the file to overwrite.
    let run = run_br(&ws, ["--no-auto-import", "create", "a"], "create_a");

    assert!(run.status.success(), "create failed: {}", run.stderr);
    assert!(!run.stderr.contains("br sync --merge"), "{}", run.stderr);
    let id = parse_created_id(&run.stdout);
    assert_eq!(jsonl_title(&ws, &id).as_deref(), Some("a"));
}
