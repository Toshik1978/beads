//! `br merge-driver`: a three-way git merge driver for `.beads/issues.jsonl`.
//!
//! Git's line merge conflicts on neighbouring lines, and an issue per line in
//! id order puts unrelated issues next to each other, so two branches that
//! touched different beads conflicted on rebase. The driver merges by id.

use crate::common;

use beads::model::{Comment, Dependency, DependencyType, Issue, Status};
use beads::sync::jsonl_format;
use chrono::{DateTime, Duration, Utc};
use common::cli::{BrWorkspace, run_br};
use common::fixtures;
use std::path::{Path, PathBuf};
use std::process::Command;

fn issue(id: &str) -> Issue {
    let mut issue = fixtures::issue(id);
    issue.id = id.to_string();
    issue
}

fn later(issue: &Issue, seconds: i64) -> DateTime<Utc> {
    issue.updated_at + Duration::seconds(seconds)
}

fn comment(issue_id: &str, id: i64, text: &str, at: DateTime<Utc>) -> Comment {
    Comment {
        id,
        issue_id: issue_id.to_string(),
        author: "tester".to_string(),
        body: text.to_string(),
        created_at: at,
    }
}

fn write(path: &Path, issues: &[Issue]) {
    let mut body = String::new();
    for issue in issues {
        body.push_str(&jsonl_format::to_line(issue).unwrap());
        body.push('\n');
    }
    std::fs::write(path, body).unwrap();
}

fn read(path: &Path) -> Vec<Issue> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

struct Sides {
    workspace: BrWorkspace,
    base: PathBuf,
    ours: PathBuf,
    theirs: PathBuf,
}

fn sides(base: &[Issue], ours: &[Issue], theirs: &[Issue]) -> Sides {
    let workspace = BrWorkspace::new();
    let dir = workspace.root.clone();
    let sides = Sides {
        workspace,
        base: dir.join("base.jsonl"),
        ours: dir.join("ours.jsonl"),
        theirs: dir.join("theirs.jsonl"),
    };
    write(&sides.base, base);
    write(&sides.ours, ours);
    write(&sides.theirs, theirs);
    sides
}

fn merge(sides: &Sides, label: &str) -> common::cli::BrRun {
    run_br(
        &sides.workspace,
        [
            "merge-driver".as_ref(),
            sides.base.as_os_str(),
            sides.ours.as_os_str(),
            sides.theirs.as_os_str(),
        ],
        label,
    )
}

fn merged(sides: &Sides, label: &str) -> Vec<Issue> {
    let run = merge(sides, label);
    assert!(run.status.success(), "merge-driver failed: {}", run.stderr);
    read(&sides.ours)
}

fn find<'a>(issues: &'a [Issue], id: &str) -> &'a Issue {
    issues
        .iter()
        .find(|issue| issue.id == id)
        .unwrap_or_else(|| panic!("{id} missing from the merge"))
}

#[test]
fn changes_to_different_issues_merge_cleanly_in_id_order() {
    let a = issue("bd-a");
    let b = issue("bd-b");
    let mut ours_a = a.clone();
    ours_a.title = "edited on ours".to_string();
    ours_a.updated_at = later(&a, 10);
    let mut theirs_b = b.clone();
    theirs_b.title = "edited on theirs".to_string();
    theirs_b.updated_at = later(&b, 10);
    let c = issue("bd-c");

    let sides = sides(
        &[a.clone(), b.clone()],
        &[ours_a, b.clone()],
        &[c, theirs_b, a],
    );
    let result = merged(&sides, "disjoint");

    let ids: Vec<_> = result.iter().map(|issue| issue.id.as_str()).collect();
    assert_eq!(ids, ["bd-a", "bd-b", "bd-c"], "sorted by id, nothing lost");
    assert_eq!(find(&result, "bd-a").title, "edited on ours");
    assert_eq!(find(&result, "bd-b").title, "edited on theirs");
}

#[test]
fn both_sides_editing_one_issue_take_the_newer_fields() {
    let a = issue("bd-a");
    let mut ours = a.clone();
    ours.title = "older edit".to_string();
    ours.updated_at = later(&a, 10);
    let mut theirs = a.clone();
    theirs.title = "newer edit".to_string();
    theirs.updated_at = later(&a, 20);

    let sides = sides(&[a], &[ours], &[theirs]);
    let result = merged(&sides, "newer_wins");

    assert_eq!(find(&result, "bd-a").title, "newer edit");
}

/// Whole-record newest-wins would drop the older side's edit to a field the
/// newer side never touched.
#[test]
fn an_edit_to_one_field_survives_a_later_edit_to_another() {
    let a = issue("bd-a");
    let mut ours = a.clone();
    ours.title = "retitled on ours".to_string();
    ours.updated_at = later(&a, 10);
    let mut theirs = a.clone();
    theirs.status = Status::Closed;
    theirs.closed_at = Some(later(&a, 20));
    theirs.close_reason = Some("done".to_string());
    theirs.updated_at = later(&a, 20);

    let sides = sides(&[a], &[ours], &[theirs]);
    let result = merged(&sides, "field_level");

    let merged = find(&result, "bd-a");
    assert_eq!(merged.title, "retitled on ours");
    assert_eq!(merged.status, Status::Closed);
    assert_eq!(merged.close_reason.as_deref(), Some("done"));
}

/// Two sessions commenting on the same epic is the common case, and taking
/// the newer side wholesale would drop one comment. Comment ids are per-clone
/// counters, so both sides' new comment can carry the same id.
#[test]
fn comments_added_on_both_sides_are_both_kept() {
    let a = issue("bd-a");
    let mut ours = a.clone();
    ours.updated_at = later(&a, 10);
    ours.comments = vec![comment("bd-a", 1, "from ours", ours.updated_at)];
    let mut theirs = a.clone();
    theirs.updated_at = later(&a, 20);
    theirs.comments = vec![comment("bd-a", 1, "from theirs", theirs.updated_at)];

    let sides = sides(&[a], &[ours], &[theirs]);
    let result = merged(&sides, "comments");

    let texts: Vec<_> = find(&result, "bd-a")
        .comments
        .iter()
        .map(|comment| comment.body.as_str())
        .collect();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(texts.contains(&"from ours") && texts.contains(&"from theirs"));
}

#[test]
fn a_label_removed_on_one_side_stays_removed_while_the_other_adds_one() {
    let mut a = issue("bd-a");
    a.labels = vec!["keep".to_string(), "drop".to_string()];
    let mut ours = a.clone();
    ours.labels = vec!["keep".to_string()];
    ours.updated_at = later(&a, 10);
    let mut theirs = a.clone();
    theirs.labels = vec!["drop".to_string(), "keep".to_string(), "new".to_string()];
    theirs.updated_at = later(&a, 20);

    let sides = sides(&[a], &[ours], &[theirs]);
    let result = merged(&sides, "labels");

    let mut labels = find(&result, "bd-a").labels.clone();
    labels.sort();
    assert_eq!(labels, ["keep", "new"]);
}

#[test]
fn dependencies_added_on_both_sides_are_both_kept() {
    let a = issue("bd-a");
    let dep = |target: &str| Dependency {
        issue_id: "bd-a".to_string(),
        depends_on_id: target.to_string(),
        dep_type: DependencyType::Blocks,
        created_at: a.updated_at,
        created_by: None,
        metadata: None,
        thread_id: None,
    };
    let mut ours = a.clone();
    ours.dependencies = vec![dep("bd-x")];
    ours.updated_at = later(&a, 10);
    let mut theirs = a.clone();
    theirs.dependencies = vec![dep("bd-y")];
    theirs.updated_at = later(&a, 20);

    let sides = sides(
        &[a, issue("bd-x"), issue("bd-y")],
        &[ours, issue("bd-x"), issue("bd-y")],
        &[theirs, issue("bd-x"), issue("bd-y")],
    );
    let result = merged(&sides, "deps");

    let mut targets: Vec<_> = find(&result, "bd-a")
        .dependencies
        .iter()
        .map(|dep| dep.depends_on_id.as_str())
        .collect();
    targets.sort_unstable();
    assert_eq!(targets, ["bd-x", "bd-y"]);
}

/// Import never lets a JSONL record resurrect a tombstone; the driver holds
/// the same line. Newest-wins here would revive a renamed-away id next to the
/// id it was renamed to.
#[test]
fn a_tombstone_beats_a_later_edit_on_the_other_side() {
    let a = issue("bd-a");
    let mut ours = a.clone();
    ours.status = Status::Tombstone;
    ours.deleted_at = Some(later(&a, 10));
    ours.updated_at = later(&a, 10);
    let mut theirs = a.clone();
    theirs.title = "edited after the delete".to_string();
    theirs.updated_at = later(&a, 20);

    let sides = sides(
        std::slice::from_ref(&a),
        std::slice::from_ref(&ours),
        std::slice::from_ref(&theirs),
    );
    let result = merged(&sides, "tombstone_ours");
    assert_eq!(find(&result, "bd-a").status, Status::Tombstone);

    // And from the other direction.
    let sides = self::sides(&[a], &[theirs], &[ours]);
    let run = merge(&sides, "tombstone_theirs");
    assert!(run.status.success(), "{}", run.stderr);
    assert_eq!(find(&read(&sides.ours), "bd-a").status, Status::Tombstone);
    assert!(
        run.stderr.contains("bd-a"),
        "the dropped edit is reported: {}",
        run.stderr
    );
}

#[test]
fn an_input_it_cannot_trust_fails_and_leaves_ours_untouched() {
    let a = issue("bd-a");
    let all = std::slice::from_ref(&a);
    let sides = sides(all, all, all);
    std::fs::write(&sides.theirs, "{not json\n").unwrap();
    let before = std::fs::read(&sides.ours).unwrap();

    let run = merge(&sides, "malformed");

    assert!(!run.status.success(), "a malformed side must not merge");
    assert!(
        run.stderr.contains("theirs.jsonl"),
        "the error names the side it could not read: {}",
        run.stderr
    );
    assert_eq!(std::fs::read(&sides.ours).unwrap(), before);
}

#[test]
fn an_empty_base_merges_two_independent_histories() {
    let sides = sides(&[], &[issue("bd-a")], &[issue("bd-b")]);
    std::fs::write(&sides.base, "").unwrap();

    let result = merged(&sides, "empty_base");

    let ids: Vec<_> = result.iter().map(|issue| issue.id.as_str()).collect();
    assert_eq!(ids, ["bd-a", "bd-b"]);
}

/// For files `br` wrote, the merged file is what a full export of the merged
/// records writes, so the first flush after a rebase does not rewrite it. (A
/// line taken unchanged from one side keeps its bytes, so a merge does not
/// also rewrite legacy lines it had no reason to touch.)
#[test]
fn the_merged_file_is_what_a_full_export_writes() {
    let jsonl = |workspace: &BrWorkspace| workspace.root.join(".beads/issues.jsonl");
    let snapshot = |workspace: &BrWorkspace, name: &str| {
        let path = workspace.root.join(name);
        std::fs::copy(jsonl(workspace), &path).unwrap();
        path
    };
    let workspace_from = |file: &Path, label: &str| {
        let workspace = BrWorkspace::new();
        br_ok(&workspace, &["init", "--prefix", "bd"], label);
        std::fs::copy(file, jsonl(&workspace)).unwrap();
        br_ok(&workspace, &["sync", "--import-only"], label);
        workspace
    };

    let origin = BrWorkspace::new();
    br_ok(&origin, &["init", "--prefix", "bd"], "init");
    br_ok(&origin, &["create", "first", "--labels", "x"], "create");
    br_ok(&origin, &["create", "second"], "create");
    let base = snapshot(&origin, "base.jsonl");
    let ids: Vec<String> = read(&base).into_iter().map(|issue| issue.id).collect();

    br_ok(&origin, &["update", &ids[0], "--title", "ours"], "ours");
    let ours = snapshot(&origin, "ours.jsonl");

    let other = workspace_from(&base, "other");
    br_ok(&other, &["comments", "add", &ids[1], "hello"], "theirs");
    br_ok(&other, &["label", "add", &ids[0], "y"], "theirs");
    let theirs = snapshot(&other, "theirs.jsonl");

    let run = run_br(
        &origin,
        [
            "merge-driver".as_ref(),
            base.as_os_str(),
            ours.as_os_str(),
            theirs.as_os_str(),
        ],
        "merge",
    );
    assert!(run.status.success(), "{}", run.stderr);
    let merged_bytes = std::fs::read_to_string(&ours).unwrap();

    let check = workspace_from(&ours, "check");
    br_ok(&check, &["sync", "--flush-only", "--force"], "export");
    assert_eq!(
        std::fs::read_to_string(jsonl(&check)).unwrap(),
        merged_bytes
    );
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .expect("git")
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn br_ok(workspace: &BrWorkspace, args: &[&str], label: &str) {
    let run = run_br(workspace, args, label);
    assert!(run.status.success(), "br {args:?}: {}", run.stderr);
}

/// The workflow the driver exists for: a branch edited one bead, main edited
/// its neighbour, and rebasing the branch conflicted on adjacent lines.
#[test]
fn a_rebase_over_edits_to_neighbouring_issues_completes_with_the_driver() {
    let workspace = BrWorkspace::new();
    let root = workspace.root.clone();
    git_ok(&root, &["init", "-q", "-b", "main"]);
    br_ok(&workspace, &["init", "--prefix", "bd"], "init");
    for title in ["one", "two", "three"] {
        br_ok(&workspace, &["create", title], "create");
    }
    git_ok(&root, &["add", "-A"]);
    git_ok(&root, &["commit", "-qm", "base"]);

    let ids: Vec<String> = read(&root.join(".beads/issues.jsonl"))
        .into_iter()
        .map(|issue| issue.id)
        .collect();
    let (first, second) = (&ids[0], &ids[1]);

    git_ok(&root, &["checkout", "-qb", "feature"]);
    br_ok(
        &workspace,
        &["update", first, "--title", "feature edit"],
        "u1",
    );
    git_ok(&root, &["commit", "-qam", "feature"]);

    git_ok(&root, &["checkout", "-q", "main"]);
    br_ok(
        &workspace,
        &["update", second, "--title", "main edit"],
        "u2",
    );
    git_ok(&root, &["commit", "-qam", "main"]);

    git_ok(&root, &["checkout", "-q", "feature"]);
    let without = git(&root, &["rebase", "main"]);
    assert!(
        !without.status.success(),
        "the scenario must conflict without the driver, or it tests nothing"
    );
    git_ok(&root, &["rebase", "--abort"]);

    let br = assert_cmd::cargo::cargo_bin!("br");
    std::fs::write(
        root.join(".git/info/attributes"),
        ".beads/issues.jsonl merge=beads\n",
    )
    .unwrap();
    git_ok(
        &root,
        &[
            "config",
            "merge.beads.driver",
            &format!("{} merge-driver %O %A %B", br.display()),
        ],
    );
    git_ok(&root, &["rebase", "main"]);

    let result = read(&root.join(".beads/issues.jsonl"));
    assert_eq!(find(&result, first).title, "feature edit");
    assert_eq!(find(&result, second).title, "main edit");
}
