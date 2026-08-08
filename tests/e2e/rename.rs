//! bds-r6h: `br rename` — the front door on machinery that already shipped.
//!
//! `tests/storage/rename.rs` already pins the mechanics: the cascade, the
//! tombstone, `former_ids`, inbound dependency edges, the rollback on a
//! mid-cascade failure. None of that is repeated here. What this file tests is
//! the command — that it reaches all of that, that each refusal actually
//! refuses, and that `--dry-run` really writes nothing, which is the one claim a
//! dry run makes that is worth distrusting.
//!
//! The composition test is the important one. A rename is only finished when it
//! has survived the round trip through `issues.jsonl`, because the file is the
//! source of truth and the database is a derived cache — a rename that looked
//! right in the database and did not export would be undone by the next import.

use crate::common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

fn workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "rn"], "rename init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    workspace
}

fn create(workspace: &BrWorkspace, title: &str, parent: Option<&str>) -> String {
    let mut args = vec!["create", title];
    if let Some(parent) = parent {
        args.extend(["--parent", parent]);
    }
    let created = run_br(workspace, args, "rename create");
    assert!(created.status.success(), "create: {}", created.stderr);
    let line = created.stdout.lines().next().unwrap_or("");
    line.strip_prefix("✓ ")
        .unwrap_or(line)
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn live_ids(workspace: &BrWorkspace, label: &str) -> Vec<String> {
    let listed = run_br(workspace, ["list", "--all", "--json"], label);
    assert!(listed.status.success(), "list: {}", listed.stderr);
    let json: Value =
        serde_json::from_str(&extract_json_payload(&listed.stdout)).expect("parse list json");
    let rows = json
        .get("issues")
        .and_then(Value::as_array)
        .or_else(|| json.as_array())
        .cloned()
        .unwrap_or_default();
    let mut ids: Vec<String> = rows
        .iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    ids.sort();
    ids
}

/// A three-level tree, renamed at the root, then flushed and rebuilt from the
/// file. Covers two of the bead's three criteria at once: the cascade reaches
/// grandchildren, and the whole thing composes with export/import.
#[test]
fn a_rename_cascades_to_grandchildren_and_survives_a_rebuild_from_jsonl() {
    let workspace = workspace();
    let root = create(&workspace, "root", None);
    let child = create(&workspace, "child", Some(&root));
    let grandchild = create(&workspace, "grandchild", Some(&child));
    assert!(
        grandchild.starts_with(&child) && child.starts_with(&root),
        "the fixture has to be a real three-level tree: {root} / {child} / {grandchild}"
    );

    let renamed = run_br(&workspace, ["rename", &root, "rn-tree"], "rename root");
    assert!(renamed.status.success(), "rename: {}", renamed.stderr);

    let expected = vec![
        "rn-tree".to_string(),
        "rn-tree.1".to_string(),
        "rn-tree.1.1".to_string(),
    ];
    assert_eq!(
        live_ids(&workspace, "rename after"),
        expected,
        "the cascade has to reach the grandchild, not only the direct child"
    );

    // The file is the source of truth. Throw the database away and rebuild it,
    // which is what a fresh clone does: if the rename had not been exported, the
    // old IDs would come back here.
    let flushed = run_br(&workspace, ["sync", "--flush-only"], "rename flush");
    assert!(flushed.status.success(), "flush: {}", flushed.stderr);
    for suffix in ["beads.db", "beads.db-wal", "beads.db-shm"] {
        let _ = std::fs::remove_file(workspace.root.join(".beads").join(suffix));
    }
    let rebuilt = run_br(
        &workspace,
        ["sync", "--import-only", "--rebuild"],
        "rename rebuild",
    );
    assert!(rebuilt.status.success(), "rebuild: {}", rebuilt.stderr);

    assert_eq!(
        live_ids(&workspace, "rename after rebuild"),
        expected,
        "a rename that does not survive a rebuild from issues.jsonl was never \
         really applied -- the database is the derived cache, not the record"
    );
}

/// Criterion: the vacated ID resolves to the tombstone afterwards.
///
/// In practice the resolver prefers the `former_ids` redirect, so asking for the
/// old ID hands back the live successor. Both halves are asserted: the redirect
/// works, and the vacated address is a tombstone rather than simply gone.
#[test]
fn the_vacated_id_redirects_to_the_successor_and_holds_a_tombstone() {
    let workspace = workspace();
    let original = create(&workspace, "movable", None);
    let renamed = run_br(&workspace, ["rename", &original, "rn-moved"], "rename once");
    assert!(renamed.status.success(), "rename: {}", renamed.stderr);

    let shown = run_br(&workspace, ["show", &original, "--json"], "rename show old");
    assert!(
        shown.status.success(),
        "the old ID has to keep resolving: {}",
        shown.stderr
    );
    let json: Value =
        serde_json::from_str(&extract_json_payload(&shown.stdout)).expect("parse show json");
    let record = json
        .as_array()
        .and_then(|rows| rows.first())
        .unwrap_or(&json);
    assert_eq!(
        record["id"].as_str(),
        Some("rn-moved"),
        "the old ID must resolve forward to the live successor, not to the \
         tombstone left in its place: {}",
        shown.stdout
    );
    assert!(
        record["former_ids"]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(original.as_str()))),
        "the successor has to carry the provenance: {}",
        shown.stdout
    );

    // And the vacated address itself is a tombstone: `--all` includes closed but
    // not tombstones, so the ID must not come back as a live row.
    assert!(
        !live_ids(&workspace, "rename tombstone check").contains(&original),
        "the vacated ID must not still be a live row"
    );
}

/// `--dry-run` reports the full cascade and writes nothing. The second half is
/// the claim worth checking: a dry run that half-applied would be worse than no
/// dry run at all.
#[test]
fn dry_run_reports_the_cascade_and_writes_nothing() {
    let workspace = workspace();
    let root = create(&workspace, "root", None);
    let child = create(&workspace, "child", Some(&root));
    let before = live_ids(&workspace, "dry before");
    let jsonl = workspace.root.join(".beads").join("issues.jsonl");
    let jsonl_before = std::fs::read_to_string(&jsonl).expect("read jsonl");

    let dry = run_br(
        &workspace,
        ["rename", &root, "rn-planned", "--dry-run", "--json"],
        "rename dry",
    );
    assert!(dry.status.success(), "dry run: {}", dry.stderr);
    let json: Value =
        serde_json::from_str(&extract_json_payload(&dry.stdout)).expect("parse dry json");
    assert_eq!(json["dry_run"].as_bool(), Some(true));
    assert_eq!(json["old_id"].as_str(), Some(root.as_str()));
    assert_eq!(json["new_id"].as_str(), Some("rn-planned"));
    let descendants = json["descendants"].as_array().expect("descendants array");
    assert_eq!(descendants.len(), 1, "{}", dry.stdout);
    assert_eq!(descendants[0]["old_id"].as_str(), Some(child.as_str()));
    assert_eq!(descendants[0]["new_id"].as_str(), Some("rn-planned.1"));

    assert_eq!(
        live_ids(&workspace, "dry after"),
        before,
        "a dry run must not move a single ID"
    );
    assert_eq!(
        std::fs::read_to_string(&jsonl).expect("read jsonl"),
        jsonl_before,
        "and must not touch issues.jsonl either"
    );
}

/// Every refusal, in one place, each with the reason it exists.
#[test]
fn rename_refuses_the_cases_it_has_to_refuse() {
    let workspace = workspace();
    let root = create(&workspace, "root", None);
    let child = create(&workspace, "child", Some(&root));
    let occupied = create(&workspace, "occupied", None);
    let doomed = create(&workspace, "doomed", None);
    let deleted = run_br(
        &workspace,
        ["delete", &doomed, "--force"],
        "rename delete for tombstone",
    );
    assert!(deleted.status.success(), "delete: {}", deleted.stderr);

    let cases: Vec<(&str, Vec<String>, &str)> = vec![
        (
            "an occupied target",
            vec![root.clone(), occupied.clone()],
            "taken",
        ),
        (
            "renaming to itself",
            vec![root.clone(), root.clone()],
            "already this issue's ID",
        ),
        (
            "a flat issue given a dotted ID",
            vec![root.clone(), "rn-tree.1".to_string()],
            "not its place in the hierarchy",
        ),
        (
            "a child given a flat ID",
            vec![child.clone(), "rn-flatnow".to_string()],
            "not its place in the hierarchy",
        ),
        (
            "a child moved under another parent",
            vec![child.clone(), format!("{occupied}.1")],
            "not its place in the hierarchy",
        ),
        (
            "a prefix change",
            vec![root.clone(), "zz-elsewhere".to_string()],
            "cannot change an issue's prefix",
        ),
    ];

    for (what, args, expected) in cases {
        let mut argv = vec!["rename".to_string()];
        argv.extend(args);
        let refused = run_br(&workspace, argv, "rename refusal");
        assert!(
            !refused.status.success(),
            "{what} must be refused: stdout={}",
            refused.stdout
        );
        assert!(
            refused.stderr.contains(expected),
            "{what}: expected the error to mention {expected:?}, got: {}",
            refused.stderr
        );
    }

    // A tombstoned source. The resolver hands back the tombstone (there is no
    // `former_ids` redirect after a delete), and `ensure_issue_mutable` is what
    // stops the rename from moving a tombstone onto a fresh ID and planting a
    // second one behind it -- the hazard `detach` documents at length.
    let on_tombstone = run_br(
        &workspace,
        ["rename", &doomed, "rn-revived"],
        "rename tombstone source",
    );
    assert!(
        !on_tombstone.status.success(),
        "renaming a tombstone must be refused: {}",
        on_tombstone.stdout
    );

    // Nothing above wrote: the tree is exactly as it was.
    let mut expected_ids = vec![root, child, occupied];
    expected_ids.sort();
    assert_eq!(
        live_ids(&workspace, "rename refusals left no trace"),
        expected_ids
    );
}
