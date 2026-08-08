//! bds-lf1: the ten date-range bounds on `br list` and `br search`.
//!
//! Two things are pinned here that prose alone cannot hold:
//!
//! **Inclusivity, at the boundary instant rather than near it.** Both ends are
//! inclusive — `updated_at <= ?` is what `ListFilters::updated_before` already
//! meant before these flags existed, and what `br stale` rests on. Every test
//! below places a row *exactly on* the bound, because a test a second away from
//! it passes whichever choice was made.
//!
//! **NULL never matches.** `closed_at`, `due_at` and `defer_until` are nullable,
//! and a bound on one of them excludes the rows that have no value. That falls
//! out of SQL's three-valued logic rather than an explicit `IS NOT NULL`, which
//! makes it exactly the kind of behaviour that would flip unnoticed if someone
//! rewrote the clause — so it is asserted, not trusted.
//!
//! The bounds are also applied by `count_issues_with_filters`, which is what
//! paginated output reports as the total. A count that applied one bound fewer
//! than the query would print a total that disagrees with the page it labels,
//! so the count path is asserted alongside the list path rather than assumed to
//! follow.

use crate::common;

use beads::model::{Issue, Priority, Status};
use beads::storage::{ListFilters, SqliteStorage};
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::test_db;

/// The instant every bound in this file is expressed against.
fn boundary() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap()
}

/// Write a row with exact timestamps.
///
/// `upsert_issue_for_import` rather than `create_issue`: the point of every test
/// here is a specific `created_at` / `closed_at` / `due_at`, and the create path
/// stamps its own.
fn write_issue(storage: &SqliteStorage, id: &str, shape: impl FnOnce(&mut Issue)) {
    let mut issue = Issue {
        id: id.to_string(),
        title: format!("row {id}"),
        status: Status::Open,
        priority: Priority(2),
        created_at: boundary(),
        updated_at: boundary(),
        ..Issue::default()
    };
    shape(&mut issue);
    storage.upsert_issue_for_import(&issue).expect("write row");
}

/// Every id the filters select, and — separately — the count the same filters
/// report, which paginated output prints as the total.
fn selected(storage: &SqliteStorage, filters: &ListFilters) -> Vec<String> {
    let listed: Vec<String> = storage
        .list_issues(filters)
        .expect("list")
        .into_iter()
        .map(|issue| issue.id)
        .collect();
    let counted = storage.count_issues_with_filters(filters).expect("count");
    assert_eq!(
        counted,
        listed.len(),
        "the count path applied a different set of bounds than the list path; \
         paginated output would print a total that contradicts its own page. \
         listed={listed:?}"
    );
    listed
}

/// `include_closed` and `include_deferred` on, so a status default cannot be
/// mistaken for a date filter doing its job.
fn unfiltered() -> ListFilters {
    ListFilters {
        include_closed: true,
        include_deferred: true,
        ..ListFilters::default()
    }
}

/// A row on the bound, a row one second earlier, a row one second later —
/// written through `shape`, which decides which column carries them.
fn seed_boundary_triple(storage: &SqliteStorage, shape: impl Fn(&mut Issue, DateTime<Utc>)) {
    for (id, at) in [
        ("bd-before", boundary() - Duration::seconds(1)),
        ("bd-on", boundary()),
        ("bd-after", boundary() + Duration::seconds(1)),
    ] {
        write_issue(storage, id, |issue| shape(issue, at));
    }
}

macro_rules! boundary_case {
    // `$column` names the field the triple varies; `$lower`/`$upper` are the
    // ListFilters bounds under test.
    ($name:ident, $shape:expr, $lower:ident, $upper:ident) => {
        #[test]
        fn $name() {
            let storage = test_db();
            seed_boundary_triple(&storage, $shape);

            let lower = ListFilters {
                $lower: Some(boundary()),
                ..unfiltered()
            };
            let mut got = selected(&storage, &lower);
            got.sort();
            assert_eq!(
                got,
                vec!["bd-after".to_string(), "bd-on".to_string()],
                "an `-after` bound is inclusive of the instant it names"
            );

            let upper = ListFilters {
                $upper: Some(boundary()),
                ..unfiltered()
            };
            let mut got = selected(&storage, &upper);
            got.sort();
            assert_eq!(
                got,
                vec!["bd-before".to_string(), "bd-on".to_string()],
                "a `-before` bound is inclusive of the instant it names"
            );
        }
    };
}

boundary_case!(
    created_bounds_are_inclusive_at_the_boundary_instant,
    |issue: &mut Issue, at: DateTime<Utc>| issue.created_at = at,
    created_after,
    created_before
);

boundary_case!(
    updated_bounds_are_inclusive_at_the_boundary_instant,
    |issue: &mut Issue, at: DateTime<Utc>| issue.updated_at = at,
    updated_after,
    updated_before
);

boundary_case!(
    closed_bounds_are_inclusive_at_the_boundary_instant,
    |issue: &mut Issue, at: DateTime<Utc>| {
        issue.status = Status::Closed;
        issue.closed_at = Some(at);
    },
    closed_after,
    closed_before
);

boundary_case!(
    due_bounds_are_inclusive_at_the_boundary_instant,
    |issue: &mut Issue, at: DateTime<Utc>| issue.due_at = Some(at),
    due_after,
    due_before
);

boundary_case!(
    defer_bounds_are_inclusive_at_the_boundary_instant,
    |issue: &mut Issue, at: DateTime<Utc>| {
        issue.status = Status::Deferred;
        issue.defer_until = Some(at);
    },
    defer_after,
    defer_before
);

/// Criterion: a row whose nullable column is NULL never matches a bound on it.
///
/// Stated the other way round, which is the version that matters: "closed in the
/// last week" must not return everything still open, and "due before Friday"
/// must not return every issue with no deadline.
#[test]
fn a_null_column_never_satisfies_a_bound_on_it() {
    let storage = test_db();
    write_issue(&storage, "bd-open", |_| {});
    write_issue(&storage, "bd-closed", |issue| {
        issue.status = Status::Closed;
        issue.closed_at = Some(boundary());
    });
    write_issue(&storage, "bd-dated", |issue| {
        issue.due_at = Some(boundary());
        issue.defer_until = Some(boundary());
    });

    for (label, filters) in [
        (
            "closed_before",
            ListFilters {
                closed_before: Some(boundary() + Duration::days(365)),
                ..unfiltered()
            },
        ),
        (
            "closed_after",
            ListFilters {
                closed_after: Some(boundary() - Duration::days(365)),
                ..unfiltered()
            },
        ),
    ] {
        assert_eq!(
            selected(&storage, &filters),
            vec!["bd-closed".to_string()],
            "{label} must exclude the rows with no closed_at"
        );
    }

    for (label, filters) in [
        (
            "due_before",
            ListFilters {
                due_before: Some(boundary() + Duration::days(365)),
                ..unfiltered()
            },
        ),
        (
            "defer_after",
            ListFilters {
                defer_after: Some(boundary() - Duration::days(365)),
                ..unfiltered()
            },
        ),
    ] {
        assert_eq!(
            selected(&storage, &filters),
            vec!["bd-dated".to_string()],
            "{label} must exclude the rows with no value in that column"
        );
    }
}

/// Criterion: two ranges compose, and they compose as AND rather than as
/// last-one-wins.
#[test]
fn two_ranges_narrow_each_other() {
    let storage = test_db();
    // created inside the window, updated outside it
    write_issue(&storage, "bd-mixed", |issue| {
        issue.created_at = boundary();
        issue.updated_at = boundary() + Duration::days(30);
    });
    // both inside
    write_issue(&storage, "bd-inside", |issue| {
        issue.created_at = boundary();
        issue.updated_at = boundary() + Duration::hours(1);
    });

    let filters = ListFilters {
        created_after: Some(boundary() - Duration::days(1)),
        updated_before: Some(boundary() + Duration::days(1)),
        ..unfiltered()
    };
    assert_eq!(
        selected(&storage, &filters),
        vec!["bd-inside".to_string()],
        "a row has to satisfy every bound, not the most recently applied one"
    );
}

/// Criterion: the filters reach `search`, not only `list`. They share a
/// `ListFilters`, and this is what proves the search SQL builder applies the
/// same bounds rather than dropping them.
#[test]
fn the_bounds_reach_search_and_not_only_list() {
    let storage = test_db();
    write_issue(&storage, "bd-old", |issue| {
        issue.title = "needle in the haystack".to_string();
        issue.created_at = boundary() - Duration::days(30);
        issue.updated_at = boundary() - Duration::days(30);
    });
    write_issue(&storage, "bd-new", |issue| {
        issue.title = "needle of recent vintage".to_string();
        issue.created_at = boundary();
        issue.updated_at = boundary();
    });

    let unbounded = storage
        .search_issues("needle", &unfiltered())
        .expect("search");
    assert_eq!(unbounded.len(), 2, "both rows match the text");

    let bounded = storage
        .search_issues(
            "needle",
            &ListFilters {
                created_after: Some(boundary() - Duration::days(1)),
                ..unfiltered()
            },
        )
        .expect("search");
    assert_eq!(
        bounded
            .iter()
            .map(|issue| issue.id.as_str())
            .collect::<Vec<_>>(),
        vec!["bd-new"],
        "the date bound has to narrow a text search too"
    );
}

/// The fast paths are the hazard: several list queries have a route that only
/// knows how to answer the default view, and each must fall back when a bound is
/// present. A route that silently ignored the bound would return *more* rows
/// than asked for, which reads as "no filter applied" rather than as a failure.
#[test]
fn no_fast_path_answers_a_bounded_query_unfiltered() {
    let storage = test_db();
    write_issue(&storage, "bd-old", |issue| {
        issue.created_at = boundary() - Duration::days(30);
        issue.updated_at = boundary() - Duration::days(30);
    });
    write_issue(&storage, "bd-new", |_| {});

    // The default view: no statuses, no labels, no sort — exactly the shape the
    // fast routes exist to serve.
    for filters in [
        ListFilters {
            created_after: Some(boundary() - Duration::days(1)),
            ..ListFilters::default()
        },
        ListFilters {
            created_after: Some(boundary() - Duration::days(1)),
            limit: Some(50),
            ..ListFilters::default()
        },
        ListFilters {
            due_before: Some(boundary()),
            ..ListFilters::default()
        },
    ] {
        let got = selected(&storage, &filters);
        assert!(
            !got.contains(&"bd-old".to_string()),
            "a fast route answered a bounded query without applying the bound: \
             got {got:?} for {filters:?}"
        );
    }

    assert!(
        ListFilters {
            defer_before: Some(boundary()),
            ..ListFilters::default()
        }
        .has_date_range_filters(),
        "every bound has to be visible to the predicate the fast routes gate on, \
         or adding one silently re-opens this hole"
    );
}
