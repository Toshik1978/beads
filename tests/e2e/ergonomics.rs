//! bds-yo8: four small consistency gaps in the CLI surface.
//!
//! `br close --reason-file`, `br close --continue`, `br update --append-notes`,
//! and `br create --notes` / `--acceptance`. Independently shippable and tested
//! independently, except where they interact — the `--continue` exit code is the
//! one with a real contract, and it gets most of the file.
//!
//! The fifth item on that bead, `br ready --explain`, was declined in review as
//! a feature wearing a leftover's clothes rather than implemented here.

use crate::common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::fs;

fn workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "eg"], "ergo init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    workspace
}

fn create(workspace: &BrWorkspace, args: &[&str]) -> String {
    let mut argv = vec!["create"];
    argv.extend_from_slice(args);
    let created = run_br(workspace, argv, "ergo create");
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

fn record(workspace: &BrWorkspace, id: &str, label: &str) -> Value {
    let shown = run_br(workspace, ["show", id, "--json"], label);
    assert!(shown.status.success(), "show: {}", shown.stderr);
    let json: Value =
        serde_json::from_str(&extract_json_payload(&shown.stdout)).expect("parse show json");
    json.as_array()
        .and_then(|rows| rows.first())
        .cloned()
        .unwrap_or(json)
}

/// `--notes` and `--acceptance` at creation. Both fields already existed on
/// `Issue` and were settable only by a follow-up `br update`, so a
/// fully-populated issue cost two commands and two `updated_at` bumps.
#[test]
fn create_sets_notes_and_acceptance_criteria_in_one_command() {
    let workspace = workspace();
    let id = create(
        &workspace,
        &[
            "populated",
            "--notes",
            "watch the cache",
            "--acceptance",
            "the suite is green",
        ],
    );

    let issue = record(&workspace, &id, "ergo show created");
    assert_eq!(issue["notes"].as_str(), Some("watch the cache"));
    assert_eq!(
        issue["acceptance_criteria"].as_str(),
        Some("the suite is green")
    );
    assert_eq!(
        issue["created_at"], issue["updated_at"],
        "setting these at creation must not cost a second write: {issue}"
    );
}

/// `--append-notes` adds a paragraph instead of replacing the field, and is
/// repeatable. The blank-line separator is the documented behaviour, not an
/// accident of formatting.
#[test]
fn append_notes_adds_paragraphs_instead_of_replacing_them() {
    let workspace = workspace();
    let id = create(&workspace, &["notes subject", "--notes", "first"]);

    for addition in ["second", "third"] {
        let appended = run_br(
            &workspace,
            ["update", &id, "--append-notes", addition],
            "ergo append",
        );
        assert!(appended.status.success(), "append: {}", appended.stderr);
    }

    assert_eq!(
        record(&workspace, &id, "ergo show appended")["notes"].as_str(),
        Some("first\n\nsecond\n\nthird"),
        "each append is a new paragraph, in order"
    );

    // Appending to an empty field does not lead with blank lines.
    let bare = create(&workspace, &["bare notes"]);
    let appended = run_br(
        &workspace,
        ["update", &bare, "--append-notes", "only"],
        "ergo append bare",
    );
    assert!(appended.status.success(), "{}", appended.stderr);
    assert_eq!(
        record(&workspace, &bare, "ergo show bare")["notes"].as_str(),
        Some("only")
    );

    // And it is mutually exclusive with the replacing form, so a caller cannot
    // ask for both and get whichever the implementation happens to apply last.
    let both = run_br(
        &workspace,
        ["update", &id, "--notes", "x", "--append-notes", "y"],
        "ergo append conflict",
    );
    assert!(!both.status.success(), "{}", both.stdout);
}

/// `--reason-file`, mirroring `br create --description-file`. The point is
/// multi-paragraph reasons that are fragile to pass through shell quoting, so the
/// test uses one and asserts the content survives verbatim.
#[test]
fn close_reads_a_reason_from_a_file_verbatim() {
    let workspace = workspace();
    let id = create(&workspace, &["file reason"]);
    let reason_path = workspace.root.join("reason.md");
    let reason = "Fixed by reverting #412.\n\n- the cache was warm\n- \"quoted\" and $unexpanded\n";
    fs::write(&reason_path, reason).expect("write reason");

    let closed = run_br(
        &workspace,
        [
            "close",
            &id,
            "--reason-file",
            reason_path.to_str().expect("utf-8 path"),
        ],
        "ergo close reason file",
    );
    assert!(closed.status.success(), "close: {}", closed.stderr);
    assert_eq!(
        record(&workspace, &id, "ergo show closed")["close_reason"].as_str(),
        Some(reason),
        "the file content is used unchanged, newlines and quoting included"
    );

    // Mutually exclusive with the inline form.
    let other = create(&workspace, &["conflicting reason"]);
    let both = run_br(
        &workspace,
        [
            "close",
            &other,
            "--reason",
            "inline",
            "--reason-file",
            reason_path.to_str().expect("utf-8 path"),
        ],
        "ergo close reason conflict",
    );
    assert!(!both.status.success(), "{}", both.stdout);
}

/// Without `--continue`, one unresolvable ID fails the command before any issue
/// is touched. That is the behaviour `--continue` exists to change, so it is
/// asserted here rather than assumed — if the default ever became tolerant,
/// `--continue` would be redundant and this should say so.
#[test]
fn a_batch_close_still_aborts_on_a_bad_id_without_continue() {
    let workspace = workspace();
    let good = create(&workspace, &["survivor"]);

    let refused = run_br(
        &workspace,
        ["close", &good, "eg-nosuch"],
        "ergo close no continue",
    );
    assert!(!refused.status.success(), "{}", refused.stdout);
    assert_eq!(
        record(&workspace, &good, "ergo show untouched")["status"].as_str(),
        Some("open"),
        "the abort has to happen before anything is closed"
    );
}

/// `--continue` closes what it can, reports what it could not, and exits
/// non-zero — with `PARTIALLY_COMPLETED` rather than `NOTHING_TO_DO`, because
/// something did happen.
#[test]
fn continue_closes_the_rest_and_reports_the_failure() {
    let workspace = workspace();
    let first = create(&workspace, &["first"]);
    let second = create(&workspace, &["second"]);

    let run = run_br(
        &workspace,
        [
            "close",
            &first,
            "eg-nosuch",
            &second,
            "--continue",
            "--json",
        ],
        "ergo close continue",
    );
    assert!(
        !run.status.success(),
        "one ID failed, so the exit code has to say so: {}",
        run.stdout
    );

    for id in [&first, &second] {
        assert_eq!(
            record(&workspace, id, "ergo show continued")["status"].as_str(),
            Some("closed"),
            "the resolvable IDs still close"
        );
    }

    // Two JSON documents come back on stdout: the close result, then the error.
    // Read them as a stream rather than guessing at line boundaries -- the error
    // document is pretty-printed and spans many lines.
    let documents: Vec<Value> = serde_json::Deserializer::from_str(&run.stdout)
        .into_iter::<Value>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap_or_else(|error| panic!("parse stdout as a JSON stream ({error}): {}", run.stdout));

    let result = documents
        .iter()
        .find(|value| value.get("closed").is_some())
        .unwrap_or_else(|| panic!("no close result document: {}", run.stdout));
    assert_eq!(
        result["closed"].as_array().map(Vec::len),
        Some(2),
        "{}",
        run.stdout
    );
    assert_eq!(
        result["skipped"][0]["id"].as_str(),
        Some("eg-nosuch"),
        "the failure has to be reported by ID, not merely counted: {}",
        run.stdout
    );

    let failure = documents
        .iter()
        .find(|value| value.get("error").is_some())
        .unwrap_or_else(|| panic!("no error document: {}", run.stdout));
    assert_eq!(
        failure["error"]["code"].as_str(),
        Some("PARTIALLY_COMPLETED"),
        "not NOTHING_TO_DO: two issues closed, so 'nothing to do' would be a lie. \
         stdout={}",
        run.stdout
    );
}

/// The contract that makes `--continue` usable in a script: re-running a batch
/// that half-succeeded exits 0, because "already closed" is not a failure.
///
/// This is where `--continue` deliberately *replaces* the default exit-code rule
/// rather than adding to it — under the default, a batch where everything is
/// already closed is an error.
#[test]
fn continue_is_idempotent_over_an_already_closed_batch() {
    let workspace = workspace();
    let first = create(&workspace, &["first"]);
    let second = create(&workspace, &["second"]);

    let first_run = run_br(
        &workspace,
        ["close", &first, &second, "--continue"],
        "ergo close continue first",
    );
    assert!(first_run.status.success(), "{}", first_run.stderr);

    let second_run = run_br(
        &workspace,
        ["close", &first, &second, "--continue"],
        "ergo close continue again",
    );
    assert!(
        second_run.status.success(),
        "a re-run over an already-closed batch has to exit 0, or --continue is \
         unusable in a retry loop: {}",
        second_run.stdout
    );

    // Contrast: the default rule treats the same situation as an error. Pinned so
    // that the difference is a decision on the record rather than a surprise.
    let without = run_br(
        &workspace,
        ["close", &first, &second],
        "ergo close no continue again",
    );
    assert!(
        !without.status.success(),
        "the default rule is unchanged: nothing closed means an error"
    );
}
