//! The SQL `ORDER BY` renderer and the in-memory comparator must produce the
//! same sequence for the same spec. They are separate code paths over the
//! same `SortSpec::resolved` output, and `br list` uses one or the other
//! depending on whether client-side filters ran.

use beads::model::sort::SortSpec;
use beads::model::{Issue, IssueType, Priority, Status};
use beads::storage::ListFilters;
use chrono::{DateTime, TimeZone, Utc};
use proptest::prelude::*;

use crate::common;
use common::fixtures::IssueBuilder;

/// Fields the grammar accepts, as written on the command line.
const FIELDS: &[&str] = &[
    "priority",
    "status",
    "type",
    "assignee",
    "created_at",
    "updated_at",
    "title",
];

/// Build a spec string from indices into FIELDS plus direction markers,
/// skipping any field already used so the spec stays valid.
fn spec_string(picks: &[(usize, u8)]) -> Option<String> {
    let mut used = Vec::new();
    let mut segments = Vec::new();
    for (index, direction) in picks {
        let field = FIELDS[index % FIELDS.len()];
        if used.contains(&field) {
            continue;
        }
        used.push(field);
        segments.push(match direction % 3 {
            0 => field.to_string(),
            1 => format!("-{field}"),
            _ => format!("+{field}"),
        });
    }
    (!segments.is_empty()).then(|| segments.join(","))
}

/// A small, fixed pool so `created_at`/`updated_at` collide often across a
/// generated batch. A generator that hands out a distinct timestamp to every
/// issue makes `--sort updated,priority` and `--sort updated` produce
/// identical orderings, so the second key never gets exercised and both
/// engines "agree" for reasons that have nothing to do with this feature —
/// exactly the false-confidence failure this epic hit twice already.
fn timestamp_pool() -> [DateTime<Utc>; 4] {
    [
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2026, 1, 3, 0, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2026, 1, 4, 0, 0, 0).unwrap(),
    ]
}

/// Every known `Status` except `Tombstone`, plus two customs.
///
/// `Tombstone` is the one deliberate omission, and for a reason that is not
/// about sorting: `sqlite.rs` filters tombstones out of `list_issues` even
/// under `include_closed`, so generating one would make the SQL side return a
/// *different set* of issues than the in-memory side sorted. The test would
/// fail on membership and tell us nothing about ordering. Every other variant
/// belongs here — leaving `Deferred` and `Pinned` out left half the rank table
/// unexercised by the differential.
fn status_strategy() -> impl Strategy<Value = Status> {
    prop_oneof![
        3 => prop_oneof![
            Just(Status::Open),
            Just(Status::InProgress),
            Just(Status::Blocked),
            Just(Status::Deferred),
            Just(Status::Draft),
            Just(Status::Closed),
            Just(Status::Pinned),
        ],
        1 => prop_oneof![
            Just(Status::Custom("triage".to_string())),
            Just(Status::Custom("needs_review".to_string())),
        ],
    ]
}

/// Every known `IssueType`, plus a custom. Nothing is excluded here — unlike
/// `Status`, no type variant is filtered out of `list_issues`.
fn issue_type_strategy() -> impl Strategy<Value = IssueType> {
    prop_oneof![
        4 => prop_oneof![
            Just(IssueType::Task),
            Just(IssueType::Bug),
            Just(IssueType::Feature),
            Just(IssueType::Epic),
            Just(IssueType::Chore),
            Just(IssueType::Docs),
            Just(IssueType::Question),
        ],
        1 => Just(IssueType::Custom("spike".to_string())),
    ]
}

fn assignee_strategy() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        2 => Just(None),
        1 => Just(Some(String::new())),
        3 => prop_oneof![
            Just(Some("alice".to_string())),
            Just(Some("bob".to_string())),
        ],
    ]
}

/// Mixed casing plus a non-ASCII entry, so `title` exercises the ASCII-only
/// case fold both engines apply (`compare_fragments` lower-cases with
/// `to_ascii_lowercase`, matching SQLite's `COLLATE NOCASE`).
fn title_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("Alpha task".to_string()),
        Just("alpha task".to_string()),
        Just("BETA REVIEW".to_string()),
        Just("Ünïcode Tïtle".to_string()),
        Just("zeta follow-up".to_string()),
    ]
}

/// One generated issue with every sort-relevant field varied, and drawn from
/// small pools so ties across a batch are common rather than incidental.
/// `id` is left at whatever `IssueBuilder` assigns here; [`assign_unique_ids`]
/// overwrites it with a unique value before persisting, since the id
/// terminator needs a way to break ties deterministically.
fn arb_issue() -> impl Strategy<Value = Issue> {
    (
        (0i32..=4).prop_map(Priority),
        status_strategy(),
        issue_type_strategy(),
        assignee_strategy(),
        title_strategy(),
        0usize..4,
        0usize..4,
    )
        .prop_map(
            |(priority, status, issue_type, assignee, title, ts_a, ts_b)| {
                let pool = timestamp_pool();
                let created_at = pool[ts_a.min(ts_b)];
                let updated_at = pool[ts_a.max(ts_b)];
                let is_closed = status == Status::Closed;

                let mut builder = IssueBuilder::new(&title)
                    .with_priority(priority)
                    .with_type(issue_type)
                    .with_status(status);
                if let Some(name) = assignee {
                    builder = builder.with_assignee(&name);
                }

                let mut issue = builder.build();
                issue.created_at = created_at;
                issue.updated_at = updated_at;
                // `IssueBuilder::with_status` stamps `Utc::now()` into
                // `closed_at` for `Closed`; pin it to `updated_at` instead so
                // the issue's story is fully told by the fixed pool above,
                // not by wall-clock time.
                issue.closed_at = is_closed.then_some(updated_at);
                issue
            },
        )
}

/// Assign unique, validation-satisfying ids to a batch after every other
/// field was generated. Doing this as a second pass — rather than folding id
/// generation into `arb_issue` — is what guarantees uniqueness: strategies
/// composed independently per issue could otherwise collide by chance, and
/// `create_issue` rejects a duplicate id outright. The `bd-XXXX` shape
/// satisfies the prefix-hash format `IssueValidator` requires (lowercase
/// alphanumeric prefix, `-`, lowercase-alphanumeric hash).
fn assign_unique_ids(mut issues: Vec<Issue>) -> Vec<Issue> {
    for (index, issue) in issues.iter_mut().enumerate() {
        issue.id = format!("bd-{index:04}");
    }
    issues
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn sql_and_in_memory_orderings_agree(
        issues in prop::collection::vec(arb_issue(), 1..12).prop_map(assign_unique_ids),
        // Up to 8 draws, not 3: duplicate fields collapse in `spec_string`,
        // so a cap of 3 could never generate a spec chaining more than three
        // keys — and `FIELDS` has seven. The upper bound exceeds that length
        // on purpose, so specs using every field are reachable despite the
        // collapse.
        picks in prop::collection::vec((0usize..7, 0u8..3), 1..8),
        reverse in any::<bool>(),
    ) {
        let Some(spec_text) = spec_string(&picks) else {
            return Ok(());
        };
        let spec: SortSpec = spec_text.parse().expect("generated specs are valid");

        let mut storage = common::test_db();
        for issue in &issues {
            storage.create_issue(issue, "proptest").expect("create");
        }

        let filters = ListFilters {
            sort: Some(spec.clone()),
            reverse,
            include_closed: true,
            include_deferred: true,
            ..ListFilters::default()
        };
        let from_sql = storage.list_issues(&filters).expect("list");
        let sql_ids: Vec<String> = from_sql.iter().map(|i| i.id.clone()).collect();

        // `comparator`, not `compare`: this is the call the CLI actually makes
        // when it sorts, so it is the one whose agreement with SQL matters.
        let mut in_memory = issues.clone();
        in_memory.sort_by(spec.comparator(reverse));
        let memory_ids: Vec<String> = in_memory.iter().map(|i| i.id.clone()).collect();

        // …and the single-shot convenience must not drift from it.
        let mut one_shot = issues.clone();
        one_shot.sort_by(|left, right| spec.compare(left, right, reverse));
        let one_shot_ids: Vec<String> = one_shot.iter().map(|i| i.id.clone()).collect();
        prop_assert_eq!(
            &memory_ids,
            &one_shot_ids,
            "spec {:?} reverse={}: compare disagreed with comparator",
            spec_text,
            reverse
        );

        prop_assert_eq!(
            sql_ids,
            memory_ids,
            "spec {:?} reverse={} produced different orderings",
            spec_text,
            reverse
        );
    }
}
