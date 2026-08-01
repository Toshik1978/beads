//! Storage engine adapter: the previous engine's API shape, implemented on `rusqlite`.
//!
//! # Why this module exists
//!
//! `beads` was written against a pure-Rust SQLite implementation that could
//! only be built on an unstable toolchain (removed in `bds-04l.4.2`; NOTICE.md
//! and `.superpowers/sdd/bds-04l/` record which one, and every measurement
//! quoted below). Replacing it with `rusqlite` (C SQLite) is what puts the
//! project on stable Rust. The *API surface* `beads`
//! actually uses is narrow — twelve `Connection` methods, two `Row`
//! accessors, `SqliteValue` and an error enum — but the *call sites* are not:
//! roughly 1,600 of them across 26 files, ~1,500 of which name `Connection`,
//! `Row` or `SqliteValue` directly. `src/storage/sqlite.rs` alone is ~26,000
//! lines.
//!
//! Rewriting those idiomatically would put an enormous diff through the
//! highest-risk file in the project, and would fold the engine swap and a
//! large refactor into a single unreviewable change. This module instead
//! reproduces its call shape on top of `rusqlite`, so the engine
//! swap is an import-only diff and every semantic difference between the two
//! engines is concentrated here, where it is unit-tested against a real
//! database.
//!
//! **This is a permanent, documented adapter boundary, not a temporary hack.**
//! It costs a small non-idiomatic surface (`&[SqliteValue]` parameter slices,
//! `Vec<Row>` materialization, `Option<&SqliteValue>` accessors) in exchange
//! for making the engine swap reviewable. Code added from here on should
//! prefer this module's API for consistency; a future task may narrow it.
//!
//! # Behavioral differences from the previous engine, established empirically
//!
//! ## Value coercion
//!
//! [`SqliteValue`] is a faithful copy of the previous engine's value type: one
//! variant per SQLite *storage class*, and [`SqliteValue::as_text`] /
//! [`SqliteValue::as_integer`] are **strict variant matches, never coercions**
//! — exactly as in the previous engine. Reading each of the five storage classes back
//! out of a real column through this shim gives:
//!
//! | Stored value (column declared `BLOB`, i.e. no affinity) | Read back as | `as_text()` | `as_integer()` |
//! |---|---|---|---|
//! | `NULL`      | `SqliteValue::Null`       | `None`          | `None`      |
//! | `42`        | `SqliteValue::Integer(42)`| `None`          | `Some(42)`  |
//! | `1.5`       | `SqliteValue::Float(1.5)` | `None`          | `None`      |
//! | `'42'`      | `SqliteValue::Text("42")` | `Some("42")`    | `None`      |
//! | `x'2a'`     | `SqliteValue::Blob([42])` | `None`          | `None`      |
//!
//! The trap this closes: `as_integer()` on a TEXT column returns `None`, and
//! `as_text()` on an INTEGER column returns `None`. Neither coerces. Column
//! *affinity* still applies on the way in — a value inserted into a column
//! declared `INTEGER` is stored as an integer if it looks like one — so the
//! storage class you read back is decided by the column's declared type, not
//! by this module. `tests::coercion_table_for_all_five_storage_classes`
//! pins both halves.
//!
//! ## `execute` and statements that return rows
//!
//! `rusqlite::Connection::execute` refuses to run a statement that produces
//! rows (`Error::ExecuteReturnedResults`); the previous engine's did not. 52 call
//! sites pass a PRAGMA or a SELECT to `execute`. Measured against SQLite
//! 3.50, these PRAGMA setter forms **do** return a row:
//!
//! ```text
//! PRAGMA journal_mode = WAL      -> 'wal'
//! PRAGMA busy_timeout = 5000     -> '5000'
//! PRAGMA journal_size_limit = N  -> 'N'
//! PRAGMA wal_autocheckpoint = N  -> 'N'
//! PRAGMA wal_checkpoint(TRUNCATE)-> '0|0|0'
//! ```
//!
//! and these do not: `foreign_keys = ON`, `user_version = N`,
//! `synchronous = NORMAL`, `temp_store = MEMORY`, `cache_size = N`.
//!
//! [`Connection::execute`] therefore prepares the statement, drains whatever
//! rows it yields, and returns a count with the same meaning the previous engine gave
//! it: **affected rows for DML, result-row count for everything else**. A
//! row-returning PRAGMA is not an error; its row is consumed and discarded.
//! Callers that need the returned value must use [`Connection::query_row`].
//!
//! Getting that count right needs both of SQLite's change counters, and
//! neither one alone is correct:
//!
//! - `rusqlite::Statement::execute` returns `sqlite3_changes()`, which is
//!   **connection-level and sticky** — DDL, PRAGMAs and transaction control do
//!   not reset it, so `execute("CREATE TABLE …")` immediately after a
//!   three-row `INSERT` reports **3** where the previous engine reported 0.
//! - A `sqlite3_total_changes()` delta fixes that, but **counts foreign-key
//!   actions and trigger rows**, which `changes()` excludes. `beads` declares
//!   15 `ON DELETE CASCADE` foreign keys and enables `PRAGMA foreign_keys`, so
//!   deleting one parent with three children would report **4** where
//!   the previous engine reported 1 — missing parity on exactly the statement class that
//!   matters.
//!
//! This module therefore uses the delta only as a gate ("did anything
//! change?") and takes the count from `changes()`. See `run_statement`, and
//! `tests::ddl_and_silent_pragmas_report_zero_even_after_dml_on_the_same_connection`
//! for the regression guard, which covers both halves.
//!
//! Note the consequence for `PRAGMA journal_mode = WAL`: `execute` tells you
//! the statement ran, not that WAL is on. SQLite reports the *resulting*
//! mode in the row rather than failing, so the only way to know is to read
//! `PRAGMA journal_mode` back.
//!
//! ## Multiple statements
//!
//! The previous engine's `execute` accepted several `;`-separated statements.
//! `rusqlite::Connection::prepare` rejects a trailing statement with
//! `Error::MultipleStatement`, which this module surfaces as
//! [`DbError::Internal`] rather than silently running only the first one.
//! Use `schema::execute_batch` for multi-statement SQL: it splits the
//! script and runs each statement, reporting the one that failed.
//!
//! ## Which recovery-triggering errors real SQLite can actually produce
//!
//! `should_attempt_jsonl_recovery` (`src/config/mod.rs`) rebuilds the database
//! from JSONL when it sees one of six variants. Driven from real conditions
//! against a real file, C SQLite produces four of them and cannot produce two:
//!
//! | Variant | Producible? | Real condition |
//! |---|---|---|
//! | `DatabaseCorrupt` | yes | scribbling on a b-tree page; truncating the file; a duplicated `sqlite_schema` row |
//! | `NotADatabase`    | yes | opening a file that is not a database |
//! | `TableExists`     | yes | `CREATE TABLE` run twice |
//! | `IndexExists`     | yes | `CREATE INDEX` run twice |
//! | `ShortRead`       | **no** | the pager absorbs `SQLITE_IOERR_SHORT_READ`; a truncated file reports corruption instead |
//! | `WalCorrupt`      | **no** | there is no such result code; a damaged `-wal` is silently discarded |
//!
//! Both unreachable variants are kept: the classifier still matches them, and
//! the conditions they stood for now arrive as `DatabaseCorrupt`, which is in
//! the same recovery set. They are dead arms, not broken ones.
//!
//! `WalCorrupt`'s condition deserves a second look, because "not producible"
//! understates it: a damaged `-wal` is not merely unreported, it is
//! *discarded*, and every committed-but-uncheckpointed row in it disappears
//! with no error. `tests::a_damaged_wal_sidecar_silently_loses_its_uncheckpointed_rows`
//! demonstrates exactly that. Detecting it needs something outside the
//! engine's error path.
//!
//! ## Open-time validation, and the busy timeout that comes with it
//!
//! `rusqlite::Connection::open` succeeds against any file — SQLite does not
//! read the header until the first statement — whereas the previous engine validated at
//! open. Because `should_attempt_jsonl_recovery` in `src/config/mod.rs`
//! classifies *the error returned by open*, this module runs a
//! `PRAGMA user_version` probe as part of [`Connection::open`] so that
//! `NotADatabase` and `DatabaseCorrupt` surface there, as they did before.
//!
//! That probe is safe under concurrency only because `rusqlite` installs a
//! **default 5-second busy timeout at open**
//! (`rusqlite-0.39.0/src/inner_connection.rs:118`, `sqlite3_busy_timeout(db,
//! 5000)`).  The previous engine installed none. This is a behavior change everywhere
//! `beads` does not set `PRAGMA busy_timeout` itself: contended opens and
//! reads now block for up to 5s instead of failing fast with
//! [`DbError::Busy`]. Nothing here overrides it, because the probe would
//! otherwise be a new spurious failure mode; tests that *want* `SQLITE_BUSY`
//! must set `PRAGMA busy_timeout = 0` explicitly, as
//! `tests::a_real_busy_error_maps_to_busy_and_is_transient` does.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::ToSql;
use rusqlite::types::{ToSqlOutput, ValueRef};
use thiserror::Error;

/// Result alias for every operation in this module.
pub type Result<T> = std::result::Result<T, DbError>;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Database error, mirroring the variants of the previous engine's error enum
/// that `beads` matches on or constructs.
///
/// The variant *shapes* are load-bearing: `should_attempt_jsonl_recovery`
/// (`src/config/mod.rs`) decides whether to rebuild the database from JSONL by
/// matching six of them, and `src/error/mod.rs` delegates `is_transient()`
/// here. Collapsing variants would silently disable database recovery.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum DbError {
    // === Database errors ===
    /// Database file is locked by another process.
    #[error("database is locked: '{path}'")]
    DatabaseLocked { path: PathBuf },

    /// Database file is corrupt (`SQLITE_CORRUPT`).
    #[error("database disk image is malformed: {detail}")]
    DatabaseCorrupt { detail: String },

    /// File is not a valid SQLite database (`SQLITE_NOTADB`).
    #[error("file is not a database: '{path}'")]
    NotADatabase { path: PathBuf },

    /// Database is full (`SQLITE_FULL`).
    #[error("database is full")]
    DatabaseFull,

    /// Schema changed since the statement was prepared (`SQLITE_SCHEMA`).
    #[error("database schema has changed")]
    SchemaChanged,

    // === I/O errors ===
    /// File I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Short read (`SQLITE_IOERR_SHORT_READ`).
    ///
    /// **Kept for the JSONL-recovery matcher, but unreachable under C
    /// SQLite**: the pager absorbs `SQLITE_IOERR_SHORT_READ` by zero-filling
    /// the partial page, and a truncated database surfaces as
    /// [`DbError::DatabaseCorrupt`] instead. See
    /// `tests::truncating_a_database_reports_corruption_not_short_read`.
    /// The two byte counts are therefore always zero when the variant comes
    /// from the engine.
    #[error("short read: expected {expected} bytes, got {actual}")]
    ShortRead { expected: usize, actual: usize },

    // === SQL errors ===
    /// SQL syntax error. The `Display` text is byte-identical to SQLite's.
    #[error("near \"{token}\": syntax error")]
    SyntaxError { token: String },

    /// Query executed successfully but produced no rows.
    #[error("query returned no rows")]
    QueryReturnedNoRows,

    /// Query executed successfully but produced more than one row.
    #[error("query returned more than one row")]
    QueryReturnedMultipleRows,

    /// No such table.
    #[error("no such table: {name}")]
    NoSuchTable { name: String },

    /// No such column.
    #[error("no such column: {name}")]
    NoSuchColumn { name: String },

    /// Table already exists.
    #[error("table {name} already exists")]
    TableExists { name: String },

    /// Index already exists.
    #[error("index {name} already exists")]
    IndexExists { name: String },

    // === Constraint errors ===
    /// UNIQUE constraint violation (`SQLITE_CONSTRAINT_UNIQUE`).
    #[error("UNIQUE constraint failed: {columns}")]
    UniqueViolation { columns: String },

    /// NOT NULL constraint violation (`SQLITE_CONSTRAINT_NOTNULL`).
    #[error("NOT NULL constraint failed: {column}")]
    NotNullViolation { column: String },

    /// CHECK constraint violation (`SQLITE_CONSTRAINT_CHECK`).
    #[error("CHECK constraint failed: {name}")]
    CheckViolation { name: String },

    /// FOREIGN KEY constraint violation (`SQLITE_CONSTRAINT_FOREIGNKEY`).
    #[error("FOREIGN KEY constraint failed")]
    ForeignKeyViolation,

    /// PRIMARY KEY constraint violation (`SQLITE_CONSTRAINT_PRIMARYKEY`).
    #[error("PRIMARY KEY constraint failed")]
    PrimaryKeyViolation,

    // === Busy / contention ===
    /// Database is busy (`SQLITE_BUSY`).
    #[error("database is busy")]
    Busy,

    /// Database is busy because WAL recovery is in progress
    /// (`SQLITE_BUSY_RECOVERY`).
    #[error("database is busy (recovery in progress)")]
    BusyRecovery,

    // === Limits ===
    /// String or BLOB exceeds the size limit (`SQLITE_TOOBIG`).
    #[error("string or BLOB exceeds size limit")]
    TooBig,

    // === WAL ===
    /// WAL file is corrupt.
    ///
    /// **Kept for the JSONL-recovery matcher, but unreachable under C
    /// SQLite**: there is no result code for it. A `-wal` whose header no
    /// longer checksums is treated as an empty WAL and discarded, taking every
    /// committed-but-uncheckpointed row with it, without an error. See
    /// `tests::a_damaged_wal_sidecar_silently_loses_its_uncheckpointed_rows`.
    #[error("WAL file is corrupt: {detail}")]
    WalCorrupt { detail: String },

    // === VFS ===
    /// Cannot open the database file (`SQLITE_CANTOPEN`).
    #[error("unable to open database file: '{path}'")]
    CannotOpen { path: PathBuf },

    // === Misc ===
    /// Attempt to write a read-only database (`SQLITE_READONLY`).
    #[error("attempt to write a readonly database")]
    ReadOnly,

    /// Interrupted (`SQLITE_INTERRUPT`).
    #[error("interrupted")]
    Interrupt,

    /// Out of memory (`SQLITE_NOMEM`).
    #[error("out of memory")]
    OutOfMemory,

    /// Anything the mapping above does not classify, carrying SQLite's own
    /// message.
    ///
    /// This is not only a fallback: `src/config/mod.rs` and
    /// `src/cli/commands/mod.rs` construct it directly, and
    /// `is_recoverable_database_internal_error` inspects its text.
    #[error("internal error: {0}")]
    Internal(String),
}

impl DbError {
    /// Returns `true` if the error is transient and the operation can be
    /// retried.
    ///
    /// `src/error/mod.rs` delegates `BeadsError::is_transient` to this.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Busy | Self::BusyRecovery | Self::DatabaseLocked { .. }
        )
    }
}

/// Pull a quoted or trailing identifier out of a SQLite message such as
/// `table foo already exists` / `index foo already exists`.
fn identifier_between(msg: &str, prefix: &str, suffix: &str) -> Option<String> {
    let rest = msg.strip_prefix(prefix)?;
    let name = rest.strip_suffix(suffix)?;
    Some(name.trim().to_string())
}

/// Classify a `SQLITE_ERROR`-class message, which SQLite reports as free text
/// rather than as a distinct result code.
///
/// The prefixes below are SQLite's own wording, checked against the bundled
/// amalgamation; each one exists because `beads` matches the resulting variant
/// (or, for `no such column`, matches the `Display` text in
/// `src/storage/schema.rs`).
fn classify_generic_message(msg: &str) -> DbError {
    if let Some(name) = identifier_between(msg, "table ", " already exists") {
        return DbError::TableExists { name };
    }
    if let Some(name) = identifier_between(msg, "index ", " already exists") {
        return DbError::IndexExists { name };
    }
    if let Some(name) = msg.strip_prefix("no such table: ") {
        return DbError::NoSuchTable {
            name: name.to_string(),
        };
    }
    if let Some(name) = msg.strip_prefix("no such column: ") {
        return DbError::NoSuchColumn {
            name: name.to_string(),
        };
    }
    if let Some(token) = identifier_between(msg, "near \"", "\": syntax error") {
        return DbError::SyntaxError { token };
    }
    DbError::Internal(msg.to_string())
}

/// Map a `SQLITE_CONSTRAINT` failure onto the specific violation, using the
/// *extended* result code. Without extended codes UNIQUE and PRIMARY KEY are
/// indistinguishable; `src/storage/sqlite.rs` and `src/sync/mod.rs` both
/// branch on that distinction.
fn classify_constraint(extended: i32, msg: &str) -> DbError {
    match extended {
        rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => DbError::UniqueViolation {
            columns: msg
                .strip_prefix("UNIQUE constraint failed: ")
                .unwrap_or(msg)
                .to_string(),
        },
        rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => DbError::PrimaryKeyViolation,
        rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => DbError::ForeignKeyViolation,
        rusqlite::ffi::SQLITE_CONSTRAINT_NOTNULL => DbError::NotNullViolation {
            column: msg
                .strip_prefix("NOT NULL constraint failed: ")
                .unwrap_or(msg)
                .to_string(),
        },
        rusqlite::ffi::SQLITE_CONSTRAINT_CHECK => DbError::CheckViolation {
            name: msg
                .strip_prefix("CHECK constraint failed: ")
                .unwrap_or(msg)
                .to_string(),
        },
        _ => DbError::Internal(msg.to_string()),
    }
}

/// Translate a `rusqlite::Error` into the [`DbError`] shape callers expect.
///
/// `path` is the database path the connection was opened with; SQLite does
/// not repeat it in path-shaped errors, but these variants carry it.
fn map_error(err: &rusqlite::Error, path: &str) -> DbError {
    use rusqlite::ffi::ErrorCode;

    let (ffi_err, msg) = match err {
        rusqlite::Error::QueryReturnedNoRows => return DbError::QueryReturnedNoRows,
        rusqlite::Error::QueryReturnedMoreThanOneRow => {
            return DbError::QueryReturnedMultipleRows;
        }
        rusqlite::Error::MultipleStatement => {
            return DbError::Internal(
                "multiple statements provided to execute; use schema::execute_batch".to_string(),
            );
        }
        rusqlite::Error::ExecuteReturnedResults => {
            return DbError::Internal("execute returned results".to_string());
        }
        rusqlite::Error::SqliteFailure(e, m) => (*e, m.clone().unwrap_or_default()),
        rusqlite::Error::SqlInputError { error, msg, .. } => (*error, msg.clone()),
        other => return DbError::Internal(other.to_string()),
    };

    let extended = ffi_err.extended_code;
    match ffi_err.code {
        ErrorCode::DatabaseCorrupt => DbError::DatabaseCorrupt { detail: msg },
        ErrorCode::NotADatabase => DbError::NotADatabase { path: path.into() },
        ErrorCode::CannotOpen => DbError::CannotOpen { path: path.into() },
        ErrorCode::DiskFull => DbError::DatabaseFull,
        ErrorCode::SchemaChanged => DbError::SchemaChanged,
        ErrorCode::DatabaseBusy => {
            if extended == rusqlite::ffi::SQLITE_BUSY_RECOVERY {
                DbError::BusyRecovery
            } else {
                DbError::Busy
            }
        }
        ErrorCode::DatabaseLocked => DbError::DatabaseLocked { path: path.into() },
        ErrorCode::ConstraintViolation => classify_constraint(extended, &msg),
        ErrorCode::SystemIoFailure => {
            if extended == rusqlite::ffi::SQLITE_IOERR_SHORT_READ {
                DbError::ShortRead {
                    expected: 0,
                    actual: 0,
                }
            } else {
                DbError::Internal(msg)
            }
        }
        ErrorCode::TooBig => DbError::TooBig,
        ErrorCode::ReadOnly => DbError::ReadOnly,
        ErrorCode::OperationInterrupted => DbError::Interrupt,
        ErrorCode::OutOfMemory => DbError::OutOfMemory,
        ErrorCode::Unknown => classify_generic_message(&msg),
        _ => DbError::Internal(msg),
    }
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// A SQLite dynamically-typed value: one variant per storage class.
///
/// Layout-compatible with the previous engine's value type at the source level:
/// `Text` and `Blob` carry `Arc` payloads so cloning is O(1), and pattern
/// matches such as `SqliteValue::Text(s) => s.as_ref()` keep working.
#[derive(Debug, Clone, PartialEq)]
pub enum SqliteValue {
    /// A NULL value.
    Null,
    /// A signed 64-bit integer.
    Integer(i64),
    /// A 64-bit IEEE floating point number.
    Float(f64),
    /// A UTF-8 text string.
    Text(Arc<str>),
    /// A binary large object.
    Blob(Arc<[u8]>),
}

impl SqliteValue {
    /// Extract a text reference.
    ///
    /// **Strict**: returns `Some` only for [`SqliteValue::Text`]. An integer
    /// column does not coerce to text. This matches the previous engine.
    #[inline]
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Extract an integer value.
    ///
    /// **Strict**: returns `Some` only for [`SqliteValue::Integer`]. A text
    /// column holding `"42"` does not coerce. This matches the previous engine.
    #[inline]
    #[must_use]
    pub const fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// Build a value from a borrowed `rusqlite` value, one storage class to
    /// one variant.
    ///
    /// SQLite guarantees TEXT is valid UTF-8 only when it was written as
    /// UTF-8; a database written by another tool can hold invalid sequences,
    /// so this replaces them rather than failing the whole query.
    fn from_value_ref(value: ValueRef<'_>) -> Self {
        match value {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(i) => Self::Integer(i),
            ValueRef::Real(f) => Self::Float(f),
            ValueRef::Text(bytes) => Self::Text(String::from_utf8_lossy(bytes).into()),
            ValueRef::Blob(bytes) => Self::Blob(Arc::from(bytes)),
        }
    }
}

impl ToSql for SqliteValue {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Self::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            Self::Integer(i) => ToSqlOutput::Borrowed(ValueRef::Integer(*i)),
            Self::Float(f) => ToSqlOutput::Borrowed(ValueRef::Real(*f)),
            Self::Text(s) => ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes())),
            Self::Blob(b) => ToSqlOutput::Borrowed(ValueRef::Blob(b)),
        })
    }
}

impl From<i64> for SqliteValue {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<i32> for SqliteValue {
    fn from(value: i32) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<f64> for SqliteValue {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

impl From<String> for SqliteValue {
    fn from(value: String) -> Self {
        Self::Text(value.into())
    }
}

impl From<&str> for SqliteValue {
    fn from(value: &str) -> Self {
        Self::Text(value.into())
    }
}

impl From<Arc<str>> for SqliteValue {
    fn from(value: Arc<str>) -> Self {
        Self::Text(value)
    }
}

impl From<Vec<u8>> for SqliteValue {
    fn from(value: Vec<u8>) -> Self {
        Self::Blob(value.into())
    }
}

impl From<&[u8]> for SqliteValue {
    fn from(value: &[u8]) -> Self {
        Self::Blob(Arc::from(value))
    }
}

impl From<Arc<[u8]>> for SqliteValue {
    fn from(value: Arc<[u8]>) -> Self {
        Self::Blob(value)
    }
}

/// `None` becomes SQL NULL. This blanket impl is what lets 700+ call sites
/// write `SqliteValue::from(issue.owner.as_deref())`.
impl<T: Into<Self>> From<Option<T>> for SqliteValue {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// One materialized result row.
///
/// Queries return `Vec<Row>` rather than a streaming cursor because that is
/// the shape ~1,600 call sites expect. It also sidesteps `rusqlite`'s
/// `Statement`/`Rows` lifetime chain, which would otherwise leak into every
/// caller's signature.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    values: Vec<SqliteValue>,
}

impl Row {
    /// All column values, in declaration order.
    #[inline]
    #[must_use]
    pub fn values(&self) -> &[SqliteValue] {
        &self.values
    }

    /// The value at `index`, or `None` if the row has no such column.
    ///
    /// Returning `Option<&SqliteValue>` (rather than `rusqlite`'s
    /// `Result<T, Error>`) is what makes the dominant idiom
    /// `row.get(0).and_then(SqliteValue::as_text)` compile unchanged.
    #[inline]
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&SqliteValue> {
        self.values.get(index)
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// A database connection.
///
/// The inner handle is an `Option` solely so that
/// [`Connection::close_in_place`] can hand ownership to `rusqlite`'s
/// `close(self)` from behind a `&mut self`. It is `Some` for the whole
/// lifetime of a normally-used connection; every accessor goes through
/// [`Connection::handle`], which reports a closed connection as an error
/// rather than panicking.
pub struct Connection {
    inner: Option<rusqlite::Connection>,
    path: String,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `rusqlite::Connection` has no useful Debug output, so the path is
        // the whole of it.
        f.debug_struct("Connection")
            .field("path", &self.path)
            .field("open", &self.inner.is_some())
            .finish_non_exhaustive()
    }
}

/// Drain every row a prepared statement yields, returning them.
fn collect_rows(
    stmt: &mut rusqlite::Statement<'_>,
    params: &[SqliteValue],
    path: &str,
) -> Result<Vec<Row>> {
    let column_count = stmt.column_count();
    let mut out = Vec::new();
    let mut rows = stmt
        .query(rusqlite::params_from_iter(params.iter()))
        .map_err(|e| map_error(&e, path))?;
    while let Some(row) = rows.next().map_err(|e| map_error(&e, path))? {
        let mut values = Vec::with_capacity(column_count);
        for i in 0..column_count {
            let value = row.get_ref(i).map_err(|e| map_error(&e, path))?;
            values.push(SqliteValue::from_value_ref(value));
        }
        out.push(Row { values });
    }
    Ok(out)
}

/// Run a prepared statement for its effect, returning the count the previous
/// engine returned: affected rows for DML, result-row count otherwise.
///
/// The no-result-set branch needs both of SQLite's change counters, because
/// neither alone reproduces it:
///
/// - `sqlite3_changes()` — what `rusqlite::Statement::execute` returns — is the
///   row count of the most recent statement that *changed* something. It is
///   **connection-level and sticky**: DDL, PRAGMAs and transaction control do
///   not reset it, so `execute("CREATE TABLE …")` right after a three-row
///   `INSERT` reports 3 where the previous engine reported 0.
/// - `sqlite3_total_changes()` is monotonic, so a delta around the statement is
///   correctly 0 for DDL and PRAGMAs — but it **counts rows changed by foreign
///   key actions and by triggers**, which `changes()` excludes. `beads`
///   declares 15 `ON DELETE CASCADE` foreign keys in `src/storage/schema.rs`
///   and turns `PRAGMA foreign_keys = ON` on at `schema.rs:571`, so deleting
///   one parent with three children would report 4 where the previous engine reported 1.
///
/// So the delta is used only as a *gate* — "did this statement change
/// anything?" — and the count itself comes from `changes()`. That combination
/// is 0 for DDL/PRAGMA/transaction control and cascade-free for DML, which is
/// exactly the previous engine's `Connection::execute` contract. The gate also handles the
/// statement that changes nothing (`DELETE … WHERE` matching no row, `INSERT OR
/// IGNORE` that ignores): the delta is 0, so the stale `changes()` is never
/// consulted.
///
/// This diverges from `rusqlite`'s documented `Statement::execute` semantics on
/// purpose; the shim exists to make 343 `execute` / `execute_with_params` call
/// sites behave exactly as they did under the previous engine.
fn run_statement(
    conn: &rusqlite::Connection,
    stmt: &mut rusqlite::Statement<'_>,
    params: &[SqliteValue],
    path: &str,
) -> Result<usize> {
    if stmt.column_count() == 0 {
        // No result set: DML, DDL, or a PRAGMA setter that returns nothing.
        let before = conn.total_changes();
        stmt.execute(rusqlite::params_from_iter(params.iter()))
            .map_err(|e| map_error(&e, path))?;
        if conn.total_changes() == before {
            return Ok(0);
        }
        return Ok(usize::try_from(conn.changes()).unwrap_or(usize::MAX));
    }
    // Produces rows (SELECT, or a PRAGMA whose setter form echoes the value).
    // Drain them so the statement actually runs to completion, and report the
    // row count, which is what the previous engine's `Connection::execute` returns for
    // non-DML.
    let mut count = 0usize;
    let mut rows = stmt
        .query(rusqlite::params_from_iter(params.iter()))
        .map_err(|e| map_error(&e, path))?;
    while rows.next().map_err(|e| map_error(&e, path))?.is_some() {
        count += 1;
    }
    Ok(count)
}

fn exactly_one_row(rows: Vec<Row>) -> Result<Row> {
    let mut iter = rows.into_iter();
    let row = iter.next().ok_or(DbError::QueryReturnedNoRows)?;
    if iter.next().is_some() {
        return Err(DbError::QueryReturnedMultipleRows);
    }
    Ok(row)
}

impl Connection {
    /// Open (creating if necessary) the database at `path`.
    ///
    /// Unlike `rusqlite`, this validates the file at open time — see the
    /// module docs — so a non-database file or a corrupt header is reported
    /// here rather than at the first query.
    pub fn open(path: impl Into<String>) -> Result<Self> {
        let path = path.into();
        let inner = rusqlite::Connection::open(&path).map_err(|e| map_error(&e, &path))?;
        let conn = Self {
            inner: Some(inner),
            path,
        };
        conn.probe_header()?;
        Ok(conn)
    }

    /// The live `rusqlite` handle, or an error if the connection was already
    /// closed by [`Connection::close_in_place`].
    #[inline]
    fn handle(&self) -> Result<&rusqlite::Connection> {
        self.inner.as_ref().ok_or_else(|| {
            DbError::Internal(format!("connection to '{}' is already closed", self.path))
        })
    }

    /// Force SQLite to read the database header now, so that malformed files
    /// fail at `open` the way they did under the previous engine.
    fn probe_header(&self) -> Result<()> {
        let conn = self.handle()?;
        let mut stmt = conn
            .prepare("PRAGMA user_version")
            .map_err(|e| map_error(&e, &self.path))?;
        run_statement(conn, &mut stmt, &[], &self.path)?;
        Ok(())
    }

    /// Close the connection, reporting any error SQLite raises while doing so.
    pub fn close(mut self) -> Result<()> {
        let path = self.path.clone();
        match self.inner.take() {
            Some(conn) => conn.close().map_err(|(_, e)| map_error(&e, &path)),
            None => Ok(()),
        }
    }

    /// Close the connection without consuming the wrapper.
    ///
    /// Exists for `impl Drop for SqliteStorage` (`src/storage/sqlite.rs`),
    /// which closes the connection *before* unlinking an ephemeral temp
    /// database and its WAL/SHM sidecars (#299). `Drop` only has `&mut self`,
    /// so the consuming [`Connection::close`] cannot be called there, and
    /// relying on the field's own drop would reverse that ordering — the files
    /// would be unlinked while SQLite still held them open.
    ///
    /// Idempotent: closing an already-closed connection is `Ok(())`. Every
    /// other method returns [`DbError::Internal`] afterwards.
    pub fn close_in_place(&mut self) -> Result<()> {
        match self.inner.take() {
            Some(conn) => conn.close().map_err(|(_, e)| map_error(&e, &self.path)),
            None => Ok(()),
        }
    }

    /// Prepare and run a single statement, returning affected rows for DML and
    /// the result-row count otherwise.
    ///
    /// Rows produced by the statement (a `SELECT`, or a PRAGMA setter that
    /// echoes its value) are drained and discarded rather than treated as an
    /// error.
    pub fn execute(&self, sql: &str) -> Result<usize> {
        self.execute_with_params(sql, &[])
    }

    /// [`Connection::execute`] with positional parameters.
    pub fn execute_with_params(&self, sql: &str, params: &[SqliteValue]) -> Result<usize> {
        let conn = self.handle()?;
        let mut stmt = conn.prepare(sql).map_err(|e| map_error(&e, &self.path))?;
        run_statement(conn, &mut stmt, params, &self.path)
    }

    /// Run a query, materializing every row.
    pub fn query(&self, sql: &str) -> Result<Vec<Row>> {
        self.query_with_params(sql, &[])
    }

    /// [`Connection::query`] with positional parameters.
    pub fn query_with_params(&self, sql: &str, params: &[SqliteValue]) -> Result<Vec<Row>> {
        let mut stmt = self
            .handle()?
            .prepare(sql)
            .map_err(|e| map_error(&e, &self.path))?;
        collect_rows(&mut stmt, params, &self.path)
    }

    /// Run a query that must produce exactly one row.
    ///
    /// Returns [`DbError::QueryReturnedNoRows`] for an empty result and
    /// [`DbError::QueryReturnedMultipleRows`] for more than one — both are
    /// matched by name at call sites, so neither may become an `Option`.
    pub fn query_row(&self, sql: &str) -> Result<Row> {
        self.query(sql).and_then(exactly_one_row)
    }

    /// [`Connection::query_row`] with positional parameters.
    pub fn query_row_with_params(&self, sql: &str, params: &[SqliteValue]) -> Result<Row> {
        self.query_with_params(sql, params)
            .and_then(exactly_one_row)
    }

    /// Prepare a statement for repeated execution.
    ///
    /// The returned statement takes `&self` on every method — `rusqlite`'s
    /// takes `&mut self` — because call sites bind it once outside a loop and
    /// then call it through a shared reference. The `RefCell` inside is what
    /// bridges the two; it is never borrowed across a call boundary, so it
    /// cannot panic.
    pub fn prepare(&self, sql: &str) -> Result<PreparedStatement<'_>> {
        let stmt = self
            .handle()?
            .prepare(sql)
            .map_err(|e| map_error(&e, &self.path))?;
        Ok(PreparedStatement {
            stmt: RefCell::new(stmt),
            conn: self,
            sql: sql.to_string(),
        })
    }
}

/// A prepared statement bound to its [`Connection`].
pub struct PreparedStatement<'conn> {
    stmt: RefCell<rusqlite::Statement<'conn>>,
    conn: &'conn Connection,
    sql: String,
}

impl std::fmt::Debug for PreparedStatement<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedStatement")
            .field("sql", &self.sql)
            .finish_non_exhaustive()
    }
}

impl PreparedStatement<'_> {
    /// Run the statement as a query with positional parameters,
    /// materializing every row.
    pub fn query_with_params(&self, params: &[SqliteValue]) -> Result<Vec<Row>> {
        let mut stmt = self.stmt.borrow_mut();
        collect_rows(&mut stmt, params, &self.conn.path)
    }
}

// ---------------------------------------------------------------------------
// compat: open flags
// ---------------------------------------------------------------------------

/// Flag-based connection opening, mirroring the previous engine's flag-based open API.
pub mod compat {
    use super::{Connection, DbError, Result, map_error};

    /// The subset of SQLite open flags `beads` uses.
    ///
    /// The bit values are SQLite's own, so a mask assembled here means the
    /// same thing it would to `sqlite3_open_v2`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct OpenFlags(u32);

    impl OpenFlags {
        /// Open the database read-only.
        pub const SQLITE_OPEN_READ_ONLY: Self = Self(0x01);
        /// Open the database for reading and writing.
        pub const SQLITE_OPEN_READ_WRITE: Self = Self(0x02);
        /// Create the database if it does not exist.
        pub const SQLITE_OPEN_CREATE: Self = Self(0x04);
        /// Interpret the path as a URI.
        pub const SQLITE_OPEN_URI: Self = Self(0x40);
        /// Use full mutex protection.
        pub const SQLITE_OPEN_FULL_MUTEX: Self = Self(0x0001_0000);

        /// Whether every bit of `flag` is set.
        #[must_use]
        pub const fn contains(self, flag: Self) -> bool {
            self.0 & flag.0 == flag.0
        }

        fn to_rusqlite(self) -> Result<rusqlite::OpenFlags> {
            let read_only = self.contains(Self::SQLITE_OPEN_READ_ONLY);
            let read_write = self.contains(Self::SQLITE_OPEN_READ_WRITE);
            let create = self.contains(Self::SQLITE_OPEN_CREATE);

            let mut flags = rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
            match (read_only, read_write, create) {
                (true, false, false) => flags |= rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                (false, true, false) => flags |= rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
                (false, true, true) => {
                    flags |= rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                        | rusqlite::OpenFlags::SQLITE_OPEN_CREATE;
                }
                _ => {
                    return Err(DbError::Internal(format!(
                        "unsupported open flag combination: {:#x}",
                        self.0
                    )));
                }
            }
            if self.contains(Self::SQLITE_OPEN_URI) {
                flags |= rusqlite::OpenFlags::SQLITE_OPEN_URI;
            }
            if self.contains(Self::SQLITE_OPEN_FULL_MUTEX) {
                flags.remove(rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX);
                flags |= rusqlite::OpenFlags::SQLITE_OPEN_FULL_MUTEX;
            }
            Ok(flags)
        }
    }

    /// Open a connection with explicit flags.
    pub fn open_with_flags(path: &str, flags: OpenFlags) -> Result<Connection> {
        let rusqlite_flags = flags.to_rusqlite()?;
        let inner = rusqlite::Connection::open_with_flags(path, rusqlite_flags)
            .map_err(|e| map_error(&e, path))?;
        let conn = Connection {
            inner: Some(inner),
            path: path.to_string(),
        };
        conn.probe_header()?;
        Ok(conn)
    }
}

#[cfg(test)]
mod tests {
    use super::compat::{OpenFlags, open_with_flags};
    use super::{Connection, DbError, Row, SqliteValue};
    use std::io::{Seek, SeekFrom, Write};
    use std::sync::Arc;

    /// A scratch database in a fresh temp directory. The directory is kept
    /// alive by the returned guard.
    struct Scratch {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        conn: Connection,
    }

    fn scratch() -> Scratch {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("scratch.db");
        let conn = Connection::open(path.to_string_lossy().as_ref()).expect("open");
        Scratch {
            _dir: dir,
            path,
            conn,
        }
    }

    // -- values -------------------------------------------------------------

    #[test]
    fn every_from_impl_round_trips_through_a_real_column() {
        let s = scratch();
        // No declared type -> BLOB affinity -> values are stored exactly as
        // bound, so this table shows what each `From` impl actually produced.
        s.conn.execute("CREATE TABLE v (x)").expect("create");

        let cases: Vec<(SqliteValue, SqliteValue)> = vec![
            (SqliteValue::from(7i64), SqliteValue::Integer(7)),
            (SqliteValue::from(7i32), SqliteValue::Integer(7)),
            (SqliteValue::from(1.5f64), SqliteValue::Float(1.5)),
            (
                SqliteValue::from(String::from("owned")),
                SqliteValue::Text("owned".into()),
            ),
            (
                SqliteValue::from("borrowed"),
                SqliteValue::Text("borrowed".into()),
            ),
            (
                SqliteValue::from(Arc::<str>::from("arced")),
                SqliteValue::Text("arced".into()),
            ),
            (
                SqliteValue::from(vec![1u8, 2, 3]),
                SqliteValue::Blob(Arc::from([1u8, 2, 3].as_slice())),
            ),
            (
                SqliteValue::from([4u8, 5].as_slice()),
                SqliteValue::Blob(Arc::from([4u8, 5].as_slice())),
            ),
            (
                SqliteValue::from(Arc::<[u8]>::from([6u8].as_slice())),
                SqliteValue::Blob(Arc::from([6u8].as_slice())),
            ),
            (SqliteValue::from(Option::<&str>::None), SqliteValue::Null),
            (
                SqliteValue::from(Some("present")),
                SqliteValue::Text("present".into()),
            ),
            (SqliteValue::from(Option::<i64>::None), SqliteValue::Null),
            (SqliteValue::from(Some(11i64)), SqliteValue::Integer(11)),
        ];

        for (bound, expected) in cases {
            s.conn.execute("DELETE FROM v").expect("clear");
            s.conn
                .execute_with_params("INSERT INTO v (x) VALUES (?)", std::slice::from_ref(&bound))
                .expect("insert");
            let row = s.conn.query_row("SELECT x FROM v").expect("select");
            assert_eq!(
                row.get(0),
                Some(&expected),
                "value {bound:?} did not round-trip"
            );
        }
    }

    #[test]
    fn extreme_integers_round_trip_and_u64_has_no_silent_conversion() {
        // There is deliberately no `From<u64>` and no `From<usize>`: SQLite
        // integers are i64, so an out-of-range magnitude must be a compile
        // error at the call site rather than a silent wrap. What the caller
        // is forced into instead is a checked conversion.
        assert!(i64::try_from(u64::MAX).is_err());
        assert_eq!(i64::try_from(9_223_372_036_854_775_807u64), Ok(i64::MAX));

        // And the boundary values survive a real column.
        let s = scratch();
        s.conn.execute("CREATE TABLE big (x)").expect("create");
        for value in [i64::MIN, -1, 0, i64::MAX] {
            s.conn.execute("DELETE FROM big").expect("clear");
            s.conn
                .execute_with_params("INSERT INTO big VALUES (?)", &[SqliteValue::from(value)])
                .expect("insert");
            let row = s.conn.query_row("SELECT x FROM big").expect("select");
            assert_eq!(
                row.get(0).and_then(SqliteValue::as_integer),
                Some(value),
                "i64 boundary {value} did not round-trip"
            );
        }
    }

    #[test]
    fn coercion_table_for_all_five_storage_classes() {
        let s = scratch();
        s.conn.execute("CREATE TABLE c (x)").expect("create");
        s.conn
            .execute("INSERT INTO c (x) VALUES (NULL), (42), (1.5), ('42'), (x'2a')")
            .expect("insert");
        let rows = s
            .conn
            .query("SELECT x FROM c ORDER BY rowid")
            .expect("select");
        assert_eq!(rows.len(), 5);

        let observed: Vec<(SqliteValue, Option<String>, Option<i64>)> = rows
            .iter()
            .map(|row| {
                let value = row.get(0).expect("column 0").clone();
                let text = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .map(str::to_string);
                let integer = row.get(0).and_then(SqliteValue::as_integer);
                (value, text, integer)
            })
            .collect();

        assert_eq!(observed[0], (SqliteValue::Null, None, None), "NULL");
        assert_eq!(
            observed[1],
            (SqliteValue::Integer(42), None, Some(42)),
            "INTEGER: as_text must NOT coerce"
        );
        assert_eq!(observed[2], (SqliteValue::Float(1.5), None, None), "REAL");
        assert_eq!(
            observed[3],
            (SqliteValue::Text("42".into()), Some("42".to_string()), None),
            "TEXT: as_integer must NOT coerce"
        );
        assert_eq!(
            observed[4],
            (SqliteValue::Blob(Arc::from([42u8].as_slice())), None, None),
            "BLOB"
        );
    }

    #[test]
    fn column_affinity_decides_the_storage_class_read_back() {
        // The other half of the coercion story: what you get out depends on
        // the column's declared type, not on this module.
        let s = scratch();
        s.conn
            .execute("CREATE TABLE a (i INTEGER, t TEXT, b BLOB)")
            .expect("create");
        s.conn
            .execute_with_params(
                "INSERT INTO a (i, t, b) VALUES (?, ?, ?)",
                &[
                    SqliteValue::from("42"),
                    SqliteValue::from(42i64),
                    SqliteValue::from("42"),
                ],
            )
            .expect("insert");
        let row = s.conn.query_row("SELECT i, t, b FROM a").expect("select");
        // TEXT '42' into an INTEGER-affinity column becomes an integer.
        assert_eq!(row.get(0), Some(&SqliteValue::Integer(42)));
        // INTEGER 42 into a TEXT-affinity column becomes text.
        assert_eq!(row.get(1), Some(&SqliteValue::Text("42".into())));
        // BLOB affinity is no affinity: the value stays text.
        assert_eq!(row.get(2), Some(&SqliteValue::Text("42".into())));
    }

    #[test]
    fn null_reads_back_as_null_variant() {
        let s = scratch();
        s.conn.execute("CREATE TABLE n (x TEXT)").expect("create");
        s.conn
            .execute_with_params("INSERT INTO n (x) VALUES (?)", &[SqliteValue::Null])
            .expect("insert");
        let row = s.conn.query_row("SELECT x FROM n").expect("select");
        assert_eq!(row.get(0), Some(&SqliteValue::Null));
        assert_eq!(row.get(0).and_then(SqliteValue::as_text), None);
    }

    #[test]
    fn row_values_and_get_report_the_same_columns() {
        let s = scratch();
        let row = s.conn.query_row("SELECT 1, 'two', NULL").expect("select");
        assert_eq!(
            row.values(),
            &[
                SqliteValue::Integer(1),
                SqliteValue::Text("two".into()),
                SqliteValue::Null
            ]
        );
        assert_eq!(row.get(0), Some(&SqliteValue::Integer(1)));
        assert_eq!(row.get(2), Some(&SqliteValue::Null));
        assert_eq!(
            row.get(3),
            None,
            "out-of-range index must be None, not a panic"
        );
        // The dominant call-site idiom.
        assert_eq!(row.get(1).and_then(SqliteValue::as_text), Some("two"));
        // And the function-path form used with `.map(Row::values)`.
        let rows = s.conn.query("SELECT 1").expect("select");
        assert_eq!(
            rows.first().map(Row::values),
            Some([SqliteValue::Integer(1)].as_slice())
        );
    }

    // -- execute ------------------------------------------------------------

    #[test]
    fn execute_reports_affected_rows_for_dml() {
        let s = scratch();
        s.conn
            .execute("CREATE TABLE d (id INTEGER PRIMARY KEY, v TEXT)")
            .expect("create");
        assert_eq!(
            s.conn
                .execute("INSERT INTO d (id, v) VALUES (1, 'a'), (2, 'b'), (3, 'c')")
                .expect("insert"),
            3,
            "INSERT must report the number of inserted rows"
        );
        assert_eq!(
            s.conn
                .execute("UPDATE d SET v = 'z' WHERE id IN (1, 2)")
                .expect("update"),
            2,
            "UPDATE must report the number of updated rows"
        );
        assert_eq!(
            s.conn
                .execute("DELETE FROM d WHERE id = 3")
                .expect("delete"),
            1,
            "DELETE must report the number of deleted rows"
        );
        assert_eq!(
            s.conn
                .execute("DELETE FROM d WHERE id = 999")
                .expect("delete none"),
            0,
            "a DELETE matching nothing must report 0"
        );
        assert_eq!(
            s.conn
                .execute_with_params(
                    "UPDATE d SET v = ? WHERE id = ?",
                    &[SqliteValue::from("p"), SqliteValue::from(1i64)],
                )
                .expect("update with params"),
            1
        );
    }

    #[test]
    fn execute_survives_row_returning_pragmas_and_wal_really_turns_on() {
        let s = scratch();
        // Row-returning setter forms (Trap 1). Each must not error.
        assert_eq!(
            s.conn
                .execute("PRAGMA journal_mode = WAL")
                .expect("journal_mode must not error"),
            1,
            "PRAGMA journal_mode = WAL returns one row"
        );
        assert_eq!(
            s.conn
                .execute("PRAGMA busy_timeout = 5000")
                .expect("busy_timeout"),
            1
        );
        assert_eq!(
            s.conn
                .execute("PRAGMA wal_autocheckpoint = 1")
                .expect("wal_autocheckpoint"),
            1
        );
        assert_eq!(
            s.conn
                .execute("PRAGMA journal_size_limit = 1")
                .expect("journal_size_limit"),
            1
        );
        assert_eq!(
            s.conn
                .execute("PRAGMA wal_checkpoint(TRUNCATE)")
                .expect("wal_checkpoint"),
            1
        );
        // Non-row-returning setter forms.
        assert_eq!(
            s.conn
                .execute("PRAGMA foreign_keys = ON")
                .expect("foreign_keys"),
            0
        );
        assert_eq!(
            s.conn
                .execute("PRAGMA user_version = 9")
                .expect("user_version"),
            0
        );
        assert_eq!(
            s.conn
                .execute("PRAGMA synchronous = NORMAL")
                .expect("synchronous"),
            0
        );

        // Trap 2: WAL must actually be on, not merely "did not error".
        let mode = s
            .conn
            .query_row("PRAGMA journal_mode")
            .expect("read journal_mode");
        assert_eq!(
            mode.get(0).and_then(SqliteValue::as_text),
            Some("wal"),
            "PRAGMA journal_mode = WAL must leave the database in WAL mode"
        );
        // And the settings that returned no rows must have stuck too.
        let uv = s
            .conn
            .query_row("PRAGMA user_version")
            .expect("user_version");
        assert_eq!(uv.get(0).and_then(SqliteValue::as_integer), Some(9));
        let fk = s
            .conn
            .query_row("PRAGMA foreign_keys")
            .expect("foreign_keys");
        assert_eq!(fk.get(0).and_then(SqliteValue::as_integer), Some(1));

        // The WAL sidecar exists on disk, which is the file-level proof.
        let wal = s.path.with_extension("db-wal");
        s.conn.execute("CREATE TABLE w (x)").expect("create");
        s.conn.execute("INSERT INTO w VALUES (1)").expect("insert");
        assert!(wal.exists(), "WAL mode must create a -wal sidecar");
    }

    #[test]
    fn ddl_and_silent_pragmas_report_zero_even_after_dml_on_the_same_connection() {
        // Regression guard for a real trap. `rusqlite::Statement::execute`
        // returns `sqlite3_changes()`, a *connection-level* counter that DDL
        // and PRAGMA statements do not reset -- so a naive shim reports the
        // previous DML's row count here.  The previous engine returned 0. The ordering
        // matters: without the INSERT first, both implementations return 0 and
        // the difference is invisible.
        let s = scratch();
        s.conn.execute("CREATE TABLE t (x)").expect("create");
        assert_eq!(
            s.conn
                .execute("INSERT INTO t VALUES (1), (2), (3)")
                .expect("insert"),
            3
        );

        assert_eq!(
            s.conn.execute("CREATE TABLE u (y)").expect("ddl"),
            0,
            "DDL must report 0, not the preceding INSERT's row count"
        );
        assert_eq!(
            s.conn
                .execute("PRAGMA user_version = 3")
                .expect("silent pragma"),
            0,
            "a silent PRAGMA must report 0, not the preceding INSERT's row count"
        );
        assert_eq!(
            s.conn
                .execute("PRAGMA foreign_keys = ON")
                .expect("silent pragma"),
            0
        );
        assert_eq!(
            s.conn.execute("BEGIN").expect("begin"),
            0,
            "transaction control must report 0"
        );
        assert_eq!(s.conn.execute("COMMIT").expect("commit"), 0);
        // And DML after all that still reports its own count, not a running
        // total.
        assert_eq!(
            s.conn.execute("DELETE FROM t WHERE x > 1").expect("delete"),
            2
        );

        // The other half, and the reason the count cannot simply be a
        // `total_changes` delta: `total_changes` counts foreign-key actions,
        // `changes()` does not, and the previous engine reported only the directly
        // affected rows. `beads` declares 15 `ON DELETE CASCADE` foreign keys
        // and enables `PRAGMA foreign_keys`, so this is a live shape.
        s.conn
            .execute("PRAGMA foreign_keys = ON")
            .expect("enable fks");
        assert_eq!(
            s.conn
                .query_row("PRAGMA foreign_keys")
                .expect("read fk pragma")
                .get(0)
                .and_then(SqliteValue::as_integer),
            Some(1),
            "the cascade assertion below is meaningless unless FKs are on"
        );
        s.conn
            .execute("CREATE TABLE parent (id INTEGER PRIMARY KEY)")
            .expect("create parent");
        s.conn
            .execute(
                "CREATE TABLE child (
                     id INTEGER PRIMARY KEY,
                     parent_id INTEGER REFERENCES parent(id) ON DELETE CASCADE
                 )",
            )
            .expect("create child");
        s.conn
            .execute("INSERT INTO parent (id) VALUES (1), (2)")
            .expect("insert parents");
        s.conn
            .execute("INSERT INTO child (id, parent_id) VALUES (1, 1), (2, 1), (3, 1)")
            .expect("insert children");

        assert_eq!(
            s.conn
                .execute("DELETE FROM parent WHERE id = 1")
                .expect("cascading delete"),
            1,
            "a cascading DELETE must report only the directly deleted row, not \
             the three cascaded children -- a total_changes delta would say 4"
        );
        // The cascade really did fire; without this the assertion above would
        // also pass on a database where FKs were silently off.
        assert_eq!(
            s.conn
                .query("SELECT id FROM child")
                .expect("children")
                .len(),
            0,
            "ON DELETE CASCADE did not fire, so the count above proves nothing"
        );
    }

    #[test]
    fn execute_on_a_select_reports_the_result_row_count() {
        // the previous engine's documented behavior for non-DML: rows.len().
        let s = scratch();
        s.conn.execute("CREATE TABLE q (x)").expect("create");
        s.conn
            .execute("INSERT INTO q VALUES (1), (2), (3)")
            .expect("insert");
        assert_eq!(s.conn.execute("SELECT x FROM q").expect("select"), 3);
        assert_eq!(
            s.conn
                .execute("SELECT x FROM q WHERE x > 99")
                .expect("empty"),
            0
        );
    }

    #[test]
    fn execute_rejects_multiple_statements_instead_of_running_only_the_first() {
        let s = scratch();
        let err = s
            .conn
            .execute("CREATE TABLE m1 (x); CREATE TABLE m2 (x)")
            .expect_err("multi-statement execute must fail");
        assert!(
            matches!(&err, DbError::Internal(detail) if detail.contains("multiple statements")),
            "unexpected error: {err:?}"
        );
        // Neither table may exist: the call must be all-or-nothing loud.
        assert!(matches!(
            s.conn
                .query_row("SELECT 1 FROM sqlite_schema WHERE name = 'm1'"),
            Err(DbError::QueryReturnedNoRows)
        ));
        // `schema::execute_batch` is the supported route.
        crate::storage::schema::execute_batch(&s.conn, "CREATE TABLE m1 (x); CREATE TABLE m2 (x)")
            .expect("batch");
        assert_eq!(
            s.conn
                .query("SELECT name FROM sqlite_schema WHERE name IN ('m1','m2')")
                .expect("schema")
                .len(),
            2
        );
    }

    // -- query_row ----------------------------------------------------------

    #[test]
    fn query_row_on_an_empty_result_is_query_returned_no_rows() {
        let s = scratch();
        s.conn.execute("CREATE TABLE e (x)").expect("create");
        let err = s
            .conn
            .query_row("SELECT x FROM e")
            .expect_err("empty query_row must be an error, not Ok");
        assert!(
            matches!(err, DbError::QueryReturnedNoRows),
            "unexpected error: {err:?}"
        );
        let err = s
            .conn
            .query_row_with_params("SELECT x FROM e WHERE x = ?", &[SqliteValue::from(1i64)])
            .expect_err("empty query_row_with_params must be an error");
        assert!(
            matches!(err, DbError::QueryReturnedNoRows),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn query_row_on_multiple_rows_is_query_returned_multiple_rows() {
        let s = scratch();
        s.conn.execute("CREATE TABLE e (x)").expect("create");
        s.conn
            .execute("INSERT INTO e VALUES (1), (2)")
            .expect("insert");
        let err = s
            .conn
            .query_row("SELECT x FROM e")
            .expect_err("two rows must be an error");
        assert!(
            matches!(err, DbError::QueryReturnedMultipleRows),
            "unexpected error: {err:?}"
        );
    }

    // -- constraints --------------------------------------------------------

    #[test]
    fn unique_and_primary_key_violations_are_distinguishable() {
        let s = scratch();
        s.conn
            .execute("CREATE TABLE k (id TEXT PRIMARY KEY, u TEXT UNIQUE)")
            .expect("create");
        s.conn
            .execute("INSERT INTO k (id, u) VALUES ('a', 'x')")
            .expect("insert");

        // A UNIQUE index violation.
        let unique_err = s
            .conn
            .execute("INSERT INTO k (id, u) VALUES ('b', 'x')")
            .expect_err("duplicate u");
        assert!(
            matches!(&unique_err, DbError::UniqueViolation { columns } if columns.contains("k.u")),
            "unexpected error: {unique_err:?}"
        );

        // A PRIMARY KEY violation. `id TEXT PRIMARY KEY` on a rowid table is
        // implemented as a unique index, so SQLite reports it as
        // SQLITE_CONSTRAINT_PRIMARYKEY only on a WITHOUT ROWID / INTEGER
        // PRIMARY KEY table -- exercise the form that really produces it.
        s.conn
            .execute("CREATE TABLE p (id INTEGER PRIMARY KEY, v TEXT)")
            .expect("create p");
        s.conn
            .execute("INSERT INTO p VALUES (1, 'a')")
            .expect("insert p");
        let pk_err = s
            .conn
            .execute("INSERT INTO p VALUES (1, 'b')")
            .expect_err("duplicate pk");
        assert!(
            matches!(pk_err, DbError::PrimaryKeyViolation),
            "unexpected error: {pk_err:?}"
        );

        // The point of the test: the two are not the same variant.
        assert!(!matches!(unique_err, DbError::PrimaryKeyViolation));
        assert!(!matches!(pk_err, DbError::UniqueViolation { .. }));
    }

    #[test]
    fn not_null_and_check_violations_keep_their_own_variants() {
        let s = scratch();
        s.conn
            .execute("CREATE TABLE nn (a TEXT NOT NULL, b INTEGER CHECK (b > 0))")
            .expect("create");
        let err = s
            .conn
            .execute_with_params("INSERT INTO nn (a, b) VALUES (?, 1)", &[SqliteValue::Null])
            .expect_err("null into NOT NULL");
        assert!(
            matches!(&err, DbError::NotNullViolation { column } if column.contains("nn.a")),
            "unexpected error: {err:?}"
        );
        let err = s
            .conn
            .execute("INSERT INTO nn (a, b) VALUES ('x', -1)")
            .expect_err("check violation");
        assert!(
            matches!(err, DbError::CheckViolation { .. }),
            "unexpected error: {err:?}"
        );
    }

    // -- error classification: the recovery-triggering variants --------------

    #[test]
    fn ddl_run_twice_yields_table_exists_and_index_exists() {
        let s = scratch();
        s.conn.execute("CREATE TABLE dup (x)").expect("create");
        let err = s
            .conn
            .execute("CREATE TABLE dup (x)")
            .expect_err("second CREATE TABLE");
        assert!(
            matches!(&err, DbError::TableExists { name } if name == "dup"),
            "unexpected error: {err:?}"
        );

        s.conn
            .execute("CREATE INDEX idx_dup ON dup (x)")
            .expect("index");
        let err = s
            .conn
            .execute("CREATE INDEX idx_dup ON dup (x)")
            .expect_err("second CREATE INDEX");
        assert!(
            matches!(&err, DbError::IndexExists { name } if name == "idx_dup"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn opening_a_non_database_file_yields_not_a_database() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("not-a-db.txt");
        std::fs::write(&path, b"this is plainly not a SQLite database, it is prose")
            .expect("write");
        let err = Connection::open(path.to_string_lossy().as_ref())
            .expect_err("opening prose as a database must fail");
        assert!(
            matches!(err, DbError::NotADatabase { .. }),
            "unexpected error: {err:?}"
        );
    }

    /// Build a multi-page table and report `(page_size, rootpage)` so a test
    /// can damage the exact page SQLite has to walk.
    fn build_multipage_table(path: &std::path::Path, filler: char) -> (u64, u64) {
        let conn = Connection::open(path.to_string_lossy().as_ref()).expect("open");
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .expect("create");
        for i in 0..200 {
            conn.execute_with_params(
                "INSERT INTO t (id, v) VALUES (?, ?)",
                &[
                    SqliteValue::from(i64::from(i)),
                    SqliteValue::from(format!("row-{i}-{}", filler.to_string().repeat(40))),
                ],
            )
            .expect("insert");
        }
        let page_size = conn
            .query_row("PRAGMA page_size")
            .expect("page_size")
            .get(0)
            .and_then(SqliteValue::as_integer)
            .expect("page_size value");
        let rootpage = conn
            .query_row("SELECT rootpage FROM sqlite_schema WHERE name = 't'")
            .expect("rootpage")
            .get(0)
            .and_then(SqliteValue::as_integer)
            .expect("rootpage value");
        conn.close().expect("close");
        assert!(rootpage > 1, "table root must not be the schema page");
        (
            u64::try_from(page_size).expect("page_size fits"),
            u64::try_from(rootpage).expect("rootpage fits"),
        )
    }

    #[test]
    fn scribbling_on_a_page_yields_database_corrupt() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("corrupt.db");
        let (page_size, rootpage) = build_multipage_table(&path, 'p');

        // Overwrite the interior of the table's own b-tree root, past the
        // page header, so the file still looks like a database until SQLite
        // walks it.
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("reopen");
            file.seek(SeekFrom::Start((rootpage - 1) * page_size + 12))
                .expect("seek");
            file.write_all(&[0xFF; 600]).expect("scribble");
            file.flush().expect("flush");
        }

        let conn = Connection::open(path.to_string_lossy().as_ref()).expect("reopen");
        let err = conn
            .query("SELECT id, v FROM t ORDER BY id")
            .expect_err("reading a scribbled page must fail");
        assert!(
            matches!(err, DbError::DatabaseCorrupt { .. }),
            "unexpected error: {err:?}"
        );
        // And the Display text is the one `is_recoverable_database_internal_error`
        // looks for.
        assert!(
            err.to_string()
                .to_ascii_lowercase()
                .contains("database disk image is malformed"),
            "corruption message changed: {err}"
        );
    }

    #[test]
    fn truncating_a_database_reports_corruption_not_short_read() {
        // Findings-grade, and the reason this test is not called
        // `..._yields_short_read`: truncating a database mid-page is the
        // condition `DbError::ShortRead` exists for, but C SQLite's pager
        // absorbs SQLITE_IOERR_SHORT_READ (it zero-fills the partial page)
        // and the failure surfaces as SQLITE_CORRUPT instead. Both variants
        // are in the JSONL-recovery set, so recovery still fires -- but
        // `ShortRead` itself has no producing condition under this engine.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("short.db");
        let _ = build_multipage_table(&path, 'q');
        let len = std::fs::metadata(&path).expect("metadata").len();
        assert!(len > 4096, "fixture must span several pages");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("reopen");
        file.set_len(len - 2048).expect("truncate mid-page");
        drop(file);

        let opened = Connection::open(path.to_string_lossy().as_ref());
        let err = match opened {
            Err(e) => e,
            Ok(conn) => conn
                .query("SELECT id, v FROM t ORDER BY id")
                .expect_err("reading a truncated database must fail"),
        };
        assert!(
            matches!(err, DbError::DatabaseCorrupt { .. }),
            "unexpected error: {err:?}"
        );
        assert!(
            !matches!(err, DbError::ShortRead { .. }),
            "if this ever becomes ShortRead the module docs are stale"
        );
    }

    #[test]
    fn a_duplicate_schema_entry_is_reported_as_corruption() {
        // The condition `is_duplicate_schema_entry_open_error`
        // (`src/config/mod.rs`) was written for. Under the previous engine it arrived as
        // `Internal("... table X already exists")`; real SQLite raises
        // SQLITE_CORRUPT with "malformed database schema (t) - table t
        // already exists", so it lands in `DatabaseCorrupt` -- still inside
        // the JSONL-recovery set, but via a different arm.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("dupschema.db");
        {
            let conn = Connection::open(path.to_string_lossy().as_ref()).expect("open");
            conn.execute("CREATE TABLE t (x)").expect("create t");
            conn.execute("CREATE TABLE u (y)").expect("create u");
            conn.execute("PRAGMA writable_schema = ON")
                .expect("writable_schema");
            conn.execute(
                "UPDATE sqlite_schema SET name = 't', tbl_name = 't', \
                 sql = 'CREATE TABLE t(y)' WHERE name = 'u'",
            )
            .expect("duplicate the schema row");
            conn.close().expect("close");
        }

        let conn = Connection::open(path.to_string_lossy().as_ref()).expect("reopen");
        let err = conn
            .query("SELECT * FROM t")
            .expect_err("a duplicated schema row must fail");
        assert!(
            matches!(err, DbError::DatabaseCorrupt { .. }),
            "unexpected error: {err:?}"
        );
        let text = err.to_string().to_ascii_lowercase();
        assert!(
            text.contains("malformed database schema"),
            "message changed: {err}"
        );
        assert!(text.contains("already exists"), "message changed: {err}");
    }

    #[test]
    fn config_recovery_substrings_still_match_what_sqlite_emits() {
        // `is_recoverable_database_internal_error` (`src/config/mod.rs`)
        // matches three literal substrings against the error text. They were
        // written for the previous engine's wording; this pins which ones real SQLite
        // still produces.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("substr.db");
        let (page_size, rootpage) = build_multipage_table(&path, 'r');
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("reopen");
            file.seek(SeekFrom::Start((rootpage - 1) * page_size + 12))
                .expect("seek");
            file.write_all(&[0xFF; 600]).expect("scribble");
            file.flush().expect("flush");
        }
        let conn = Connection::open(path.to_string_lossy().as_ref()).expect("reopen");
        let text = conn
            .query("SELECT id, v FROM t ORDER BY id")
            .expect_err("corrupt")
            .to_string()
            .to_ascii_lowercase();

        // (1) Still produced verbatim: SQLITE_CORRUPT's errstr.
        assert!(
            text.contains("database disk image is malformed"),
            "SQLite stopped using this wording: {text}"
        );
        // (2) and (3) are previous-engine-only wordings. Asserting their absence is
        // the point: they are dead branches in the classifier, kept only so
        // a pre-existing on-disk error message still routes to recovery.
        assert!(
            !text.contains("malformed database disk image"),
            "reversed wording appeared; the classifier arm is no longer dead"
        );
        assert!(
            !text.contains("missing from index"),
            "SQLite emits this only in PRAGMA integrity_check output, not in errors"
        );
    }

    #[test]
    fn a_damaged_wal_sidecar_silently_loses_its_uncheckpointed_rows() {
        // Findings-grade, and the single most important negative result in
        // this module: C SQLite has no "WAL corrupt" result code, so
        // `DbError::WalCorrupt` has no producing condition. What happens
        // instead is worse than an error -- a WAL whose header no longer
        // checksums is treated as an *empty* WAL, and every frame it held is
        // discarded without a word.
        //
        // The database must be snapshotted while the connection is still
        // open: SQLite checkpoints and deletes the `-wal` on the last
        // connection's clean close, so there would be nothing left to damage
        // after a `drop`. Row 1 is checkpointed into the main file, row 2 is
        // left in the WAL, and only row 2 disappears -- which is what proves
        // the WAL was silently dropped rather than the whole database failing.
        let dir = tempfile::tempdir().expect("temp dir");
        let live = dir.path().join("live.db");
        let live_wal = dir.path().join("live.db-wal");
        let copy = dir.path().join("copy.db");
        let copy_wal = dir.path().join("copy.db-wal");

        let conn = Connection::open(live.to_string_lossy().as_ref()).expect("open");
        conn.execute("PRAGMA journal_mode = WAL").expect("wal");
        conn.execute("CREATE TABLE t (x)").expect("create");
        conn.execute("INSERT INTO t VALUES (1)").expect("insert 1");
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
            .expect("checkpoint row 1 into the main file");
        conn.execute("INSERT INTO t VALUES (2)")
            .expect("insert 2, which stays in the WAL");

        // Snapshot both files into memory while the connection is open: the
        // `-wal` only exists until the last connection closes cleanly.
        assert!(live_wal.exists(), "row 2 must be sitting in a live -wal");
        let db_bytes = std::fs::read(&live).expect("read db");
        let wal_bytes = std::fs::read(&live_wal).expect("read wal");
        drop(conn);

        let stage = |wal: &[u8]| {
            std::fs::write(&copy, &db_bytes).expect("stage db");
            std::fs::write(&copy_wal, wal).expect("stage wal");
        };

        // Sanity check the fixture before damaging it: the undamaged copy must
        // see both rows, i.e. row 2 was committed and did reach the snapshot.
        // This is deliberately weaker than it looks -- it does NOT prove row 2
        // lived in the WAL rather than the main file, because a copy where
        // everything had been checkpointed would pass it too. The final
        // assertion is what carries the weight: it is the only one that fails
        // if row 2 was not WAL-resident, since a checkpointed row 2 would
        // survive the damaged WAL.
        stage(&wal_bytes);
        {
            let intact = Connection::open(copy.to_string_lossy().as_ref()).expect("open copy");
            let rows = intact
                .query("SELECT x FROM t ORDER BY x")
                .expect("read intact copy");
            assert_eq!(
                rows.len(),
                2,
                "fixture is wrong: row 2 was never committed into the snapshot"
            );
            intact.close().expect("close intact");
        }

        // Re-stage, then destroy the WAL header's magic/checksum.
        stage(&wal_bytes);
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&copy_wal)
                .expect("open wal");
            file.write_all(&[0xAB; 32]).expect("scribble wal header");
            file.flush().expect("flush");
        }

        let conn = Connection::open(copy.to_string_lossy().as_ref()).expect("reopen");
        let rows = conn
            .query("SELECT x FROM t ORDER BY x")
            .expect("a damaged WAL must not surface as an error");
        assert_eq!(
            rows.iter()
                .filter_map(|r| r.get(0).and_then(SqliteValue::as_integer))
                .collect::<Vec<_>>(),
            vec![1],
            "the checkpointed row must survive and the WAL-resident row must be \
             silently gone -- if this ever errors instead, WalCorrupt has become \
             producible and the module docs are stale"
        );
    }

    #[test]
    fn invalid_utf8_in_a_text_column_is_replaced_rather_than_failing_the_query() {
        // `SqliteValue::from_value_ref` uses `String::from_utf8_lossy`, so a
        // database written by another tool with non-UTF-8 TEXT yields U+FFFD
        // instead of aborting the whole query. Pin that, since it is a silent
        // substitution.
        let s = scratch();
        s.conn.execute("CREATE TABLE u (x)").expect("create");
        // CAST a blob holding a lone 0xFF byte to TEXT: SQLite stores the raw
        // bytes and reports the column as TEXT without validating UTF-8.
        s.conn
            .execute("INSERT INTO u (x) VALUES (CAST(x'41ff42' AS TEXT))")
            .expect("insert");
        let row = s.conn.query_row("SELECT x FROM u").expect("select");
        assert_eq!(
            row.get(0).and_then(SqliteValue::as_text),
            Some("A\u{FFFD}B"),
            "invalid UTF-8 must be replaced, not dropped and not an error"
        );
    }

    #[test]
    fn is_transient_covers_busy_and_locked_only() {
        assert!(DbError::Busy.is_transient());
        assert!(DbError::BusyRecovery.is_transient());
        assert!(DbError::DatabaseLocked { path: "x".into() }.is_transient());
        assert!(!DbError::QueryReturnedNoRows.is_transient());
        assert!(!DbError::DatabaseFull.is_transient());
        assert!(
            !DbError::DatabaseCorrupt {
                detail: "x".to_string()
            }
            .is_transient()
        );
    }

    #[test]
    fn a_real_busy_error_maps_to_busy_and_is_transient() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("busy.db");
        let path_str = path.to_string_lossy().to_string();
        let writer = Connection::open(path_str.as_str()).expect("open writer");
        writer.execute("CREATE TABLE t (x)").expect("create");

        let blocked = Connection::open(path_str.as_str()).expect("open blocked");
        // Zero timeout so the second writer fails immediately instead of
        // waiting; without this the test would depend on wall-clock timing.
        blocked.execute("PRAGMA busy_timeout = 0").expect("timeout");
        writer.execute("BEGIN EXCLUSIVE").expect("begin exclusive");

        let err = blocked
            .execute("INSERT INTO t VALUES (1)")
            .expect_err("a second writer must be refused");
        assert!(matches!(err, DbError::Busy), "unexpected error: {err:?}");
        assert!(
            err.is_transient(),
            "SQLITE_BUSY must be transient so callers retry"
        );
        writer.execute("ROLLBACK").expect("rollback");
    }

    #[test]
    fn internal_carries_sqlite_message_text_for_unclassified_failures() {
        let s = scratch();
        let err = s
            .conn
            .query("SELECT * FROM definitely_absent")
            .expect_err("missing table");
        assert!(
            matches!(&err, DbError::NoSuchTable { name } if name == "definitely_absent"),
            "unexpected error: {err:?}"
        );
        let err = s.conn.execute("THIS IS NOT SQL").expect_err("syntax error");
        assert!(
            matches!(&err, DbError::SyntaxError { token } if token == "THIS"),
            "unexpected error: {err:?}"
        );
        assert_eq!(
            err.to_string(),
            "near \"THIS\": syntax error",
            "SyntaxError's Display must stay byte-identical to SQLite's"
        );

        // A message with no recognizable shape falls through to `Internal`
        // carrying SQLite's own text, which is what
        // `is_recoverable_database_internal_error` reads.
        let err = s
            .conn
            .execute("SELECT abs('a', 'b')")
            .expect_err("wrong argument count");
        assert!(
            matches!(&err, DbError::Internal(detail) if detail.contains("abs")),
            "unexpected error: {err:?}"
        );

        // `no such column` must keep its Display text: `schema::execute_batch`
        // matches on the string, not the variant.
        s.conn.execute("CREATE TABLE nc (a)").expect("create");
        let err = s
            .conn
            .query("SELECT missing_col FROM nc")
            .expect_err("missing column");
        assert!(
            err.to_string().contains("no such column"),
            "schema::execute_batch greps this text: {err}"
        );
    }

    // -- prepare ------------------------------------------------------------

    #[test]
    fn prepared_statements_run_repeatedly_through_a_shared_reference() {
        let s = scratch();
        s.conn
            .execute("CREATE TABLE t (k TEXT, v INTEGER)")
            .expect("create");
        s.conn
            .execute("INSERT INTO t VALUES ('a', 1), ('b', 2), ('a', 3)")
            .expect("insert");

        // The call-site shape: prepare once, then call through `&stmt` inside
        // a loop.
        let stmt = s
            .conn
            .prepare("SELECT v FROM t WHERE k = ? ORDER BY v")
            .expect("prepare");
        let mut seen = Vec::new();
        for key in ["a", "b", "a"] {
            let rows = stmt
                .query_with_params(&[SqliteValue::from(key)])
                .expect("query");
            seen.push(rows.len());
        }
        assert_eq!(seen, vec![2, 1, 2], "a prepared statement must be reusable");

        let one = stmt
            .query_with_params(&[SqliteValue::from("b")])
            .expect("query");
        assert_eq!(
            one.first()
                .and_then(|r| r.get(0))
                .and_then(SqliteValue::as_integer),
            Some(2)
        );

        let none = stmt
            .query_with_params(&[SqliteValue::from("zz")])
            .expect("query");
        assert!(none.is_empty());
    }

    // -- open flags ---------------------------------------------------------

    #[test]
    fn read_only_flags_open_existing_and_refuse_writes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("ro.db");
        let path_str = path.to_string_lossy().to_string();
        {
            let conn = Connection::open(path_str.as_str()).expect("open");
            conn.execute("CREATE TABLE t (x)").expect("create");
            conn.execute("INSERT INTO t VALUES (1)").expect("insert");
            conn.close().expect("close");
        }

        let ro = open_with_flags(&path_str, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("open ro");
        assert_eq!(ro.query("SELECT x FROM t").expect("read").len(), 1);
        let err = ro
            .execute("INSERT INTO t VALUES (2)")
            .expect_err("read-only connection must refuse writes");
        assert!(
            matches!(err, DbError::ReadOnly),
            "unexpected error: {err:?}"
        );
        ro.close().expect("close ro");
    }

    #[test]
    fn read_only_open_of_a_missing_file_is_cannot_open() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("absent.db");
        let err = open_with_flags(
            missing.to_string_lossy().as_ref(),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect_err("read-only open of a missing file must fail");
        assert!(
            matches!(err, DbError::CannotOpen { .. }),
            "unexpected error: {err:?}"
        );
    }

    // -- close --------------------------------------------------------------

    #[test]
    fn close_in_place_closes_before_the_wrapper_is_dropped() {
        // `impl Drop for SqliteStorage` (src/storage/sqlite.rs) closes the
        // connection and only then unlinks an ephemeral temp database and its
        // sidecars (#299). It has `&mut self`, so it needs a non-consuming
        // close; letting the field's own drop do it would reverse the
        // ordering.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("inplace.db");
        let mut conn = Connection::open(path.to_string_lossy().as_ref()).expect("open");
        conn.execute("PRAGMA journal_mode = WAL").expect("wal");
        conn.execute("CREATE TABLE t (x)").expect("create");
        conn.execute("INSERT INTO t VALUES (1)").expect("insert");
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
            .expect("checkpoint, as Drop does");

        conn.close_in_place().expect("close in place");
        // The sidecars are gone, so unlinking the database family now is safe
        // -- that is the whole point of closing before the unlink.
        assert!(
            !dir.path().join("inplace.db-wal").exists(),
            "closing must remove the WAL sidecar before the caller unlinks"
        );

        // Idempotent, and every other method now reports a closed connection
        // instead of panicking.
        conn.close_in_place().expect("second close is a no-op");
        let err = conn
            .query("SELECT 1")
            .expect_err("using a closed connection must be an error");
        assert!(
            matches!(&err, DbError::Internal(d) if d.contains("already closed")),
            "unexpected error: {err:?}"
        );
        // Dropping the wrapper afterwards must not double-close.
        drop(conn);

        // The data really was committed before the close.
        let again = Connection::open(path.to_string_lossy().as_ref()).expect("reopen");
        assert_eq!(again.query("SELECT x FROM t").expect("read").len(), 1);
    }

    #[test]
    fn close_releases_the_file() {
        let s = scratch();
        s.conn.execute("CREATE TABLE t (x)").expect("create");
        let path = s.path.clone();
        s.conn.close().expect("close");
        // A fresh connection sees the committed schema.
        let again = Connection::open(path.to_string_lossy().as_ref()).expect("reopen");
        assert_eq!(
            again
                .query("SELECT name FROM sqlite_schema WHERE name = 't'")
                .expect("schema")
                .len(),
            1
        );
    }
}
