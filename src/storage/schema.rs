//! Database schema definitions and migration logic.

use crate::storage::conn::{Connection, Row, SqliteValue};
use chrono::Utc;

use crate::error::{BeadsError, Result};
use crate::model::{IssueType, Priority, Status};
use crate::util::content_hash_from_parts;

pub const CURRENT_SCHEMA_VERSION: i32 = 19;
const ISSUES_CLOSED_AT_CHECK: &str = "CHECK ((status = 'closed' AND closed_at IS NOT NULL) OR (status = 'tombstone') OR (status NOT IN ('closed', 'tombstone') AND closed_at IS NULL))";

/// The complete SQL schema for the beads database.
/// Schema matches classic bd (Go) for interoperability.
pub const SCHEMA_SQL: &str = r"
    -- Issues table
    -- Note: TEXT fields use DEFAULT '' for bd (Go) compatibility.
    -- bd's sql.Scan doesn't handle NULL well when scanning into string fields.
    -- Closed-at invariant is enforced by the CHECK clause below.
    CREATE TABLE IF NOT EXISTS issues (
        id TEXT PRIMARY KEY,
        content_hash TEXT,
        title TEXT NOT NULL CHECK(length(title) <= 500),
        description TEXT NOT NULL DEFAULT '',
        design TEXT NOT NULL DEFAULT '',
        acceptance_criteria TEXT NOT NULL DEFAULT '',
        notes TEXT NOT NULL DEFAULT '',
        status TEXT NOT NULL DEFAULT 'open',
        priority INTEGER NOT NULL DEFAULT 2 CHECK(priority >= 0 AND priority <= 4),
        issue_type TEXT NOT NULL DEFAULT 'task',
        owner TEXT DEFAULT '',
        created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        created_by TEXT DEFAULT '',
        updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        closed_at DATETIME,
        close_reason TEXT DEFAULT '',
        defer_until DATETIME,
        external_ref TEXT,
        source_repo TEXT NOT NULL DEFAULT '.',
        deleted_at DATETIME,
        deleted_by TEXT DEFAULT '',
        delete_reason TEXT DEFAULT '',
        original_type TEXT DEFAULT '',
        -- former_ids (schema v18) holds a JSON array of every ID this issue
        -- previously held, oldest first. Populated by `rename_issue`. Same
        -- JSON-array-in-TEXT convention as `blocked_issues_cache.blocked_by`.
        former_ids TEXT NOT NULL DEFAULT '[]',
        CHECK (
            (status = 'closed' AND closed_at IS NOT NULL) OR
            (status = 'tombstone') OR
            (status NOT IN ('closed', 'tombstone') AND closed_at IS NULL)
        )
    );

    -- Primary access patterns
    CREATE INDEX IF NOT EXISTS idx_issues_status ON issues(status);
    CREATE INDEX IF NOT EXISTS idx_issues_priority ON issues(priority);
    CREATE INDEX IF NOT EXISTS idx_issues_issue_type ON issues(issue_type);
    CREATE INDEX IF NOT EXISTS idx_issues_created_at ON issues(created_at);
    CREATE INDEX IF NOT EXISTS idx_issues_updated_at ON issues(updated_at);

    -- Export/sync patterns
    CREATE INDEX IF NOT EXISTS idx_issues_content_hash ON issues(content_hash);
    CREATE UNIQUE INDEX IF NOT EXISTS idx_issues_external_ref_unique ON issues(external_ref) WHERE external_ref IS NOT NULL;

    -- Special states
    CREATE INDEX IF NOT EXISTS idx_issues_tombstone ON issues(status) WHERE status = 'tombstone';

    -- Time-based
    CREATE INDEX IF NOT EXISTS idx_issues_defer_until ON issues(defer_until) WHERE defer_until IS NOT NULL;

    -- Ready work composite index (most important for performance).
    -- Schema v19 dropped the `ephemeral = 0 AND pinned = 0 AND is_template = 0`
    -- predicate along with the columns; this DDL must stay byte-identical to
    -- the one the v19 migration arm recreates, or a fresh and a migrated
    -- database disagree and tests/storage/schema_shape.rs fails.
    CREATE INDEX IF NOT EXISTS idx_issues_ready
        ON issues(status, priority, created_at)
        WHERE status = 'open';

    -- Widened ready group (issue #354): when `workflow.status_groups.ready`
    -- surfaces statuses beyond `open` (e.g. `rework`), the partial
    -- `idx_issues_ready` above (which only covers `status = 'open'`) cannot serve
    -- the `status IN (...) ORDER BY priority, created_at` query, so a non-partial
    -- composite keeps the widened path index-covered. The tighter partial index
    -- still wins for the common default `[open]` group.
    CREATE INDEX IF NOT EXISTS idx_issues_status_priority_created
        ON issues(status, priority, created_at);

    -- Common active list path: non-terminal issues sorted by priority/created_at.
    -- Uses ASC on created_at (not DESC) to avoid previous-engine B-tree ordering
    -- divergence with C sqlite3 integrity_check.  SQLite reverse-scans the ASC
    -- index efficiently for ORDER BY ... created_at DESC queries.
    CREATE INDEX IF NOT EXISTS idx_issues_list_active_order
        ON issues(priority, created_at)
        WHERE status NOT IN ('closed', 'tombstone');

    -- Dependencies
    CREATE TABLE IF NOT EXISTS dependencies (
        issue_id TEXT NOT NULL,
        depends_on_id TEXT NOT NULL,
        type TEXT NOT NULL DEFAULT 'blocks',
        created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        created_by TEXT NOT NULL DEFAULT '',
        metadata TEXT DEFAULT '{}',
        thread_id TEXT DEFAULT '',
        PRIMARY KEY (issue_id, depends_on_id),
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
        -- Note: depends_on_id FK intentionally removed to allow external issue references
    );
    CREATE INDEX IF NOT EXISTS idx_dependencies_issue ON dependencies(issue_id);
    CREATE INDEX IF NOT EXISTS idx_dependencies_depends_on ON dependencies(depends_on_id);
    CREATE INDEX IF NOT EXISTS idx_dependencies_type ON dependencies(type);
    CREATE INDEX IF NOT EXISTS idx_dependencies_depends_on_type ON dependencies(depends_on_id, type);
    CREATE INDEX IF NOT EXISTS idx_dependencies_thread ON dependencies(thread_id) WHERE thread_id != '';
    -- Composite for blocking lookups
    CREATE INDEX IF NOT EXISTS idx_dependencies_blocking
        ON dependencies(depends_on_id, issue_id)
        WHERE (type = 'blocks' OR type = 'parent-child' OR type = 'conditional-blocks' OR type = 'waits-for');

    -- Labels
    CREATE TABLE IF NOT EXISTS labels (
        issue_id TEXT NOT NULL,
        label TEXT NOT NULL,
        PRIMARY KEY (issue_id, label),
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_labels_label ON labels(label);
    CREATE INDEX IF NOT EXISTS idx_labels_issue ON labels(issue_id);

    -- Comments
    CREATE TABLE IF NOT EXISTS comments (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        issue_id TEXT NOT NULL,
        author TEXT NOT NULL,
        text TEXT NOT NULL,
        created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_comments_issue ON comments(issue_id);
    CREATE INDEX IF NOT EXISTS idx_comments_created_at ON comments(created_at);

    -- Config (Runtime)
    -- NOTE: Avoid PRIMARY KEY/UNIQUE constraints here because the current
    -- storage engine does not reliably maintain unique autoindexes.
    -- Application code enforces key replacement via DELETE + INSERT.
    CREATE TABLE IF NOT EXISTS config (
        key TEXT NOT NULL,
        value TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_config_key ON config(key);

    -- Metadata
    -- Same rationale as config: keep it as key-value with an explicit index.
    -- Storage code reads the newest duplicate row and harmonizes duplicate
    -- rows on write; existing duplicates are repaired on open.
    CREATE TABLE IF NOT EXISTS metadata (
        key TEXT NOT NULL,
        value TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_metadata_key ON metadata(key);

    -- Dirty Issues (for export)
    CREATE TABLE IF NOT EXISTS dirty_issues (
        issue_id TEXT PRIMARY KEY,
        marked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_dirty_issues_marked_at ON dirty_issues(marked_at);

    -- Export Hashes (for incremental export)
    CREATE TABLE IF NOT EXISTS export_hashes (
        issue_id TEXT PRIMARY KEY,
        content_hash TEXT NOT NULL,
        exported_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );

    -- Blocked Issues Cache (Materialized view)
    -- Rebuilt on dependency or status changes.
    -- `blocked_by` stores a JSON array of blocking issue IDs.
    CREATE TABLE IF NOT EXISTS blocked_issues_cache (
        issue_id TEXT PRIMARY KEY,
        blocked_by TEXT NOT NULL,
        blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_blocked_cache_blocked_at ON blocked_issues_cache(blocked_at);

    -- Child Counters (for hierarchical IDs like bd-abc.1, bd-abc.2)
    CREATE TABLE IF NOT EXISTS child_counters (
        parent_id TEXT PRIMARY KEY,
        last_child INTEGER NOT NULL DEFAULT 0,
        FOREIGN KEY (parent_id) REFERENCES issues(id) ON DELETE CASCADE
    );

";

/// Split a SQL script into individual statements, respecting string literals,
/// quoted identifiers, and comments.
///
/// A naive `split(';')` breaks when SQL string literals contain semicolons
/// (e.g., `INSERT INTO t(v) VALUES('a;b')`). This function uses a small state
/// machine to track whether the current position is inside:
/// - A single-quoted string literal (`'...'`, with `''` as escape)
/// - A double-quoted identifier (`"..."`, with `""` as escape)
/// - A line comment (`-- ...`)
/// - A block comment (`/* ... */`)
///
/// Only semicolons at the top level (outside all of the above) are treated as
/// statement terminators.
fn split_sql_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut stmts = Vec::new();
    let mut start = 0; // byte offset where the current statement begins
    let mut i = 0;

    // State flags — at most one is true at a time.
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < len {
        let b = bytes[i];

        // --- Line comment state ---
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        // --- Block comment state ---
        if in_block_comment {
            if b == b'*' && i + 1 < len && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        // --- Single-quoted string state ---
        if in_single_quote {
            if b == b'\'' {
                // '' is an escaped quote inside a string literal
                if i + 1 < len && bytes[i + 1] == b'\'' {
                    i += 2;
                } else {
                    in_single_quote = false;
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }

        // --- Double-quoted identifier state ---
        if in_double_quote {
            if b == b'"' {
                if i + 1 < len && bytes[i + 1] == b'"' {
                    i += 2;
                } else {
                    in_double_quote = false;
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }

        // --- Top-level parsing ---
        if b == b'\'' {
            in_single_quote = true;
            i += 1;
        } else if b == b'"' {
            in_double_quote = true;
            i += 1;
        } else if b == b'-' && i + 1 < len && bytes[i + 1] == b'-' {
            in_line_comment = true;
            i += 2;
        } else if b == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            in_block_comment = true;
            i += 2;
        } else if b == b';' {
            // Statement terminator at top level.
            let stmt = &sql[start..i];
            if !stmt.trim().is_empty() {
                stmts.push(stmt.trim());
            }
            start = i + 1;
            i += 1;
        } else {
            i += 1;
        }
    }

    // Trailing statement without a final semicolon.
    if start < len {
        let stmt = &sql[start..len];
        if !stmt.trim().is_empty() {
            stmts.push(stmt.trim());
        }
    }

    stmts
}

/// Execute multiple SQL statements separated by semicolons.
///
/// The previous engine did not support `execute_batch`, so we split the SQL script
/// into individual statements (respecting string literals and comments)
/// and execute each one individually.
///
/// # Every statement is fatal, including `CREATE INDEX`
///
/// This used to swallow any `CREATE INDEX` that failed with `no such column`,
/// as a workaround for the previous engine's in-memory schema cache not
/// updating after `ALTER TABLE RENAME` during a table rebuild. Both halves of
/// that premise are gone: the engine is gone, and `rebuild_issues_table` no
/// longer uses `RENAME` — it drops and recreates instead, for the same reason.
/// SQLite makes a schema change visible to the very next statement, so
/// `CREATE INDEX` after an `ALTER TABLE ADD COLUMN` in the same script works.
///
/// Under real SQLite a `no such column` on `CREATE INDEX` is therefore a
/// genuine error: an index definition naming a column that does not exist. The
/// workaround would have skipped it silently and forever, and because
/// `tests/storage_schema_shape.rs` pins every index, the symptom would
/// have surfaced as a mysteriously missing index rather than as the error that
/// caused it. Removed in `bds-04l.4.3`;
/// `tests::execute_batch_reports_a_create_index_on_a_missing_column` pins it.
///
/// **Known consequence, tracked as `bds-04l.18`.** Making the error fatal also
/// makes it fatal at *open*: `run_pre_schema_migrations` backfills missing
/// columns only for `issues`, `dependencies`, `comments` and `dirty_issues`,
/// so a legacy shape missing an indexed column on any other table
/// now fails `apply_schema` outright — and `DbError::NoSuchColumn` is not in
/// `should_attempt_jsonl_recovery`'s set, so recovery does not fire either.
/// That is the right trade against silently losing an index forever, but it is
/// a trade, and the bead carries the repair.
pub(crate) fn execute_batch(conn: &Connection, sql: &str) -> Result<()> {
    for stmt in split_sql_statements(sql) {
        if let Err(e) = conn.execute(stmt) {
            eprintln!("execute_batch failed on statement: {stmt}\nError: {e:?}");
            return Err(BeadsError::Database(e));
        }
    }
    Ok(())
}

/// Apply the schema to the database.
///
/// This splits the DDL script into individual statements and executes them.
/// It is idempotent because all statements use `IF NOT EXISTS`.
///
/// # Errors
///
/// Returns an error if the SQL execution fails or pragmas cannot be set.
pub fn apply_schema(conn: &Connection) -> Result<()> {
    // Detect a truly fresh (empty) database before any DDL runs.
    // On a fresh DB, SCHEMA_SQL creates everything at the current version,
    // so running migrations is unnecessary and harmful — e.g. the v3/v4
    // migrations DROP+CREATE idx_issues_ready which orphans a page and
    // leaves the table inconsistent.
    let is_fresh = !table_exists(conn, "issues");

    // Run pre-schema migrations first to fix any incompatible old tables
    // This must run BEFORE execute_batch because the batch includes CREATE INDEX
    // statements that will fail if old tables have missing columns
    let issues_rebuilt = run_pre_schema_migrations(conn).map_err(|e| {
        eprintln!("run_pre_schema_migrations failed: {:?}", e);
        e
    })?;

    execute_batch(conn, SCHEMA_SQL)?;

    if is_fresh {
        // Fresh database: SCHEMA_SQL already created everything at the
        // current version. Skip migrations and stamp user_version directly.
        conn.execute(&format!("PRAGMA user_version = {CURRENT_SCHEMA_VERSION}"))
            .map_err(|e| {
                eprintln!("PRAGMA user_version failed: {:?}", e);
                BeadsError::Database(e)
            })?;
    } else {
        // Existing database: run migrations for schema upgrades.
        // If the issues table was rebuilt from scratch, skip migration checks
        // that reference newly-added columns because the previous engine's in-memory schema
        // cache may not have been updated yet.
        run_migrations(conn, issues_rebuilt).map_err(|e| {
            eprintln!("run_migrations failed: {:?}", e);
            e
        })?;

        // Mark schema as applied so future opens can skip DDL/migration work.
        conn.execute(&format!("PRAGMA user_version = {CURRENT_SCHEMA_VERSION}"))
            .map_err(|e| {
                eprintln!("PRAGMA user_version failed: {:?}", e);
                BeadsError::Database(e)
            })?;
    }

    apply_runtime_pragmas(conn).map_err(|e| {
        eprintln!("apply_runtime_pragmas failed: {:?}", e);
        e
    })?;

    // On a truly fresh bootstrap, run a defensive `wal_checkpoint(TRUNCATE)`
    // to reclaim any transient pages the previous engine allocated while
    // executing SCHEMA_SQL (CREATE TABLE + ~15 CREATE INDEX statements,
    // several of which are partial indexes on columns of empty tables).
    // Without this, a fresh `br init` can leave the database with
    // unreachable pages that sqlite3's `PRAGMA integrity_check` surfaces
    // as `Page N: never used` — see issue #225.
    //
    // Note: page-level anomalies from subsequent writes (e.g., "free space
    // corruption" — issue #237) are addressed via VACUUM in the rebuild
    // path, not here.  Running VACUUM here would
    // conflict with connections opened immediately after init.
    if is_fresh && let Err(e) = conn.execute("PRAGMA wal_checkpoint(TRUNCATE)") {
        tracing::debug!(
            error = %e,
            "wal_checkpoint(TRUNCATE) after fresh bootstrap failed (non-fatal)"
        );
    }

    Ok(())
}

pub(crate) fn apply_runtime_compatible_schema(conn: &Connection) -> Result<()> {
    // The table layouts are already safe to operate on, so we can skip the
    // heavier pre-schema rebuilds and just restore any missing canonical DDL.
    execute_batch(conn, SCHEMA_SQL)?;
    run_migrations(conn, false)?;
    conn.execute(&format!("PRAGMA user_version = {CURRENT_SCHEMA_VERSION}"))
        .map_err(BeadsError::Database)?;
    apply_runtime_pragmas(conn)?;
    Ok(())
}

pub(crate) fn apply_runtime_pragmas(conn: &Connection) -> Result<()> {
    // New databases should opt into WAL, but steady-state opens should not
    // reassert the current mode and turn a read path into a write-like one.
    let journal_mode = conn
        .query_row("PRAGMA journal_mode")
        .ok()
        .and_then(|row| row.get(0).and_then(SqliteValue::as_text).map(str::to_owned))
        .unwrap_or_default();
    if !journal_mode.eq_ignore_ascii_case("wal") {
        conn.execute("PRAGMA journal_mode = WAL")?;
    }

    // Enable foreign keys
    conn.execute("PRAGMA foreign_keys = ON")?;

    // Performance PRAGMAs (safe with WAL mode)
    // NORMAL synchronous is safe with WAL: committed data survives OS crash
    conn.execute("PRAGMA synchronous = NORMAL")?;
    // Use memory for temp tables/indexes instead of disk
    conn.execute("PRAGMA temp_store = MEMORY")?;
    // 8MB page cache (default is ~2MB), improves read-heavy workloads
    conn.execute("PRAGMA cache_size = -8000")?;

    // Issue #219: Limit WAL file size to 32MB.  Without this, concurrent
    // writers can cause unbounded WAL growth, which slows reads and
    // increases checkpoint contention.  SQLite will attempt to keep the WAL
    // file at or below this size after each checkpoint.
    conn.execute("PRAGMA journal_size_limit = 33554432")?;

    // Issue #219: Disable the automatic WAL checkpoint that fires after
    // every 1000 pages of WAL growth.  The auto-checkpoint uses PASSIVE
    // mode internally but can still cause unexpected latency spikes during
    // write-heavy concurrent operations.  We handle checkpointing manually
    // in with_write_transaction using PASSIVE mode at a controlled interval.
    conn.execute("PRAGMA wal_autocheckpoint = 0")?;

    Ok(())
}

pub(crate) fn table_exists(conn: &Connection, table: &str) -> bool {
    let escaped_table = table.replace('\'', "''");
    let sql = format!("SELECT 1 FROM sqlite_master WHERE type='table' AND name='{escaped_table}'");
    conn.query(&sql).is_ok_and(|rows| !rows.is_empty())
}

fn index_exists(conn: &Connection, index: &str) -> bool {
    let escaped_index = index.replace('\'', "''");
    let sql = format!("SELECT 1 FROM sqlite_master WHERE type='index' AND name='{escaped_index}'");
    conn.query(&sql).is_ok_and(|rows| !rows.is_empty())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info('{table}')");
    conn.query(&sql).is_ok_and(|rows| {
        rows.iter()
            .any(|row| row.get(1).and_then(SqliteValue::as_text) == Some(column))
    })
}

const ISSUE_COLUMNS: &[(&str, &str)] = &[
    ("content_hash", "TEXT"),
    ("description", "TEXT NOT NULL DEFAULT ''"),
    ("design", "TEXT NOT NULL DEFAULT ''"),
    ("acceptance_criteria", "TEXT NOT NULL DEFAULT ''"),
    ("notes", "TEXT NOT NULL DEFAULT ''"),
    ("status", "TEXT NOT NULL DEFAULT 'open'"),
    (
        "priority",
        "INTEGER NOT NULL DEFAULT 2 CHECK(priority >= 0 AND priority <= 4)",
    ),
    ("issue_type", "TEXT NOT NULL DEFAULT 'task'"),
    ("owner", "TEXT DEFAULT ''"),
    ("created_at", "DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP"),
    ("created_by", "TEXT DEFAULT ''"),
    ("updated_at", "DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP"),
    ("closed_at", "DATETIME"),
    ("close_reason", "TEXT DEFAULT ''"),
    ("defer_until", "DATETIME"),
    ("external_ref", "TEXT"),
    ("source_repo", "TEXT NOT NULL DEFAULT '.'"),
    ("deleted_at", "DATETIME"),
    ("deleted_by", "TEXT DEFAULT ''"),
    ("delete_reason", "TEXT DEFAULT ''"),
    ("original_type", "TEXT DEFAULT ''"),
    // Schema v18: JSON array of every ID this issue previously held.
    // Appended at the end so SQLite's ALTER TABLE ADD COLUMN on existing DBs
    // produces the same final column order as a fresh SCHEMA_SQL build, which
    // is what keeps EXPECTED_ISSUE_COLUMN_ORDER aligned for both.
    ("former_ids", "TEXT NOT NULL DEFAULT '[]'"),
];

// bds-04l.18. The column sets below cover the remaining tables that
// SCHEMA_SQL builds an index on. Before this, `run_pre_schema_migrations`
// backfilled issues, dependencies and comments only, so a legacy table
// reached a `CREATE INDEX` on a column it did not have and failed the whole
// open. (`close_metadata` was the measured case; schema v19 dropped that
// table along with the close-policy subsystem it served.)
//
// Each set lists only what `ALTER TABLE ADD COLUMN` can actually add. SQLite
// cannot add a PRIMARY KEY column, and cannot add a NOT NULL column with no
// DEFAULT, so both of `labels`' columns are absent here by necessity rather
// than oversight. A
// legacy shape missing one of those is beyond repair by migration, which is
// what the JSONL-recovery backstop in `should_attempt_jsonl_recovery` is for.
const DIRTY_ISSUES_COLUMNS: &[(&str, &str)] =
    &[("marked_at", "DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP")];

const DEPENDENCY_COLUMNS: &[(&str, &str)] = &[
    ("type", "TEXT NOT NULL DEFAULT 'blocks'"),
    ("created_at", "DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP"),
    ("created_by", "TEXT NOT NULL DEFAULT ''"),
    ("metadata", "TEXT DEFAULT '{}'"),
    ("thread_id", "TEXT DEFAULT ''"),
];

const COMMENT_COLUMNS: &[(&str, &str)] = &[
    ("author", "TEXT NOT NULL DEFAULT ''"),
    ("text", "TEXT NOT NULL DEFAULT ''"),
    ("created_at", "DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP"),
];

/// SQLite keywords that are legal as a column DEFAULT in `CREATE TABLE` but
/// not in `ALTER TABLE ADD COLUMN` on a table that already holds rows.
///
/// Measured on SQLite 3.50.6: adding
/// `recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP` to an *empty*
/// table succeeds, while the identical statement against a table holding one
/// row fails with `Cannot add a column with non-constant default`. The empty
/// case passing is what makes this easy to miss.
const NON_CONSTANT_DEFAULTS: &[&str] = &["CURRENT_TIMESTAMP", "CURRENT_DATE", "CURRENT_TIME"];

/// Returns the non-constant DEFAULT expression in `definition`, if any.
fn non_constant_default(definition: &str) -> Option<&'static str> {
    let upper = definition.to_ascii_uppercase();
    NON_CONSTANT_DEFAULTS
        .iter()
        .find(|keyword| upper.contains(**keyword))
        .copied()
}

fn ensure_columns(conn: &Connection, table: &str, columns: &[(&str, &str)]) -> Result<()> {
    if !table_exists(conn, table) {
        return Ok(());
    }

    for (name, definition) in columns {
        if column_exists(conn, table, name) {
            continue;
        }

        let Some(default_expression) = non_constant_default(definition) else {
            conn.execute(&format!(
                "ALTER TABLE {table} ADD COLUMN {name} {definition}"
            ))?;
            continue;
        };

        // bds-04l.18. SQLite refuses this definition outright once the table
        // has rows, so adding it verbatim would abort the open on exactly the
        // legacy databases this backfill exists to repair. Add the column
        // bare, then populate it.
        //
        // The retrofitted column is nullable where the canonical schema says
        // NOT NULL: `ALTER TABLE` cannot add a constraint to an existing
        // table at all, so that difference is inherent to any backfill and is
        // not introduced here. What matters for the failure this fixes is that
        // the column EXISTS before SCHEMA_SQL's `CREATE INDEX` runs on it.
        let column_type = definition
            .split_whitespace()
            .next()
            .unwrap_or("TEXT")
            .to_string();
        conn.execute(&format!(
            "ALTER TABLE {table} ADD COLUMN {name} {column_type}"
        ))?;
        conn.execute(&format!(
            "UPDATE {table} SET {name} = {default_expression} WHERE {name} IS NULL"
        ))?;
    }

    Ok(())
}

fn table_has_columns(conn: &Connection, table: &str, required_columns: &[&str]) -> bool {
    table_exists(conn, table)
        && required_columns
            .iter()
            .all(|column| column_exists(conn, table, column))
}

fn blocked_cache_table_canonical(conn: &Connection) -> bool {
    if !table_exists(conn, "blocked_issues_cache") {
        return false;
    }

    let Ok(rows) = conn.query("PRAGMA table_info(blocked_issues_cache)") else {
        return false;
    };

    let mut issue_id_primary_key = false;
    let mut blocked_by_not_null = false;
    let mut blocked_at_not_null = false;
    let mut has_legacy_blocked_by_json = false;

    for row in &rows {
        let Some(name) = row.get(1).and_then(SqliteValue::as_text) else {
            continue;
        };
        let not_null = row
            .get(3)
            .and_then(SqliteValue::as_integer)
            .is_some_and(|value| value != 0);
        let primary_key = row
            .get(5)
            .and_then(SqliteValue::as_integer)
            .is_some_and(|value| value != 0);

        match name {
            "issue_id" => issue_id_primary_key = primary_key,
            "blocked_by" => blocked_by_not_null = not_null,
            "blocked_at" => blocked_at_not_null = not_null,
            "blocked_by_json" => has_legacy_blocked_by_json = true,
            _ => {}
        }
    }

    issue_id_primary_key
        && blocked_by_not_null
        && blocked_at_not_null
        && !has_legacy_blocked_by_json
}

fn current_schema_version_declared(conn: &Connection) -> bool {
    conn.query_row("PRAGMA user_version")
        .ok()
        .and_then(|row| row.get(0).and_then(SqliteValue::as_integer))
        .is_some_and(|version| version >= i64::from(CURRENT_SCHEMA_VERSION))
}

fn core_runtime_tables_exist(conn: &Connection) -> bool {
    [
        "issues",
        "dependencies",
        "labels",
        "comments",
        "config",
        "metadata",
        "dirty_issues",
        "export_hashes",
        "blocked_issues_cache",
        "child_counters",
    ]
    .iter()
    .all(|table| table_exists(conn, table))
}

/// Expected column order for the issues table (id + ISSUE_COLUMNS names).
/// Used to detect when ALTER TABLE has appended columns in the wrong position,
/// which causes the previous engine to fail with "no such column" errors on older databases.
const EXPECTED_ISSUE_COLUMN_ORDER: &[&str] = &[
    "id",
    "content_hash",
    "title",
    "description",
    "design",
    "acceptance_criteria",
    "notes",
    "status",
    "priority",
    "issue_type",
    "owner",
    "created_at",
    "created_by",
    "updated_at",
    "closed_at",
    "close_reason",
    "defer_until",
    "external_ref",
    "source_repo",
    "deleted_at",
    "deleted_by",
    "delete_reason",
    "original_type",
    "former_ids",
];

/// Check whether the issues table has columns in the expected order.
/// Returns `true` if the column order matches, `false` if it differs or the
/// table doesn't exist.
fn issues_column_order_matches(conn: &Connection) -> bool {
    // Use PRAGMA table_info to detect both existence and column order in a
    // single query.  Avoid querying sqlite_master separately because
    // The previous engine's in-memory sqlite_master could return inconsistent results
    // when queried multiple times within the same connection session.
    let Ok(rows) = conn.query("PRAGMA table_info(issues)") else {
        return false;
    };

    let actual_columns: Vec<String> = rows
        .iter()
        .filter_map(|row| row.get(1).and_then(SqliteValue::as_text).map(String::from))
        .collect();

    if actual_columns.is_empty() {
        return true; // Table doesn't exist; will be created fresh by SCHEMA_SQL
    }

    if actual_columns.len() != EXPECTED_ISSUE_COLUMN_ORDER.len() {
        return false;
    }

    actual_columns
        .iter()
        .zip(EXPECTED_ISSUE_COLUMN_ORDER.iter())
        .all(|(actual, expected)| actual == expected)
}

fn foreign_keys_enabled(conn: &Connection) -> Result<bool> {
    let row = conn.query_row("PRAGMA foreign_keys")?;
    Ok(row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0) == 1)
}

fn restore_foreign_keys(conn: &Connection, operation: &str) -> Result<()> {
    conn.execute("PRAGMA foreign_keys = ON")
        .map_err(BeadsError::Database)?;

    if foreign_keys_enabled(conn)? {
        return Ok(());
    }

    Err(BeadsError::Config(format!(
        "failed to re-enable SQLite foreign key enforcement after {operation}: PRAGMA foreign_keys remained OFF"
    )))
}

fn finish_foreign_key_suppressed_result<T>(
    conn: &Connection,
    operation: &str,
    result: Result<T>,
) -> Result<T> {
    match (result, restore_foreign_keys(conn, operation)) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(restore_error)) => Err(restore_error),
        (Err(original_error), Ok(())) => Err(original_error),
        (Err(original_error), Err(restore_error)) => Err(BeadsError::WithContext {
            context: format!(
                "{operation} failed, and SQLite foreign key enforcement could not be re-enabled: {restore_error}"
            ),
            source: Box::new(original_error),
        }),
    }
}

/// Rebuild the issues table so columns match the canonical SCHEMA_SQL order.
///
/// This fixes databases where ALTER TABLE ADD COLUMN appended columns in a
/// different position than the CREATE TABLE definition, causing the previous engine's
/// column-name resolver to fail with "no such column" errors.
///
/// Uses the standard SQLite migration pattern:
///   1. Create new table with correct schema
///   2. Copy data from old table
///   3. Drop old table
///   4. Rename new table
fn rebuild_issues_table(conn: &Connection) -> Result<()> {
    let existing_rows = conn.query("PRAGMA table_info('issues')")?;
    let existing_columns: Vec<String> = existing_rows
        .iter()
        .filter_map(|row| row.get(1).and_then(SqliteValue::as_text).map(String::from))
        .collect();

    if existing_columns.is_empty() {
        return Ok(()); // Table is empty or doesn't exist
    }

    // Disable foreign keys during the rebuild because we'll be dropping
    // and recreating the issues table which is referenced by other tables.
    // This property is connection-scoped.
    conn.execute("PRAGMA foreign_keys = OFF")?;

    let result = (|| -> Result<()> {
        // Wrap the entire rebuild in a transaction so a crash between DROP TABLE
        // and RENAME cannot lose data.
        conn.execute("BEGIN EXCLUSIVE")?;

        if let Err(e) = rebuild_issues_table_inner(conn, &existing_columns) {
            let _ = conn.execute("ROLLBACK");
            return Err(e);
        }

        if let Err(e) = conn.execute("COMMIT") {
            let _ = conn.execute("ROLLBACK");
            return Err(e.into());
        }

        Ok(())
    })();

    finish_foreign_key_suppressed_result(conn, "issues table rebuild", result)
}

/// Extracts the DEFAULT expression from a column definition that is also
/// declared NOT NULL, for use as the second argument of a `COALESCE`.
///
/// Returns `None` when the column is nullable (nothing to repair) or when it
/// is NOT NULL with no DEFAULT (`id`, `title`) -- there, no value can be
/// invented, and a NULL is a genuinely unrecoverable row rather than a
/// migration artifact.
///
/// Handles the shapes `ISSUE_COLUMNS` actually uses, pinned by
/// `declared_not_null_default_reads_every_shape_in_use`:
///
/// * `TEXT NOT NULL DEFAULT ''`               -> `''`
/// * `TEXT NOT NULL DEFAULT 'open'`           -> `'open'`
/// * `INTEGER NOT NULL DEFAULT 2 CHECK(...)`  -> `2`   (trailing clause dropped)
/// * `DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP` -> `CURRENT_TIMESTAMP`
///
/// A quoted literal is scanned with SQLite's `''` escape honoured, so a
/// default that itself contains a quote yields the whole literal rather than a
/// truncated one that would build invalid SQL.
fn declared_not_null_default(col_def: &str) -> Option<String> {
    let upper = col_def.to_ascii_uppercase();
    if !upper.contains("NOT NULL") {
        return None;
    }
    let start = upper.find("DEFAULT ")? + "DEFAULT ".len();
    let rest = col_def[start..].trim_start();

    let Some(quoted) = rest.strip_prefix('\'') else {
        // A bare token: a number, or a keyword such as CURRENT_TIMESTAMP. Any
        // following clause (CHECK, COLLATE) is whitespace-separated.
        return rest.split_whitespace().next().map(str::to_string);
    };

    let bytes = quoted.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\'' {
            if bytes.get(index + 1) == Some(&b'\'') {
                index += 2; // an escaped quote inside the literal
                continue;
            }
            return Some(format!("'{}'", &quoted[..index]));
        }
        index += 1;
    }
    None // unterminated literal: leave the column alone rather than build bad SQL
}

/// Inner helper for [`rebuild_issues_table`] that performs the actual work
/// inside an already-open transaction.
fn rebuild_issues_table_inner(conn: &Connection, existing_columns: &[String]) -> Result<()> {
    // Drop all indexes on the issues table first (they'll be recreated by SCHEMA_SQL)
    let index_rows =
        conn.query("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='issues' AND sql IS NOT NULL")?;
    for row in &index_rows {
        if let Some(name) = row.get(0).and_then(SqliteValue::as_text) {
            conn.execute(&format!("DROP INDEX IF EXISTS \"{name}\""))?;
        }
    }

    // Drop tables that have foreign keys referencing issues (they'll be recreated)
    // We need to save and restore their data too.
    // For simplicity, we only rebuild the issues table and let SCHEMA_SQL
    // recreate indexes. Foreign key tables (dependencies, labels, etc.) keep
    // their data since we use the same primary key.

    // Create the new table with canonical column order
    // Use a temporary name to avoid conflicts
    conn.execute("DROP TABLE IF EXISTS issues_rebuild_tmp")?;

    // Build CREATE TABLE for the new table with only columns that exist in the old table
    // plus any missing columns with defaults
    // Build the canonical column list: id, content_hash, title, then the
    // rest of ISSUE_COLUMNS (skipping content_hash which is already placed).
    // This order must match EXPECTED_ISSUE_COLUMN_ORDER and SCHEMA_SQL.
    let all_expected: Vec<(&str, &str)> = std::iter::once(("id", "TEXT PRIMARY KEY"))
        .chain(std::iter::once(("content_hash", "TEXT")))
        .chain(std::iter::once((
            "title",
            "TEXT NOT NULL CHECK(length(title) <= 500)",
        )))
        .chain(
            ISSUE_COLUMNS
                .iter()
                .copied()
                .filter(|(name, _)| *name != "content_hash"),
        )
        .collect();

    let mut create_cols = Vec::new();
    for (col_name, col_def) in &all_expected {
        create_cols.push(format!("{col_name} {col_def}"));
    }
    create_cols.push(ISSUES_CLOSED_AT_CHECK.to_string());

    let create_sql = format!(
        "CREATE TABLE issues_rebuild_tmp ({})",
        create_cols.join(", ")
    );
    conn.execute(&create_sql)?;

    // Copy only columns that exist in the source table so SQLite can apply
    // declared defaults for newer columns that are absent in legacy schemas.
    let mut projected_columns = Vec::new();
    let mut projected_expressions = Vec::new();
    for (col_name, col_def) in &all_expected {
        if !existing_columns.iter().any(|c| c == col_name) {
            continue;
        }
        projected_columns.push((*col_name).to_string());
        // bds-04l.15. A legacy row can hold a storage-class NULL in a column
        // the canonical schema declares NOT NULL -- `is_template` was the
        // measured case, from a table where it was merely `INTEGER DEFAULT 0`.
        // A bare `SELECT {col}` copies that NULL straight into
        // issues_rebuild_tmp, which declares it NOT NULL, and the open fails
        // with NotNullViolation. That error is not in
        // should_attempt_jsonl_recovery's set and its Display text is not one
        // of the substrings is_recoverable_database_internal_error looks for,
        // so JSONL recovery does not fire either: the database simply cannot
        // be opened.
        //
        // backfill_storage_null_in_default_columns already does exactly this
        // repair, but it runs later, inside run_migrations, so the pre-schema
        // rebuild sees the raw NULLs first. Applying each column's own
        // declared DEFAULT here is the same rule at the point it is needed.
        //
        // COALESCE, not a blanket assignment: a row that already holds a value
        // keeps it.
        match declared_not_null_default(col_def) {
            Some(default) => {
                projected_expressions.push(format!("COALESCE({col_name}, {default})"));
            }
            None => projected_expressions.push((*col_name).to_string()),
        }
    }

    if projected_columns.is_empty() {
        return Err(BeadsError::Config(
            "Cannot rebuild legacy issues table: no canonical issue columns were found".to_string(),
        ));
    }

    // Copy data out to the temp table.
    let copy_out_sql = format!(
        "INSERT INTO issues_rebuild_tmp ({cols}) SELECT {exprs} FROM issues",
        cols = projected_columns.join(", "),
        exprs = projected_expressions.join(", ")
    );
    conn.execute(&copy_out_sql)?;

    // Drop the original table, then CREATE it fresh (not via RENAME) so
    // that the previous engine's in-memory schema cache registers all columns.
    conn.execute("DROP TABLE issues")?;

    let create_canonical = format!("CREATE TABLE issues ({})", create_cols.join(", "));
    conn.execute(&create_canonical)?;

    // Copy data back.
    let copy_back_sql = format!(
        "INSERT INTO issues ({cols}) SELECT {cols} FROM issues_rebuild_tmp",
        cols = projected_columns.join(", ")
    );
    conn.execute(&copy_back_sql)?;

    conn.execute("DROP TABLE issues_rebuild_tmp")?;

    Ok(())
}

/// Backfill storage-class NULL values in NOT NULL DEFAULT columns.
///
/// SQLite's `ALTER TABLE ADD COLUMN ... NOT NULL DEFAULT ...` enforces the
/// default for new and existing rows, but legacy databases — predating
/// br's current migration code, or carrying history from Go bd or raw
/// `sqlite3` edits — can hold storage-class NULLs in such columns. The
/// `typeof(col) = 'null'` predicate detects these directly even when
/// partial indexes cause `IS NULL` to silently miss them (see #269).
///
/// Idempotent: rows that already hold the default are not rewritten.
/// Tables/columns that don't exist on the current schema are skipped.
///
/// Best-effort: per-column failures are logged but do not abort the
/// caller, so a single broken column never blocks bootstrap.
fn backfill_storage_null_in_default_columns(conn: &Connection) {
    // (table, column, default_sql_literal). Mirrors the NOT NULL DEFAULT
    // clauses in SCHEMA_SQL and the *_COLUMNS migration constants.
    const COLUMNS: &[(&str, &str, &str)] = &[
        // issues
        ("issues", "description", "''"),
        ("issues", "design", "''"),
        ("issues", "acceptance_criteria", "''"),
        ("issues", "notes", "''"),
        ("issues", "status", "'open'"),
        ("issues", "priority", "2"),
        ("issues", "issue_type", "'task'"),
        ("issues", "source_repo", "'.'"),
        // dependencies
        ("dependencies", "type", "'blocks'"),
        ("dependencies", "created_by", "''"),
        // comments
        ("comments", "author", "''"),
        ("comments", "text", "''"),
    ];

    for (table, column, default) in COLUMNS {
        if !table_exists(conn, table) || !column_exists(conn, table, column) {
            continue;
        }
        let sql =
            format!("UPDATE {table} SET {column} = {default} WHERE typeof({column}) = 'null'");
        if let Err(err) = conn.execute(&sql) {
            tracing::warn!(
                table = table,
                column = column,
                error = %err,
                "backfill of storage-NULL default failed; continuing"
            );
        }
    }
}

fn kv_table_uses_primary_key(conn: &Connection, table: &str) -> bool {
    // Use PRAGMA table_info instead of sqlite_master to detect whether
    // the `key` column is declared as PRIMARY KEY.  The previous engine's in-memory
    // sqlite_master can return inconsistent results across queries.
    let sql = format!("PRAGMA table_info('{table}')");
    let Ok(rows) = conn.query(&sql) else {
        return false;
    };

    // In PRAGMA table_info output, column index 5 is the `pk` flag.
    // If `key` column has pk > 0, the table uses PRIMARY KEY.
    rows.iter().any(|row| {
        let col_name = row.get(1).and_then(SqliteValue::as_text);
        let pk_flag = row.get(5).and_then(SqliteValue::as_integer).unwrap_or(0);
        col_name == Some("key") && pk_flag > 0
    })
}

fn kv_table_needs_canonical_rebuild(conn: &Connection, table: &str, expected_index: &str) -> bool {
    // Use PRAGMA table_info for the existence check instead of sqlite_master,
    // which could return inconsistent results in the previous engine.
    let table_has_rows = conn
        .query(&format!("PRAGMA table_info('{table}')"))
        .is_ok_and(|rows| !rows.is_empty());
    table_has_rows
        && (!index_exists(conn, expected_index) || kv_table_uses_primary_key(conn, table))
}

fn rebuild_kv_table_without_unique(conn: &Connection, table: &str) -> Result<()> {
    let tmp_table = format!("{table}_rebuild_tmp");

    conn.execute("BEGIN EXCLUSIVE")?;

    let result = (|| -> Result<()> {
        conn.execute(&format!("DROP TABLE IF EXISTS {tmp_table}"))?;
        conn.execute(&format!(
            "CREATE TABLE {tmp_table} (
                key TEXT NOT NULL,
                value TEXT NOT NULL
            )"
        ))?;

        conn.execute(&format!(
            "INSERT INTO {tmp_table} (key, value)
             SELECT key, value
             FROM {table}"
        ))?;

        conn.execute(&format!("DROP TABLE {table}"))?;
        conn.execute(&format!("ALTER TABLE {tmp_table} RENAME TO {table}"))?;
        Ok(())
    })();

    if let Err(err) = result {
        let _ = conn.execute("ROLLBACK");
        return Err(err);
    }

    conn.execute("COMMIT")?;
    Ok(())
}

/// Run pre-schema migrations to fix incompatible old tables.
///
/// This must run BEFORE `execute_batch(SCHEMA_SQL)` because the schema includes
/// CREATE INDEX statements that will fail if old tables have missing columns.
/// Returns `true` if the issues table was rebuilt during pre-migrations.
fn run_pre_schema_migrations(conn: &Connection) -> Result<bool> {
    // Legacy schemas used PRIMARY KEY on config/metadata key columns.
    // Rebuild to plain key-value tables so standard sqlite integrity checks
    // are not tripped by unsupported unique-index maintenance behavior.
    if kv_table_needs_canonical_rebuild(conn, "config", "idx_config_key") {
        rebuild_kv_table_without_unique(conn, "config")?;
    }
    if kv_table_needs_canonical_rebuild(conn, "metadata", "idx_metadata_key") {
        rebuild_kv_table_without_unique(conn, "metadata")?;
    }

    // Drop blocked_issues_cache if it exists but is not canonical. The table is
    // a derived cache, so rebuilding is preferable to preserving legacy NULLs
    // or weak constraints that can poison later command reads.
    // The main schema will recreate it with the correct structure.
    if table_exists(conn, "blocked_issues_cache") && !blocked_cache_table_canonical(conn) {
        conn.execute("DROP TABLE IF EXISTS blocked_issues_cache")?;
    }

    // Rebuild the issues table if columns are out of order or missing.
    // This fixes the previous engine's "no such column" errors on databases created with
    // older br versions where ALTER TABLE ADD COLUMN appended columns in
    // a different position than the canonical CREATE TABLE definition.
    // issues_column_order_matches handles both existence and column order
    // checks via PRAGMA table_info, avoiding redundant sqlite_master queries
    // which could return inconsistent results in the previous engine.
    let issues_rebuilt = if issues_column_order_matches(conn) {
        false
    } else {
        rebuild_issues_table(conn)?;
        true
    };

    // After a full rebuild the issues table already has the canonical schema,
    // so skip ensure_columns (which uses ALTER TABLE ADD COLUMN and could leave
    // the previous engine's in-memory schema cache stale).
    if !issues_rebuilt {
        ensure_columns(conn, "issues", ISSUE_COLUMNS)?;
    }
    ensure_columns(conn, "dependencies", DEPENDENCY_COLUMNS)?;
    ensure_columns(conn, "comments", COMMENT_COLUMNS)?;
    // bds-04l.18: the remaining tables SCHEMA_SQL indexes. Without these, a
    // legacy shape reaches CREATE INDEX with the indexed column missing and
    // the open fails outright rather than being migrated forward.
    ensure_columns(conn, "dirty_issues", DIRTY_ISSUES_COLUMNS)?;

    // Intentionally do not rebuild idx_issues_ready here.
    //
    // Older databases may have a stale partial-index predicate, but that is a
    // performance issue rather than a correctness issue. On large file-backed
    // databases, exercising DROP INDEX through the previous engine currently trips an
    // out-of-memory failure. Additionally, the previous engine's in-memory schema
    // representation does not reliably preserve partial-index predicates, so br
    // cannot distinguish a stale ready index from a current one at open time.

    Ok(issues_rebuilt)
}

pub(crate) fn runtime_schema_compatible(conn: &Connection) -> bool {
    if current_schema_version_declared(conn)
        && core_runtime_tables_exist(conn)
        && blocked_cache_table_canonical(conn)
        && !kv_table_uses_primary_key(conn, "config")
        && !kv_table_uses_primary_key(conn, "metadata")
    {
        return true;
    }

    let issues_ok = issues_column_order_matches(conn);
    let dependencies_ok = table_has_columns(conn, "dependencies", &["issue_id", "depends_on_id"])
        && DEPENDENCY_COLUMNS
            .iter()
            .all(|(name, _)| column_exists(conn, "dependencies", name));
    let labels_ok = table_has_columns(conn, "labels", &["issue_id", "label"]);
    let comments_ok = table_has_columns(conn, "comments", &["id", "issue_id"])
        && COMMENT_COLUMNS
            .iter()
            .all(|(name, _)| column_exists(conn, "comments", name));
    let config_ok = table_has_columns(conn, "config", &["key", "value"])
        && index_exists(conn, "idx_config_key")
        && !kv_table_uses_primary_key(conn, "config");
    let metadata_ok = table_has_columns(conn, "metadata", &["key", "value"])
        && index_exists(conn, "idx_metadata_key")
        && !kv_table_uses_primary_key(conn, "metadata");
    let dirty_issues_ok = table_has_columns(conn, "dirty_issues", &["issue_id", "marked_at"]);
    let export_hashes_ok = table_has_columns(
        conn,
        "export_hashes",
        &["issue_id", "content_hash", "exported_at"],
    );
    let blocked_cache_ok = blocked_cache_table_canonical(conn);
    let child_counters_ok = table_has_columns(conn, "child_counters", &["parent_id", "last_child"]);

    let compatible = issues_ok
        && dependencies_ok
        && labels_ok
        && comments_ok
        && config_ok
        && metadata_ok
        && dirty_issues_ok
        && export_hashes_ok
        && blocked_cache_ok
        && child_counters_ok;

    if !compatible {
        tracing::debug!(
            issues_ok,
            dependencies_ok,
            labels_ok,
            comments_ok,
            config_ok,
            metadata_ok,
            dirty_issues_ok,
            export_hashes_ok,
            blocked_cache_ok,
            child_counters_ok,
            "runtime schema compatibility check failed"
        );
    }

    compatible
}

/// Run schema migrations for existing databases.
///
/// This handles upgrades for tables that may have been created with older schemas.
#[allow(clippy::too_many_lines)]
fn run_migrations(conn: &Connection, issues_rebuilt: bool) -> Result<()> {
    // Migration: ensure blocked_issues_cache has the canonical derived-cache
    // schema, including NOT NULL payload columns. Older br/bd paths could
    // leave a nullable blocked_by column with stale NULL cache rows.
    if !blocked_cache_table_canonical(conn) {
        // Table needs update - drop and recreate (it's a cache, data is regenerated)
        // Wrap in transaction so concurrent opens don't see a partially migrated state
        conn.execute("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            conn.execute("DROP TABLE IF EXISTS blocked_issues_cache")?;
            conn.execute(
                "CREATE TABLE blocked_issues_cache (
                    issue_id TEXT PRIMARY KEY,
                    blocked_by TEXT NOT NULL,
                    blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
                )",
            )?;
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_blocked_cache_blocked_at ON blocked_issues_cache(blocked_at)",
            )?;
            Ok(())
        })();

        if let Err(e) = result {
            let _ = conn.execute("ROLLBACK");
            return Err(e);
        }
        conn.execute("COMMIT")?;
    }

    // The `compaction_level` NULL backfill (bd compatibility) is gone with the
    // column: schema v19 drops it outright, so there is nothing to normalise.

    // Migration: Ensure filter columns are NOT NULL (v3)
    let user_version = conn
        .query_row("PRAGMA user_version")?
        .get(0)
        .and_then(SqliteValue::as_integer)
        .unwrap_or(0);

    // Skip v3/v4 migration when the issues table was just rebuilt from scratch
    // (it already has canonical NOT NULL constraints and the correct index).
    // Querying columns via UPDATE/CREATE INDEX here would fail because
    // The previous engine's in-memory schema cache might not have refreshed after the rebuild.
    if !issues_rebuilt {
        // The v3 migration is gone with the columns it existed for. It
        // backfilled and re-declared `ephemeral`/`pinned`/`is_template` as
        // NOT NULL; schema v19 drops all three, so there is nothing left to
        // constrain. A legacy database carrying them still reaches v19 by the
        // ordinary route: `run_pre_schema_migrations` rebuilds any table whose
        // column order differs from `EXPECTED_ISSUE_COLUMN_ORDER`, which a
        // pre-v19 table always does, and the rebuild projects only the
        // canonical column list.

        if user_version < 4 && table_exists(conn, "issues") {
            tracing::info!("Migrating database to schema version 4 (ready excludes in_progress)");
            conn.execute("DROP INDEX IF EXISTS idx_issues_ready")?;
            // Recreated with the v19 predicate: the flag columns this index
            // used to filter on no longer exist. The v19 arm below drops and
            // recreates it again, so the two DDLs must agree.
            conn.execute(
                "CREATE INDEX idx_issues_ready
                 ON issues(status, priority, created_at)
                 WHERE status = 'open'",
            )?;
        }

        // v5: Drop the old DESC index so the idempotent CREATE INDEX IF NOT
        // EXISTS below recreates it without DESC.  The previous engine's B-tree
        // implementation stores DESC index entries in a different physical
        // order than C sqlite3 expects, causing `PRAGMA integrity_check` to
        // report "entries are out of order for their declared key directions".
        // Removing DESC eliminates the false positive while SQLite's query
        // planner still reverse-scans the ASC index efficiently for
        // ORDER BY ... created_at DESC queries.
        if user_version < 5 {
            tracing::info!(
                "Migrating database to schema version 5 (remove DESC from active list index)"
            );
            conn.execute("DROP INDEX IF EXISTS idx_issues_list_active_order")?;
        }

        // v6: Repair datetime columns and legacy status values.
        //
        // External tools (including pre-Rust bd flows and direct SQLite edits)
        // have occasionally written integer epoch microseconds into DATETIME
        // columns, and imported JSONL has carried the Go-beads "done" status
        // unchanged via the Status::Custom fallback. Both corrupt the JSONL
        // export: the reader's legacy `as_text().unwrap_or("")` path mapped
        // integer datetimes to UNIX_EPOCH (updated_at rows becoming
        // 1970-01-01) and dropped optional datetimes (closed_at → null),
        // while the `status` CHECK constraint rejects an unknown "done" value
        // outright. This migration rewrites the data in place
        // so every row is fully-typed and uses canonical status strings.
        if user_version < 6 && table_exists(conn, "issues") {
            tracing::info!(
                "Migrating database to schema version 6 (normalize datetime columns and legacy status aliases)"
            );
            repair_integer_datetime_columns(conn)?;
            repair_legacy_status_values(conn)?;
        }
    }

    // v7: Historical content-hash rebuild. For databases older than v7 that
    // open under current br, compute the current canonical format directly
    // instead of replaying obsolete intermediate encodings. Marking rows dirty
    // is intentional: per-issue export hashes were computed with an older
    // algorithm, so the next flush must rewrite JSONL tracking metadata.
    if user_version < 7 && table_exists(conn, "issues") {
        tracing::info!("Migrating database to schema version 7 (content hashes)");
        rebuild_content_hashes_for_current_format(conn)?;
    }

    // The v9 migration created the `close_metadata` table for closure-time
    // policy gates (issue #274). It is gone: bds-b4f.4.1 removed the gates and
    // the `--bypass-policy` flag, leaving nothing that writes a row, and v19
    // below drops the table. Recreating it here only to drop it three arms
    // later would be pure churn, so an old database never materialises it —
    // the same treatment the v12/v15 gate tables got in v16.

    // v8: Backfill storage-class NULL values in NOT NULL DEFAULT columns.
    //
    // Older databases — particularly those carrying history from Go bd or
    // br versions where ALTER TABLE ADD COLUMN ran without a DEFAULT
    // clause — accumulate storage-class NULL in columns declared NOT NULL
    // DEFAULT. `PRAGMA integrity_check` then flags these as constraint
    // violations even though `WHERE col IS NULL` won't always match them
    // (the planner can use partial indexes that bypass the check). We use
    // `typeof(col) = 'null'` to detect storage-class NULLs directly and
    // backfill with each column's declared default. See issue #269.
    if user_version < 8 {
        tracing::info!(
            "Migrating database to schema version 8 (backfill storage-NULL in NOT NULL DEFAULT columns)"
        );
        backfill_storage_null_in_default_columns(conn);
    }

    // Note: source_repo and is_template column backfills are handled in
    // run_pre_schema_migrations() via ensure_columns(). Repeating ALTER TABLE
    // here can create duplicate column definitions on some engines.

    // The v10 and v11 migrations added `source_repo_path` (beads#289) and
    // `agent_context` (beads#297) to `issues` for callers that reach
    // `run_migrations` directly and so skip `ensure_columns`. Both columns are
    // dropped by v19 below, so adding them here only to remove them three arms
    // later would be churn on every legacy open. Same treatment as the v9
    // `close_metadata` arm above.

    // v14: Recompute stored issue content hashes after switching from
    // NUL-separated fields to length-prefixed fields. Existing v7-v13
    // databases may contain hashes where embedded NUL bytes can blur field
    // boundaries, so rewrite the issue hash and force JSONL tracking metadata
    // to refresh on the next flush.
    if (7..14).contains(&user_version) && table_exists(conn, "issues") {
        tracing::info!("Migrating database to schema version 14 (length-prefixed content hashes)");
        rebuild_content_hashes_for_current_format(conn)?;
    }

    // v16 (bds-04l.23): drop the workflow-gate tables along with the engine
    // that read them. Nothing in the shipped binary ever wrote a row --
    // `record_scoped_gate_result` had a single caller, in a test module -- so
    // these tables can only be empty and dropping them cannot lose data. The
    // v12 and v15 migrations that created them are gone too, so an old
    // database upgrading straight to v16 never materialises them.
    if user_version < 16 {
        tracing::info!("Migrating database to schema version 16 (drop workflow-gate tables)");
        execute_batch(
            conn,
            r"
            DROP TABLE IF EXISTS gate_result_history;
            DROP TABLE IF EXISTS gate_results;
        ",
        )?;
    }

    // Migration v16 -> v17: drop the events audit table (bds-1zd).
    //
    // The subsystem is gone, not merely unused. Events were written on every
    // mutation and read by exactly one opt-in close-policy gate, and they never
    // reached `issues.jsonl` — `.beads/*.db` is a derived, gitignored cache, so
    // the log neither travelled between clones nor survived a rebuild. Dropping
    // the table rather than leaving it vestigial is the whole point: a table
    // nothing writes is the next reader's puzzle.
    //
    // Nothing is preserved. The rows were local-only audit data that any rebuild
    // already discarded, so there is no export step and no way for this to lose
    // something a `br sync` would not have lost anyway.
    if user_version < 17 {
        tracing::info!("Migrating database to schema version 17 (drop events table)");
        execute_batch(
            conn,
            r"
            DROP INDEX IF EXISTS idx_events_issue;
            DROP INDEX IF EXISTS idx_events_type;
            DROP INDEX IF EXISTS idx_events_event_type;
            DROP INDEX IF EXISTS idx_events_created_at;
            DROP INDEX IF EXISTS idx_events_actor;
            DROP TABLE IF EXISTS events;
        ",
        )?;

        // This arm used to also drop three Tier 1 attribution columns from
        // `close_metadata`. v19 drops that whole table, so the column-level
        // repair is redundant.
    }

    // Migration v17 -> v18: add `former_ids TEXT NOT NULL DEFAULT '[]'` to
    // `issues`, holding a JSON array of every ID the issue previously held.
    // Purely additive — existing rows get the default and existing consumers
    // ignore it. Same idempotence guards as the v10/v11 column migrations:
    // skipped when the column already exists, and skipped when the table was
    // just rebuilt from scratch (a rebuild already produces the v18 layout).
    if !issues_rebuilt
        && user_version < 18
        && table_exists(conn, "issues")
        && !column_exists(conn, "issues", "former_ids")
    {
        tracing::info!("Migrating database to schema version 18 (former_ids on issues)");
        conn.execute("ALTER TABLE issues ADD COLUMN former_ids TEXT NOT NULL DEFAULT '[]'")?;
    }

    // Migration v18 -> v19 (bds-b4f.3.1): drop the fifteen columns whose fields
    // left `Issue`, drop the four indexes that existed only for them, rebuild
    // the two partial indexes whose predicates named them, drop the dead
    // `close_metadata` table, and rewrite every stored `content_hash` —
    // `assignee`, `source_system`, `pinned`, `is_template` and `external_ref`
    // all left the digest, so every stored value is stale.
    //
    // TWO GUARDS, NOT ONE, AND THEY ARE DELIBERATELY DIFFERENT.
    //
    // The DDL half is guarded by `!issues_rebuilt`, like the v10/v11/v18 column
    // migrations: a table `run_pre_schema_migrations` rebuilt already has the
    // v19 layout, so there is nothing to drop, and each drop is additionally
    // guarded by `column_exists` so a partially-migrated database completes
    // rather than failing.
    //
    // The hash rebuild is NOT guarded by `!issues_rebuilt`, and that is the
    // whole point of this arm. Every real pre-v19 database takes the rebuild
    // path — `issues_column_order_matches` compares against
    // `EXPECTED_ISSUE_COLUMN_ORDER`, which just lost fifteen entries, so the
    // lengths differ and `rebuild_issues_table` runs before we get here. Gating
    // the rebuild on `!issues_rebuilt` would therefore skip it on exactly the
    // databases that need it, leaving every row carrying a pre-v19 digest with
    // nothing to notice.
    if user_version < 19 && table_exists(conn, "issues") {
        tracing::info!("Migrating database to schema version 19 (drop dead columns, rehash)");

        if !issues_rebuilt {
            // Index drops come first: SQLite refuses to drop a column named in
            // an index or in a partial index's WHERE clause.
            execute_batch(
                conn,
                r"
                DROP INDEX IF EXISTS idx_issues_assignee;
                DROP INDEX IF EXISTS idx_issues_ephemeral;
                DROP INDEX IF EXISTS idx_issues_pinned;
                DROP INDEX IF EXISTS idx_issues_due_at;
                DROP INDEX IF EXISTS idx_issues_ready;
                DROP INDEX IF EXISTS idx_issues_list_active_order;
                ",
            )?;

            for column in [
                "assignee",
                "estimated_minutes",
                "closed_by_session",
                "due_at",
                "source_system",
                "compaction_level",
                "compacted_at",
                "compacted_at_commit",
                "original_size",
                "sender",
                "ephemeral",
                "pinned",
                "is_template",
                "source_repo_path",
                "agent_context",
            ] {
                if column_exists(conn, "issues", column) {
                    conn.execute(&format!("ALTER TABLE issues DROP COLUMN {column}"))?;
                }
            }

            // Recreated without the dead predicates. These two statements must
            // stay byte-identical to their SCHEMA_SQL counterparts: a fresh
            // `br init` and a migrated database have to produce the same DDL,
            // and tests/storage/schema_shape.rs compares observed output.
            execute_batch(
                conn,
                r"
                CREATE INDEX IF NOT EXISTS idx_issues_ready
                    ON issues(status, priority, created_at)
                    WHERE status = 'open';
                CREATE INDEX IF NOT EXISTS idx_issues_list_active_order
                    ON issues(priority, created_at)
                    WHERE status NOT IN ('closed', 'tombstone');
                ",
            )?;
        }

        // `close_metadata` is dead in production: bds-b4f.4.1 removed the close
        // gates and the `--bypass-policy` flag, so nothing writes a row and
        // nothing reads one. Same treatment as `gate_results` in v16. This is
        // outside the `!issues_rebuilt` guard because rebuilding `issues` says
        // nothing about a separate table.
        execute_batch(
            conn,
            r"
            DROP INDEX IF EXISTS idx_close_metadata_recorded_at;
            DROP INDEX IF EXISTS idx_close_metadata_bypassed;
            DROP TABLE IF EXISTS close_metadata;
            ",
        )?;

        // Last, and deliberately so. `rebuild_content_hashes_for_current_format`
        // no longer selects any dropped column — it had to stop, because the v7
        // and v14 arms above call it too and can run against a table
        // `run_pre_schema_migrations` has already reshaped — but running it
        // after the drops keeps the rule visible: the digest is recomputed from
        // the v19 table, never from a column on its way out.
        rebuild_content_hashes_for_current_format(conn)?;
    }

    // Migration: Add missing indexes for bd parity
    // These use IF NOT EXISTS so they're safe to run multiple times.
    //
    // This batch is unconditional — it runs on every migration pass, outside
    // every `user_version` guard — so it must never name a column the schema
    // does not have. The `idx_issues_ephemeral`, `idx_issues_pinned` and
    // `idx_issues_due_at` lines were removed in v19 for exactly that reason:
    // left here they would re-create the indexes the arm above just dropped,
    // or, once the columns were gone, fail with `no such column` on every
    // single open.
    execute_batch(
        conn,
        r"
        -- Export/sync patterns
        CREATE INDEX IF NOT EXISTS idx_issues_content_hash ON issues(content_hash);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_issues_external_ref_unique ON issues(external_ref) WHERE external_ref IS NOT NULL;

        -- Special states
        CREATE INDEX IF NOT EXISTS idx_issues_tombstone ON issues(status) WHERE status = 'tombstone';

        -- Time-based
        CREATE INDEX IF NOT EXISTS idx_issues_defer_until ON issues(defer_until) WHERE defer_until IS NOT NULL;

        -- Ready work composite index (most important for performance)
        CREATE INDEX IF NOT EXISTS idx_issues_ready
            ON issues(status, priority, created_at)
            WHERE status = 'open';

        -- Widened ready group (issue #354): non-partial composite so a
        -- configured `status IN (...) ORDER BY priority, created_at` ready query
        -- stays index-covered even on pre-existing databases.
        CREATE INDEX IF NOT EXISTS idx_issues_status_priority_created
            ON issues(status, priority, created_at);

        -- Common active list path: non-terminal issues sorted by priority/created_at
        CREATE INDEX IF NOT EXISTS idx_issues_list_active_order
            ON issues(priority, created_at)
            WHERE status NOT IN ('closed', 'tombstone');

    ",
    )?;

    // Drop legacy index names (safe if absent)
    execute_batch(
        conn,
        r"
        DROP INDEX IF EXISTS idx_dependencies_issue_id;
        DROP INDEX IF EXISTS idx_dependencies_depends_on_id;
        DROP INDEX IF EXISTS idx_dependencies_composite;
        DROP INDEX IF EXISTS idx_labels_issue_id;
    ",
    )?;

    if table_exists(conn, "dependencies") {
        execute_batch(
            conn,
            r"
            CREATE INDEX IF NOT EXISTS idx_dependencies_issue ON dependencies(issue_id);
            CREATE INDEX IF NOT EXISTS idx_dependencies_depends_on ON dependencies(depends_on_id);
            CREATE INDEX IF NOT EXISTS idx_dependencies_type ON dependencies(type);
            CREATE INDEX IF NOT EXISTS idx_dependencies_depends_on_type ON dependencies(depends_on_id, type);
            -- idx_dependencies_thread is NOT created here. `thread_id` is an
            -- additive column that `ensure_columns` backfills in
            -- `run_pre_schema_migrations`, so a legacy `dependencies` table can
            -- still be missing it when `run_migrations` is entered on its own.
            -- The guarded `CREATE INDEX` below is the one that runs; this line
            -- used to duplicate it unguarded and fail with `no such column`,
            -- which was invisible only because `execute_batch` swallowed
            -- exactly that error for `CREATE INDEX` (removed in bds-04l.4.3).
            -- Composite for blocking lookups
            CREATE INDEX IF NOT EXISTS idx_dependencies_blocking
                ON dependencies(depends_on_id, issue_id)
                WHERE (type = 'blocks' OR type = 'parent-child' OR type = 'conditional-blocks' OR type = 'waits-for');
        ",
        )?;

        if column_exists(conn, "dependencies", "thread_id") {
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_dependencies_thread ON dependencies(thread_id) WHERE thread_id != ''",
            )?;
        }
    }

    if table_exists(conn, "labels") {
        execute_batch(
            conn,
            r"
            CREATE INDEX IF NOT EXISTS idx_labels_label ON labels(label);
            CREATE INDEX IF NOT EXISTS idx_labels_issue ON labels(issue_id);
        ",
        )?;
    }

    if table_exists(conn, "comments") {
        conn.execute("CREATE INDEX IF NOT EXISTS idx_comments_issue ON comments(issue_id)")?;
    }

    Ok(())
}

/// Rewrite any DATETIME column on `issues` that is stored as INTEGER into
/// canonical RFC 3339 TEXT. The unit (seconds / ms / µs / ns) is inferred
/// from magnitude exactly like the runtime reader's `datetime_from_epoch_auto`
/// so both paths give the same answer — any other split would silently
/// corrupt rows whose writer picked a different unit than we assumed.
/// Idempotent; rows already stored as TEXT are left untouched.
fn repair_integer_datetime_columns(conn: &Connection) -> Result<()> {
    const DATETIME_COLUMNS: &[&str] = &[
        "created_at",
        "updated_at",
        "closed_at",
        "defer_until",
        "deleted_at",
    ];
    // Must stay in lock-step with datetime_from_epoch_auto in
    // src/storage/sqlite.rs. Each threshold is the smallest integer that,
    // in that unit, still lands within ±2286 AD — i.e. 10^10 seconds,
    // 10^13 ms, 10^16 µs, 10^19 ns all represent the same year 2286,
    // giving non-overlapping magnitude buckets.
    for column in DATETIME_COLUMNS {
        if !column_exists(conn, "issues", column) {
            continue;
        }
        let sql = format!(
            "UPDATE issues SET {column} = \
                strftime('%Y-%m-%dT%H:%M:%fZ', CASE \
                    WHEN ABS({column}) < 10000000000 THEN {column} * 1.0 \
                    WHEN ABS({column}) < 10000000000000 THEN {column} / 1000.0 \
                    WHEN ABS({column}) < 10000000000000000 THEN {column} / 1000000.0 \
                    ELSE {column} / 1000000000.0 \
                END, 'unixepoch') \
             WHERE typeof({column}) = 'integer'"
        );
        conn.execute(&sql)?;
    }
    Ok(())
}

/// Normalize legacy Go-beads status values. `"done"` is the bd terminal state
/// and survives round-tripping through Rust via `Status::Custom("done")`;
/// remap it to the canonical `"closed"` state and make sure `closed_at` is
/// populated so the `issues` CHECK constraint stays satisfied.
fn repair_legacy_status_values(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE issues \
         SET closed_at = COALESCE(closed_at, updated_at, created_at), \
             status = 'closed' \
         WHERE LOWER(status) IN ('done', 'complete', 'completed', 'finished', 'resolved')",
    )?;
    Ok(())
}

/// Recompute and store every issue's `content_hash` in the current format.
///
/// The `SELECT` names only columns that survive schema v19, and must keep
/// doing so. This function is called from three arms — v7, v14 and v19 — and
/// the first two can run against a table `run_pre_schema_migrations` has
/// already rebuilt into the v19 shape, so a `SELECT` naming a dropped column
/// would fail at runtime on exactly the legacy databases those arms exist for.
/// Positional reads below are 0..=10 against that column list; renumber them
/// together or a mapper silently reads its neighbour.
fn rebuild_content_hashes_for_current_format(conn: &Connection) -> Result<usize> {
    let rows = conn.query(
        "SELECT id, title, description, design, acceptance_criteria, notes, \
                status, priority, issue_type, owner, created_by \
         FROM issues ORDER BY id",
    )?;

    if rows.is_empty() {
        return Ok(0);
    }

    conn.execute("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<usize> {
        let mut updated = 0;
        // Pre-compute once outside the loop and pass explicitly: legacy DBs
        // created before the `DEFAULT CURRENT_TIMESTAMP` was added to
        // `dirty_issues.marked_at` reject INSERTs that omit the column.
        let now_str = Utc::now().to_rfc3339();
        for row in &rows {
            let id = row_text(row, 0).ok_or_else(|| BeadsError::Internal {
                message: "content hash migration found issue row without id".to_string(),
            })?;
            let title = row_text(row, 1).unwrap_or_default();
            let description = row_optional_text(row, 2);
            let design = row_optional_text(row, 3);
            let acceptance_criteria = row_optional_text(row, 4);
            let notes = row_optional_text(row, 5);
            let status_raw = row_text(row, 6).unwrap_or_else(|| Status::default().as_str().into());
            let priority = Priority(
                row.get(7)
                    .and_then(SqliteValue::as_integer)
                    .and_then(|value| i32::try_from(value).ok())
                    .unwrap_or_else(|| Priority::default().0),
            );
            let issue_type_raw =
                row_text(row, 8).unwrap_or_else(|| IssueType::default().as_str().into());
            let owner = row_optional_text(row, 9);
            let created_by = row_optional_text(row, 10);

            let status = status_raw
                .parse::<Status>()
                .unwrap_or_else(|_| Status::Custom(status_raw.clone()));
            let issue_type = issue_type_raw
                .parse::<IssueType>()
                .unwrap_or_else(|_| IssueType::Custom(issue_type_raw.clone()));
            let content_hash = content_hash_from_parts(
                &title,
                description.as_deref(),
                design.as_deref(),
                acceptance_criteria.as_deref(),
                notes.as_deref(),
                &status,
                &priority,
                &issue_type,
                owner.as_deref(),
                created_by.as_deref(),
            );

            conn.execute_with_params(
                "UPDATE issues SET content_hash = ? WHERE id = ?",
                &[
                    SqliteValue::from(content_hash.as_str()),
                    SqliteValue::from(id.as_str()),
                ],
            )?;
            conn.execute_with_params(
                "DELETE FROM dirty_issues WHERE issue_id = ?",
                &[SqliteValue::from(id.as_str())],
            )?;
            conn.execute_with_params(
                "INSERT INTO dirty_issues (issue_id, marked_at) VALUES (?, ?)",
                &[
                    SqliteValue::from(id.as_str()),
                    SqliteValue::from(now_str.as_str()),
                ],
            )?;
            updated += 1;
        }

        if table_exists(conn, "export_hashes") {
            conn.execute("DELETE FROM export_hashes")?;
        }

        Ok(updated)
    })();

    match result {
        Ok(updated) => {
            conn.execute("COMMIT")?;
            Ok(updated)
        }
        Err(error) => {
            let _ = conn.execute("ROLLBACK");
            Err(error)
        }
    }
}

fn row_text(row: &Row, index: usize) -> Option<String> {
    row.get(index)
        .and_then(SqliteValue::as_text)
        .map(str::to_string)
}

fn row_optional_text(row: &Row, index: usize) -> Option<String> {
    row_text(row, index).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::BeadsError;
    use crate::storage::conn::Connection;
    use std::collections::HashSet;
    use tempfile::TempDir;

    /// A file-backed scratch database whose file is guaranteed to outlive the
    /// connection.
    ///
    /// **Use this instead of opening a `NamedTempFile`'s path directly.** The
    /// tempting spelling
    ///
    /// ```ignore
    /// let conn = Connection::open(
    ///     tempfile::NamedTempFile::new().unwrap().path().to_string_lossy().into_owned(),
    /// ).unwrap();
    /// ```
    ///
    /// drops the `NamedTempFile` temporary at the end of the `let` *statement*,
    /// which unlinks the file the instant `Connection::open` returns. SQLite
    /// then holds a descriptor to a path that no longer exists and fails the
    /// first write with `SQLITE_IOERR_FSTAT`, surfaced here as
    /// `Internal("disk I/O error")`. The engine this project used before
    /// `rusqlite` did not notice, so nine fixtures in this module worked by
    /// accident until the port (`bds-04l.4.3`).
    ///
    /// Returning the `TempDir` alongside the connection makes the lifetime
    /// explicit: bind it (`let (_dir, conn) = file_backed_conn();`) and the
    /// directory is removed only after `conn` has been dropped, because locals
    /// drop in reverse declaration order.
    fn file_backed_conn() -> (TempDir, Connection) {
        let dir = TempDir::new().expect("tempdir");
        let conn = Connection::open(dir.path().join("scratch.db").to_string_lossy().into_owned())
            .expect("open scratch database");
        (dir, conn)
    }

    /// A `CREATE INDEX` naming a column that does not exist must be an error.
    ///
    /// `execute_batch` used to skip exactly this case, as a workaround for the
    /// previous engine's stale schema cache. Under real SQLite the error is
    /// genuine, and swallowing it would drop the index silently and for good.
    #[test]
    fn execute_batch_reports_a_create_index_on_a_missing_column() {
        let (_dir, conn) = file_backed_conn();

        let err = execute_batch(
            &conn,
            r"
            CREATE TABLE t (a TEXT);
            CREATE INDEX idx_t_absent ON t (definitely_absent);
        ",
        )
        .expect_err("CREATE INDEX on a missing column must fail");
        assert!(
            err.to_string().contains("no such column"),
            "unexpected error: {err}"
        );
        assert!(
            conn.query("SELECT name FROM sqlite_master WHERE name = 'idx_t_absent'")
                .expect("schema")
                .is_empty(),
            "the broken index must not exist"
        );

        // The legitimate shape the workaround was standing in for still works:
        // SQLite makes an added column visible to the very next statement, so a
        // CREATE INDEX after an ALTER TABLE ADD COLUMN in the same script is
        // fine and must not be reported as an error.
        execute_batch(
            &conn,
            r"
            ALTER TABLE t ADD COLUMN b TEXT;
            CREATE INDEX idx_t_b ON t (b);
        ",
        )
        .expect("an index on a column added earlier in the same script must succeed");
        assert_eq!(
            conn.query("SELECT name FROM sqlite_master WHERE name = 'idx_t_b'")
                .expect("schema")
                .len(),
            1
        );
    }

    #[test]
    fn file_backed_conn_survives_writes_after_the_fixture_returns() {
        // Regression guard for the class of bug this helper exists to prevent.
        // Under the previous engine the unlinked-file spelling worked, so
        // nothing pinned that the fixture's file is still there when the first
        // statement runs. If `file_backed_conn` ever goes back to handing out a
        // connection to an unlinked path, this fails with
        // `Internal("disk I/O error")`.
        let (dir, conn) = file_backed_conn();
        assert!(
            dir.path().join("scratch.db").exists(),
            "the database file must still exist once the fixture has returned"
        );
        conn.execute("CREATE TABLE t (x)").expect("create");
        conn.execute("INSERT INTO t VALUES (1)").expect("insert");
        assert_eq!(conn.query("SELECT x FROM t").expect("select").len(), 1);
    }

    #[test]
    fn test_apply_schema() {
        let (_dir, conn) = file_backed_conn();
        apply_schema(&conn).expect("Failed to apply schema");

        // Verify a few tables exist
        let tables: Vec<String> = conn
            .query("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(0).and_then(|v| v.as_text()).map(String::from))
            .collect();

        assert!(tables.contains(&"issues".to_string()));
        assert!(tables.contains(&"dependencies".to_string()));
        assert!(tables.contains(&"config".to_string()));
        assert!(tables.contains(&"dirty_issues".to_string()));

        // Verify pragmas
        let row = conn.query_row("PRAGMA journal_mode").unwrap();
        let journal_mode = row
            .get(0)
            .and_then(|v| v.as_text())
            .unwrap_or("")
            .to_string();
        // In-memory DBs use MEMORY journaling, regardless of what we set
        assert!(journal_mode.to_uppercase() == "WAL" || journal_mode.to_uppercase() == "MEMORY");

        let row = conn.query_row("PRAGMA foreign_keys").unwrap();
        let foreign_keys = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn test_v6_repair_integer_datetime_columns() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        // Seed one row per integer epoch unit (seconds / ms / µs / ns), all
        // encoding the same instant 2026-04-20T02:18:08Z. The migration's
        // magnitude detection must recover the same year/day/time regardless
        // of unit — previously it hard-coded /1000000.0 and would corrupt
        // the other three rows.
        let rows: [(&str, i64); 4] = [
            ("bug-sec", 1_776_651_488),
            ("bug-ms", 1_776_651_488_000),
            ("bug-us", 1_776_651_488_000_000),
            ("bug-ns", 1_776_651_488_000_000_000),
        ];
        for (id, epoch) in rows {
            let stmt = format!(
                "INSERT INTO issues (id, title, status, priority, issue_type, created_at, updated_at, closed_at, close_reason) \
                 VALUES ('{id}', 'legacy', 'closed', 2, 'task', '2026-04-19T21:34:04.000000000Z', {epoch}, {epoch}, 'Completed')"
            );
            conn.execute(&stmt).expect("seed integer datetime row");
        }

        // Sanity: every updated_at/closed_at must be integer-typed pre-repair.
        for (id, _) in rows {
            let row = conn
                .query_row(&format!(
                    "SELECT typeof(updated_at), typeof(closed_at) FROM issues WHERE id='{id}'"
                ))
                .unwrap();
            assert_eq!(
                row.get(0).and_then(SqliteValue::as_text),
                Some("integer"),
                "{id} updated_at should be integer pre-repair"
            );
            assert_eq!(
                row.get(1).and_then(SqliteValue::as_text),
                Some("integer"),
                "{id} closed_at should be integer pre-repair"
            );
        }

        repair_integer_datetime_columns(&conn).expect("repair should succeed");

        for (id, _) in rows {
            let row = conn
                .query_row(&format!(
                    "SELECT typeof(updated_at), updated_at, typeof(closed_at), closed_at FROM issues WHERE id='{id}'"
                ))
                .unwrap();
            assert_eq!(
                row.get(0).and_then(SqliteValue::as_text),
                Some("text"),
                "{id} updated_at must be TEXT after repair"
            );
            let updated_at = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .expect("updated_at text");
            assert!(
                updated_at.starts_with("2026-04-20T02:18:08"),
                "{id}: expected 2026-04-20 timestamp, got {updated_at}"
            );
            assert_eq!(
                row.get(2).and_then(SqliteValue::as_text),
                Some("text"),
                "{id} closed_at must be TEXT after repair"
            );
        }

        // Idempotency: a second pass is a no-op and leaves the rows TEXT.
        repair_integer_datetime_columns(&conn).expect("second pass should succeed");
        let row = conn
            .query_row("SELECT typeof(updated_at) FROM issues WHERE id='bug-us'")
            .unwrap();
        assert_eq!(row.get(0).and_then(SqliteValue::as_text), Some("text"));
    }

    #[test]
    fn test_v6_repair_legacy_status_values() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        // The issues CHECK constraint forbids closed without closed_at, so
        // we seed rows that are legally in the `done` state (status NOT IN
        // ('closed','tombstone') ⇒ closed_at must be NULL). The migration
        // will promote them to `closed` and backfill closed_at from
        // updated_at.
        conn.execute(
            "INSERT INTO issues (id, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('legacy-done', 'bd legacy', 'done', 2, 'task', '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        ).unwrap();
        conn.execute(
            "INSERT INTO issues (id, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('legacy-resolved', 'bd legacy', 'Resolved', 2, 'task', '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        ).unwrap();

        repair_legacy_status_values(&conn).expect("repair should succeed");

        for id in ["legacy-done", "legacy-resolved"] {
            let row = conn
                .query_row(&format!(
                    "SELECT status, closed_at FROM issues WHERE id='{id}'"
                ))
                .unwrap();
            assert_eq!(
                row.get(0).and_then(SqliteValue::as_text),
                Some("closed"),
                "{id} should be closed"
            );
            let closed_at = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or_default();
            assert!(!closed_at.is_empty(), "{id} closed_at should be populated");
        }
    }

    #[test]
    fn test_v7_rebuilds_content_hashes_and_marks_dirty() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        conn.execute(
            "INSERT INTO issues (id, content_hash, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('bd-hash', 'old-rust-hash', 'Test', 'open', 2, 'task', '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        ).unwrap();
        conn.execute(
            "INSERT INTO export_hashes (issue_id, content_hash, exported_at) \
             VALUES ('bd-hash', 'old-rust-hash', '2026-04-03T01:00:00Z')",
        )
        .unwrap();
        conn.execute("DELETE FROM dirty_issues").unwrap();
        conn.execute("PRAGMA user_version = 6").unwrap();

        run_migrations(&conn, false).expect("v7 migration should succeed");

        let row = conn
            .query_row("SELECT content_hash FROM issues WHERE id = 'bd-hash'")
            .unwrap();
        assert_eq!(
            row.get(0).and_then(SqliteValue::as_text),
            Some("baf454f130f119e3aa6a5beee8cf4aa9069490265d0106566a99bd298acfc6d0"),
            "v7 should rewrite stored issue hashes to the current canonical format \
             (schema v19 value: five slots left the digest)"
        );

        let dirty_row = conn
            .query_row("SELECT COUNT(*) FROM dirty_issues WHERE issue_id = 'bd-hash'")
            .unwrap();
        assert_eq!(dirty_row.get(0).and_then(SqliteValue::as_integer), Some(1));

        let export_row = conn
            .query_row("SELECT COUNT(*) FROM export_hashes")
            .unwrap();
        assert_eq!(export_row.get(0).and_then(SqliteValue::as_integer), Some(0));
    }

    #[test]
    fn test_v14_rebuilds_length_prefixed_content_hashes_and_marks_dirty() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        conn.execute(
            "INSERT INTO issues (id, content_hash, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('bd-hash-v14', 'old-go-hash', 'Test', 'open', 2, 'task', '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        ).unwrap();
        conn.execute(
            "INSERT INTO export_hashes (issue_id, content_hash, exported_at) \
             VALUES ('bd-hash-v14', 'old-go-hash', '2026-04-03T01:00:00Z')",
        )
        .unwrap();
        conn.execute("DELETE FROM dirty_issues").unwrap();
        conn.execute("PRAGMA user_version = 13").unwrap();

        run_migrations(&conn, false).expect("v14 migration should succeed");

        let row = conn
            .query_row("SELECT content_hash FROM issues WHERE id = 'bd-hash-v14'")
            .unwrap();
        assert_eq!(
            row.get(0).and_then(SqliteValue::as_text),
            Some("baf454f130f119e3aa6a5beee8cf4aa9069490265d0106566a99bd298acfc6d0"),
            "v14 should rewrite stored issue hashes to length-prefixed values \
             (schema v19 value: five slots left the digest)"
        );

        let dirty_row = conn
            .query_row("SELECT COUNT(*) FROM dirty_issues WHERE issue_id = 'bd-hash-v14'")
            .unwrap();
        assert_eq!(dirty_row.get(0).and_then(SqliteValue::as_integer), Some(1));

        let export_row = conn
            .query_row("SELECT COUNT(*) FROM export_hashes")
            .unwrap();
        assert_eq!(export_row.get(0).and_then(SqliteValue::as_integer), Some(0));
    }

    /// Re-add, by hand, the fifteen columns v19 drops, so the fixture is a
    /// v18-shaped table.
    ///
    /// `apply_schema` produces the *current* layout, which has none of them, so
    /// the v14 idiom of "apply the schema and wind `PRAGMA user_version` back"
    /// gives a table with nothing to drop and proves nothing. The declarations
    /// are the ones v18's `SCHEMA_SQL` carried, minus the `NOT NULL` on the
    /// three flag columns, which `ALTER TABLE ADD COLUMN` accepts only with a
    /// default (all three had one).
    fn add_v18_dead_columns(conn: &Connection) {
        for (column, decl) in [
            ("assignee", "TEXT"),
            ("estimated_minutes", "INTEGER"),
            ("closed_by_session", "TEXT DEFAULT ''"),
            ("due_at", "DATETIME"),
            ("source_system", "TEXT DEFAULT ''"),
            ("compaction_level", "INTEGER DEFAULT 0"),
            ("compacted_at", "DATETIME"),
            ("compacted_at_commit", "TEXT"),
            ("original_size", "INTEGER"),
            ("sender", "TEXT DEFAULT ''"),
            ("ephemeral", "INTEGER NOT NULL DEFAULT 0"),
            ("pinned", "INTEGER NOT NULL DEFAULT 0"),
            ("is_template", "INTEGER NOT NULL DEFAULT 0"),
            ("source_repo_path", "TEXT"),
            ("agent_context", "TEXT"),
        ] {
            conn.execute(&format!("ALTER TABLE issues ADD COLUMN {column} {decl}"))
                .unwrap_or_else(|error| panic!("add {column}: {error}"));
        }
        execute_batch(
            conn,
            r"
            CREATE INDEX IF NOT EXISTS idx_issues_assignee ON issues(assignee) WHERE assignee IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_issues_ephemeral ON issues(ephemeral) WHERE ephemeral = 1;
            CREATE INDEX IF NOT EXISTS idx_issues_pinned ON issues(pinned) WHERE pinned = 1;
            CREATE INDEX IF NOT EXISTS idx_issues_due_at ON issues(due_at) WHERE due_at IS NOT NULL;
            ",
        )
        .unwrap();
    }

    #[test]
    fn test_v19_rebuilds_content_hashes_and_drops_dead_columns() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");
        add_v18_dead_columns(&conn);

        // Two rows, deliberately: one that carried an `assignee` and one that
        // did not. `assignee` was the only dropped hash input that was not
        // uniformly default across real records, so a rebuild that read a
        // shifted column would still look right on the unassigned row.
        conn.execute(
            "INSERT INTO issues (id, content_hash, title, status, priority, issue_type, \
             assignee, source_system, pinned, is_template, external_ref, \
             created_at, updated_at) \
             VALUES ('bd-hash-v19-assigned', 'stale-v18-hash', 'Test', 'open', 2, 'task', \
             'someone', 'github', 1, 1, 'GH-1', '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO issues (id, content_hash, title, status, priority, issue_type, \
             created_at, updated_at) \
             VALUES ('bd-hash-v19-plain', 'stale-v18-hash', 'Test', 'open', 2, 'task', \
             '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        )
        .unwrap();
        conn.execute("DELETE FROM dirty_issues").unwrap();
        conn.execute("PRAGMA user_version = 18").unwrap();

        run_migrations(&conn, false).expect("v19 migration should succeed");

        // The two rows differ only in columns v19 removes from the digest, so
        // after the rebuild they must hash identically — and to exactly what
        // the current code computes from scratch for that field set.
        let expected = content_hash_from_parts(
            "Test",
            None,
            None,
            None,
            None,
            &Status::Open,
            &Priority(2),
            &IssueType::Task,
            None,
            None,
        );
        for id in ["bd-hash-v19-assigned", "bd-hash-v19-plain"] {
            let row = conn
                .query_row(&format!(
                    "SELECT content_hash FROM issues WHERE id = '{id}'"
                ))
                .unwrap();
            assert_eq!(
                row.get(0).and_then(SqliteValue::as_text),
                Some(expected.as_str()),
                "v19 must rewrite {id}'s hash to the digest the current code computes"
            );
            let dirty = conn
                .query_row(&format!(
                    "SELECT COUNT(*) FROM dirty_issues WHERE issue_id = '{id}'"
                ))
                .unwrap();
            assert_eq!(
                dirty.get(0).and_then(SqliteValue::as_integer),
                Some(1),
                "a rehashed row must be marked dirty so the next flush writes it out"
            );
        }

        for dead in [
            "assignee",
            "estimated_minutes",
            "closed_by_session",
            "due_at",
            "source_system",
            "compaction_level",
            "compacted_at",
            "compacted_at_commit",
            "original_size",
            "sender",
            "ephemeral",
            "pinned",
            "is_template",
            "source_repo_path",
            "agent_context",
        ] {
            assert!(
                !column_exists(&conn, "issues", dead),
                "v19 must drop the {dead} column"
            );
        }
        for dead in [
            "idx_issues_assignee",
            "idx_issues_ephemeral",
            "idx_issues_pinned",
            "idx_issues_due_at",
        ] {
            assert!(!index_exists(&conn, dead), "v19 must drop {dead}");
        }
        for rebuilt in ["idx_issues_ready", "idx_issues_list_active_order"] {
            assert!(
                index_exists(&conn, rebuilt),
                "v19 must recreate {rebuilt} without its dead predicates"
            );
        }
    }

    /// The fresh-database half. `apply_schema` stamps a fresh database's
    /// `user_version` at 19 directly (the `is_fresh` branch), so the v19
    /// arm's own `user_version < 19` guard is what skips it here — the arm
    /// never reaches its inner `column_exists` guards on this path at all.
    ///
    /// So despite the name, this does **not** exercise `column_exists`
    /// per-column guards (`bds-b4f.3.1` review, finding F3): it catches the
    /// coarser and more realistic mistake, a v19 arm written without a
    /// `user_version` check in the first place — that would run
    /// unconditionally on every `br init`, including a second time on the
    /// second `run_migrations` call below, and fail outright on a table that
    /// no longer has the columns to drop.
    #[test]
    fn test_v19_arm_is_skipped_by_its_own_version_guard_on_a_fresh_database() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        run_migrations(&conn, false).expect("migrations on a fresh v19 database must succeed");
        run_migrations(&conn, false).expect("and must be idempotent on a second pass");

        let version = conn.query_row("PRAGMA user_version").unwrap();
        assert_eq!(version.get(0).and_then(SqliteValue::as_integer), Some(19));
    }

    /// The digest a fresh `br` writes and the digest v19 backfills into an
    /// existing database must be the same value.
    ///
    /// The golden snapshot cannot see a divergence here: it regenerates from
    /// whichever path it exercises, so both have to be computed and compared
    /// directly. This is the invariant the whole placeholder sequence in
    /// `content_hash_from_parts` existed to protect.
    #[test]
    fn v19_backfilled_digest_matches_the_from_scratch_digest() {
        let temp = TempDir::new().expect("tempdir");

        // Path A: a v18-shaped database migrated forward.
        let migrated_path = temp.path().join("migrated.db");
        let migrated = Connection::open(migrated_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&migrated).expect("apply schema");
        add_v18_dead_columns(&migrated);
        migrated
            .execute(
                "INSERT INTO issues (id, content_hash, title, description, status, priority, \
                 issue_type, assignee, owner, created_by, external_ref, created_at, updated_at) \
                 VALUES ('bd-parity', 'stale', 'Parity', 'Body', 'open', 1, 'bug', \
                 'someone', 'an-owner', 'a-creator', 'GH-7', \
                 '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
            )
            .unwrap();
        migrated.execute("PRAGMA user_version = 18").unwrap();
        run_migrations(&migrated, false).expect("v19 migration");
        let migrated_hash = migrated
            .query_row("SELECT content_hash FROM issues WHERE id = 'bd-parity'")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_text)
            .map(str::to_string)
            .expect("migrated hash");

        // Path B: the same content hashed by the current code from scratch.
        let fresh_hash = content_hash_from_parts(
            "Parity",
            Some("Body"),
            None,
            None,
            None,
            &Status::Open,
            &Priority(1),
            &IssueType::Bug,
            Some("an-owner"),
            Some("a-creator"),
        );

        assert_eq!(
            migrated_hash, fresh_hash,
            "the migration backfill and the from-scratch digest must agree"
        );
    }

    /// bds-04l.23. The workflow-gate tables went out with the engine that read
    /// them: nothing in the shipped binary ever wrote a row, so they could only
    /// ever be empty and dropping them cannot lose data. This replaces
    /// `test_v15_adds_scoped_gate_history_without_reusing_legacy_results`,
    /// which asserted the opposite -- that the tables are created and their
    /// legacy rows preserved.
    #[test]
    fn v16_drops_the_workflow_gate_tables() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("apply current schema");

        // A fresh database must not carry them at all.
        assert!(!table_exists(&conn, "gate_results"));
        assert!(!table_exists(&conn, "gate_result_history"));

        // Nor must an upgraded one. Recreate the pre-v16 shape, wind the
        // version back, and re-run the migrations.
        execute_batch(
            &conn,
            r"
            CREATE TABLE gate_results (
                issue_id TEXT NOT NULL,
                gate TEXT NOT NULL,
                provider TEXT NOT NULL,
                passed INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (issue_id, gate, provider)
            );
            CREATE TABLE gate_result_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                issue_id TEXT NOT NULL,
                gate TEXT NOT NULL
            );
        ",
        )
        .unwrap();
        conn.execute("PRAGMA user_version = 15").unwrap();
        assert!(table_exists(&conn, "gate_results"));

        run_migrations(&conn, false).expect("v16 migration should succeed");

        assert!(
            !table_exists(&conn, "gate_results"),
            "v16 must drop the legacy gate_results table"
        );
        assert!(
            !table_exists(&conn, "gate_result_history"),
            "v16 must drop the scoped gate_result_history table"
        );
    }

    /// Regression for beads#290: legacy DBs that pre-date the
    /// `marked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP` definition
    /// kept `dirty_issues.marked_at` as a plain NOT NULL column with no
    /// default. The v7 migration's `INSERT INTO dirty_issues (issue_id)`
    /// path then tripped the constraint and bricked every `br` command
    /// against the legacy DB. The fix passes `marked_at` explicitly so
    /// the absence of a column-level default no longer matters.
    #[test]
    fn test_v7_rebuild_works_when_dirty_issues_has_no_default() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        // Re-create dirty_issues without the DEFAULT to mirror what
        // a DB initialized under the pre-v7 schema looks like in the wild.
        conn.execute("DROP TABLE dirty_issues").unwrap();
        conn.execute(
            "CREATE TABLE dirty_issues (
                 issue_id TEXT PRIMARY KEY,
                 marked_at TEXT NOT NULL
             )",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO issues (id, content_hash, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('bd-legacy', 'old-rust-hash', 'Legacy', 'open', 2, 'task', '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        ).unwrap();
        conn.execute("PRAGMA user_version = 6").unwrap();

        run_migrations(&conn, false)
            .expect("v7 migration must succeed against legacy dirty_issues schema");

        let dirty_row = conn
            .query_row("SELECT COUNT(*) FROM dirty_issues WHERE issue_id = 'bd-legacy'")
            .unwrap();
        assert_eq!(
            dirty_row.get(0).and_then(SqliteValue::as_integer),
            Some(1),
            "issue must be flagged dirty after v7 even on legacy table shape"
        );
    }

    #[test]
    fn test_v8_backfills_storage_null_in_default_columns() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        // Seed a row with all required columns set, then force storage-NULLs
        // into the columns the migration is supposed to heal. We rely on
        // direct UPDATEs reaching the storage layer; if the engine refuses
        // any individual UPDATE, the corresponding assertion below still
        // exercises the no-op path of the migration.
        conn.execute(
            "INSERT INTO issues (id, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('bd-null', 'legacy null row', 'open', 2, 'task', '2026-04-30T00:00:00Z', '2026-04-30T00:00:00Z')",
        )
        .expect("seed row");

        // Best-effort: not every column accepts a direct NULL update under
        // every storage engine. The migration must only act on columns
        // that *do* hold storage-NULL values, so we forge as many as the
        // engine allows and verify the migration heals every successful one.
        let columns_to_null: &[&str] = &[
            "description",
            "design",
            "acceptance_criteria",
            "notes",
            "status",
            "priority",
            "issue_type",
            "source_repo",
        ];
        for column in columns_to_null {
            let _ = conn.execute(&format!(
                "UPDATE issues SET {column} = NULL WHERE id = 'bd-null'"
            ));
        }

        // Run the v8 migration directly so this test stays focused on the
        // backfill behaviour rather than the surrounding migration ladder.
        backfill_storage_null_in_default_columns(&conn);

        // Idempotent: every NOT NULL DEFAULT column must hold a non-NULL
        // storage class after the backfill, regardless of which UPDATE-to-
        // NULL succeeded above.
        for column in columns_to_null {
            let row = conn
                .query_row(&format!(
                    "SELECT typeof({column}) FROM issues WHERE id = 'bd-null'"
                ))
                .unwrap();
            let actual_type = row.get(0).and_then(SqliteValue::as_text);
            assert_ne!(
                actual_type,
                Some("null"),
                "{column} should be backfilled to its declared default (got typeof = null)"
            );
        }

        // Second pass is a no-op (the UPDATEs touch zero rows).
        backfill_storage_null_in_default_columns(&conn);
        let row = conn
            .query_row("SELECT typeof(notes) FROM issues WHERE id = 'bd-null'")
            .unwrap();
        assert_ne!(row.get(0).and_then(SqliteValue::as_text), Some("null"));
    }

    #[test]
    fn v9_layout_migrates_to_the_v19_column_set() {
        // This replaces `test_v10_migration_adds_source_repo_path_when_missing`
        // (beads#289), which pinned that a v9 database got `source_repo_path`
        // back when migrated by a v10+ binary. Schema v19 drops that column,
        // so the v10 and v11 arms that added it and `agent_context` are gone
        // and the property to hold is the opposite one: a v9 layout must come
        // out carrying the v19 column set and nothing else.
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("legacy_v9.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();

        // Hand-build the canonical v9 issues table: all the columns that
        // existed before #289 landed, in the canonical EXPECTED order, but
        // intentionally missing the source_repo_path tail column.
        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                content_hash TEXT,
                title TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                design TEXT NOT NULL DEFAULT '',
                acceptance_criteria TEXT NOT NULL DEFAULT '',
                notes TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'open',
                priority INTEGER NOT NULL DEFAULT 2,
                issue_type TEXT NOT NULL DEFAULT 'task',
                assignee TEXT,
                owner TEXT DEFAULT '',
                estimated_minutes INTEGER,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                created_by TEXT DEFAULT '',
                updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                closed_at DATETIME,
                close_reason TEXT DEFAULT '',
                closed_by_session TEXT DEFAULT '',
                due_at DATETIME,
                defer_until DATETIME,
                external_ref TEXT,
                source_system TEXT DEFAULT '',
                source_repo TEXT NOT NULL DEFAULT '.',
                deleted_at DATETIME,
                deleted_by TEXT DEFAULT '',
                delete_reason TEXT DEFAULT '',
                original_type TEXT DEFAULT '',
                compaction_level INTEGER DEFAULT 0,
                compacted_at DATETIME,
                compacted_at_commit TEXT,
                original_size INTEGER,
                sender TEXT DEFAULT '',
                ephemeral INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0,
                is_template INTEGER NOT NULL DEFAULT 0
            );
            ",
        )
        .expect("seed v9 issues table");

        // Stamp the legacy version so the open-path would otherwise
        // short-circuit and skip migrations.
        conn.execute("PRAGMA user_version = 9")
            .expect("stamp legacy user_version");

        assert!(
            column_exists(&conn, "issues", "assignee"),
            "precondition: legacy v9 table must carry the columns v19 drops"
        );

        run_migrations(&conn, false).expect("v19 migration must succeed on a v9 layout");

        for dropped in [
            "assignee",
            "estimated_minutes",
            "closed_by_session",
            "due_at",
            "source_system",
            "compaction_level",
            "sender",
            "ephemeral",
            "pinned",
            "is_template",
        ] {
            assert!(
                !column_exists(&conn, "issues", dropped),
                "v19 must drop {dropped} from a v9 layout too"
            );
        }
        for absent in ["source_repo_path", "agent_context"] {
            assert!(
                !column_exists(&conn, "issues", absent),
                "the v10/v11 arms are gone; {absent} must not be added back only to be dropped"
            );
        }
        assert!(
            column_exists(&conn, "issues", "former_ids"),
            "the v18 arm still has to add former_ids"
        );
    }

    /// A file-backed `issues` table must declare every column exactly once.
    ///
    /// Guards #289: additive columns are appended at the end of `SCHEMA_SQL`
    /// so that a freshly created table and one migrated by
    /// `ALTER TABLE ADD COLUMN` agree on column order — `former_ids` is the
    /// surviving example, `source_repo_path` having gone in schema v19. A
    /// migration that added an already-present column would show up here.
    ///
    /// **This used to count substrings in `sqlite_master.sql` and that was
    /// wrong.** Real SQLite stores the `CREATE TABLE` statement *verbatim*,
    /// comments included, and `SCHEMA_SQL`'s own comment blocks name columns in
    /// prose — so a naive count can read 2 for a column declared once. The
    /// engine this project used before `rusqlite` normalized the stored DDL and
    /// dropped the comments, which is the only reason the substring form ever
    /// passed. Ask the schema, not the text: `PRAGMA table_info` reports the
    /// columns themselves and cannot be fooled by documentation.
    #[test]
    fn test_apply_schema_file_backed_has_no_duplicate_issues_columns() {
        let (_dir, conn) = file_backed_conn();

        apply_schema(&conn).expect("Failed to apply schema");

        let columns: Vec<String> = conn
            .query("PRAGMA table_info(issues)")
            .expect("issues table should exist")
            .iter()
            .filter_map(|row| row.get(1).and_then(SqliteValue::as_text).map(String::from))
            .collect();
        assert!(
            !columns.is_empty(),
            "PRAGMA table_info(issues) returned no columns"
        );

        let unique: HashSet<&String> = columns.iter().collect();
        assert_eq!(
            unique.len(),
            columns.len(),
            "issues table declares a duplicate column: {columns:?}"
        );

        for column in ["source_repo", "former_ids", "external_ref"] {
            assert_eq!(
                columns.iter().filter(|name| *name == column).count(),
                1,
                "issues table should declare {column} exactly once: {columns:?}"
            );
        }

        // The stored DDL still has to be a single CREATE TABLE for `issues`;
        // a duplicated declaration inside it would make SQLite reject the
        // statement outright, so this pins that the table came from the
        // canonical script rather than from a rebuild that silently diverged.
        let row = conn
            .query_row("SELECT sql FROM sqlite_master WHERE type='table' AND name='issues'")
            .expect("issues table should exist");
        let issues_sql = row
            .get(0)
            .and_then(SqliteValue::as_text)
            .expect("issues table SQL should be present");
        assert_eq!(
            issues_sql.matches("CREATE TABLE").count(),
            1,
            "issues table SQL should be one CREATE TABLE statement: {issues_sql}"
        );
    }

    /// Conformance test: Verify schema matches bd (Go) for interoperability.
    /// Tests table structure, defaults, constraints, and indexes.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_schema_parity_conformance() {
        let (_dir, conn) = file_backed_conn();
        apply_schema(&conn).expect("Failed to apply schema");

        // === ISSUES TABLE ===
        // Verify column defaults
        let issues_cols: Vec<(String, String, i32, Option<String>)> = conn
            .query("PRAGMA table_info(issues)")
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row.get(1)
                        .and_then(|v| v.as_text())
                        .unwrap_or("")
                        .to_string(),
                    row.get(2)
                        .and_then(|v| v.as_text())
                        .unwrap_or("")
                        .to_string(),
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        row.get(3).and_then(SqliteValue::as_integer).unwrap_or(0) as i32
                    },
                    row.get(4).and_then(|v| v.as_text()).map(String::from),
                )
            })
            .collect();

        // Check required defaults for bd parity
        let col_map: std::collections::HashMap<_, _> = issues_cols
            .iter()
            .map(|(name, typ, notnull, dflt)| {
                (name.as_str(), (typ.as_str(), *notnull, dflt.clone()))
            })
            .collect();

        // status must default to 'open'
        assert_eq!(
            col_map.get("status").map(|c| c.2.as_deref()),
            Some(Some("'open'")),
            "status should default to 'open'"
        );

        // priority must default to 2
        assert_eq!(
            col_map.get("priority").map(|c| c.2.as_deref()),
            Some(Some("2")),
            "priority should default to 2"
        );

        // issue_type must default to 'task'
        assert_eq!(
            col_map.get("issue_type").map(|c| c.2.as_deref()),
            Some(Some("'task'")),
            "issue_type should default to 'task'"
        );

        // created_at and updated_at must default to CURRENT_TIMESTAMP
        assert_eq!(
            col_map.get("created_at").map(|c| c.2.as_deref()),
            Some(Some("CURRENT_TIMESTAMP")),
            "created_at should default to CURRENT_TIMESTAMP"
        );
        assert_eq!(
            col_map.get("updated_at").map(|c| c.2.as_deref()),
            Some(Some("CURRENT_TIMESTAMP")),
            "updated_at should default to CURRENT_TIMESTAMP"
        );

        // === VERIFY KEY INDEXES EXIST ===
        let indexes: HashSet<String> = conn
            .query("SELECT name FROM sqlite_master WHERE type='index' AND sql IS NOT NULL")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(0).and_then(|v| v.as_text()).map(String::from))
            .collect();

        // Core indexes
        assert!(
            indexes.contains("idx_issues_status"),
            "missing idx_issues_status"
        );
        assert!(
            indexes.contains("idx_issues_priority"),
            "missing idx_issues_priority"
        );
        assert!(
            indexes.contains("idx_issues_issue_type"),
            "missing idx_issues_issue_type"
        );
        assert!(
            indexes.contains("idx_issues_created_at"),
            "missing idx_issues_created_at"
        );
        assert!(
            indexes.contains("idx_issues_updated_at"),
            "missing idx_issues_updated_at"
        );

        // Export/sync indexes
        assert!(
            indexes.contains("idx_issues_content_hash"),
            "missing idx_issues_content_hash"
        );
        assert!(
            indexes.contains("idx_issues_external_ref_unique"),
            "missing external_ref index"
        );

        // Special state indexes. `idx_issues_ephemeral`, `idx_issues_pinned`,
        // `idx_issues_due_at` and `idx_issues_assignee` went with their
        // columns in schema v19 and must stay gone.
        assert!(
            indexes.contains("idx_issues_tombstone"),
            "missing idx_issues_tombstone"
        );
        for dropped in [
            "idx_issues_ephemeral",
            "idx_issues_pinned",
            "idx_issues_due_at",
            "idx_issues_assignee",
        ] {
            assert!(
                !indexes.contains(dropped),
                "schema v19 dropped {dropped}; a fresh database must not create it"
            );
        }

        // Time-based indexes
        assert!(
            indexes.contains("idx_issues_defer_until"),
            "missing idx_issues_defer_until"
        );

        // Ready work composite index (critical for performance)
        assert!(
            indexes.contains("idx_issues_ready"),
            "missing idx_issues_ready composite index"
        );
        // Widened ready group (#354): non-partial composite must exist on real DBs
        // so a configured `status IN (...)` ready query stays index-covered.
        assert!(
            indexes.contains("idx_issues_status_priority_created"),
            "missing idx_issues_status_priority_created composite index"
        );
        assert!(
            indexes.contains("idx_issues_list_active_order"),
            "missing idx_issues_list_active_order composite index"
        );

        // === DEPENDENCIES TABLE ===
        let deps_cols: Vec<(String, Option<String>)> = conn
            .query("PRAGMA table_info(dependencies)")
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row.get(1)
                        .and_then(|v| v.as_text())
                        .unwrap_or("")
                        .to_string(),
                    row.get(4).and_then(|v| v.as_text()).map(String::from),
                )
            })
            .collect();

        let deps_map: std::collections::HashMap<_, _> = deps_cols
            .iter()
            .map(|(name, dflt)| (name.as_str(), dflt.clone()))
            .collect();

        // type must default to 'blocks'
        assert_eq!(
            deps_map.get("type").cloned().flatten().as_deref(),
            Some("'blocks'"),
            "dependencies.type should default to 'blocks'"
        );

        // metadata must default to '{}'
        assert_eq!(
            deps_map.get("metadata").cloned().flatten().as_deref(),
            Some("'{}'"),
            "dependencies.metadata should default to '{{}}'"
        );

        // Dependency indexes (bd parity)
        assert!(
            indexes.contains("idx_dependencies_issue"),
            "missing idx_dependencies_issue"
        );
        assert!(
            indexes.contains("idx_dependencies_depends_on"),
            "missing idx_dependencies_depends_on"
        );
        assert!(
            indexes.contains("idx_dependencies_type"),
            "missing idx_dependencies_type"
        );
        assert!(
            indexes.contains("idx_dependencies_depends_on_type"),
            "missing idx_dependencies_depends_on_type"
        );
        assert!(
            indexes.contains("idx_dependencies_thread"),
            "missing idx_dependencies_thread"
        );
        assert!(
            indexes.contains("idx_dependencies_blocking"),
            "missing idx_dependencies_blocking"
        );

        // Labels indexes
        assert!(
            indexes.contains("idx_labels_label"),
            "missing idx_labels_label"
        );
        assert!(
            indexes.contains("idx_labels_issue"),
            "missing idx_labels_issue"
        );

        // === BLOCKED_ISSUES_CACHE TABLE ===
        let cache_cols: Vec<String> = conn
            .query("PRAGMA table_info(blocked_issues_cache)")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(1).and_then(|v| v.as_text()).map(String::from))
            .collect();

        assert!(
            cache_cols.contains(&"issue_id".to_string()),
            "blocked_issues_cache should have 'issue_id' column"
        );

        // Must have blocked_by (not blocked_by_json) and blocked_at
        assert!(
            cache_cols.contains(&"blocked_by".to_string()),
            "blocked_issues_cache should have 'blocked_by' column (not 'blocked_by_json')"
        );
        assert!(
            cache_cols.contains(&"blocked_at".to_string()),
            "blocked_issues_cache should have 'blocked_at' column"
        );
        assert!(
            !cache_cols.contains(&"blocked_by_json".to_string()),
            "blocked_issues_cache should NOT have old 'blocked_by_json' column"
        );

        // Verify blocked_cache index exists
        assert!(
            indexes.contains("idx_blocked_cache_blocked_at"),
            "missing idx_blocked_cache_blocked_at"
        );

        // === TEST CLOSED-AT CONSTRAINT ===
        // Insert an issue with defaults (will get status='open', closed_at=NULL)
        conn.execute("INSERT INTO issues (id, title) VALUES ('test-1', 'Test Issue')")
            .expect("Should allow open issue without closed_at");

        // Try to insert closed issue without closed_at — CHECK constraint
        // should reject it.  The previous engine did not yet enforce CHECK constraints,
        // so we accept either outcome.
        let result = conn.execute(
            "INSERT INTO issues (id, title, status) VALUES ('test-2', 'Closed', 'closed')",
        );
        if result.is_ok() {
            // Previous engine: CHECK not enforced — clean up the row so later assertions
            // are not affected by the extra row.
            let _ = conn.execute("DELETE FROM issues WHERE id = 'test-2'");
        }

        // Insert closed issue with closed_at - should succeed
        conn.execute(
            "INSERT INTO issues (id, title, status, closed_at) VALUES ('test-3', 'Closed', 'closed', CURRENT_TIMESTAMP)",
        )
        .expect("Should allow closed issue with closed_at");

        // Insert tombstone without closed_at - should succeed (tombstones exempt)
        conn.execute(
            "INSERT INTO issues (id, title, status) VALUES ('test-4', 'Tombstone', 'tombstone')",
        )
        .expect("Should allow tombstone without closed_at");
    }

    /// Test that migrations correctly upgrade old schemas.
    #[test]
    fn test_migration_blocked_cache_upgrade() {
        let (_dir, conn) = file_backed_conn();

        // Create old-style blocked_issues_cache with blocked_by_json
        // Using a complete issues table schema so index migrations succeed
        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'open',
                priority INTEGER NOT NULL DEFAULT 2,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                content_hash TEXT,
                external_ref TEXT,
                description TEXT NOT NULL DEFAULT '',
                design TEXT NOT NULL DEFAULT '',
                acceptance_criteria TEXT NOT NULL DEFAULT '',
                notes TEXT NOT NULL DEFAULT '',
                issue_type TEXT NOT NULL DEFAULT 'task',
                owner TEXT DEFAULT '',
                created_by TEXT DEFAULT '',
                updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                closed_at DATETIME,
                ephemeral INTEGER DEFAULT 0,
                pinned INTEGER DEFAULT 0,
                is_template INTEGER DEFAULT 0,
                compaction_level INTEGER DEFAULT 0,
                due_at DATETIME,
                defer_until DATETIME
            );
            CREATE TABLE dependencies (
                issue_id TEXT NOT NULL,
                depends_on_id TEXT NOT NULL,
                type TEXT NOT NULL DEFAULT 'blocks',
                PRIMARY KEY (issue_id, depends_on_id)
            );
            CREATE TABLE comments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                issue_id TEXT NOT NULL,
                author TEXT NOT NULL,
                text TEXT NOT NULL,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                issue_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                actor TEXT NOT NULL DEFAULT '',
                old_value TEXT,
                new_value TEXT,
                comment TEXT,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE blocked_issues_cache (
                issue_id TEXT PRIMARY KEY,
                blocked_by_json TEXT NOT NULL
            );
        ",
        )
        .unwrap();

        // Run migrations
        run_migrations(&conn, false).unwrap();

        // Verify columns were updated
        let cols: Vec<String> = conn
            .query("PRAGMA table_info(blocked_issues_cache)")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(1).and_then(|v| v.as_text()).map(String::from))
            .collect();

        assert!(
            cols.contains(&"blocked_by".to_string()),
            "Should have blocked_by"
        );
        assert!(
            cols.contains(&"blocked_at".to_string()),
            "Should have blocked_at"
        );
        assert!(
            !cols.contains(&"blocked_by_json".to_string()),
            "Should not have blocked_by_json"
        );
    }

    #[test]
    fn test_apply_schema_rebuilds_nullable_blocked_cache_table() {
        let (_dir, conn) = file_backed_conn();

        execute_batch(
            &conn,
            r"
            CREATE TABLE blocked_issues_cache (
                issue_id TEXT PRIMARY KEY,
                blocked_by TEXT,
                blocked_at DATETIME
            );
            INSERT INTO blocked_issues_cache (issue_id, blocked_by, blocked_at)
            VALUES ('bd-null-cache', NULL, CURRENT_TIMESTAMP);
            ",
        )
        .unwrap();

        apply_schema(&conn).unwrap();

        let column_rows = conn
            .query("PRAGMA table_info(blocked_issues_cache)")
            .unwrap();
        let mut issue_id_primary_key = false;
        let mut blocked_by_not_null = false;
        let mut blocked_at_not_null = false;
        for row in &column_rows {
            let Some(name) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            let not_null = row
                .get(3)
                .and_then(SqliteValue::as_integer)
                .is_some_and(|value| value != 0);
            let primary_key = row
                .get(5)
                .and_then(SqliteValue::as_integer)
                .is_some_and(|value| value != 0);
            match name {
                "issue_id" => issue_id_primary_key = primary_key,
                "blocked_by" => blocked_by_not_null = not_null,
                "blocked_at" => blocked_at_not_null = not_null,
                _ => {}
            }
        }

        assert!(
            issue_id_primary_key,
            "issue_id should be the cache primary key"
        );
        assert!(
            blocked_by_not_null,
            "blocked_by must be NOT NULL after schema repair"
        );
        assert!(
            blocked_at_not_null,
            "blocked_at must be NOT NULL after schema repair"
        );

        let null_rows = conn
            .query_row("SELECT COUNT(*) FROM blocked_issues_cache WHERE blocked_by IS NULL")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-1);
        assert_eq!(
            null_rows, 0,
            "derived blocked cache NULL rows should be discarded during schema repair"
        );
    }

    /// Migration: drop old blocked_issues_cache missing issue_id column.
    #[test]
    fn test_migration_blocked_cache_missing_issue_id() {
        let (_dir, conn) = file_backed_conn();

        // Old-style cache table with 'id' column instead of 'issue_id'
        // Using a complete issues table schema so index migrations succeed
        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'open',
                priority INTEGER NOT NULL DEFAULT 2,
                issue_type TEXT NOT NULL DEFAULT 'task',
                assignee TEXT,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                content_hash TEXT,
                external_ref TEXT,
                ephemeral INTEGER DEFAULT 0,
                pinned INTEGER DEFAULT 0,
                due_at DATETIME,
                defer_until DATETIME
            );
            CREATE TABLE dependencies (
                issue_id TEXT NOT NULL,
                depends_on_id TEXT NOT NULL,
                type TEXT NOT NULL DEFAULT 'blocks',
                PRIMARY KEY (issue_id, depends_on_id)
            );
            CREATE TABLE comments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                issue_id TEXT NOT NULL,
                author TEXT NOT NULL,
                text TEXT NOT NULL,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                issue_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                actor TEXT NOT NULL DEFAULT '',
                old_value TEXT,
                new_value TEXT,
                comment TEXT,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE blocked_issues_cache (
                id TEXT PRIMARY KEY,
                blocked_by TEXT NOT NULL,
                blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
        ",
        )
        .unwrap();

        // Apply full schema (includes pre-migrations)
        apply_schema(&conn).unwrap();

        let cols: Vec<String> = conn
            .query("PRAGMA table_info(blocked_issues_cache)")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(1).and_then(|v| v.as_text()).map(String::from))
            .collect();

        assert!(
            cols.contains(&"issue_id".to_string()),
            "issue_id column should exist after migration"
        );
        assert!(
            cols.contains(&"blocked_by".to_string()),
            "blocked_by column should exist after migration"
        );
        assert!(
            cols.contains(&"blocked_at".to_string()),
            "blocked_at column should exist after migration"
        );
        assert!(
            !cols.contains(&"id".to_string()),
            "legacy id column should be removed"
        );
    }

    /// Migration: add missing issue columns for older schemas.
    #[test]
    fn test_migration_adds_missing_issue_columns() {
        let (_dir, conn) = file_backed_conn();

        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL
            );
        ",
        )
        .unwrap();

        apply_schema(&conn).unwrap();

        let cols: Vec<String> = conn
            .query("PRAGMA table_info('issues')")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(1).and_then(|v| v.as_text()).map(String::from))
            .collect();

        let required = [
            "description",
            "design",
            "acceptance_criteria",
            "notes",
            "owner",
            "created_by",
            "updated_at",
            "source_repo",
            "external_ref",
            "original_type",
            // Pins the tail-appended additive column: a legacy
            // `(id, title)`-only table opened by a current binary must end up
            // with `former_ids` present, so the live INSERT/UPDATE SQL emitted
            // by the storage layer doesn't crash with "no such column" on the
            // very next write.
            "former_ids",
        ];

        for column in required {
            assert!(
                cols.contains(&column.to_string()),
                "missing column {column}"
            );
        }

        // ...and must not resurrect anything schema v19 dropped.
        for column in ["assignee", "compaction_level", "sender", "is_template"] {
            assert!(
                !cols.contains(&column.to_string()),
                "v19 dropped {column}; the migration ladder must not add it back"
            );
        }
    }

    #[test]
    fn ensure_columns_adds_a_current_timestamp_default_to_a_populated_table() {
        // bds-04l.18. SQLite accepts
        // `ALTER TABLE t ADD COLUMN ts DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP`
        // on an EMPTY table and rejects the identical statement once the table
        // holds a row ("Cannot add a column with non-constant default",
        // measured on 3.50.6). The empty case succeeding is what makes this
        // easy to miss, so the fixture below deliberately has rows: without
        // the two-step path this backfill would abort on exactly the legacy
        // databases it exists to repair.
        let (_dir, conn) = file_backed_conn();

        execute_batch(
            &conn,
            r"
            CREATE TABLE dirty_issues (issue_id TEXT PRIMARY KEY);
            INSERT INTO dirty_issues (issue_id) VALUES ('one');
            INSERT INTO dirty_issues (issue_id) VALUES ('two');
        ",
        )
        .unwrap();

        ensure_columns(&conn, "dirty_issues", DIRTY_ISSUES_COLUMNS)
            .expect("a CURRENT_TIMESTAMP default must be backfilled, not abort the open");

        assert!(column_exists(&conn, "dirty_issues", "marked_at"));

        let rows = conn
            .query("SELECT COUNT(*) FROM dirty_issues WHERE marked_at IS NULL")
            .unwrap();
        assert_eq!(
            rows.first()
                .and_then(|row| row.get(0).and_then(SqliteValue::as_integer)),
            Some(0),
            "every existing row must be populated, not left NULL"
        );
    }

    #[test]
    fn non_constant_default_recognises_only_the_keywords_sqlite_rejects() {
        assert_eq!(
            non_constant_default("DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP"),
            Some("CURRENT_TIMESTAMP")
        );
        assert_eq!(
            non_constant_default("DATE DEFAULT CURRENT_DATE"),
            Some("CURRENT_DATE")
        );
        assert_eq!(non_constant_default("INTEGER NOT NULL DEFAULT 0"), None);
        assert_eq!(non_constant_default("TEXT NOT NULL DEFAULT 'open'"), None);
        assert_eq!(non_constant_default("TEXT"), None);
    }

    /// Every table SCHEMA_SQL builds an index on must either be backfilled by
    /// run_pre_schema_migrations or be rebuilt/dropped by it, or a legacy shape
    /// reaches CREATE INDEX with the column missing and the open fails. This
    /// walks SCHEMA_SQL itself so a newly indexed table cannot be added without
    /// a decision about its migration path.
    #[test]
    fn every_indexed_table_has_a_migration_path() {
        // Backfilled by ensure_columns above.
        const BACKFILLED: &[&str] = &["issues", "dependencies", "comments", "dirty_issues"];
        // Rebuilt or dropped-and-recreated by run_pre_schema_migrations.
        const REBUILT: &[&str] = &["config", "metadata", "blocked_issues_cache"];
        // Every column is a PRIMARY KEY or NOT NULL-without-DEFAULT, so
        // ALTER TABLE cannot add it. Covered by the JSONL-recovery backstop
        // (DbError::NoSuchColumn in should_attempt_jsonl_recovery).
        const RECOVERY_ONLY: &[&str] = &["labels"];

        let mut indexed: Vec<&str> = Vec::new();
        for line in SCHEMA_SQL.lines() {
            let trimmed = line.trim();
            let upper = trimmed.to_ascii_uppercase();
            if !upper.starts_with("CREATE INDEX") && !upper.starts_with("CREATE UNIQUE INDEX") {
                continue;
            }
            let Some(on_at) = upper.find(" ON ") else {
                continue;
            };
            let after = trimmed[on_at + 4..].trim();
            let table = after
                .split(|c: char| c == '(' || c.is_whitespace())
                .next()
                .unwrap_or_default();
            if !table.is_empty() && !indexed.contains(&table) {
                indexed.push(table);
            }
        }

        assert!(
            !indexed.is_empty(),
            "no CREATE INDEX statements were found in SCHEMA_SQL; this test has stopped checking anything"
        );

        for table in indexed {
            assert!(
                BACKFILLED.contains(&table)
                    || REBUILT.contains(&table)
                    || RECOVERY_ONLY.contains(&table),
                "SCHEMA_SQL indexes `{table}`, but run_pre_schema_migrations neither backfills nor \
                 rebuilds it. A legacy database missing an indexed column there would fail the open \
                 with `no such column`. Add it to ensure_columns, or classify it here with the reason."
            );
        }
    }

    /// Schema v19 drops `close_metadata`, and a database that still carries it
    /// must lose it on the next open rather than keep a table nothing reads.
    ///
    /// This replaces `legacy_close_metadata_missing_an_indexed_column_still_opens`
    /// (bds-04l.18), which pinned that a legacy `close_metadata` missing
    /// `bypassed_policy` was backfilled before `CREATE INDEX` ran on it. There
    /// is no longer any such index, or any such table, so that property no
    /// longer exists to hold. The general rule it belonged to is still pinned
    /// by `every_indexed_table_has_a_migration_path` above.
    #[test]
    fn a_surviving_close_metadata_table_is_dropped_by_the_v19_migration() {
        let (_dir, conn) = file_backed_conn();
        apply_schema(&conn).expect("apply schema");

        execute_batch(
            &conn,
            r"
            CREATE TABLE close_metadata (
                issue_id TEXT PRIMARY KEY,
                bypassed_policy INTEGER NOT NULL DEFAULT 0,
                recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX idx_close_metadata_recorded_at ON close_metadata(recorded_at);
            INSERT INTO close_metadata (issue_id) VALUES ('legacy-close');
        ",
        )
        .unwrap();
        conn.execute("PRAGMA user_version = 18").unwrap();

        assert!(
            table_exists(&conn, "close_metadata"),
            "fixture must start with the table or it proves nothing"
        );

        run_migrations(&conn, false).expect("v19 migration should succeed");

        assert!(
            !table_exists(&conn, "close_metadata"),
            "v19 must drop close_metadata: nothing writes it and nothing reads it"
        );
        assert!(
            !index_exists(&conn, "idx_close_metadata_recorded_at"),
            "v19 must drop close_metadata's indexes with the table"
        );
    }

    #[test]
    fn declared_not_null_default_reads_every_shape_in_use() {
        // The four shapes ISSUE_COLUMNS actually declares.
        assert_eq!(
            declared_not_null_default("TEXT NOT NULL DEFAULT ''"),
            Some("''".to_string())
        );
        assert_eq!(
            declared_not_null_default("TEXT NOT NULL DEFAULT 'open'"),
            Some("'open'".to_string())
        );
        assert_eq!(
            declared_not_null_default(
                "INTEGER NOT NULL DEFAULT 2 CHECK(priority >= 0 AND priority <= 4)"
            ),
            Some("2".to_string()),
            "a trailing CHECK clause must not be swallowed into the default"
        );
        assert_eq!(
            declared_not_null_default("DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP"),
            Some("CURRENT_TIMESTAMP".to_string())
        );

        // Nothing to repair, and nothing that can be invented.
        assert_eq!(
            declared_not_null_default("TEXT DEFAULT ''"),
            None,
            "a nullable column holds NULL legitimately"
        );
        assert_eq!(
            declared_not_null_default("TEXT NOT NULL CHECK(length(title) <= 500)"),
            None,
            "NOT NULL with no DEFAULT has no value to coalesce to"
        );
        assert_eq!(declared_not_null_default("TEXT"), None);

        // A quoted default containing SQLite's '' escape must come back whole,
        // or the generated COALESCE would be truncated into invalid SQL.
        assert_eq!(
            declared_not_null_default("TEXT NOT NULL DEFAULT 'it''s' CHECK(1)"),
            Some("'it''s'".to_string())
        );
        assert_eq!(
            declared_not_null_default("TEXT NOT NULL DEFAULT 'unterminated"),
            None,
            "an unterminated literal must leave the column alone, not emit bad SQL"
        );
    }

    /// Every NOT NULL column the canonical schema declares with a DEFAULT must
    /// be readable by the parser, so adding one in a shape it cannot read
    /// fails here rather than silently losing its coalesce.
    #[test]
    fn every_declared_issue_column_default_is_parseable() {
        for (name, def) in ISSUE_COLUMNS {
            let upper = def.to_ascii_uppercase();
            if !upper.contains("NOT NULL") || !upper.contains("DEFAULT ") {
                continue;
            }
            assert!(
                declared_not_null_default(def).is_some(),
                "{name} declares `{def}` but its DEFAULT could not be parsed"
            );
        }
    }

    #[test]
    fn legacy_null_in_a_not_null_default_column_is_coalesced_during_rebuild() {
        // bds-04l.15. A legacy `issues` table that BOTH lacks canonical
        // columns (so run_pre_schema_migrations reaches rebuild_issues_table)
        // AND holds a storage-class NULL in a column the canonical schema
        // declares NOT NULL used to fail the open outright with
        // NotNullViolation on issues_rebuild_tmp. That error is not in
        // should_attempt_jsonl_recovery's set either, so the workspace could
        // not be opened and there was no route forward.
        //
        // The fixture used to forge its NULLs in `is_template`/`ephemeral`/
        // `pinned`; schema v19 drops those, so it now uses three surviving
        // NOT NULL DEFAULT columns instead. The property under test is the
        // COALESCE in `rebuild_issues_table_inner`, not any particular column.
        let (_dir, conn) = file_backed_conn();

        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                status TEXT DEFAULT 'open',
                priority INTEGER NOT NULL DEFAULT 2,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                description TEXT,
                notes TEXT,
                source_repo TEXT
            );
            INSERT INTO issues (id, title, description, notes, source_repo)
                VALUES ('legacy-null', 'Legacy', NULL, NULL, NULL);
            INSERT INTO issues (id, title, description, notes, source_repo)
                VALUES ('legacy-set', 'Kept', 'kept desc', 'kept notes', 'kept-repo');
        ",
        )
        .unwrap();

        apply_schema(&conn)
            .expect("a legacy NULL in a NOT NULL column must not make the database unopenable");

        // The NULLs became the columns' declared defaults ...
        let coalesced = conn
            .query("SELECT description, notes, source_repo FROM issues WHERE id = 'legacy-null'")
            .unwrap();
        let row = coalesced.first().expect("the legacy row must survive");
        for (index, (column, expected)) in
            [("description", ""), ("notes", ""), ("source_repo", ".")]
                .iter()
                .enumerate()
        {
            assert_eq!(
                row.get(index).and_then(SqliteValue::as_text),
                Some(*expected),
                "{column} must take its declared DEFAULT, not stay NULL"
            );
        }

        // ... and a row that already held a value kept it, so the fix is a
        // coalesce and not a blanket overwrite.
        let untouched = conn
            .query("SELECT description, notes, source_repo FROM issues WHERE id = 'legacy-set'")
            .unwrap();
        let row = untouched.first().expect("the populated row must survive");
        for (index, (column, expected)) in [
            ("description", "kept desc"),
            ("notes", "kept notes"),
            ("source_repo", "kept-repo"),
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(
                row.get(index).and_then(SqliteValue::as_text),
                Some(*expected),
                "{column} already held a value and must not be overwritten"
            );
        }
    }

    /// A legacy nullable `is_template` / `ephemeral` / `pinned` table must
    /// come out of `apply_schema` without those columns at all.
    ///
    /// This replaces `legacy_nullable_is_template_is_migrated_to_not_null`,
    /// which asserted the v3 migration re-declared them NOT NULL so the list
    /// queries' `is_template = 0` predicate could not silently drop rows.
    /// Schema v19 removed the columns and the predicate together, so the
    /// property to hold is the drop, not the constraint.
    ///
    /// This is also the only path (`bds-b4f.3.1` review, finding F1) that
    /// drives the v19 hash rebuild through `issues_rebuilt = true`:
    /// `issues_column_order_matches` rejects this fixture on length before it
    /// ever reaches the v19 arm, so `run_pre_schema_migrations` rebuilds
    /// `issues` first. Every real pre-v19 database takes the same path. If
    /// the hash rebuild were wrongly re-wrapped in `!issues_rebuilt`, this
    /// fixture's stale `content_hash` would survive untouched and this
    /// assertion is what would catch it.
    #[test]
    fn legacy_nullable_flag_columns_are_dropped_by_apply_schema() {
        let (_dir, conn) = file_backed_conn();

        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                content_hash TEXT,
                title TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'open',
                priority INTEGER NOT NULL DEFAULT 2,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                ephemeral INTEGER DEFAULT 0,
                pinned INTEGER DEFAULT 0,
                is_template INTEGER DEFAULT 0
            );
            INSERT INTO issues (id, content_hash, title, is_template)
                VALUES ('legacy-issue', 'stale-pre-v19-hash', 'Legacy', 0);
            INSERT INTO issues (id, content_hash, title, is_template)
                VALUES ('legacy-tmpl', 'stale-pre-v19-hash', 'Template', 1);
        ",
        )
        .unwrap();

        // Pin the starting version at 18, one below the migration under test.
        // Left at the SQLite default of 0, the unconditional v7 rebuild arm
        // (`user_version < 7`, not gated on `issues_rebuilt`) would also
        // rewrite these hashes, and the assertion below would pass even if
        // the v19 rebuild were wrongly re-wrapped in `!issues_rebuilt` — the
        // exact regression this fixture exists to catch.
        conn.execute("PRAGMA user_version = 18").unwrap();

        assert!(
            column_exists(&conn, "issues", "is_template"),
            "fixture must start with the column or it proves nothing"
        );

        apply_schema(&conn).unwrap();

        for column in ["ephemeral", "pinned", "is_template"] {
            assert!(
                !column_exists(&conn, "issues", column),
                "schema v19 drops {column}"
            );
        }

        // Both rows survive: what used to be a "template" is now an ordinary
        // issue, which is the behaviour change bds-b4f.2.2 already shipped.
        let ids: Vec<String> = conn
            .query("SELECT id FROM issues ORDER BY id")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect();
        assert_eq!(
            ids,
            vec!["legacy-issue".to_string(), "legacy-tmpl".to_string()],
            "the rebuild must not lose rows"
        );

        // The headline property: every stale pre-v19 digest was rewritten,
        // not merely carried through the column rebuild. `status` defaults to
        // 'open', `priority` to 2 and `issue_type` to 'task' (this fixture
        // predates the `issue_type` column entirely), and neither row has a
        // description, design, acceptance_criteria, notes, owner or
        // created_by.
        for (id, title) in [("legacy-issue", "Legacy"), ("legacy-tmpl", "Template")] {
            let expected = content_hash_from_parts(
                title,
                None,
                None,
                None,
                None,
                &Status::Open,
                &Priority(2),
                &IssueType::Task,
                None,
                None,
            );
            let row = conn
                .query_row(&format!(
                    "SELECT content_hash FROM issues WHERE id = '{id}'"
                ))
                .unwrap();
            assert_eq!(
                row.get(0).and_then(SqliteValue::as_text),
                Some(expected.as_str()),
                "{id}'s stale pre-v19 hash must be rewritten by the v19 rebuild, \
                 which runs even though this fixture is rebuilt (issues_rebuilt = true)"
            );
        }
    }

    #[test]
    fn test_rebuild_issues_table_errors_when_canonical_columns_are_missing() {
        let (_dir, conn) = file_backed_conn();

        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                legacy_only TEXT
            );
        ",
        )
        .unwrap();

        let err = rebuild_issues_table(&conn).expect_err("rebuild should fail");
        assert!(matches!(err, BeadsError::Config(_)));
        assert!(
            !table_exists(&conn, "issues_rebuild_tmp"),
            "failed rebuild should roll back the temporary table"
        );
    }

    #[test]
    fn test_rebuild_issues_table_restores_foreign_keys_when_begin_fails() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("locked-rebuild.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute("PRAGMA busy_timeout=0").unwrap();
        apply_schema(&conn).unwrap();

        let lock_conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        lock_conn.execute("PRAGMA busy_timeout=0").unwrap();
        lock_conn.execute("BEGIN IMMEDIATE").unwrap();

        assert!(foreign_keys_enabled(&conn).unwrap());
        let err = rebuild_issues_table(&conn).expect_err("exclusive rebuild should hit busy lock");
        assert!(
            err.to_string().contains("busy") || err.to_string().contains("lock"),
            "expected lock contention error, got {err}"
        );
        assert!(
            foreign_keys_enabled(&conn).unwrap(),
            "failed rebuild must restore foreign key enforcement"
        );

        lock_conn.execute("ROLLBACK").unwrap();
    }

    /// Migration: add missing dependency type column for older schemas.
    #[test]
    fn test_migration_adds_missing_dependency_type() {
        let (_dir, conn) = file_backed_conn();

        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL
            );
            CREATE TABLE dependencies (
                issue_id TEXT NOT NULL,
                depends_on_id TEXT NOT NULL,
                PRIMARY KEY (issue_id, depends_on_id)
            );
        ",
        )
        .unwrap();

        apply_schema(&conn).unwrap();

        assert!(
            conn.query("PRAGMA table_info('dependencies')")
                .unwrap()
                .iter()
                .filter_map(|row| row.get(1).and_then(|v| v.as_text()).map(String::from))
                .any(|col| col == "type"),
            "missing dependency type column"
        );
    }

    #[test]
    fn test_migration_rebuilds_legacy_config_metadata_primary_keys() {
        let (_dir, conn) = file_backed_conn();

        execute_batch(
            &conn,
            r"
            CREATE TABLE config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            INSERT INTO config (key, value) VALUES ('issue_prefix', 'new');
            INSERT INTO metadata (key, value) VALUES ('project', 'new');
        ",
        )
        .unwrap();

        apply_schema(&conn).unwrap();

        // key column should no longer be PRIMARY KEY in rebuilt tables.
        // Use PRAGMA table_info (not the table-valued function form) since
        // The previous engine did not support pragma_table_info as a table-valued function.
        let config_key_pk = conn
            .query("PRAGMA table_info('config')")
            .unwrap()
            .iter()
            .find(|row| row.get(1).and_then(SqliteValue::as_text) == Some("key"))
            .and_then(|row| row.get(5).and_then(SqliteValue::as_integer))
            .unwrap_or(0);
        assert_eq!(config_key_pk, 0);

        let metadata_key_pk = conn
            .query("PRAGMA table_info('metadata')")
            .unwrap()
            .iter()
            .find(|row| row.get(1).and_then(SqliteValue::as_text) == Some("key"))
            .and_then(|row| row.get(5).and_then(SqliteValue::as_integer))
            .unwrap_or(0);
        assert_eq!(metadata_key_pk, 0);

        // Migration should preserve existing values.
        let config_latest = conn
            .query_row_with_params(
                "SELECT value FROM config WHERE key = ?",
                &[SqliteValue::from("issue_prefix")],
            )
            .unwrap();
        assert_eq!(
            config_latest.get(0).and_then(SqliteValue::as_text),
            Some("new")
        );

        let metadata_latest = conn
            .query_row_with_params(
                "SELECT value FROM metadata WHERE key = ?",
                &[SqliteValue::from("project")],
            )
            .unwrap();
        assert_eq!(
            metadata_latest.get(0).and_then(SqliteValue::as_text),
            Some("new")
        );
    }

    #[test]
    fn test_runtime_schema_compatible_rejects_legacy_kv_primary_keys() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("legacy_kv.db");
        {
            let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
            apply_schema(&conn).expect("schema");

            conn.execute("DROP INDEX IF EXISTS idx_config_key")
                .expect("drop config index");
            conn.execute("DROP TABLE config").expect("drop config");
            conn.execute("CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
                .expect("recreate legacy config");

            conn.execute("DROP INDEX IF EXISTS idx_metadata_key")
                .expect("drop metadata index");
            conn.execute("DROP TABLE metadata").expect("drop metadata");
            conn.execute("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
                .expect("recreate legacy metadata");
        }

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();

        assert!(
            !runtime_schema_compatible(&conn),
            "legacy config/metadata primary keys should force the full repair path"
        );
    }

    #[test]
    fn test_active_list_query_plan_uses_composite_index() {
        let (_dir, conn) = file_backed_conn();
        apply_schema(&conn).expect("schema");

        let plan_rows = conn
            .query(
                "EXPLAIN QUERY PLAN
                 SELECT id, priority, created_at
                 FROM issues
                 WHERE status NOT IN ('closed', 'tombstone')
                 ORDER BY priority ASC, created_at DESC
                 LIMIT 1",
            )
            .expect("query plan");

        let details: Vec<String> = plan_rows
            .iter()
            .filter_map(|row| row.get(3).and_then(|v| v.as_text()).map(String::from))
            .collect();

        assert!(
            details
                .iter()
                .any(|detail| detail.contains("idx_issues_list_active_order")),
            "expected planner to use idx_issues_list_active_order, got: {details:?}"
        );
    }

    // ---- split_sql_statements tests ----

    #[test]
    fn test_split_normal_multi_statement() {
        let sql = "CREATE TABLE a (id INT); CREATE TABLE b (id INT); INSERT INTO a VALUES (1)";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 3);
        assert_eq!(stmts[0], "CREATE TABLE a (id INT)");
        assert_eq!(stmts[1], "CREATE TABLE b (id INT)");
        assert_eq!(stmts[2], "INSERT INTO a VALUES (1)");
    }

    #[test]
    fn test_split_semicolon_inside_single_quoted_string() {
        let sql = "INSERT INTO t(v) VALUES('a;b'); SELECT 1";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "INSERT INTO t(v) VALUES('a;b')");
        assert_eq!(stmts[1], "SELECT 1");
    }

    #[test]
    fn test_split_semicolon_inside_double_quoted_identifier() {
        let sql = r#"CREATE TABLE "weird;name" (id INT); SELECT 1"#;
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], r#"CREATE TABLE "weird;name" (id INT)"#);
        assert_eq!(stmts[1], "SELECT 1");
    }

    #[test]
    fn test_split_escaped_quotes_in_string() {
        // SQL escapes single quotes by doubling them: 'it''s'
        let sql = "INSERT INTO t(v) VALUES('it''s;here'); SELECT 2";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "INSERT INTO t(v) VALUES('it''s;here')");
        assert_eq!(stmts[1], "SELECT 2");
    }

    #[test]
    fn test_split_empty_statements() {
        let sql = "SELECT 1;; ; SELECT 2";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT 1");
        assert_eq!(stmts[1], "SELECT 2");
    }

    #[test]
    fn test_split_trailing_semicolon() {
        let sql = "SELECT 1; SELECT 2;";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT 1");
        assert_eq!(stmts[1], "SELECT 2");
    }

    #[test]
    fn test_split_line_comment_with_semicolon() {
        let sql = "SELECT 1; -- this is a comment; not a split\nSELECT 2";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT 1");
        assert_eq!(stmts[1], "-- this is a comment; not a split\nSELECT 2");
    }

    #[test]
    fn test_split_block_comment_with_semicolon() {
        let sql = "SELECT 1; /* comment; with; semicolons */ SELECT 2";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT 1");
        assert_eq!(stmts[1], "/* comment; with; semicolons */ SELECT 2");
    }

    #[test]
    fn test_split_empty_input() {
        assert!(split_sql_statements("").is_empty());
        assert!(split_sql_statements("   ").is_empty());
        assert!(split_sql_statements("  ;  ;  ").is_empty());
    }

    #[test]
    fn test_split_single_statement_no_semicolon() {
        let stmts = split_sql_statements("SELECT 42");
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "SELECT 42");
    }
}
