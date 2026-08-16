//! Schema-shape coverage for the database `br init` produces.
//!
//! # Why this file exists
//!
//! Nothing else in the tree pins the *shape* of the SQLite database. Correctness
//! tests exercise behaviour through the storage API, so a dropped index, a
//! renamed column, or a stale `PRAGMA user_version` can survive a whole green
//! suite. That is precisely the failure mode a storage-engine replacement
//! introduces, so these tests assert the shape directly.
//!
//! Every expectation below was derived from a database this machine actually
//! produced (`br init` into a fresh temp dir, then read back through the
//! compiled-in engine) — not from `src/storage/schema.rs`'s source text and not
//! from any `bd`-comparison constant. `SCHEMA_SQL` is the *input*; these are the
//! observed *output*, which is the only thing a port can be checked against.
//!
//! # Note on the storage-engine port
//!
//! The only engine-coupled code is the "Engine-coupled readers" section below:
//! [`open_db`] and the `PRAGMA`/`sqlite_master` readers it feeds. Everything
//! else — every `EXPECTED_*` constant and every assertion — is meant to survive
//! the swap unchanged. If changing one of the constants ever looks necessary,
//! the schema has moved and that is the bug this file is here to catch.
//!
//! The assertions were deliberately written against engine-neutral facts, and
//! that neutrality was *measured*, not assumed: before the swap, the same schema
//! was built twice, once by `br init` on the pure-Rust engine and once by piping
//! `SCHEMA_SQL` through the `sqlite3` CLI, and every normalised value pinned
//! below was byte-identical between the two — every table DDL and every index
//! DDL. See [`normalize_ddl`] for the three renderings that had to be
//! normalised away.
//!
//! One engine difference could not be normalised away and is avoided instead:
//! the pure-Rust engine reported `PRAGMA index_list`'s `partial` column as `0`
//! for every index, where real SQLite reports `1` for those that
//! really are partial. Partiality is therefore taken from the DDL's `WHERE`
//! clause, never from that column. Now that the engine is `rusqlite`, that
//! column is truthful and the workaround can go — tracked as `bds-04l.13`.
//!
//! `issues.jsonl`'s serialized field set is a hard external invariant — an
//! external consumer parses this file directly and the field set is a
//! published interface — so it is pinned here too.

// `common` is now the `test-support` crate; aliased so that the 753
// `common::` paths in this suite keep working unchanged.
use crate::common;

use beads::model::{Comment, Dependency, DependencyType, Issue, IssueType, Priority, Status};
use beads::storage::conn::Connection;
use beads::storage::conn::SqliteValue;
use beads::storage::schema::CURRENT_SCHEMA_VERSION;
use chrono::Utc;
use common::cli::{BrWorkspace, run_br};
use serde_json::Value;
use std::path::Path;

// ---------------------------------------------------------------------------
// Expected shape
//
// Everything below was generated from a database this machine produced and then
// cross-checked against a `sqlite3`-built copy of the same `SCHEMA_SQL`. Do not
// hand-edit an entry to make a test pass: if one of these no longer matches, the
// schema moved, and that is the finding.
// ---------------------------------------------------------------------------

/// One column as `PRAGMA table_info` reports it:
/// `(name, declared type, NOT NULL, DEFAULT, primary-key position)`.
///
/// The primary-key position is 1-based and `0` for a non-key column, so a
/// composite key such as `dependencies (issue_id, depends_on_id)` shows as
/// `1, 2` and a reordering of it fails.
type ColumnShape = (&'static str, &'static str, bool, Option<&'static str>, i64);

/// One foreign key as `PRAGMA foreign_key_list` reports it:
/// `(table, from column, parent table, parent column, on_update, on_delete)`.
///
/// `on_delete` is the load-bearing field: every one of these is `CASCADE`, and
/// a port that silently downgraded one to `NO ACTION` would leave orphan rows
/// behind every issue deletion without failing a single correctness test.
type ForeignKey = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
);

/// One index: `(table, name, unique, key columns in key order, WHERE clause)`.
///
/// The `WHERE` clause is stored whole and compared for equality rather than
/// matched predicate-by-predicate. Substring matching would have been blind to
/// boolean connectives — `idx_dependencies_blocking`'s four `OR`s rewritten as
/// `AND`s still contain every individual predicate, match zero rows, and would
/// have passed. `None` asserts the index has no `WHERE` clause at all.
type IndexShape = (
    &'static str,
    &'static str,
    bool,
    &'static [&'static str],
    Option<&'static str>,
);

/// Every user table a fresh `br init` leaves behind, sorted.
///
/// `sqlite_sequence` is deliberately absent: it is engine bookkeeping created
/// implicitly by `AUTOINCREMENT`, filtered out by [`table_names`]'s `sqlite_%`
/// clause, and asserted separately by
/// `autoincrement_bookkeeping_table_is_present` — which is the only structural
/// signal that `AUTOINCREMENT` survived, since no `PRAGMA` exposes it.
const EXPECTED_TABLES: &[&str] = &[
    "blocked_issues_cache",
    "child_counters",
    "comments",
    "config",
    "dependencies",
    "dirty_issues",
    "export_hashes",
    "issues",
    "labels",
    "metadata",
];

/// Full column shape of every table, in declaration order.
///
/// Column *order* is load-bearing: `src/storage/schema.rs` documents that
/// migrations append at the end so fresh and migrated databases agree, and
/// `EXPECTED_ISSUE_COLUMN_ORDER` there enforces the same sequence at runtime. A
/// reordering here means those two have diverged.
///
/// `NOT NULL`, `DEFAULT` and the primary-key position are included because an
/// earlier version of this file compared names and types only, and a mutation
/// that stripped `NOT NULL` from `issues.title`, `DEFAULT '.'` from
/// `issues.source_repo` and `DEFAULT '{}'` from `dependencies.metadata` passed
/// every test.
const EXPECTED_TABLE_COLUMNS: &[(&str, &[ColumnShape])] = &[
    (
        "blocked_issues_cache",
        &[
            ("issue_id", "TEXT", false, None, 1),
            ("blocked_by", "TEXT", true, None, 0),
            ("blocked_at", "DATETIME", true, Some("CURRENT_TIMESTAMP"), 0),
        ],
    ),
    (
        "child_counters",
        &[
            ("parent_id", "TEXT", false, None, 1),
            ("last_child", "INTEGER", true, Some("0"), 0),
        ],
    ),
    (
        "comments",
        &[
            ("id", "INTEGER", false, None, 1),
            ("issue_id", "TEXT", true, None, 0),
            ("author", "TEXT", true, None, 0),
            ("text", "TEXT", true, None, 0),
            ("created_at", "DATETIME", true, Some("CURRENT_TIMESTAMP"), 0),
        ],
    ),
    (
        "config",
        &[
            ("key", "TEXT", true, None, 0),
            ("value", "TEXT", true, None, 0),
        ],
    ),
    (
        "dependencies",
        &[
            ("issue_id", "TEXT", true, None, 1),
            ("depends_on_id", "TEXT", true, None, 2),
            ("type", "TEXT", true, Some("'blocks'"), 0),
            ("created_at", "DATETIME", true, Some("CURRENT_TIMESTAMP"), 0),
            ("created_by", "TEXT", true, Some("''"), 0),
            ("metadata", "TEXT", false, Some("'{}'"), 0),
            ("thread_id", "TEXT", false, Some("''"), 0),
        ],
    ),
    (
        "dirty_issues",
        &[
            ("issue_id", "TEXT", false, None, 1),
            ("marked_at", "DATETIME", true, Some("CURRENT_TIMESTAMP"), 0),
        ],
    ),
    (
        "export_hashes",
        &[
            ("issue_id", "TEXT", false, None, 1),
            ("content_hash", "TEXT", true, None, 0),
            (
                "exported_at",
                "DATETIME",
                true,
                Some("CURRENT_TIMESTAMP"),
                0,
            ),
        ],
    ),
    (
        "issues",
        &[
            ("id", "TEXT", false, None, 1),
            ("content_hash", "TEXT", false, None, 0),
            ("title", "TEXT", true, None, 0),
            ("description", "TEXT", true, Some("''"), 0),
            ("design", "TEXT", true, Some("''"), 0),
            ("acceptance_criteria", "TEXT", true, Some("''"), 0),
            ("notes", "TEXT", true, Some("''"), 0),
            ("status", "TEXT", true, Some("'open'"), 0),
            ("priority", "INTEGER", true, Some("2"), 0),
            ("issue_type", "TEXT", true, Some("'task'"), 0),
            ("owner", "TEXT", false, Some("''"), 0),
            ("created_at", "DATETIME", true, Some("CURRENT_TIMESTAMP"), 0),
            ("created_by", "TEXT", false, Some("''"), 0),
            ("updated_at", "DATETIME", true, Some("CURRENT_TIMESTAMP"), 0),
            ("closed_at", "DATETIME", false, None, 0),
            ("close_reason", "TEXT", false, Some("''"), 0),
            ("defer_until", "DATETIME", false, None, 0),
            ("external_ref", "TEXT", false, None, 0),
            ("source_repo", "TEXT", true, Some("'.'"), 0),
            ("deleted_at", "DATETIME", false, None, 0),
            ("deleted_by", "TEXT", false, Some("''"), 0),
            ("delete_reason", "TEXT", false, Some("''"), 0),
            ("original_type", "TEXT", false, Some("''"), 0),
            ("former_ids", "TEXT", true, Some("'[]'"), 0),
        ],
    ),
    (
        "labels",
        &[
            ("issue_id", "TEXT", true, None, 1),
            ("label", "TEXT", true, None, 2),
        ],
    ),
    (
        "metadata",
        &[
            ("key", "TEXT", true, None, 0),
            ("value", "TEXT", true, None, 0),
        ],
    ),
];

/// Every foreign key in the schema. Compared as a whole sorted set, so a
/// removed key fails as loudly as an added one.
const EXPECTED_FOREIGN_KEYS: &[ForeignKey] = &[
    (
        "blocked_issues_cache",
        "issue_id",
        "issues",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    (
        "child_counters",
        "parent_id",
        "issues",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    (
        "comments",
        "issue_id",
        "issues",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    (
        "dependencies",
        "issue_id",
        "issues",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    (
        "dirty_issues",
        "issue_id",
        "issues",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    (
        "export_hashes",
        "issue_id",
        "issues",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    ("labels", "issue_id", "issues", "id", "NO ACTION", "CASCADE"),
];

/// Normalised `CREATE TABLE` text for every table.
///
/// This is the belt-and-braces guard for the two things no `PRAGMA` exposes:
/// `CHECK` constraints and `AUTOINCREMENT`. It necessarily overlaps
/// [`EXPECTED_TABLE_COLUMNS`] and [`EXPECTED_FOREIGN_KEYS`]; that redundancy is
/// deliberate, because those two give a precise one-column failure message
/// while this one gives completeness. Read their failures first.
///
/// Comparing DDL *text* is normally engine-fragile, which is why the
/// [`normalize_ddl`] rendering differences were measured across two engines
/// rather than guessed. If this test fails alone, with every structured test
/// green, suspect a third rendering difference before suspecting the schema.
const EXPECTED_TABLE_DDL: &[(&str, &str)] = &[
    (
        "blocked_issues_cache",
        "create table blocked_issues_cache issue_id text primary key, blocked_by text not \
         null, blocked_at datetime not null default current_timestamp, foreign key issue_id \
         references issues id on delete cascade",
    ),
    (
        "child_counters",
        "create table child_counters parent_id text primary key, last_child integer not null \
         default 0, foreign key parent_id references issues id on delete cascade",
    ),
    (
        "comments",
        "create table comments id integer primary key autoincrement, issue_id text not null, \
         author text not null, text text not null, created_at datetime not null default \
         current_timestamp, foreign key issue_id references issues id on delete cascade",
    ),
    (
        "config",
        "create table config key text not null, value text not null",
    ),
    (
        "dependencies",
        "create table dependencies issue_id text not null, depends_on_id text not null, type \
         text not null default 'blocks', created_at datetime not null default \
         current_timestamp, created_by text not null default '', metadata text default '{}', \
         thread_id text default '', primary key issue_id, depends_on_id , foreign key \
         issue_id references issues id on delete cascade",
    ),
    (
        "dirty_issues",
        "create table dirty_issues issue_id text primary key, marked_at datetime not null \
         default current_timestamp, foreign key issue_id references issues id on delete \
         cascade",
    ),
    (
        "export_hashes",
        "create table export_hashes issue_id text primary key, content_hash text not null, \
         exported_at datetime not null default current_timestamp, foreign key issue_id \
         references issues id on delete cascade",
    ),
    (
        "issues",
        "create table issues id text primary key, content_hash text, title text not null \
         check length title <= 500 , description text not null default '', design text not \
         null default '', acceptance_criteria text not null default '', notes text not null \
         default '', status text not null default 'open', priority integer not null default 2 \
         check priority >= 0 and priority <= 4 , issue_type text not null default 'task', \
         owner text default '', created_at datetime \
         not null default current_timestamp, created_by text default '', updated_at datetime \
         not null default current_timestamp, closed_at datetime, close_reason text default \
         '', defer_until datetime, \
         external_ref text, source_repo text not null default \
         '.', deleted_at datetime, deleted_by text default '', delete_reason text default '', \
         original_type text default '', former_ids \
         text not null default '[]', check status \
         = 'closed' and closed_at is not null or status = 'tombstone' or status not in \
         'closed', 'tombstone' and closed_at is null",
    ),
    (
        "labels",
        "create table labels issue_id text not null, label text not null, primary key \
         issue_id, label , foreign key issue_id references issues id on delete cascade",
    ),
    (
        "metadata",
        "create table metadata key text not null, value text not null",
    ),
];

/// Every index, not a curated subset. Name-only pinning let
/// `idx_issues_status ON issues(priority)` through: same name, wrong column,
/// useless index, green suite.
const EXPECTED_INDEX_SHAPE: &[IndexShape] = &[
    (
        "blocked_issues_cache",
        "idx_blocked_cache_blocked_at",
        false,
        &["blocked_at"],
        None,
    ),
    (
        "comments",
        "idx_comments_created_at",
        false,
        &["created_at"],
        None,
    ),
    ("comments", "idx_comments_issue", false, &["issue_id"], None),
    ("config", "idx_config_key", false, &["key"], None),
    (
        "dependencies",
        "idx_dependencies_blocking",
        false,
        &["depends_on_id", "issue_id"],
        Some(
            "type = 'blocks' or type = 'parent-child' or type = 'conditional-blocks' or type \
             = 'waits-for'",
        ),
    ),
    (
        "dependencies",
        "idx_dependencies_depends_on",
        false,
        &["depends_on_id"],
        None,
    ),
    (
        "dependencies",
        "idx_dependencies_depends_on_type",
        false,
        &["depends_on_id", "type"],
        None,
    ),
    (
        "dependencies",
        "idx_dependencies_issue",
        false,
        &["issue_id"],
        None,
    ),
    (
        "dependencies",
        "idx_dependencies_thread",
        false,
        &["thread_id"],
        Some("thread_id != ''"),
    ),
    (
        "dependencies",
        "idx_dependencies_type",
        false,
        &["type"],
        None,
    ),
    (
        "dirty_issues",
        "idx_dirty_issues_marked_at",
        false,
        &["marked_at"],
        None,
    ),
    (
        "issues",
        "idx_issues_content_hash",
        false,
        &["content_hash"],
        None,
    ),
    (
        "issues",
        "idx_issues_created_at",
        false,
        &["created_at"],
        None,
    ),
    (
        "issues",
        "idx_issues_defer_until",
        false,
        &["defer_until"],
        Some("defer_until is not null"),
    ),
    (
        "issues",
        "idx_issues_external_ref_unique",
        true,
        &["external_ref"],
        Some("external_ref is not null"),
    ),
    (
        "issues",
        "idx_issues_issue_type",
        false,
        &["issue_type"],
        None,
    ),
    (
        "issues",
        "idx_issues_list_active_order",
        false,
        &["priority", "created_at"],
        Some("status not in 'closed', 'tombstone'"),
    ),
    ("issues", "idx_issues_priority", false, &["priority"], None),
    (
        "issues",
        "idx_issues_ready",
        false,
        &["status", "priority", "created_at"],
        Some("status = 'open'"),
    ),
    ("issues", "idx_issues_status", false, &["status"], None),
    (
        "issues",
        "idx_issues_status_priority_created",
        false,
        &["status", "priority", "created_at"],
        None,
    ),
    (
        "issues",
        "idx_issues_tombstone",
        false,
        &["status"],
        Some("status = 'tombstone'"),
    ),
    (
        "issues",
        "idx_issues_updated_at",
        false,
        &["updated_at"],
        None,
    ),
    ("labels", "idx_labels_issue", false, &["issue_id"], None),
    ("labels", "idx_labels_label", false, &["label"], None),
    ("metadata", "idx_metadata_key", false, &["key"], None),
];

/// `br init` must leave the database file in WAL mode.
///
/// Read from the file header rather than `PRAGMA journal_mode`, because the
/// header is persistent, engine-independent and definitive: bytes 18 and 19 of
/// any SQLite file are the write and read format versions, `1` for a rollback
/// journal and `2` for WAL. A file `br` produced reads `02 02`; one `sqlite3`
/// creates with default settings reads `01 01`.
///
/// This matters more after the port than before it. `apply_runtime_pragmas`
/// only issues `PRAGMA journal_mode = WAL` when the current mode is not already
/// WAL — and the pure-Rust engine reported `wal` for a brand-new database, so
/// that branch never executed before the swap. Real SQLite defaults to a
/// rollback journal, so it goes live for the first time under `rusqlite`. If it
/// is dropped or reordered, the database silently stops being WAL, readers start
/// blocking writers, and nothing else in the suite notices.
const WAL_FORMAT_VERSION: u8 = 2;

/// Every key `Issue` can emit into `issues.jsonl`, in declaration order.
///
/// HARD EXTERNAL INVARIANT: an external consumer parses this file directly and
/// the field set is a published interface. No key may be added, removed, or
/// renamed. If one of these tests fails, the fix is almost never to update the
/// constant.
///
/// `content_hash` is deliberately absent — it carries `#[serde(skip)]` and is a
/// database-only column.
const EXPECTED_JSONL_KEYS: &[&str] = &[
    "acceptance_criteria",
    "close_reason",
    "closed_at",
    "comments",
    "created_at",
    "created_by",
    "defer_until",
    "delete_reason",
    "deleted_at",
    "deleted_by",
    "dependencies",
    "description",
    "design",
    "external_ref",
    "former_ids",
    "id",
    "issue_type",
    "labels",
    "notes",
    "original_type",
    "owner",
    "priority",
    "source_repo",
    "status",
    "title",
    "updated_at",
];

/// The subset that is emitted even for an otherwise-empty issue: every other
/// key carries `skip_serializing_if`. A consumer may rely on these always being
/// present, so moving one out of this set is as breaking as removing it.
const ALWAYS_SERIALIZED_JSONL_KEYS: &[&str] = &[
    "created_at",
    "id",
    "issue_type",
    "priority",
    "status",
    "title",
    "updated_at",
];

/// Keys of a `dependencies[]` entry inside an issue line. Note `type`, not
/// `dep_type`: the Rust field carries `#[serde(rename = "type")]` and the wire
/// name is the contract.
const EXPECTED_DEPENDENCY_KEYS: &[&str] = &[
    "created_at",
    "created_by",
    "depends_on_id",
    "issue_id",
    "metadata",
    "thread_id",
    "type",
];

/// Keys of a `comments[]` entry inside an issue line. Note `text`, not `body`.
const EXPECTED_COMMENT_KEYS: &[&str] = &["author", "created_at", "id", "issue_id", "text"];

// ---------------------------------------------------------------------------
// Engine-coupled readers (port these; leave the constants alone)
// ---------------------------------------------------------------------------

fn open_db(db_path: &Path) -> Connection {
    Connection::open(db_path.to_string_lossy().into_owned())
        .unwrap_or_else(|error| panic!("open {}: {error}", db_path.display()))
}

fn text_column(conn: &Connection, sql: &str, index: usize) -> Vec<String> {
    conn.query(sql)
        .unwrap_or_else(|error| panic!("query {sql:?}: {error}"))
        .into_iter()
        .filter_map(|row| {
            row.get(index)
                .and_then(SqliteValue::as_text)
                .map(ToOwned::to_owned)
        })
        .collect()
}

/// Full column shape in declaration order, via `PRAGMA table_info`.
///
/// Reads every column of the pragma: `name` (1), `type` (2), `notnull` (3),
/// `dflt_value` (4) and `pk` (5). Discarding the last three is what let a
/// six-constraint mutation through a previous version of this file.
fn column_shape(
    conn: &Connection,
    table: &str,
) -> Vec<(String, String, bool, Option<String>, i64)> {
    let sql = format!("PRAGMA table_info({table})");
    conn.query(&sql)
        .unwrap_or_else(|error| panic!("table_info({table}): {error}"))
        .into_iter()
        .map(|row| {
            let text = |position: usize| {
                row.get(position)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or_default()
                    .to_owned()
            };
            let integer = |position: usize| row.get(position).and_then(SqliteValue::as_integer);
            let default = row
                .get(4)
                .and_then(SqliteValue::as_text)
                .map(ToOwned::to_owned);
            (
                text(1),
                text(2),
                integer(3).unwrap_or(0) != 0,
                default,
                integer(5).unwrap_or(0),
            )
        })
        .collect()
}

/// Foreign keys of a table, via `PRAGMA foreign_key_list`, sorted for stable
/// comparison: `(from column, parent table, parent column, on_update, on_delete)`.
fn foreign_keys(conn: &Connection, table: &str) -> Vec<(String, String, String, String, String)> {
    let sql = format!("PRAGMA foreign_key_list({table})");
    let mut keys: Vec<_> = conn
        .query(&sql)
        .unwrap_or_else(|error| panic!("foreign_key_list({table}): {error}"))
        .into_iter()
        .map(|row| {
            let text = |position: usize| {
                row.get(position)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or_default()
                    .to_owned()
            };
            // 2 = parent table, 3 = from column, 4 = parent column,
            // 5 = on_update, 6 = on_delete.
            (text(3), text(2), text(4), text(5), text(6))
        })
        .collect();
    keys.sort();
    keys
}

/// The defining DDL of a table, from `sqlite_master`.
fn table_ddl(conn: &Connection, table: &str) -> String {
    object_sql(conn, "table", table)
}

fn table_names(conn: &Connection) -> Vec<String> {
    let mut names = text_column(
        conn,
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        0,
    );
    names.sort_unstable();
    names
}

fn index_names(conn: &Connection) -> Vec<String> {
    let mut names = text_column(
        conn,
        "SELECT name FROM sqlite_master WHERE type = 'index' AND name NOT LIKE 'sqlite_%'",
        0,
    );
    names.sort_unstable();
    names
}

/// Whether a named index is UNIQUE, via `PRAGMA index_list(table)`.
fn index_is_unique(conn: &Connection, table: &str, index: &str) -> bool {
    let sql = format!("PRAGMA index_list({table})");
    let rows = conn
        .query(&sql)
        .unwrap_or_else(|error| panic!("index_list({table}): {error}"));
    let row = rows
        .iter()
        .find(|row| row.get(1).and_then(SqliteValue::as_text) == Some(index))
        .unwrap_or_else(|| panic!("index {index} is not listed on table {table}"));
    row.get(2)
        .and_then(SqliteValue::as_integer)
        .unwrap_or_else(|| panic!("index_list({table}) unique column is not an integer"))
        != 0
}

/// The defining DDL of a named index, from `sqlite_master`.
fn index_ddl(conn: &Connection, index: &str) -> String {
    object_sql(conn, "index", index)
}

/// The stored `sql` text of a `sqlite_master` object.
fn object_sql(conn: &Connection, kind: &str, name: &str) -> String {
    let sql = format!("SELECT sql FROM sqlite_master WHERE type = '{kind}' AND name = ?");
    let rows = conn
        .query_with_params(&sql, &[SqliteValue::from(name)])
        .unwrap_or_else(|error| panic!("sqlite_master lookup for {kind} {name}: {error}"));
    rows.first()
        .unwrap_or_else(|| panic!("{kind} {name} is not present in sqlite_master"))
        .get(0)
        .and_then(SqliteValue::as_text)
        .unwrap_or_else(|| panic!("{kind} {name} has no defining SQL"))
        .to_owned()
}

/// Key columns of an index, in key order, via `PRAGMA index_info(index)`.
fn index_columns(conn: &Connection, index: &str) -> Vec<String> {
    let sql = format!("PRAGMA index_info({index})");
    conn.query(&sql)
        .unwrap_or_else(|error| panic!("index_info({index}): {error}"))
        .into_iter()
        .map(|row| {
            row.get(2)
                .and_then(SqliteValue::as_text)
                .unwrap_or("<expression>")
                .to_owned()
        })
        .collect()
}

/// Whether `sqlite_master` holds an object with this exact name, including the
/// `sqlite_`-prefixed internals that [`table_names`] filters out.
fn master_object_exists(conn: &Connection, name: &str) -> bool {
    !conn
        .query_with_params(
            "SELECT name FROM sqlite_master WHERE name = ?",
            &[SqliteValue::from(name)],
        )
        .unwrap_or_else(|error| panic!("sqlite_master lookup for {name}: {error}"))
        .is_empty()
}

fn user_version(conn: &Connection) -> i64 {
    conn.query_row("PRAGMA user_version")
        .expect("PRAGMA user_version")
        .get(0)
        .and_then(SqliteValue::as_integer)
        .expect("user_version is an integer")
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Reduce SQL text to a form two different engines agree on.
///
/// Three rendering differences were measured between the previous engine (which re-renders
/// DDL from its parsed AST) and `sqlite3` (which stores the submitted text
/// verbatim), by building the same `SCHEMA_SQL` with both and diffing all 53
/// objects. Each is normalised away here:
///
/// 1. **Parenthesisation.** The previous engine re-parenthesised every boolean expression:
///    `WHERE a AND b AND c` comes back as `WHERE ((a AND b) AND c)`. Parens
///    become spaces.
/// 2. **`--` comments.** `sqlite3` stores them verbatim inside the DDL;
///    the previous engine dropped them. Stripped to end-of-line. Safe here: no string
///    literal in `SCHEMA_SQL` contains a double hyphen.
/// 3. **Reserved-word quoting and `IF NOT EXISTS`.** The previous engine emitted `"key"`
///    and keeps `IF NOT EXISTS`; `sqlite3` emits `key` and drops it. Quotes are
///    removed and the clause elided.
///
/// After this, every table DDL and every index DDL is byte-identical
/// between the two engines — which is the evidence that the pinned constants
/// will survive the `rusqlite` port untouched.
fn normalize_ddl(sql: &str) -> String {
    let uncommented: String = sql
        .lines()
        .map(|line| line.split_once("--").map_or(line, |(before, _)| before))
        .collect::<Vec<_>>()
        .join(" ");
    let stripped: String = uncommented
        .chars()
        .map(|c| if c == '(' || c == ')' { ' ' } else { c })
        .filter(|c| *c != '"')
        .collect();
    stripped
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("if not exists ", "")
}

fn owned_columns(columns: &[ColumnShape]) -> Vec<(String, String, bool, Option<String>, i64)> {
    columns
        .iter()
        .map(|(name, declared, notnull, default, pk)| {
            (
                (*name).to_owned(),
                (*declared).to_owned(),
                *notnull,
                default.map(ToOwned::to_owned),
                *pk,
            )
        })
        .collect()
}

fn owned_names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// A workspace with a fresh `br init` database and nothing else.
fn init_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "shape"], "init");
    assert!(
        init.status.success(),
        "br init failed: stdout={} stderr={}",
        init.stdout,
        init.stderr
    );
    workspace
}

fn db_path(workspace: &BrWorkspace) -> std::path::PathBuf {
    workspace.root.join(".beads").join("beads.db")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn fresh_init_creates_exactly_the_expected_tables() {
    let workspace = init_workspace();
    let conn = open_db(&db_path(&workspace));

    assert_eq!(
        table_names(&conn),
        owned_names(EXPECTED_TABLES),
        "the set of tables produced by `br init` changed"
    );
}

#[test]
fn autoincrement_bookkeeping_table_is_present() {
    let workspace = init_workspace();
    let conn = open_db(&db_path(&workspace));

    // No PRAGMA reports AUTOINCREMENT, and `sqlite_sequence` is filtered out of
    // `table_names` by the `sqlite_%` clause, so without this assertion a port
    // that dropped AUTOINCREMENT from `comments` would be
    // invisible to every other test here except the DDL comparison. Rowid reuse after a delete would silently follow.
    assert!(
        master_object_exists(&conn, "sqlite_sequence"),
        "sqlite_sequence is absent, so no table declares AUTOINCREMENT any more"
    );
}

#[test]
fn every_table_has_pinned_column_and_ddl_coverage() {
    // A guard on the pins themselves: a table added to EXPECTED_TABLES without
    // a column shape or a DDL entry would otherwise be silently uncovered, which
    // is exactly how nine of the fourteen tables went unpinned before.
    let mut with_columns: Vec<&str> = EXPECTED_TABLE_COLUMNS.iter().map(|(t, _)| *t).collect();
    with_columns.sort_unstable();
    assert_eq!(
        with_columns, EXPECTED_TABLES,
        "EXPECTED_TABLE_COLUMNS does not cover exactly the expected tables"
    );

    let mut with_ddl: Vec<&str> = EXPECTED_TABLE_DDL.iter().map(|(t, _)| *t).collect();
    with_ddl.sort_unstable();
    assert_eq!(
        with_ddl, EXPECTED_TABLES,
        "EXPECTED_TABLE_DDL does not cover exactly the expected tables"
    );
}

#[test]
fn table_columns_match_expected_shape() {
    let workspace = init_workspace();
    let conn = open_db(&db_path(&workspace));

    for (table, expected) in EXPECTED_TABLE_COLUMNS {
        assert_eq!(
            column_shape(&conn, table),
            owned_columns(expected),
            "`{table}`'s columns drifted — name, declared type, NOT NULL, \
             DEFAULT, order or primary-key position. Tuples are \
             (name, type, notnull, default, pk position)."
        );
    }
}

#[test]
fn table_foreign_keys_match_expected() {
    let workspace = init_workspace();
    let conn = open_db(&db_path(&workspace));

    for table in EXPECTED_TABLES {
        let expected: Vec<(String, String, String, String, String)> = EXPECTED_FOREIGN_KEYS
            .iter()
            .filter(|(owner, ..)| owner == table)
            .map(|(_, from, parent, parent_column, on_update, on_delete)| {
                (
                    (*from).to_owned(),
                    (*parent).to_owned(),
                    (*parent_column).to_owned(),
                    (*on_update).to_owned(),
                    (*on_delete).to_owned(),
                )
            })
            .collect();
        assert_eq!(
            foreign_keys(&conn, table),
            expected,
            "`{table}`'s foreign keys drifted. Tuples are (from column, parent \
             table, parent column, on_update, on_delete); a CASCADE downgraded \
             to NO ACTION leaves orphan rows behind every delete."
        );
    }
}

#[test]
fn table_ddl_matches_expected_normalized_form() {
    let workspace = init_workspace();
    let conn = open_db(&db_path(&workspace));

    for (table, expected) in EXPECTED_TABLE_DDL {
        assert_eq!(
            normalize_ddl(&table_ddl(&conn, table)),
            *expected,
            "`{table}`'s CREATE TABLE text drifted. This is the only assertion \
             that sees CHECK constraints and AUTOINCREMENT, so check those \
             first; if the structured column and foreign-key tests are green, \
             suspect a CHECK, an AUTOINCREMENT, or a new engine rendering \
             difference (see normalize_ddl)."
        );
    }
}

#[test]
fn fresh_init_creates_exactly_the_expected_indexes() {
    let workspace = init_workspace();
    let conn = open_db(&db_path(&workspace));

    // Derived from EXPECTED_INDEX_SHAPE rather than kept as a second list, so an
    // index can never be named here without also having its shape pinned.
    let mut expected: Vec<&str> = EXPECTED_INDEX_SHAPE
        .iter()
        .map(|(_, name, ..)| *name)
        .collect();
    expected.sort_unstable();

    assert_eq!(
        index_names(&conn),
        owned_names(&expected),
        "the set of indexes produced by `br init` changed — a missing index is \
         a silent performance regression no correctness test will catch"
    );
}

#[test]
fn every_index_keeps_its_uniqueness_key_columns_and_where_clause() {
    let workspace = init_workspace();
    let conn = open_db(&db_path(&workspace));

    for (table, index, unique, columns, where_clause) in EXPECTED_INDEX_SHAPE {
        assert_eq!(
            index_is_unique(&conn, table, index),
            *unique,
            "index `{index}` on `{table}` changed its UNIQUE flag — losing it \
             silently drops a constraint"
        );
        assert_eq!(
            index_columns(&conn, index),
            owned_names(columns),
            "index `{index}` on `{table}` changed its key columns or their \
             order — same name, different index"
        );

        let ddl = normalize_ddl(&index_ddl(&conn, index));
        let actual = ddl.split_once(" where ").map(|(_, clause)| clause);
        assert_eq!(
            actual, *where_clause,
            "index `{index}` on `{table}` changed its WHERE clause. Losing one \
             widens the index; gaining one narrows it; swapping AND for OR can \
             leave it matching nothing. Full DDL: {ddl}"
        );
    }
}

#[test]
fn fresh_init_stamps_the_current_schema_version() {
    let workspace = init_workspace();
    let conn = open_db(&db_path(&workspace));

    // Two assertions on purpose. The literal catches an unintended bump of the
    // constant; the constant comparison catches a database that was created or
    // migrated without the stamp being written.
    assert_eq!(
        user_version(&conn),
        19,
        "PRAGMA user_version on a fresh database changed"
    );
    assert_eq!(
        user_version(&conn),
        i64::from(CURRENT_SCHEMA_VERSION),
        "PRAGMA user_version does not match CURRENT_SCHEMA_VERSION"
    );
}

/// NOTE ON FALSIFIABILITY: this test was inert before the storage-engine swap.
/// The pure-Rust engine had no non-WAL journal mode to fall back to — it
/// rejected `PRAGMA journal_mode = DELETE` at parse time and stamped WAL into
/// the header of every database it created — so no mutation of `src/` could
/// drive it to failure. Under `rusqlite` a rollback journal is the *default*,
/// so this now genuinely pins that `apply_runtime_pragmas` turns WAL on. See
/// [`WAL_FORMAT_VERSION`].
#[test]
fn fresh_init_leaves_the_database_file_in_wal_mode() {
    let workspace = init_workspace();
    let header = std::fs::read(db_path(&workspace)).expect("read the database file");

    assert!(
        header.len() > 19,
        "the database file is too short to contain a SQLite header"
    );
    assert_eq!(
        (header[18], header[19]),
        (WAL_FORMAT_VERSION, WAL_FORMAT_VERSION),
        "the database `br init` produced is not in WAL mode — header write/read \
         format versions are (write, read) and must both be 2; 1 means a \
         rollback journal, which makes readers block writers"
    );
}

#[test]
fn fresh_init_writes_the_expected_metadata_json() {
    let workspace = init_workspace();
    let raw = std::fs::read_to_string(workspace.root.join(".beads").join("metadata.json"))
        .expect("metadata.json is written by br init");
    let parsed: Value = serde_json::from_str(&raw).expect("metadata.json is valid JSON");

    let object = parsed.as_object().expect("metadata.json is a JSON object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();

    assert_eq!(
        keys,
        vec!["database", "jsonl_export"],
        "metadata.json's key set changed"
    );
    assert_eq!(
        object.get("database").and_then(Value::as_str),
        Some("beads.db"),
        "metadata.json no longer points at beads.db"
    );
    assert_eq!(
        object.get("jsonl_export").and_then(Value::as_str),
        Some("issues.jsonl"),
        "metadata.json no longer points at issues.jsonl"
    );
}

/// Build an `Issue` with every optional field populated, so serialization
/// cannot hide a key behind `skip_serializing_if`.
fn fully_populated_issue() -> Issue {
    let now = Utc::now();
    Issue {
        id: "shape-1".to_owned(),
        content_hash: Some("deadbeef".to_owned()),
        title: "title".to_owned(),
        description: Some("description".to_owned()),
        design: Some("design".to_owned()),
        acceptance_criteria: Some("acceptance".to_owned()),
        notes: Some("notes".to_owned()),
        status: Status::Open,
        priority: Priority::HIGH,
        issue_type: IssueType::Task,
        owner: Some("owner".to_owned()),
        created_at: now,
        created_by: Some("creator".to_owned()),
        updated_at: now,
        closed_at: Some(now),
        close_reason: Some("done".to_owned()),
        defer_until: Some(now),
        external_ref: Some("github:org/repo#1".to_owned()),
        source_repo: Some("repo".to_owned()),
        deleted_at: Some(now),
        deleted_by: Some("deleter".to_owned()),
        delete_reason: Some("reason".to_owned()),
        original_type: Some("bug".to_owned()),
        former_ids: vec!["shape-0".to_owned()],
        labels: vec!["label".to_owned()],
        dependencies: vec![fully_populated_dependency()],
        comments: vec![sample_comment()],
    }
}

fn fully_populated_dependency() -> Dependency {
    Dependency {
        issue_id: "shape-1".to_owned(),
        depends_on_id: "shape-2".to_owned(),
        dep_type: DependencyType::Blocks,
        created_at: Utc::now(),
        created_by: Some("creator".to_owned()),
        metadata: Some("{}".to_owned()),
        thread_id: Some("thread".to_owned()),
    }
}

fn sample_comment() -> Comment {
    Comment {
        id: 1,
        issue_id: "shape-1".to_owned(),
        author: "author".to_owned(),
        body: "body".to_owned(),
        created_at: Utc::now(),
    }
}

fn sorted_keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("serialized issue is a JSON object")
        .keys()
        .cloned()
        .collect();
    keys.sort_unstable();
    keys
}

#[test]
fn serialized_issue_emits_exactly_the_external_consumer_field_set() {
    let value = serde_json::to_value(fully_populated_issue()).expect("issue serializes");

    assert_eq!(
        sorted_keys(&value),
        owned_names(EXPECTED_JSONL_KEYS),
        "the serialized field set of `Issue` changed — an external consumer parses \
         issues.jsonl directly, so adding, removing, or renaming a key breaks it"
    );
}

#[test]
fn serialized_dependency_and_comment_emit_their_wire_field_sets() {
    let dependency =
        serde_json::to_value(fully_populated_dependency()).expect("dependency serializes");
    assert_eq!(
        sorted_keys(&dependency),
        owned_names(EXPECTED_DEPENDENCY_KEYS),
        "a `dependencies[]` entry's field set changed"
    );

    let comment = serde_json::to_value(sample_comment()).expect("comment serializes");
    assert_eq!(
        sorted_keys(&comment),
        owned_names(EXPECTED_COMMENT_KEYS),
        "a `comments[]` entry's field set changed"
    );
}

#[test]
fn serialized_empty_issue_still_emits_the_unconditional_field_set() {
    let value = serde_json::to_value(Issue::default()).expect("issue serializes");

    assert_eq!(
        sorted_keys(&value),
        owned_names(ALWAYS_SERIALIZED_JSONL_KEYS),
        "the set of always-emitted issue keys changed — a key moving in or out \
         of this set changes what a consumer may assume is present"
    );
}

#[test]
fn issues_jsonl_written_by_br_stays_inside_the_declared_field_set() {
    let workspace = init_workspace();

    // `br create` auto-flushes to issues.jsonl, so no separate export step.
    let create = run_br(
        &workspace,
        [
            "create",
            "--title",
            "shape probe",
            "--type",
            "task",
            "--json",
        ],
        "create",
    );
    assert!(
        create.status.success(),
        "create failed: stdout={} stderr={}",
        create.stdout,
        create.stderr
    );

    let jsonl = std::fs::read_to_string(workspace.root.join(".beads").join("issues.jsonl"))
        .expect("issues.jsonl is written by br create's auto-flush");
    let line = jsonl
        .lines()
        .find(|line| !line.trim().is_empty())
        .expect("issues.jsonl has at least one issue line");
    let issue: Value = serde_json::from_str(line).expect("each issues.jsonl line is valid JSON");

    let keys = sorted_keys(&issue);
    // The file's key set is exactly `Issue`'s own declared field set: the
    // record is the struct's derived serialization, nothing added.
    let declared: Vec<String> = owned_names(EXPECTED_JSONL_KEYS);
    let unexpected: Vec<&String> = keys.iter().filter(|key| !declared.contains(key)).collect();
    assert!(
        unexpected.is_empty(),
        "br wrote issue keys that are not part of the declared field set: {unexpected:?}"
    );

    for key in ALWAYS_SERIALIZED_JSONL_KEYS {
        assert!(
            keys.iter().any(|k| k == key),
            "br omitted `{key}`, which must always be present in issues.jsonl"
        );
    }
}

#[test]
fn exported_records_carry_no_format_version_key() {
    let workspace = init_workspace();
    let create = run_br(
        &workspace,
        ["create", "--title", "Marker check", "--silent"],
        "create",
    );
    assert!(
        create.status.success(),
        "create failed: stdout={} stderr={}",
        create.stdout,
        create.stderr
    );

    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert!(
        flush.status.success(),
        "sync --flush-only failed: stdout={} stderr={}",
        flush.stdout,
        flush.stderr
    );

    let jsonl = std::fs::read_to_string(workspace.root.join(".beads").join("issues.jsonl"))
        .expect("read issues.jsonl");
    let line = jsonl
        .lines()
        .find(|line| !line.trim().is_empty())
        .expect("issues.jsonl has at least one issue line");
    let record: Value = serde_json::from_str(line).expect("record parses");

    assert!(
        record.get("format_version").is_none(),
        "format_version was removed but still appears in an exported record: {record}"
    );
}

/// Criterion: a record carrying keys `Issue` does not model imports cleanly
/// and those keys do not survive a round trip through the database.
///
/// This is the invariant the whole removal in bds-b4f.1.1 rests on, restored
/// from the deleted `tests/storage/jsonl_format_version.rs` (see
/// `git show 098b379:tests/storage/jsonl_format_version.rs`) after that file
/// went out with the interchange-marker mechanism it was written to pin —
/// carrying `format_version` was only one of several things it tested, and
/// this one is not about the marker at all. It survives the marker's removal
/// unchanged: an unmodelled key, whether it is a leftover `format_version`
/// from a pre-removal file or any other foreign field, is simply dropped by
/// `Issue`'s ordinary derive-`Deserialize` behaviour on import, and is
/// therefore absent the next time the workspace exports. That is the entire
/// "an old file still imports" claim the removal depends on, and every task
/// after this one that deletes an `Issue` field depends on it holding for the
/// field it removes.
#[test]
fn a_record_with_an_unrecognised_key_imports_cleanly_and_the_key_is_dropped_on_export() {
    let workspace = init_workspace();
    let create = run_br(
        &workspace,
        ["create", "--title", "foreign key probe", "--type", "task"],
        "create",
    );
    assert!(
        create.status.success(),
        "create failed: stdout={} stderr={}",
        create.stdout,
        create.stderr
    );

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let jsonl = std::fs::read_to_string(&jsonl_path).expect("read issues.jsonl");
    let line = jsonl
        .lines()
        .find(|line| !line.trim().is_empty())
        .expect("issues.jsonl has at least one issue line");
    let mut record: Value = serde_json::from_str(line).expect("record parses");

    let object = record.as_object_mut().expect("issue record is an object");
    // A leftover marker from a pre-removal file, and an unrelated foreign
    // field no build of `br` has ever modelled -- both are unrecognised keys
    // to this build, and both must be handled identically: dropped, not
    // refused and not carried through.
    object.insert("format_version".to_string(), Value::from(1));
    object.insert("lease_granted_node".to_string(), Value::from("node-7"));
    // Move the clock forward so the importer applies this record as an
    // update rather than skipping it as no-newer-than what's already in the
    // database: the point under test is what survives an *applied* record,
    // not whether this one was applied at all.
    object.insert(
        "updated_at".to_string(),
        Value::from("2099-01-01T00:00:00Z"),
    );
    std::fs::write(&jsonl_path, format!("{record}\n")).expect("write edited issues.jsonl");

    let import = run_br(&workspace, ["sync", "--import-only"], "import");
    assert!(
        import.status.success(),
        "a record with an unrecognised key must still import: stdout={} stderr={}",
        import.stdout,
        import.stderr
    );
    assert!(
        !import.stderr.contains("WARN"),
        "importing a record with an unrecognised key must not warn (RUST_LOG=beads=warn \
         is set for this run, so any warning would appear here): {}",
        import.stderr
    );

    // Importing an already-current file leaves nothing dirty, so an
    // unforced flush would be a no-op ("Nothing to export (no dirty
    // issues)") and would leave the hand-edited file on disk unchanged --
    // which would make this test pass for the wrong reason, having asserted
    // nothing about what the importer actually did to the record. `--force`
    // makes the export unconditional, which is what actually exercises "the
    // key is dropped by the next export".
    let flush = run_br(&workspace, ["sync", "--flush-only", "--force"], "flush");
    assert!(
        flush.status.success(),
        "sync --flush-only --force failed: stdout={} stderr={}",
        flush.stdout,
        flush.stderr
    );

    let round_tripped = std::fs::read_to_string(&jsonl_path).expect("read issues.jsonl");
    let line = round_tripped
        .lines()
        .find(|line| !line.trim().is_empty())
        .expect("issues.jsonl has at least one issue line");
    let exported: Value = serde_json::from_str(line).expect("record parses");

    assert!(
        exported.get("lease_granted_node").is_none(),
        "an unrecognised key must be dropped on import, not carried through to the \
         next export: {exported}"
    );
    assert!(
        exported.get("format_version").is_none(),
        "a leftover format_version key must be dropped like any other unrecognised \
         key, not resurrected on export: {exported}"
    );
}
