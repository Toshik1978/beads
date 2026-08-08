//! bds-3rt: the negative filters — `--exclude-label`, `--exclude-type`,
//! `--no-labels`, `--no-parent`.
//!
//! Three things are worth asserting rather than reading off the SQL:
//!
//! **Repeated exclusions mean "none of these".** `--exclude-label a
//! --exclude-label b` is a union, not an intersection. The positive `--label`
//! form is an AND, so the temptation to make its complement symmetric is real
//! and would be wrong: "not both a and b" is not a filter anyone asks for.
//!
//! **An exclusion composes with the positive form of the same field.** They are
//! separate clauses on one query, so `--label urgent --exclude-label wontfix`
//! narrows twice rather than the second overriding the first.
//!
//! **The prefix case.** A label that is a prefix of another (`ui` and `ui-perf`)
//! must not be caught by an exclusion naming the shorter one. The exclusions
//! compare whole values via `NOT IN`, and this is what pins that they never grew
//! a `LIKE`.
//!
//! `list`, `search` and `ready` share one `ExclusionFilters`, and each of the
//! three query builders is exercised, because sharing the struct does not by
//! itself prove all three builders apply it.

use crate::common;

use beads::model::{Issue, IssueType, Priority, Status};
use beads::storage::{ExclusionFilters, ListFilters, ReadyFilters, ReadySortPolicy, SqliteStorage};
use chrono::Utc;
use common::test_db;

fn write_issue(storage: &SqliteStorage, id: &str, shape: impl FnOnce(&mut Issue)) {
    let mut issue = Issue {
        id: id.to_string(),
        title: format!("row {id}"),
        status: Status::Open,
        priority: Priority(2),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        ..Issue::default()
    };
    shape(&mut issue);
    storage.upsert_issue_for_import(&issue).expect("write row");
}

fn labelled(storage: &mut SqliteStorage, id: &str, labels: &[&str]) {
    write_issue(storage, id, |_| {});
    for label in labels {
        storage.add_label(id, label).expect("add label");
    }
}

fn listed(storage: &SqliteStorage, exclude: ExclusionFilters) -> Vec<String> {
    let filters = ListFilters {
        include_closed: true,
        include_deferred: true,
        exclude,
        ..ListFilters::default()
    };
    let mut ids: Vec<String> = storage
        .list_issues(&filters)
        .expect("list")
        .into_iter()
        .map(|issue| issue.id)
        .collect();
    let counted = storage.count_issues_with_filters(&filters).expect("count");
    assert_eq!(
        counted,
        ids.len(),
        "the count path has to apply the same exclusions as the query, or a \
         paginated total contradicts its own page: {ids:?}"
    );
    ids.sort();
    ids
}

#[test]
fn repeated_label_exclusions_reject_any_of_them_not_all_of_them() {
    let mut storage = test_db();
    labelled(&mut storage, "bd-a", &["alpha"]);
    labelled(&mut storage, "bd-b", &["beta"]);
    labelled(&mut storage, "bd-ab", &["alpha", "beta"]);
    labelled(&mut storage, "bd-none", &[]);

    let got = listed(
        &storage,
        ExclusionFilters {
            labels: vec!["alpha".to_string(), "beta".to_string()],
            ..ExclusionFilters::default()
        },
    );
    assert_eq!(
        got,
        vec!["bd-none".to_string()],
        "two --exclude-label values mean 'neither'; an intersection reading \
         would have kept bd-a and bd-b"
    );
}

/// Criterion: a label that is a prefix of another is not excluded by it.
#[test]
fn an_exclusion_matches_a_whole_label_and_not_a_prefix_of_one() {
    let mut storage = test_db();
    labelled(&mut storage, "bd-short", &["ui"]);
    labelled(&mut storage, "bd-long", &["ui-perf"]);

    assert_eq!(
        listed(
            &storage,
            ExclusionFilters {
                labels: vec!["ui".to_string()],
                ..ExclusionFilters::default()
            }
        ),
        vec!["bd-long".to_string()],
        "excluding `ui` must leave `ui-perf` alone"
    );

    assert_eq!(
        listed(
            &storage,
            ExclusionFilters {
                labels: vec!["ui-perf".to_string()],
                ..ExclusionFilters::default()
            }
        ),
        vec!["bd-short".to_string()],
        "and the other direction, so the test cannot pass by accident of ordering"
    );
}

/// Criterion: composition with the positive form of the same field.
#[test]
fn a_label_exclusion_composes_with_a_label_filter() {
    let mut storage = test_db();
    labelled(&mut storage, "bd-keep", &["urgent"]);
    labelled(&mut storage, "bd-drop", &["urgent", "wontfix"]);
    labelled(&mut storage, "bd-other", &["wontfix"]);

    let filters = ListFilters {
        include_closed: true,
        labels: Some(vec!["urgent".to_string()]),
        exclude: ExclusionFilters {
            labels: vec!["wontfix".to_string()],
            ..ExclusionFilters::default()
        },
        ..ListFilters::default()
    };
    let ids: Vec<String> = storage
        .list_issues(&filters)
        .expect("list")
        .into_iter()
        .map(|issue| issue.id)
        .collect();
    assert_eq!(
        ids,
        vec!["bd-keep".to_string()],
        "both clauses apply; neither replaces the other"
    );
}

#[test]
fn no_labels_rejects_every_issue_carrying_any_label() {
    let mut storage = test_db();
    labelled(&mut storage, "bd-bare", &[]);
    labelled(&mut storage, "bd-tagged", &["anything"]);

    assert_eq!(
        listed(
            &storage,
            ExclusionFilters {
                no_labels: true,
                ..ExclusionFilters::default()
            }
        ),
        vec!["bd-bare".to_string()]
    );
}

#[test]
fn a_type_exclusion_composes_and_repeats() {
    let storage = test_db();
    write_issue(&storage, "bd-bug", |issue| {
        issue.issue_type = IssueType::Bug;
    });
    write_issue(&storage, "bd-chore", |issue| {
        issue.issue_type = IssueType::Chore;
    });
    write_issue(&storage, "bd-feature", |issue| {
        issue.issue_type = IssueType::Feature;
    });

    assert_eq!(
        listed(
            &storage,
            ExclusionFilters {
                types: vec![IssueType::Bug, IssueType::Chore],
                ..ExclusionFilters::default()
            }
        ),
        vec!["bd-feature".to_string()]
    );
}

/// `--no-parent` asks about the `parent-child` dependency row, which is where
/// parenthood is actually recorded. A dotted ID is a *consequence* of having a
/// parent, so a filter that read the ID shape would answer a different question —
/// and would be wrong for a child whose parent was removed.
#[test]
fn no_parent_rejects_children_by_their_dependency_row() {
    let mut storage = test_db();
    write_issue(&storage, "bd-top", |_| {});
    write_issue(&storage, "bd-orphan", |_| {});
    write_issue(&storage, "bd-top.1", |_| {});
    storage
        .set_parent("bd-top.1", Some("bd-top"), "tester")
        .expect("set parent");

    assert_eq!(
        listed(
            &storage,
            ExclusionFilters {
                no_parent: true,
                ..ExclusionFilters::default()
            }
        ),
        vec!["bd-orphan".to_string(), "bd-top".to_string()],
        "the child goes; the parent and the unrelated issue stay"
    );
}

/// The exclusions live on `ReadyFilters` too, and `ready` builds its candidate
/// query separately from `list`. Sharing the struct does not prove the second
/// builder applies it.
#[test]
fn the_exclusions_reach_ready_as_well_as_list() {
    let mut storage = test_db();
    labelled(&mut storage, "bd-plain", &[]);
    labelled(&mut storage, "bd-noisy", &["wontfix"]);

    let all = storage
        .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Priority)
        .expect("ready");
    assert_eq!(all.len(), 2, "both are open and unblocked");

    let filtered = storage
        .get_ready_issues(
            &ReadyFilters {
                exclude: ExclusionFilters {
                    labels: vec!["wontfix".to_string()],
                    ..ExclusionFilters::default()
                },
                ..ReadyFilters::default()
            },
            ReadySortPolicy::Priority,
        )
        .expect("ready");
    assert_eq!(
        filtered
            .iter()
            .map(|issue| issue.id.as_str())
            .collect::<Vec<_>>(),
        vec!["bd-plain"]
    );
}

/// And on `search`, which is the third builder.
#[test]
fn the_exclusions_reach_search() {
    let mut storage = test_db();
    labelled(&mut storage, "bd-hit", &[]);
    labelled(&mut storage, "bd-miss", &["wontfix"]);
    for id in ["bd-hit", "bd-miss"] {
        let mut issue = storage.get_issue(id).unwrap().expect("exists");
        issue.title = format!("shared needle {id}");
        storage.upsert_issue_for_import(&issue).expect("retitle");
    }

    let filters = ListFilters {
        include_closed: true,
        exclude: ExclusionFilters {
            labels: vec!["wontfix".to_string()],
            ..ExclusionFilters::default()
        },
        ..ListFilters::default()
    };
    let found = storage.search_issues("needle", &filters).expect("search");
    assert_eq!(
        found
            .iter()
            .map(|issue| issue.id.as_str())
            .collect::<Vec<_>>(),
        vec!["bd-hit"]
    );
}

/// The fast routes again. A route that cannot apply an exclusion but answers the
/// query anyway returns the rows the caller asked to be rid of — a failure that
/// looks exactly like "the flag does nothing".
#[test]
fn no_fast_path_answers_an_exclusion_bearing_query_unfiltered() {
    let mut storage = test_db();
    labelled(&mut storage, "bd-keep", &[]);
    labelled(&mut storage, "bd-noise", &["wontfix"]);

    for filters in [
        // The default view: no statuses, no sort, no labels — the shape the fast
        // routes exist for.
        ListFilters {
            exclude: ExclusionFilters {
                labels: vec!["wontfix".to_string()],
                ..ExclusionFilters::default()
            },
            ..ListFilters::default()
        },
        ListFilters {
            limit: Some(50),
            exclude: ExclusionFilters {
                no_labels: true,
                ..ExclusionFilters::default()
            },
            ..ListFilters::default()
        },
    ] {
        let ids: Vec<String> = storage
            .list_issues(&filters)
            .expect("list")
            .into_iter()
            .map(|issue| issue.id)
            .collect();
        assert_eq!(
            ids,
            vec!["bd-keep".to_string()],
            "a fast route answered without applying the exclusion: {filters:?}"
        );
    }

    assert!(
        !ExclusionFilters {
            no_parent: true,
            ..ExclusionFilters::default()
        }
        .is_empty(),
        "every exclusion has to be visible to the predicate the fast routes gate \
         on, or adding one silently re-opens this hole"
    );
}
