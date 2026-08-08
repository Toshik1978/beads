//! bds-lf1: the date-range flags as a user meets them.
//!
//! The storage-level bounds are pinned in `tests/storage/date_range_filters.rs`.
//! What is left to check here is the wiring nobody would notice was missing: the
//! flags exist on both commands, they reach `ListFilters` rather than being
//! parsed and dropped, a bad value is rejected instead of silently ignored, and
//! `--closed-after` does not hand back an empty list because the default view
//! hides the only rows it can ever match.

use crate::common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

fn workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "bd"], "date flags init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    workspace
}

fn create(workspace: &BrWorkspace, title: &str) -> String {
    let created = run_br(workspace, ["create", title], "date flags create");
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

fn ids(stdout: &str) -> Vec<String> {
    let payload = extract_json_payload(stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");
    let rows = json
        .get("issues")
        .and_then(Value::as_array)
        .or_else(|| json.as_array())
        .cloned()
        .unwrap_or_default();
    rows.iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// Everything created just now is inside a window that starts yesterday, and
/// outside one that ended yesterday. Two assertions rather than one, because a
/// flag that parsed and was then dropped would pass the first on its own.
#[test]
fn list_applies_created_and_updated_windows() {
    let workspace = workspace();
    let id = create(&workspace, "fresh issue");

    let inside = run_br(
        &workspace,
        ["list", "--created-after=-1d", "--json"],
        "date flags inside",
    );
    assert!(inside.status.success(), "{}", inside.stderr);
    assert!(
        ids(&inside.stdout).contains(&id),
        "a window that starts yesterday has to contain something made today: {}",
        inside.stdout
    );

    let outside = run_br(
        &workspace,
        ["list", "--created-before=-1d", "--json"],
        "date flags outside",
    );
    assert!(outside.status.success(), "{}", outside.stderr);
    assert!(
        !ids(&outside.stdout).contains(&id),
        "a window that ended yesterday must not contain it: {}",
        outside.stdout
    );

    let updated = run_br(
        &workspace,
        ["list", "--updated-before=-1d", "--json"],
        "date flags updated",
    );
    assert!(updated.status.success(), "{}", updated.stderr);
    assert!(!ids(&updated.stdout).contains(&id), "{}", updated.stdout);
}

/// The same flags on `search`. They ride on the shared `ListArgs`, so this is
/// really asserting that nothing in the search path drops them.
#[test]
fn search_applies_the_same_windows() {
    let workspace = workspace();
    let id = create(&workspace, "findable haystack");

    let inside = run_br(
        &workspace,
        ["search", "haystack", "--created-after=-1d", "--json"],
        "date flags search inside",
    );
    assert!(inside.status.success(), "{}", inside.stderr);
    assert!(ids(&inside.stdout).contains(&id), "{}", inside.stdout);

    let outside = run_br(
        &workspace,
        ["search", "haystack", "--created-before=-1d", "--json"],
        "date flags search outside",
    );
    assert!(outside.status.success(), "{}", outside.stderr);
    assert!(!ids(&outside.stdout).contains(&id), "{}", outside.stdout);
}

/// `--closed-after` implies `--all`.
///
/// Without that, the flag is satisfiable only by rows the default view hides, so
/// every invocation returns nothing — and returns it successfully, which is the
/// worst way for a filter to be broken.
#[test]
fn a_closed_bound_does_not_need_all_to_be_passed_as_well() {
    let workspace = workspace();
    let open = create(&workspace, "stays open");
    let closed = create(&workspace, "gets closed");
    let close = run_br(
        &workspace,
        ["close", &closed, "--reason", "done"],
        "date flags close",
    );
    assert!(close.status.success(), "{}", close.stderr);

    let listed = run_br(
        &workspace,
        ["list", "--closed-after=-1d", "--json"],
        "date flags closed window",
    );
    assert!(listed.status.success(), "{}", listed.stderr);
    let got = ids(&listed.stdout);
    assert!(
        got.contains(&closed),
        "the closed issue is the only thing this filter can match: {}",
        listed.stdout
    );
    assert!(
        !got.contains(&open),
        "and an issue with no closed_at must not come along: {}",
        listed.stdout
    );
}

#[test]
fn an_unparseable_bound_is_rejected_rather_than_ignored() {
    let workspace = workspace();
    create(&workspace, "anything");

    for command in ["list", "search"] {
        let mut args = vec![command];
        if command == "search" {
            args.push("anything");
        }
        args.extend(["--updated-after=last-fortnight"]);
        let rejected = run_br(&workspace, args, "date flags bad value");
        assert!(
            !rejected.status.success(),
            "{command} accepted a value it cannot have understood: {}",
            rejected.stdout
        );
        assert!(
            rejected.stderr.contains("updated_after"),
            "the error has to name the flag that was wrong: {}",
            rejected.stderr
        );
    }
}
