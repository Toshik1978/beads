//! A branch-follow hook rebuilt the database from the rebase base's older JSONL.
//!
//! The incident: a hook that runs on a branch switch or rebase rebuilt the
//! database from whatever `issues.jsonl` the checkout showed at that moment.
//! Mid-rebase that file is the *base's* copy, older than the work being
//! replayed. The rebuild succeeded, the rebase then restored the newer JSONL,
//! and `br` reported "In sync" over a database that was missing issues (and had
//! stale statuses) the JSONL still held.
//!
//! This replays that sequence through the `br` binary alone, with no git hook:
//! flush an old state, flush a newer one, rebuild from the old file, put the
//! newer file back, then run a command. Whatever `br` chooses to do, it must
//! never end up claiming the database is in sync while the JSONL holds ids the
//! database lacks.

use crate::common;

use common::cli::{BrWorkspace, parse_created_id, parse_list_issues, run_br};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn br_ok(ws: &BrWorkspace, args: &[&str], label: &str) -> String {
    let run = run_br(ws, args.iter().copied(), label);
    assert!(run.status.success(), "{label} failed: {}", run.stderr);
    run.stdout
}

fn create(ws: &BrWorkspace, title: &str) -> String {
    let id = parse_created_id(&br_ok(ws, &["create", title], "create"));
    assert!(!id.is_empty(), "create {title} gave no id");
    id
}

fn jsonl_statuses(path: &Path) -> BTreeMap<String, String> {
    fs::read_to_string(path)
        .expect("read jsonl")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).expect("jsonl line");
            (
                v["id"].as_str().expect("id").to_string(),
                v["status"].as_str().expect("status").to_string(),
            )
        })
        .collect()
}

fn db_statuses(ws: &BrWorkspace) -> BTreeMap<String, String> {
    let out = br_ok(ws, &["list", "--all", "--json", "--limit", "0"], "list");
    parse_list_issues(&out)
        .iter()
        .map(|v| {
            (
                v["id"].as_str().expect("id").to_string(),
                v["status"].as_str().expect("status").to_string(),
            )
        })
        .collect()
}

#[test]
fn rebase_onto_an_older_jsonl_never_ends_in_sync_over_missing_content() {
    let ws = BrWorkspace::new();
    let jsonl = ws.root.join(".beads").join("issues.jsonl");
    br_ok(&ws, &["init"], "init");

    // 1. Old state: five issues, two closed, one deleted.
    let old: Vec<String> = (0..5).map(|i| create(&ws, &format!("old {i}"))).collect();
    br_ok(&ws, &["close", &old[0], "--reason", "done"], "close old 0");
    br_ok(&ws, &["close", &old[1], "--reason", "done"], "close old 1");
    br_ok(&ws, &["delete", &old[2], "--force"], "delete old 2");
    br_ok(&ws, &["sync", "--flush-only"], "flush old");
    let old_jsonl = fs::read(&jsonl).expect("read old jsonl");

    // 2. Newer state: three more issues, another close, another delete.
    for i in 0..3 {
        create(&ws, &format!("new {i}"));
    }
    br_ok(&ws, &["close", &old[3], "--reason", "done"], "close old 3");
    br_ok(&ws, &["delete", &old[4], "--force"], "delete old 4");
    br_ok(&ws, &["sync", "--flush-only"], "flush new");
    let new_jsonl = fs::read(&jsonl).expect("read new jsonl");
    assert_ne!(old_jsonl, new_jsonl, "the two states must differ");

    // 3. The hook: rebuild from the older file.
    fs::write(&jsonl, &old_jsonl).expect("restore old jsonl");
    let rebuild = run_br(
        &ws,
        ["sync", "--import-only", "--rebuild"],
        "rebuild from old",
    );
    assert!(rebuild.status.success(), "rebuild: {}", rebuild.stderr);

    // 4. The rebase finishes and the newer JSONL is back.
    fs::write(&jsonl, &new_jsonl).expect("restore new jsonl");
    let wanted = jsonl_statuses(&jsonl);

    // 5. The next ordinary command.
    let after = run_br(&ws, ["create", "after rebase"], "create after rebase");

    if after.status.success() {
        let new_id = parse_created_id(&after.stdout);
        let db = db_statuses(&ws);
        for (id, status) in &wanted {
            // A tombstone is deliberately hidden from `list`, so a deleted id
            // must be absent (a live row is a deleted issue come back); every
            // other id must be present with the JSONL's status.
            if status == "tombstone" {
                assert!(
                    !db.contains_key(id),
                    "{id} is deleted in the JSONL and must not be live: {:?}",
                    db.get(id)
                );
                continue;
            }
            assert_eq!(
                db.get(id),
                Some(status),
                "{id} must carry the JSONL's status after the next command"
            );
        }
        assert!(db.contains_key(&new_id), "the new issue must exist");
    } else {
        assert!(
            after.stderr.contains("br sync --merge"),
            "a refusal must point at `br sync --merge`: {}",
            after.stderr
        );
    }

    // In every case `In sync` is only acceptable when nothing is missing.
    let db = db_statuses(&ws);
    let missing: Vec<&String> = wanted
        .iter()
        .filter(|(_, s)| *s != "tombstone")
        .map(|(id, _)| id)
        .filter(|id| !db.contains_key(*id))
        .collect();
    let status = br_ok(&ws, &["sync", "--status"], "sync status");
    assert!(
        missing.is_empty() || !status.contains("In sync"),
        "reported In sync while missing {missing:?}:\n{status}"
    );
}
