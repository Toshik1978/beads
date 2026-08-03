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

/// Build an issue with the sort-relevant fields pinned, then persist it.
///
/// `IssueBuilder` has no timestamp setters, so those are assigned on the
/// struct before `create_issue`.
fn seed(storage: &mut SqliteStorage, id: &str, issue: Issue) -> String {
    storage.create_issue(&issue, "tester").expect("create");
    id.to_string()
}

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
    // Two p1 issues and one p0; the p1 pair must order by recency.
    seed(
        &mut storage,
        "test-older",
        built("test-older", Priority(1), at(1, 1)),
    );
    seed(
        &mut storage,
        "test-newer",
        built("test-newer", Priority(1), at(6, 1)),
    );
    seed(
        &mut storage,
        "test-crit",
        built("test-crit", Priority(0), at(1, 1)),
    );

    let issues = storage
        .list_issues(&filters_sorted("priority,updated"))
        .expect("list");

    assert_eq!(ids(&issues), vec!["test-crit", "test-newer", "test-older"]);
}

#[test]
fn status_orders_by_workflow_rank_not_alphabetically() {
    let mut storage = test_db();
    let open = fixtures::IssueBuilder::new("open one")
        .with_id("aaa-open")
        .build();
    let blocked = fixtures::IssueBuilder::new("blocked one")
        .with_id("zzz-blocked")
        .with_status(Status::Blocked)
        .build();
    storage.create_issue(&open, "tester").expect("create");
    storage.create_issue(&blocked, "tester").expect("create");

    let issues = storage
        .list_issues(&filters_sorted("status"))
        .expect("list");

    // Alphabetically 'blocked' precedes 'open'; by workflow rank it does not.
    assert_eq!(ids(&issues), vec!["aaa-open", "zzz-blocked"]);
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

#[test]
fn bare_priority_still_breaks_ties_by_created_at_descending() {
    let mut storage = test_db();
    seed(
        &mut storage,
        "test-first",
        built("test-first", Priority(1), at(1, 1)),
    );
    seed(
        &mut storage,
        "test-second",
        built("test-second", Priority(1), at(6, 1)),
    );

    let issues = storage
        .list_issues(&filters_sorted("priority"))
        .expect("list");

    // Newest first within the band — the legacy carve-out, unchanged.
    assert_eq!(ids(&issues), vec!["test-second", "test-first"]);
}
