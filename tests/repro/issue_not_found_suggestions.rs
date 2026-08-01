// bds-k7e. A mistyped issue ID produced a flat "Issue not found" and the
// generic "Run 'br list'" hint, while `find_similar_ids` and
// `StructuredError::issue_not_found` -- written to turn exactly this into a
// "Did you mean ...?" -- sat in the tree with no caller at all.
//
// Two things had to change together. The suggester is now wired at the render
// boundary in `handle_error`, and its distance bound scales with the ID being
// searched for: the old hardcoded `<= 3` matched every ID in a workspace,
// because beads IDs differ only in a short random suffix.

use crate::common;
use common::cli::{BrRun, BrWorkspace, run_br};

/// Creates three issues and returns their IDs.
fn workspace_with_issues() -> (BrWorkspace, Vec<String>) {
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    let ids = ["Alpha", "Beta", "Gamma"]
        .into_iter()
        .map(|title| {
            run_br(&workspace, ["create", title, "--silent"], "create")
                .stdout
                .trim()
                .to_string()
        })
        .collect();

    (workspace, ids)
}

/// Runs `br` and asserts it failed, which every test here relies on.
fn run_br_failing<const N: usize>(workspace: &BrWorkspace, args: [&str; N], label: &str) -> BrRun {
    let run = run_br(workspace, args, label);
    assert!(
        !run.status.success(),
        "expected `br {}` to fail: {}",
        args.join(" "),
        run.stdout
    );
    run
}

/// One wrong character in the last position -- the commonest way to mistype an
/// ID copied by hand.
fn mistype(id: &str) -> String {
    let mut typo = id.to_string();
    typo.pop();
    typo.push(if id.ends_with('x') { 'y' } else { 'x' });
    typo
}

#[test]
fn a_mistyped_id_suggests_the_near_miss_in_text_output() {
    let (workspace, ids) = workspace_with_issues();
    let typo = mistype(&ids[0]);

    let run = run_br_failing(&workspace, ["show", &typo], "show_typo");

    assert!(
        run.stderr.contains(&format!("Did you mean '{}'?", ids[0])),
        "expected a suggestion naming {}, got: {}",
        ids[0],
        run.stderr
    );
}

#[test]
fn a_mistyped_id_suggests_the_near_miss_in_json_output() {
    let (workspace, ids) = workspace_with_issues();
    let typo = mistype(&ids[0]);

    let run = run_br_failing(&workspace, ["--json", "show", &typo], "show_typo_json");
    let payload: serde_json::Value =
        serde_json::from_str(run.stdout.trim()).expect("json error envelope on stdout");

    let error = &payload["error"];
    assert_eq!(error["code"], "ISSUE_NOT_FOUND");
    assert_eq!(error["context"]["searched_id"], typo);
    assert_eq!(
        error["context"]["similar_ids"],
        serde_json::json!([ids[0]]),
        "only the near miss should be suggested, got: {}",
        error["context"]["similar_ids"]
    );
}

/// The guard against the old hardcoded threshold. With `distance <= 3` and a
/// 3-character suffix, every ID in the workspace was a candidate, so this
/// asserts the *absence* of the other two issues as much as the presence of
/// the right one.
#[test]
fn unrelated_ids_are_not_offered_as_suggestions() {
    let (workspace, ids) = workspace_with_issues();
    let typo = mistype(&ids[0]);

    let run = run_br_failing(&workspace, ["--json", "show", &typo], "show_typo_json");
    let payload: serde_json::Value =
        serde_json::from_str(run.stdout.trim()).expect("json error envelope on stdout");
    let similar = payload["error"]["context"]["similar_ids"]
        .as_array()
        .expect("similar_ids array");

    for unrelated in &ids[1..] {
        assert!(
            !similar.iter().any(|value| value == unrelated),
            "{unrelated} is a different issue, not a misspelling of {typo}"
        );
    }
}

/// The bead is explicit that the fallback must be preserved, not replaced.
#[test]
fn an_id_with_no_near_match_keeps_the_br_list_hint() {
    let (workspace, _ids) = workspace_with_issues();

    let run = run_br_failing(&workspace, ["--json", "show", "zzz-999"], "show_unrelated");
    let payload: serde_json::Value =
        serde_json::from_str(run.stdout.trim()).expect("json error envelope on stdout");

    assert_eq!(
        payload["error"]["hint"], "Run 'br list' to see available issues.",
        "with nothing close, the original hint is what should appear"
    );
    assert_eq!(
        payload["error"]["context"]["similar_ids"],
        serde_json::json!([])
    );
}
