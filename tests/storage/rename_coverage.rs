//! Guards `rename_issue`'s table list against schema drift.
//!
//! `ID_BEARING_COLUMNS` is hand-written, and a hand-written list of schema facts
//! rots the moment someone adds a table. This reads the live schema instead and
//! fails when the two disagree, so the gap is a red test rather than a dangling
//! reference discovered months later.
//!
//! **What this guard does not cover.** It walks `PRAGMA foreign_key_list`, which
//! only sees columns SQLite itself recognizes as holding another row's rowid.
//! An issue ID stored *inside* a TEXT column as JSON — `blocked_issues_cache.blocked_by`,
//! a JSON array of blocker IDs — looks like an ordinary TEXT column to
//! `PRAGMA table_info`/`foreign_key_list` alike, so no amount of schema
//! introspection here will ever catch it. That case is handled separately, by
//! cache invalidation inside `rename_issue` itself. Do not extend this guard to
//! try to parse blob contents; it structurally cannot generalize to that.

use crate::common;

use beads::storage::ID_BEARING_COLUMNS;
use common::test_db;
use std::collections::BTreeSet;

/// `dependencies.depends_on_id` holds an issue ID but declares no foreign key —
/// removed deliberately at `schema.rs:132` so `external:` references can dangle.
/// It is the one column the FK sweep cannot find on its own, which is exactly
/// why it gets its own existence check below rather than being trusted blind:
/// an unchecked exemption is a second hand-written list of schema facts,
/// hiding inside the guard meant to catch hand-written lists of schema facts.
const FK_EXEMPT_ID_COLUMNS: &[(&str, &str)] = &[("dependencies", "depends_on_id")];

#[test]
fn every_id_bearing_column_is_covered_by_rename_issue() {
    let storage = test_db();

    // Fact-check the exempt list against the live schema before trusting it.
    // The FK sweep below cannot see these columns at all (that is the reason
    // they are exempt), so nothing else in this test would notice if one were
    // dropped or renamed — `covered` and `from_schema` would go on agreeing
    // with each other while both silently described a column that no longer
    // exists.
    let mut from_schema: BTreeSet<(String, String)> = BTreeSet::new();
    for (table, column) in FK_EXEMPT_ID_COLUMNS {
        let exists = storage
            .schema_has_column(table, column)
            .expect("table_info query");
        assert!(
            exists,
            "FK_EXEMPT_ID_COLUMNS names {table}.{column}, but the live schema has no such \
             column — it was dropped or renamed out from under the exemption. Update \
             FK_EXEMPT_ID_COLUMNS (in this file) and ID_BEARING_COLUMNS \
             (src/storage/sqlite.rs) together."
        );
        from_schema.insert(((*table).to_string(), (*column).to_string()));
    }

    let tables = storage.schema_table_names().expect("list tables");
    for table in &tables {
        if table == "issues" {
            continue; // its own `id` column is rewritten directly, not via the list
        }

        for column in storage
            .columns_referencing_issues(table)
            .expect("foreign key list")
        {
            from_schema.insert((table.clone(), column));
        }
    }

    let covered: BTreeSet<(String, String)> = ID_BEARING_COLUMNS
        .iter()
        .map(|(t, c)| ((*t).to_string(), (*c).to_string()))
        .collect();

    let missing: Vec<_> = from_schema.difference(&covered).collect();
    assert!(
        missing.is_empty(),
        "these columns hold issue IDs but rename_issue does not rewrite them, so a \
         rename would leave them pointing at a vacated ID: {missing:?}\n\
         Add each pair to ID_BEARING_COLUMNS in src/storage/sqlite.rs."
    );

    let stale: Vec<_> = covered.difference(&from_schema).collect();
    assert!(
        stale.is_empty(),
        "ID_BEARING_COLUMNS names columns that no longer exist in the schema: {stale:?}\n\
         Remove each pair from src/storage/sqlite.rs."
    );
}
