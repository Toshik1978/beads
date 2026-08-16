//! bds-o9a: compare-and-set guard on `br update` (`--if-status`).
//!
//! `--if-assignee` was the other half of this pair; it was removed along with
//! `assignee` itself (bds-b4f.2.6), since claiming *is* assigning and
//! `--status in_progress` already covers the workflow.
//!
//! The guard exists so that concurrent agents sharing one workspace can say
//! "move this to in_progress only if it is still open" without a
//! read-then-write race. Two properties carry that promise and are asserted
//! here rather than assumed:
//!
//! 1. **Exactly one winner.** Two writers racing the same guarded transition
//!    must not both succeed, even though both observed the pre-state.
//! 2. **A failed guard writes nothing.** Not the field, not `updated_at`, not
//!    the transition comment. A guard that half-applies is worse than no guard,
//!    because the caller's fallback path now runs against a mutated row.
//!
//! The third property is legibility: the caller must be able to tell "the guard
//! did not hold" from "no such issue", or a retry loop cannot decide whether to
//! retry or stop. That is what the CLI-level tests at the bottom pin.

use crate::common;

use beads::error::BeadsError;
use beads::model::{Priority, Status};
use beads::storage::{IssueUpdate, SqliteStorage};
use chrono::{TimeZone, Utc};
use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

fn seed_issue(storage: &mut SqliteStorage, id: &str) {
    let t = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let issue = beads::model::Issue {
        id: id.to_string(),
        title: format!("Guarded issue {id}"),
        status: Status::Open,
        priority: Priority(2),
        issue_type: beads::model::IssueType::Task,
        created_at: t,
        updated_at: t,
        ..beads::model::Issue::default()
    };
    storage.create_issue(&issue, "seed").unwrap();
}

fn guard_on_status(expected: &str) -> IssueUpdate {
    IssueUpdate {
        status: Some(Status::InProgress),
        expect_status: Some(expected.to_string()),
        ..IssueUpdate::default()
    }
}

#[test]
fn a_status_guard_that_holds_applies_the_update() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    seed_issue(&mut storage, "cas-hold");

    let issue = storage
        .update_issue("cas-hold", &guard_on_status("open"), "alice")
        .expect("guard holds");
    assert_eq!(issue.status, Status::InProgress);
}

#[test]
fn a_status_guard_that_does_not_hold_names_both_values() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    seed_issue(&mut storage, "cas-miss");

    let error = storage
        .update_issue("cas-miss", &guard_on_status("in_progress"), "alice")
        .expect_err("guard must not hold against an open issue");

    match error {
        BeadsError::PreconditionFailed {
            id,
            field,
            expected,
            actual,
        } => {
            assert_eq!(id, "cas-miss");
            assert_eq!(field, "status");
            assert_eq!(expected, "in_progress");
            assert_eq!(
                actual, "open",
                "the error has to name what was found, or the caller cannot \
                 decide what to do next without a second round trip"
            );
        }
        other => panic!("expected PreconditionFailed, got {other:?}"),
    }
}

/// Criterion: a failed guard writes nothing and leaves `updated_at` untouched.
///
/// `updated_at` is the load-bearing one. Every local write advances it strictly
/// (bds-6nz), and import resolves a same-timestamp difference in the JSONL's
/// favour (bds-n8d) — so a refused update that still bumped the timestamp would
/// make the *file* look stale against a database that did not change.
#[test]
fn a_failed_guard_writes_nothing_at_all() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    seed_issue(&mut storage, "cas-inert");
    let before = storage.get_issue("cas-inert").unwrap().expect("seeded");

    let update = IssueUpdate {
        status: Some(Status::InProgress),
        title: Some("should never land".to_string()),
        transition_comment: Some("should never be inserted".to_string()),
        expect_status: Some("blocked".to_string()),
        ..IssueUpdate::default()
    };
    storage
        .update_issue("cas-inert", &update, "alice")
        .expect_err("guard must not hold");

    let after = storage
        .get_issue("cas-inert")
        .unwrap()
        .expect("still there");
    assert_eq!(after.updated_at, before.updated_at, "updated_at moved");
    assert_eq!(after.title, before.title);
    assert_eq!(after.status, before.status);
    assert!(
        storage.get_comments("cas-inert").unwrap().is_empty(),
        "the transition comment is inserted by the same function and must be \
         downstream of the guard, not beside it"
    );
}

/// Criterion: two concurrent updates with the same guard produce exactly one
/// winner.
///
/// Both threads pass `--if-status open` against a row that is open, so both
/// guards hold at the moment each thread reads. Only the transaction that
/// commits first may write; the second must observe `in_progress` and refuse.
#[test]
#[allow(clippy::needless_collect)]
fn two_writers_racing_one_guarded_transition_produce_one_winner() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("beads.db");
    {
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        seed_issue(&mut storage, "cas-race");
    }

    let barrier = Arc::new(Barrier::new(2));
    let db_lock = Arc::new(std::sync::Mutex::new(()));
    let path = db_path.to_string_lossy().into_owned();

    let handles: Vec<_> = ["alice", "bob"]
        .iter()
        .map(|actor| {
            let barrier = Arc::clone(&barrier);
            let db_lock = Arc::clone(&db_lock);
            let path = path.clone();
            let actor = (*actor).to_string();
            thread::spawn(move || {
                barrier.wait();
                let _guard = db_lock.lock().unwrap();
                let mut storage = SqliteStorage::open(Path::new(&path)).unwrap();
                let update = IssueUpdate {
                    status: Some(Status::InProgress),
                    expect_status: Some("open".to_string()),
                    ..IssueUpdate::default()
                };
                storage.update_issue("cas-race", &update, &actor)
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "exactly one writer may win: {results:?}"
    );
    let loser = results
        .iter()
        .find_map(|r| r.as_ref().err())
        .expect("one writer must lose");
    assert!(
        matches!(loser, BeadsError::PreconditionFailed { .. }),
        "the loser must lose *because of the guard*, not incidentally: {loser:?}"
    );
}

// --- CLI surface -----------------------------------------------------------

fn init_workspace(label: &str) -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "bd"], label);
    assert!(init.status.success(), "init failed: {}", init.stderr);
    workspace
}

fn create_open_issue(workspace: &BrWorkspace, title: &str, label: &str) -> String {
    let created = run_br(workspace, ["create", title], label);
    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr
    );
    let line = created.stdout.lines().next().unwrap_or("");
    line.strip_prefix("✓ ")
        .unwrap_or(line)
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Criterion: "the guard did not hold" is distinguishable from "the issue does
/// not exist" — by exit code and by error code, not only by prose.
#[test]
fn the_cli_reports_a_failed_guard_distinguishably_from_a_missing_issue() {
    let workspace = init_workspace("cas cli init");
    let id = create_open_issue(&workspace, "guarded", "cas cli create");

    let refused = run_br(
        &workspace,
        [
            "update",
            &id,
            "--status",
            "in_progress",
            "--if-status",
            "blocked",
            "--json",
        ],
        "cas cli refused",
    );
    assert!(!refused.status.success());
    assert_eq!(
        refused.status.code(),
        Some(4),
        "a failed guard must not share IssueNotFound's exit code (3): stderr={}",
        refused.stderr
    );

    // `--json` puts the structured error on stdout under an `error` envelope,
    // which is what an agent parses.
    let payload = extract_json_payload(&refused.stdout);
    let json: Value = serde_json::from_str(&payload)
        .unwrap_or_else(|error| panic!("parse error json ({error}): {}", refused.stdout));
    let error = &json["error"];
    assert_eq!(error["code"].as_str(), Some("PRECONDITION_FAILED"));
    assert_eq!(error["context"]["field"].as_str(), Some("status"));
    assert_eq!(error["context"]["expected"].as_str(), Some("blocked"));
    assert_eq!(error["context"]["actual"].as_str(), Some("open"));
    assert_eq!(error["context"]["id"].as_str(), Some(id.as_str()));

    let missing = run_br(
        &workspace,
        [
            "update",
            "bd-nosuch",
            "--status",
            "in_progress",
            "--if-status",
            "open",
        ],
        "cas cli missing",
    );
    assert!(!missing.status.success());
    assert_ne!(
        missing.status.code(),
        Some(4),
        "a missing issue must not look like a failed guard: stderr={}",
        missing.stderr
    );

    // And the refused update really did not write.
    let show = run_br(&workspace, ["show", &id, "--json"], "cas cli show");
    let shown: Value = serde_json::from_str(&extract_json_payload(&show.stdout)).unwrap();
    let record = shown
        .as_array()
        .and_then(|rows| rows.first())
        .unwrap_or(&shown);
    assert_eq!(record["status"].as_str(), Some("open"));
}

#[test]
fn the_cli_holds_a_guard_that_still_matches() {
    let workspace = init_workspace("cas cli init");
    let id = create_open_issue(&workspace, "guarded ok", "cas cli create ok");

    let applied = run_br(
        &workspace,
        [
            "update",
            &id,
            "--status",
            "in_progress",
            "--if-status",
            "open",
        ],
        "cas cli applied",
    );
    assert!(applied.status.success(), "stderr={}", applied.stderr);

    let show = run_br(&workspace, ["show", &id, "--json"], "cas cli show ok");
    let shown: Value = serde_json::from_str(&extract_json_payload(&show.stdout)).unwrap();
    let record = shown
        .as_array()
        .and_then(|rows| rows.first())
        .unwrap_or(&shown);
    assert_eq!(record["status"].as_str(), Some("in_progress"));
}

/// A guard with nothing to guard is refused rather than silently ignored.
/// Label writes go through their own transactions, so a guard riding on a
/// label-only change would read as enforced and would not be.
#[test]
fn the_cli_refuses_a_guard_with_no_field_update_to_guard() {
    let workspace = init_workspace("cas cli init");
    let id = create_open_issue(&workspace, "guard nothing", "cas cli create bare");

    for extra in [vec![], vec!["--add-label", "urgent"]] {
        let mut args = vec!["update", &id, "--if-status", "open"];
        args.extend(extra.iter().copied());
        let refused = run_br(&workspace, args, "cas cli bare guard");
        assert!(
            !refused.status.success(),
            "a guard that guards nothing must be refused, not ignored: stdout={}",
            refused.stdout
        );
        assert!(
            refused.stderr.contains("guards a field update"),
            "stderr={}",
            refused.stderr
        );
    }
}
