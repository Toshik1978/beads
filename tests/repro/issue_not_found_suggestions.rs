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

/// The number of positions at which two IDs differ, or `None` when their
/// lengths differ at all.
///
/// For equal-length strings, edit distance 1 means exactly one substitution --
/// an insertion and a deletion together already cost 2 -- so two or more
/// differing positions proves an ID is beyond `SUGGESTION_MAX_DISTANCE`
/// without reimplementing Levenshtein in the test. Unequal lengths carry no
/// such proof, so they answer `None` and every caller treats that as "not
/// proven distant".
fn differing_positions(left: &str, right: &str) -> Option<usize> {
    (left.len() == right.len()).then(|| {
        left.chars()
            .zip(right.chars())
            .filter(|(left, right)| left != right)
            .count()
    })
}

/// Three issues, and a typo that is a near miss of exactly one of them.
struct Fixture {
    workspace: BrWorkspace,
    /// `ids[0]` is the near miss; the rest are provably too far to be offered.
    ids: Vec<String>,
    typo: String,
}

/// The characters a beads ID hash is drawn from, and therefore the only ones a
/// typo may use: an ID that is not well-formed fails validation before the
/// suggester ever runs, which would test something else entirely.
const ID_HASH_ALPHABET: &str = "0123456789abcdefghijklmnopqrstuvwxyz";

/// Creates three issues and picks a single-character typo of one of them that
/// no *other* ID sits within one edit of.
///
/// Every test below assumes such a typo has exactly one near miss, and that
/// assumption used to be a bet. IDs in one workspace share a prefix and end in
/// a three-character base-36 hash, so whether two of them land a single edit
/// apart is luck: about 1 in 218 per fixture, which across the three tests
/// that use it is roughly one full-suite run in 73. When the bet lost, the
/// suite went red with the product behaving perfectly -- `find_similar_ids`
/// had found two IDs one edit from the typo and honestly reported both, in the
/// plural form the singular assertion did not expect.
///
/// Searching the 3 targets x 35 substitutions for a typo that satisfies the
/// precondition costs nothing and takes the odds to about 1 in 670,000
/// (measured by simulating the hash draw, not derived -- the three candidate
/// targets share the same three IDs, so their outcomes correlate and the
/// closed form overstates the improvement). The remaining case fails by naming
/// the precondition rather than looking like a suggestion bug.
fn workspace_with_issues() -> Fixture {
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    let mut ids: Vec<String> = ["Alpha", "Beta", "Gamma"]
        .into_iter()
        .map(|title| {
            run_br(&workspace, ["create", title, "--silent"], "create")
                .stdout
                .trim()
                .to_string()
        })
        .collect();

    let (target, typo) = (0..ids.len())
        .find_map(|candidate| {
            mistypings(&ids[candidate])
                .find(|typo| {
                    ids.iter().enumerate().all(|(other, id)| {
                        other == candidate
                            || differing_positions(id, typo).is_some_and(|count| count >= 2)
                    })
                })
                .map(|typo| (candidate, typo))
        })
        .unwrap_or_else(|| {
            panic!("no single-character typo of any of {ids:?} is a near miss of just one of them")
        });
    ids.swap(0, target);

    Fixture {
        workspace,
        ids,
        typo,
    }
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

/// Every one-wrong-character-in-the-last-position misspelling of `id` -- the
/// commonest way to mistype an ID copied by hand. Exactly one substitution, so
/// each is at edit distance 1 from `id` and the suggester must offer it back.
fn mistypings(id: &str) -> impl Iterator<Item = String> + '_ {
    let stem = &id[..id.len() - id.chars().next_back().map_or(0, char::len_utf8)];
    ID_HASH_ALPHABET
        .chars()
        .filter(move |candidate| Some(*candidate) != id.chars().next_back())
        .map(move |candidate| format!("{stem}{candidate}"))
}

#[test]
fn a_mistyped_id_suggests_the_near_miss_in_text_output() {
    let Fixture {
        workspace,
        ids,
        typo,
    } = workspace_with_issues();

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
    let Fixture {
        workspace,
        ids,
        typo,
    } = workspace_with_issues();

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
///
/// `workspace_with_issues` is what gives the assertion its teeth: it picks a
/// target whose typo the other two IDs are provably two or more edits from, so
/// "not suggested" tests the bound rather than testing the dice. A revert to
/// `<= 3` would put both of them back in range and fail here.
#[test]
fn unrelated_ids_are_not_offered_as_suggestions() {
    let Fixture {
        workspace,
        ids,
        typo,
    } = workspace_with_issues();

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
    let Fixture { workspace, .. } = workspace_with_issues();

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
