//! bds-3rt: the exclusion flags as a user meets them.
//!
//! The semantics are pinned in `tests/storage/exclusion_filters.rs`. What is
//! checked here is that the flags exist on all three commands and actually
//! reach the query — the failure mode being a flag that parses, does nothing,
//! and exits 0, which no amount of storage-level testing would catch.
//!
//! Also pinned: neither `--no-assignee` nor `--unassigned` exists.
//! `assignee` and its whole query surface were removed from `Issue`
//! (bds-b4f.2.6), and the decision not to ship a `--no-assignee` synonym for
//! a filter that no longer exists either is worth a test, because the
//! natural drift is for someone to add one back later for symmetry with the
//! other three `--no-*` flags.

use crate::common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

fn workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "bd"], "exclude init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    workspace
}

fn create(workspace: &BrWorkspace, title: &str, extra: &[&str]) -> String {
    let mut args = vec!["create", title];
    args.extend_from_slice(extra);
    let created = run_br(workspace, args, "exclude create");
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
    let mut out: Vec<String> = rows
        .iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    out.sort();
    out
}

/// One workspace, four rows, all three commands. Each command is asked the same
/// question and has to give the same answer.
#[test]
fn every_exclusion_flag_reaches_list_search_and_ready() {
    let workspace = workspace();
    let plain = create(&workspace, "keepme plain", &[]);
    let noisy = create(&workspace, "keepme noisy", &["--labels", "wontfix"]);
    let bug = create(
        &workspace,
        "keepme bug",
        &["--type", "bug", "--labels", "x"],
    );
    let child = create(&workspace, "keepme child", &["--parent", &plain]);

    let cases: Vec<(&str, Vec<&str>, Vec<&String>)> = vec![
        (
            "--exclude-label",
            vec!["--exclude-label", "wontfix"],
            vec![&plain, &bug, &child],
        ),
        ("--no-labels", vec!["--no-labels"], vec![&plain, &child]),
        (
            "--exclude-type",
            vec!["--exclude-type", "bug"],
            vec![&plain, &noisy, &child],
        ),
        (
            "--no-parent",
            vec!["--no-parent"],
            vec![&plain, &noisy, &bug],
        ),
    ];

    for (label, flags, expected) in cases {
        let mut expected: Vec<String> = expected.into_iter().cloned().collect();
        expected.sort();

        for command in ["list", "search", "ready"] {
            let mut args = vec![command];
            if command == "search" {
                args.push("keepme");
            }
            args.extend(flags.iter().copied());
            args.push("--json");
            let run = run_br(&workspace, args, "exclude filter");
            assert!(run.status.success(), "{command} {label}: {}", run.stderr);
            assert_eq!(
                ids(&run.stdout),
                expected,
                "{command} did not apply {label}: {}",
                run.stdout
            );
        }
    }
}

/// Repeated values mean "none of these". This is the reading a user gets, and it
/// is the opposite of `--label`'s AND — so it is worth stating at the CLI level
/// too rather than only against the storage struct.
#[test]
fn repeated_exclude_label_means_neither() {
    let workspace = workspace();
    let neither = create(&workspace, "keepme neither", &[]);
    create(&workspace, "keepme alpha", &["--labels", "alpha"]);
    create(&workspace, "keepme beta", &["--labels", "beta"]);

    let run = run_br(
        &workspace,
        [
            "list",
            "--exclude-label",
            "alpha",
            "--exclude-label",
            "beta",
            "--json",
        ],
        "exclude repeated",
    );
    assert!(run.status.success(), "{}", run.stderr);
    assert_eq!(ids(&run.stdout), vec![neither], "{}", run.stdout);
}

/// An unfamiliar `--exclude-type` value is accepted and excludes nothing.
///
/// That is not laxness in the exclusion, it is `IssueType::Custom`: this fork
/// lets a project use type names it does not ship, so no vocabulary exists
/// against which "unknown" could be decided. `--exclude-type` therefore behaves
/// exactly like `--type` on the same value, and this test asserts that symmetry
/// rather than a rejection — if `--type` ever starts rejecting, this should fail
/// and be brought along with it. (`bds-b4h`'s `br types` is where that
/// vocabulary becomes askable.)
#[test]
fn an_unfamiliar_excluded_type_behaves_like_the_positive_form() {
    let workspace = workspace();
    let id = create(&workspace, "keepme anything", &[]);

    let excluded = run_br(
        &workspace,
        ["list", "--exclude-type", "epic-ish", "--json"],
        "exclude custom type",
    );
    assert!(
        excluded.status.success(),
        "no vocabulary exists to reject against: {}",
        excluded.stderr
    );
    assert_eq!(
        ids(&excluded.stdout),
        vec![id.clone()],
        "excluding a type nothing has must exclude nothing: {}",
        excluded.stdout
    );

    let selected = run_br(
        &workspace,
        ["list", "--type", "epic-ish", "--json"],
        "select custom type",
    );
    assert!(selected.status.success(), "{}", selected.stderr);
    assert!(
        ids(&selected.stdout).is_empty(),
        "and selecting it must select nothing, which is the same claim from the \
         other side: {}",
        selected.stdout
    );
}

/// Neither `--no-assignee` nor `--unassigned` is a flag here: `assignee` and
/// its whole query surface were removed from `Issue` (bds-b4f.2.6), and
/// shipping a `--no-assignee` synonym for a filter that no longer exists
/// would be a synonym to maintain forever — see `ExclusionArgs`.
#[test]
fn no_assignee_is_deliberately_not_a_flag() {
    let workspace = workspace();
    create(&workspace, "anything", &[]);

    let rejected = run_br(&workspace, ["list", "--no-assignee"], "exclude synonym");
    assert!(
        !rejected.status.success(),
        "--no-assignee must not exist; assignee's whole query surface is gone"
    );

    let also_rejected = run_br(&workspace, ["list", "--unassigned", "--json"], "unassigned");
    assert!(
        !also_rejected.status.success(),
        "--unassigned must not exist either; assignee's whole query surface is gone"
    );
}
