//! Multi-key `--sort` orderings applied by the SQL builder.

use crate::common;

use beads::model::sort::SortSpec;
use beads::model::{Issue, Priority, Status};
use beads::storage::{ListFilters, SqliteStorage};
use chrono::{DateTime, TimeZone, Utc};
use common::{fixtures, test_db};

fn filters_sorted(spec: &str) -> ListFilters {
    ListFilters {
        sort: Some(spec.parse::<SortSpec>().expect("valid spec")),
        ..ListFilters::default()
    }
}

fn at(month: u32, day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, month, day, 0, 0, 0).unwrap()
}

/// Build an issue with the sort-relevant fields pinned.
///
/// `IssueBuilder` has no timestamp setters, so those are assigned on the
/// struct after `build()`; the caller persists it with `create_issue`.
fn built(id: &str, priority: Priority, updated: DateTime<Utc>) -> Issue {
    let mut issue = fixtures::IssueBuilder::new(id)
        .with_id(id)
        .with_priority(priority)
        .build();
    issue.created_at = updated;
    issue.updated_at = updated;
    issue
}

fn ids(issues: &[Issue]) -> Vec<&str> {
    issues.iter().map(|i| i.id.as_str()).collect()
}

#[test]
fn priority_then_updated_orders_within_the_priority_band() {
    let mut storage = test_db();
    // Two p1 issues and one p0; the p1 pair must order by recency. The ids
    // are deliberately alphabetized *opposite* to the intended `updated_at`
    // order: if the second key were dropped (or the id terminator alone
    // decided the tie), "test-aaa-older" would sort before
    // "test-zzz-newer" and this assertion would fail — that's what makes
    // this test discriminate against a spec that parses but doesn't apply
    // its second key.
    storage
        .create_issue(&built("test-aaa-older", Priority(1), at(1, 1)), "tester")
        .expect("create");
    storage
        .create_issue(&built("test-zzz-newer", Priority(1), at(6, 1)), "tester")
        .expect("create");
    storage
        .create_issue(&built("test-crit", Priority(0), at(1, 1)), "tester")
        .expect("create");

    let issues = storage
        .list_issues(&filters_sorted("priority,updated"))
        .expect("list");

    assert_eq!(
        ids(&issues),
        vec!["test-crit", "test-zzz-newer", "test-aaa-older"]
    );
}

#[test]
fn status_orders_by_workflow_rank_not_alphabetically() {
    let mut storage = test_db();
    // Ids run counter to workflow rank: were the status key dropped, the
    // implicit `id ASC` terminator would put the blocked issue first.
    let open = fixtures::IssueBuilder::new("open one")
        .with_id("zzz-open")
        .build();
    let blocked = fixtures::IssueBuilder::new("blocked one")
        .with_id("aaa-blocked")
        .with_status(Status::Blocked)
        .build();
    storage.create_issue(&open, "tester").expect("create");
    storage.create_issue(&blocked, "tester").expect("create");

    let issues = storage
        .list_issues(&filters_sorted("status"))
        .expect("list");

    // Alphabetically 'blocked' precedes 'open'; by workflow rank it does not.
    assert_eq!(ids(&issues), vec!["zzz-open", "aaa-blocked"]);
}

#[test]
fn unassigned_sorts_last_under_both_assignee_directions() {
    let mut storage = test_db();
    let anna = fixtures::IssueBuilder::new("anna's")
        .with_id("test-a")
        .with_assignee("anna")
        .build();
    let zoe = fixtures::IssueBuilder::new("zoe's")
        .with_id("test-z")
        .with_assignee("zoe")
        .build();
    let nobody = fixtures::IssueBuilder::new("nobody's")
        .with_id("test-n")
        .build();
    for issue in [&anna, &zoe, &nobody] {
        storage.create_issue(issue, "tester").expect("create");
    }

    let ascending = storage
        .list_issues(&filters_sorted("assignee"))
        .expect("list");
    assert_eq!(ids(&ascending), vec!["test-a", "test-z", "test-n"]);

    let descending = storage
        .list_issues(&filters_sorted("-assignee"))
        .expect("list");
    assert_eq!(ids(&descending), vec!["test-z", "test-a", "test-n"]);
}

/// Seed three stale-eligible issues whose relative order differs depending
/// on whether `updated_at` alone or `updated_at` *and* `priority` drive the
/// ordering. Two of the three share an `updated_at` (the only way a
/// secondary key or an id-terminator tie-break becomes observable); the
/// third sits later so a real "most-stale-first" ordering has something to
/// place last.
fn seed_stale_trio(storage: &mut SqliteStorage) {
    // Tied on updated_at (Jan 1): id-ASC says "test-aaa" first, but
    // priority-DESC (after --reverse flips priority's natural ASC) says
    // "test-zzz" (LOW = 3) first instead.
    storage
        .create_issue(&built("test-aaa", Priority::CRITICAL, at(1, 1)), "tester")
        .expect("create");
    storage
        .create_issue(&built("test-zzz", Priority::LOW, at(1, 1)), "tester")
        .expect("create");
    // Strictly later updated_at: sorts last under `updated_at ASC` no matter
    // which secondary key is in play.
    storage
        .create_issue(&built("test-mmm", Priority::MEDIUM, at(2, 1)), "tester")
        .expect("create");
}

#[test]
fn stale_fast_path_matches_general_ordering_for_updated_at_ascending() {
    let mut storage = test_db();
    seed_stale_trio(&mut storage);

    let filters = ListFilters {
        updated_before: Some(at(3, 1)),
        sort: Some("updated_at".parse::<SortSpec>().expect("valid spec")),
        reverse: true,
        ..ListFilters::default()
    };

    let stale = storage
        .list_stale_issues_for_command_output(&filters)
        .expect("list stale");
    let general = storage.list_issues(&filters).expect("list");

    // `--sort updated_at --reverse` resolves to exactly `[UpdatedAt Asc, Id
    // Asc]`, so the guard's fast path fires; its hardcoded
    // `ORDER BY updated_at ASC, id ASC` must agree with the general path
    // driven by the same resolved spec, and the Jan-1 tie breaks by id.
    assert_eq!(ids(&stale), ids(&general));
    assert_eq!(ids(&stale), vec!["test-aaa", "test-zzz", "test-mmm"]);
}

#[test]
fn stale_fast_path_rejects_a_multi_key_spec_and_honors_the_full_ordering() {
    let mut storage = test_db();
    seed_stale_trio(&mut storage);

    let filters = ListFilters {
        updated_before: Some(at(3, 1)),
        sort: Some("updated,priority".parse::<SortSpec>().expect("valid spec")),
        reverse: true,
        ..ListFilters::default()
    };

    let stale = storage
        .list_stale_issues_for_command_output(&filters)
        .expect("list stale");

    // A field-only guard would wrongly take the `updated_at ASC, id ASC`
    // fast path here (it starts with `updated_at`) and tie-break the Jan-1
    // pair by id: "test-aaa" before "test-zzz". The correct guard rejects
    // the fast path because the resolved ordering has 3 keys, not 2, so
    // `list_issues` applies the full spec instead — `priority DESC` (LOW=3
    // before CRITICAL=0) breaks the tie the other way.
    assert_eq!(ids(&stale), vec!["test-zzz", "test-aaa", "test-mmm"]);
}

#[test]
fn bare_priority_still_breaks_ties_by_created_at_descending() {
    let mut storage = test_db();
    storage
        .create_issue(&built("test-first", Priority(1), at(1, 1)), "tester")
        .expect("create");
    storage
        .create_issue(&built("test-second", Priority(1), at(6, 1)), "tester")
        .expect("create");

    let issues = storage
        .list_issues(&filters_sorted("priority"))
        .expect("list");

    // Newest first within the band — the legacy carve-out, unchanged.
    assert_eq!(ids(&issues), vec!["test-second", "test-first"]);
}
