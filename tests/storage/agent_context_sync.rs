//! Tests for `Issue::sync_equals` visibility into `agent_context`.
//!
//! Mirrors `tests/storage/rename.rs`'s `former_ids` coverage for the
//! identical bug (bds-a23.6, then bds-a23.15): `agent_context` is
//! `#[serde]`-exported but was not compared by `sync_equals`, so an
//! `agent_context`-only delta between a local row and an incoming JSONL
//! record read as "equal" and `merge_issue`'s Case 6 (`src/sync/mod.rs`)
//! short-circuited to `Keep(left)` before ever checking which side actually
//! changed, silently dropping the incoming value.

use crate::common;

use beads::storage::IssueUpdate;
use beads::sync::{ExportConfig, ImportConfig, export_to_jsonl, import_from_jsonl};
use common::{fixtures, test_db};
use tempfile::TempDir;

/// bds-a23.15: direct `three_way_merge` coverage, mirroring
/// `merging_a_former_ids_only_change_over_an_existing_row_propagates_it`
/// (`tests/storage/rename.rs`). Base and left (local) are identical; only
/// right (incoming) carries the `agent_context` change — exactly the shape
/// `sync_equals` has to get right for `merge_issue`'s Case 6 to keep the
/// incoming side instead of reading the two as equal and keeping local.
#[test]
fn merging_an_agent_context_only_change_over_an_existing_row_propagates_it() {
    let mut base = fixtures::issue("bd-f");
    base.id = "bd-f".to_string();

    let left = base.clone(); // local DB: unchanged since base

    let mut right = base.clone(); // incoming JSONL: agent_context was set elsewhere
    right.agent_context = Some(r#"{"constraints":["no direct SQL"]}"#.to_string());

    let mut base_map = std::collections::HashMap::new();
    base_map.insert(base.id.clone(), base.clone());
    let mut left_map = std::collections::HashMap::new();
    left_map.insert(left.id.clone(), left.clone());
    let mut right_map = std::collections::HashMap::new();
    right_map.insert(right.id.clone(), right.clone());

    let context = beads::sync::MergeContext::new(base_map, left_map, right_map);
    let report =
        beads::sync::three_way_merge(&context, beads::sync::ConflictResolution::PreferNewer, None);

    assert!(
        report.conflicts.is_empty(),
        "an agent_context-only delta must not read as a conflict; got {:?}",
        report.conflicts
    );
    let kept = report
        .kept
        .iter()
        .find(|issue| issue.id == "bd-f")
        .expect("bd-f must be kept by the merge");
    assert_eq!(
        kept.agent_context.as_deref(),
        Some(r#"{"constraints":["no direct SQL"]}"#),
        "the incoming agent_context must survive the merge over the \
         existing row, not be silently dropped as 'no change'"
    );
}

/// bds-a23.15: full-pipeline regression check, mirroring
/// `rename_export_import_over_an_existing_row_propagates_former_ids`
/// (`tests/storage/rename.rs`) in composition shape. Uses the real
/// production write path for `agent_context` -- `storage.update_issue` with
/// `IssueUpdate.agent_context` (`src/storage/sqlite.rs`'s
/// `update_issue_in_tx`) -- rather than a hand-set struct field. Source
/// clone sets `agent_context` through that real path, exports, and the
/// target clone (holding the pre-change row at the same id) imports the
/// JSONL over it.
///
/// Unlike its `former_ids` model, **this test does not discriminate the
/// `sync_equals` fix above**: `import_from_jsonl` never calls
/// `sync_equals`/`merge_issue` at all -- `determine_action`
/// (`src/sync/mod.rs`) is a plain last-write-wins comparison on
/// `updated_at`. It was verified to pass identically with the
/// `sync_equals` widening reverted, because `update_issue_in_tx` already
/// bumps `updated_at` unconditionally for every field it writes, including
/// `agent_context` -- confirmed by reading that function
/// (`src/storage/sqlite.rs:3577-3579`), unlike `former_ids`'s
/// `record_rename_provenance`, which needed a companion fix to do the same.
/// Kept anyway as a regression guard on the real write path end to end; the
/// discriminating coverage for the `sync_equals` gap itself is
/// `merging_an_agent_context_only_change_over_an_existing_row_propagates_it`
/// above.
#[test]
fn agent_context_export_import_over_an_existing_row_propagates_it() {
    // Source clone: create the issue, then set agent_context through the
    // real update path -- nothing about updated_at is hand-set.
    let mut source = test_db();
    let mut issue = fixtures::issue("bd-f");
    issue.id = "bd-f".to_string();
    source.create_issue(&issue, "tester").unwrap();
    let pre_change = source.get_issue("bd-f").unwrap().expect("exists");

    source
        .update_issue(
            "bd-f",
            &IssueUpdate {
                agent_context: Some(Some(r#"{"skills":["rust"]}"#.to_string())),
                ..Default::default()
            },
            "tester",
        )
        .unwrap();

    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    export_to_jsonl(&source, &path, &ExportConfig::default()).unwrap();

    // Target clone: already has a live row at "bd-f" holding the
    // pre-change content -- the "existing row" this import must update in
    // place, matched by id, not inserted.
    let mut target = test_db();
    target.create_issue(&pre_change, "tester").unwrap();

    let result =
        import_from_jsonl(&mut target, &path, &ImportConfig::default(), Some("bd-")).unwrap();

    assert_eq!(
        result.updated_count, 1,
        "the matched existing row at bd-f must be updated, not skipped or \
         inserted-around; got {result:?}"
    );

    let moved = target.get_issue("bd-f").unwrap().expect("exists");
    assert_eq!(
        moved.agent_context.as_deref(),
        Some(r#"{"skills":["rust"]}"#),
        "the agent_context set through the real update path must survive \
         import over an existing row holding the pre-change state"
    );
}
