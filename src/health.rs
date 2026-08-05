//! Workspace **file-state** classification — is the database readable, is the
//! JSONL intact, are there stale locks or sidecars lying around.
//!
//! **There is no `br health` command, and this module does not back one.** Its
//! only consumer is `src/cli/commands/sync.rs`, which surfaces these anomalies
//! through `br sync --status` (and `--json`, as the `anomalies` array and the
//! `workspace_health` field).
//!
//! The scope is deliberately narrow: this classifies the *files*, never the
//! *data* in them. Drift inside the issue graph — a dotted ID that disagrees
//! with its `parent-child` dep, a stale projection — is not an anomaly here and
//! must not be added. Those belong to `br info --projections`
//! (`src/cli/commands/info.rs`), which reports on graph integrity. The reasoning
//! behind the split is on [`AnomalyClass`] below.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const ORPHANED_LOCK_FILE_STALE_AFTER: Duration = Duration::from_mins(30);
const CONFLICT_MARKER_PREFIX_LEN: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WorkspaceHealth {
    Healthy,
    Degraded,
    Recoverable,
    Unsafe,
}

impl WorkspaceHealth {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Recoverable => "recoverable",
            Self::Unsafe => "unsafe",
        }
    }
}

impl fmt::Display for WorkspaceHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A condition `br` actually detects and reports.
///
/// This enum used to be the vocabulary of the `doctor` subsystem, which this
/// fork does not have (see `NOTICE.md`). It carried 26 variants; 17 of them
/// were never constructed by anything that ships, and describing conditions
/// nobody looks for made the list read as a checklist `br` runs. These nine
/// are the ones `classify_file_state` and `br sync --status` genuinely
/// produce.
///
/// Most of what went described drift between the SQLite database and
/// `issues.jsonl` -- stale caches, mismatched projections, dirty-flag and
/// child-count drift. That taxonomy is not worth rebuilding here: the
/// database is a derived, gitignored cache of the JSONL, and the remedy for
/// any disagreement is already to rebuild it from the JSONL rather than to
/// classify the disagreement.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AnomalyClass {
    DatabaseMissing,
    DatabaseNotSqlite,
    SidecarMismatch { has_wal: bool, has_shm: bool },
    TruncatedWal,
    JsonlConflictMarkers,
    JournalSidecarPresent,
    OrphanedLockFile,
}

impl AnomalyClass {
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::DatabaseMissing => "database_missing",
            Self::DatabaseNotSqlite => "database_not_sqlite",
            Self::SidecarMismatch { .. } => "sidecar_mismatch",
            Self::TruncatedWal => "truncated_wal",
            Self::JsonlConflictMarkers => "jsonl_conflict_markers",
            Self::JournalSidecarPresent => "journal_sidecar_present",
            Self::OrphanedLockFile => "orphaned_lock_file",
        }
    }

    #[must_use]
    pub fn severity(&self) -> WorkspaceHealth {
        match self {
            Self::DatabaseNotSqlite | Self::DatabaseMissing | Self::TruncatedWal => {
                WorkspaceHealth::Recoverable
            }

            Self::JsonlConflictMarkers => WorkspaceHealth::Unsafe,

            Self::SidecarMismatch { .. } | Self::JournalSidecarPresent | Self::OrphanedLockFile => {
                WorkspaceHealth::Degraded
            }
        }
    }
}

impl fmt::Display for AnomalyClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseMissing => f.write_str("database file missing"),
            Self::DatabaseNotSqlite => f.write_str("database file is not SQLite"),
            Self::SidecarMismatch { has_wal, has_shm } => {
                write!(f, "sidecar mismatch (WAL={has_wal}, SHM={has_shm})")
            }
            Self::TruncatedWal => f.write_str("truncated WAL sidecar (<32 bytes)"),
            Self::JsonlConflictMarkers => f.write_str("JSONL contains merge conflict markers"),
            Self::JournalSidecarPresent => {
                f.write_str("journal sidecar present (incomplete transaction)")
            }
            Self::OrphanedLockFile => f.write_str("orphaned lock file (.beads.lock) present"),
        }
    }
}

/// One anomaly, in the shape `br sync --status --json` publishes.
///
/// This replaced a three-struct arrangement -- a classification, an audit
/// record and an entry -- that carried the same `(code, severity, message)`
/// triple plus two values derivable from it: a `health` that duplicated the
/// payload's own `workspace_health` field, and an `anomaly_count` that
/// duplicated the array length.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Anomaly {
    pub code: String,
    pub severity: String,
    pub message: String,
}

impl From<&AnomalyClass> for Anomaly {
    fn from(anomaly: &AnomalyClass) -> Self {
        Self {
            code: anomaly.code().to_string(),
            severity: anomaly.severity().as_str().to_string(),
            message: anomaly.to_string(),
        }
    }
}

/// The worst severity present, or `Healthy` when there is nothing to report.
#[must_use]
pub fn worst_severity(anomalies: &[AnomalyClass]) -> WorkspaceHealth {
    anomalies
        .iter()
        .map(AnomalyClass::severity)
        .max()
        .unwrap_or(WorkspaceHealth::Healthy)
}

#[must_use]
fn sqlite_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{}", db_path.to_string_lossy(), suffix))
}

#[must_use]
pub fn classify_file_state(db_path: &Path, jsonl_path: &Path) -> Vec<AnomalyClass> {
    let mut anomalies = Vec::new();

    if !db_path.is_file() && jsonl_path.is_file() {
        anomalies.push(AnomalyClass::DatabaseMissing);
    }

    if db_path.is_file()
        && let Ok(mut file) = std::fs::File::open(db_path)
    {
        use std::io::Read;
        let mut header = [0u8; 16];
        if file.read_exact(&mut header).is_err() || &header != b"SQLite format 3\0" {
            anomalies.push(AnomalyClass::DatabaseNotSqlite);
        }
    }

    let wal_path = sqlite_sidecar_path(db_path, "-wal");
    let shm_path = sqlite_sidecar_path(db_path, "-shm");
    let has_wal = wal_path.is_file();
    let has_shm = shm_path.is_file();

    if has_shm && !has_wal {
        anomalies.push(AnomalyClass::SidecarMismatch { has_wal, has_shm });
    }

    // A WAL header shorter than 32 bytes is a partial write — except for the
    // single value 0. A 0-byte WAL is the documented resting state after a
    // successful `PRAGMA wal_checkpoint(TRUNCATE)` with no concurrent readers,
    // which `SqliteStorage::Drop` runs on every mutating br invocation. Treating
    // it as `TruncatedWal` would false-alarm a healthy store into `Recoverable`,
    // so floor the heuristic at `> 0` — the health-path counterpart of the same
    // `> 0` floor the recovery path's `quarantine_truncated_wal_sidecar`
    // already carries (#291). Partial headers in `(0, 32)` still classify.
    if has_wal
        && let Ok(meta) = std::fs::metadata(&wal_path)
        && meta.len() > 0
        && meta.len() < 32
    {
        anomalies.push(AnomalyClass::TruncatedWal);
    }

    if jsonl_path.is_file() && jsonl_has_conflict_markers(jsonl_path) {
        anomalies.push(AnomalyClass::JsonlConflictMarkers);
    }

    let journal_path = sqlite_sidecar_path(db_path, "-journal");
    if journal_path.is_file() {
        anomalies.push(AnomalyClass::JournalSidecarPresent);
    }

    let lock_path = db_path
        .parent()
        .map(|p| p.join(".beads.lock"))
        .unwrap_or_else(|| db_path.with_file_name(".beads.lock"));
    if lock_path.is_file() && is_orphaned_lock_file(&lock_path, SystemTime::now()) {
        anomalies.push(AnomalyClass::OrphanedLockFile);
    }

    anomalies
}

/// Returns true when any line of `path` starts with a git conflict
/// marker (`<<<<<<<`, `=======`, `>>>>>>>`, or `|||||||`). Reads the
/// file as raw bytes so non-UTF-8 content cannot hide markers; any
/// open/read failure conservatively reports `false` (absence of
/// evidence, not evidence of corruption).
#[must_use]
fn jsonl_has_conflict_markers(path: &Path) -> bool {
    use std::io::BufRead as _;

    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut prefix = [0_u8; CONFLICT_MARKER_PREFIX_LEN];
    let mut prefix_len = 0_usize;
    let mut reading_prefix = true;

    loop {
        let buffer = match reader.fill_buf() {
            Ok([]) | Err(_) => return false,
            Ok(buffer) => buffer,
        };

        let mut consumed = 0;
        for &byte in buffer {
            consumed += 1;

            if reading_prefix && byte != b'\n' {
                if let Some(slot) = prefix.get_mut(prefix_len) {
                    *slot = byte;
                    prefix_len += 1;
                }
                if prefix_len == CONFLICT_MARKER_PREFIX_LEN {
                    if is_jsonl_conflict_marker_prefix(prefix) {
                        return true;
                    }
                    reading_prefix = false;
                }
            }

            if byte == b'\n' {
                prefix_len = 0;
                reading_prefix = true;
            }
        }

        reader.consume(consumed);
    }
}

fn is_jsonl_conflict_marker_prefix(prefix: [u8; CONFLICT_MARKER_PREFIX_LEN]) -> bool {
    prefix == *b"<<<<<<<" || prefix == *b">>>>>>>" || prefix == *b"=======" || prefix == *b"|||||||"
}

fn is_orphaned_lock_file(lock_path: &Path, now: SystemTime) -> bool {
    std::fs::metadata(lock_path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .is_some_and(|modified| lock_modified_time_is_stale(modified, now))
}

fn lock_modified_time_is_stale(modified: SystemTime, now: SystemTime) -> bool {
    matches!(
        now.duration_since(modified),
        Ok(age) if age >= ORPHANED_LOCK_FILE_STALE_AFTER
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn setup_workspace() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("beads.db");
        let jsonl_path = dir.path().join("issues.jsonl");
        (dir, db_path, jsonl_path)
    }

    #[test]
    fn healthy_workspace_has_no_anomalies() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(anomalies.is_empty());
        assert_eq!(worst_severity(&anomalies), WorkspaceHealth::Healthy);
    }

    #[test]
    fn missing_db_with_jsonl_is_recoverable() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert_eq!(anomalies.len(), 1);
        assert!(matches!(anomalies[0], AnomalyClass::DatabaseMissing));
        assert_eq!(worst_severity(&anomalies), WorkspaceHealth::Recoverable);
    }

    #[test]
    fn non_sqlite_db_is_recoverable() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        std::fs::write(&db_path, "this is not a sqlite file").unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::DatabaseNotSqlite))
        );
        assert_eq!(worst_severity(&anomalies), WorkspaceHealth::Recoverable);
    }

    #[test]
    fn conflict_markers_in_jsonl_is_unsafe() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(
            &jsonl_path,
            "<<<<<<< HEAD\n{\"id\":\"a\"}\n=======\n{\"id\":\"b\"}\n>>>>>>> branch\n",
        )
        .unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::JsonlConflictMarkers))
        );
        assert_eq!(worst_severity(&anomalies), WorkspaceHealth::Unsafe);
    }

    #[test]
    fn diff3_style_conflict_markers_are_detected() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(
            &jsonl_path,
            "<<<<<<< HEAD\n{\"id\":\"a\"}\n||||||| merged common ancestors\n{\"id\":\"base\"}\n=======\n{\"id\":\"b\"}\n>>>>>>> branch\n",
        )
        .unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::JsonlConflictMarkers))
        );
    }

    #[test]
    fn conflict_markers_are_detected_in_non_utf8_jsonl() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, b"{\"id\":\"a\"}\n\xff\n<<<<<<< HEAD\n").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::JsonlConflictMarkers)),
            "non-UTF-8 bytes must not hide merge conflict markers: {anomalies:?}"
        );
    }

    #[test]
    fn tiny_db_file_below_sqlite_magic_is_flagged_as_not_sqlite() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        // Only 8 bytes — less than the 16-byte SQLite magic header.
        std::fs::write(&db_path, b"short").unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::DatabaseNotSqlite))
        );
    }

    /// A `-wal` with no `-shm` is not treated as a sidecar mismatch.
    ///
    /// The tolerance predates the move to `rusqlite`, where its justification
    /// was that the pure-Rust engine simply left the `-shm` out. `bds-04l.4.3`
    /// re-derived it against real SQLite: it still holds, for two better
    /// reasons, both measured against SQLite 3.53.1.
    ///
    /// 1. **Real SQLite produces this state itself.** Under
    ///    `PRAGMA locking_mode = EXCLUSIVE` a WAL database has no `-shm` at
    ///    all — the shared-memory index exists only to coordinate multiple
    ///    processes. Observed while the connection was open: normal mode gives
    ///    `[db, db-shm, db-wal]`, exclusive mode gives `[db, db-wal]`.
    /// 2. **It is recoverable, not damaged.** A `-wal` snapshotted from a live
    ///    database and reopened without its `-shm` yields every committed row;
    ///    SQLite rebuilds the index and replays the log. That is the
    ///    crash-survivor shape, and flagging it would tell a user the
    ///    workspace is broken at the moment it is quietly repairing itself.
    ///
    /// The converse stays an anomaly for a reason that survives the engine
    /// change: an `-shm` with no `-wal` describes a log that is not there. See
    /// `shm_without_wal_is_degraded_sidecar_mismatch`.
    ///
    /// This test hand-writes the sidecar, which is why the port could not have
    /// falsified it. `wal_without_shm_left_by_the_engine_is_still_healthy`
    /// builds the same state with a real connection, so the classifier is
    /// checked against something SQLite actually wrote.
    #[test]
    fn wal_without_shm_is_not_a_sidecar_mismatch() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let wal_path = db_path.with_extension("db-wal");
        std::fs::write(&wal_path, [0u8; 64]).unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            !anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::SidecarMismatch { .. })),
            "a -wal with no -shm is a state SQLite itself produces under \
             locking_mode = EXCLUSIVE, and is recoverable after a crash, so it \
             must not be reported as a sidecar mismatch: {anomalies:?}"
        );
        assert_eq!(worst_severity(&anomalies), WorkspaceHealth::Healthy);
    }

    /// The same state, built by the engine rather than by hand.
    ///
    /// The hand-written fixture above cannot fail when the engine changes,
    /// which is exactly how its premise went stale unnoticed. This one
    /// snapshots a live WAL database's `-wal` without its `-shm` — the shape a
    /// crashed writer leaves behind — proves the database still reads back
    /// every committed row in that state, and only then asserts the classifier
    /// calls it healthy. If SQLite ever stops recovering from it, the middle
    /// assertion fails first and says which half of the premise broke.
    #[test]
    fn wal_without_shm_left_by_the_engine_is_still_healthy() {
        use crate::storage::conn::{Connection, SqliteValue};

        let (dir, db_path, jsonl_path) = setup_workspace();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();

        let live = dir.path().join("live.db");
        let live_wal = dir.path().join("live.db-wal");
        let conn = Connection::open(live.to_string_lossy().as_ref()).expect("open");
        conn.execute("PRAGMA journal_mode = WAL").expect("wal");
        conn.execute("CREATE TABLE t (x)").expect("create");
        conn.execute("INSERT INTO t VALUES (1)").expect("insert");
        assert!(
            live_wal.is_file(),
            "the fixture needs a live -wal to snapshot"
        );
        // Snapshot while the connection is open: a clean close checkpoints and
        // removes both sidecars, leaving nothing to copy.
        let db_bytes = std::fs::read(&live).expect("read db");
        let wal_bytes = std::fs::read(&live_wal).expect("read wal");
        drop(conn);

        let stage = || {
            std::fs::write(&db_path, &db_bytes).expect("stage db");
            std::fs::write(sqlite_sidecar_path(&db_path, "-wal"), &wal_bytes).expect("stage wal");
            let _ = std::fs::remove_file(sqlite_sidecar_path(&db_path, "-shm"));
        };

        stage();
        assert!(
            !sqlite_sidecar_path(&db_path, "-shm").exists(),
            "the point of the fixture is that no -shm was staged"
        );

        // The state is recoverable, which is why tolerating it is right.
        let reopened = Connection::open(db_path.to_string_lossy().as_ref()).expect("reopen");
        assert_eq!(
            reopened
                .query("SELECT x FROM t")
                .expect("read")
                .first()
                .and_then(|row| row.get(0).and_then(SqliteValue::as_integer)),
            Some(1),
            "a -wal with no -shm must still yield its committed rows"
        );
        reopened.close().expect("close");

        // Re-stage, because opening rebuilt the -shm and the close checkpointed
        // the WAL away; the classifier must see the shape under test.
        stage();
        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            !anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::SidecarMismatch { .. })),
            "an engine-produced -wal without -shm must not be a sidecar mismatch: {anomalies:?}"
        );
        assert_eq!(worst_severity(&anomalies), WorkspaceHealth::Healthy);
    }

    #[test]
    fn shm_without_wal_is_degraded_sidecar_mismatch() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let shm_path = db_path.with_extension("db-shm");
        std::fs::write(&shm_path, [0u8; 64]).unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies.iter().any(|a| {
                matches!(
                    a,
                    AnomalyClass::SidecarMismatch {
                        has_wal: false,
                        has_shm: true
                    }
                )
            }),
            "SHM-without-WAL should be a sidecar mismatch: {anomalies:?}"
        );
        assert_eq!(worst_severity(&anomalies), WorkspaceHealth::Degraded);
    }

    #[test]
    fn custom_db_filename_uses_sqlite_append_style_shm_path() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("issues.sqlite");
        let jsonl_path = dir.path().join("issues.jsonl");
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let shm_path = sqlite_sidecar_path(&db_path, "-shm");
        std::fs::write(&shm_path, [0u8; 64]).unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies.iter().any(|a| {
                matches!(
                    a,
                    AnomalyClass::SidecarMismatch {
                        has_wal: false,
                        has_shm: true
                    }
                )
            }),
            "custom DB filename SHM sidecar should be detected at {shm_path:?}: {anomalies:?}"
        );
    }

    #[test]
    fn custom_db_filename_uses_sqlite_append_style_wal_path() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("issues.sqlite");
        let jsonl_path = dir.path().join("issues.jsonl");
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let wal_path = sqlite_sidecar_path(&db_path, "-wal");
        std::fs::write(&wal_path, b"short wal").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::TruncatedWal)),
            "custom DB filename WAL sidecar should be detected at {wal_path:?}: {anomalies:?}"
        );
    }

    #[test]
    fn zero_byte_wal_is_clean_checkpoint_state_not_truncated() {
        // #358 (health-path counterpart of #291): a 0-byte WAL is the documented
        // resting state after `PRAGMA wal_checkpoint(TRUNCATE)` (run by
        // `SqliteStorage::Drop` on every mutating br invocation), not corruption.
        // Classifying it as `TruncatedWal` would false-alarm a healthy store into
        // `Recoverable`. The `< 32` guard must carry a `> 0` floor, mirroring the
        // recovery path's `quarantine_truncated_wal_sidecar` (#291).
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let wal_path = sqlite_sidecar_path(&db_path, "-wal");
        std::fs::write(&wal_path, b"").unwrap(); // 0 bytes — post-wal_checkpoint(TRUNCATE)

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            !anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::TruncatedWal)),
            "0-byte WAL is the clean post-checkpoint state, not corruption: {anomalies:?}"
        );
        assert_eq!(worst_severity(&anomalies), WorkspaceHealth::Healthy);
    }

    #[test]
    fn one_byte_wal_still_classifies_as_truncated() {
        // The `> 0` floor (#358) must not weaken the partial-write contract: a
        // WAL header with 1..32 bytes is a genuine truncation and still flags.
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let wal_path = sqlite_sidecar_path(&db_path, "-wal");
        std::fs::write(&wal_path, [0u8; 1]).unwrap(); // 1 byte — partial write

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::TruncatedWal)),
            "a 1-byte WAL is a partial write and must still classify as truncated: {anomalies:?}"
        );
    }

    #[test]
    fn classification_uses_worst_anomaly() {
        let anomalies = vec![
            AnomalyClass::SidecarMismatch {
                has_wal: true,
                has_shm: false,
            },
            AnomalyClass::JsonlConflictMarkers,
        ];
        assert_eq!(worst_severity(&anomalies), WorkspaceHealth::Unsafe);
    }

    #[test]
    fn anomaly_serializes_with_stable_code_and_severity() {
        let entry = Anomaly::from(&AnomalyClass::SidecarMismatch {
            has_wal: true,
            has_shm: false,
        });

        assert_eq!(entry.code, "sidecar_mismatch");
        assert_eq!(entry.severity, "degraded");
        assert!(entry.message.contains("WAL=true"));
    }

    #[test]
    fn anomalies_serialize_in_the_shape_sync_status_publishes() {
        let detected = vec![
            AnomalyClass::SidecarMismatch {
                has_wal: true,
                has_shm: false,
            },
            AnomalyClass::OrphanedLockFile,
        ];

        assert_eq!(worst_severity(&detected), WorkspaceHealth::Degraded);
        let published: Vec<Anomaly> = detected.iter().map(Anomaly::from).collect();
        assert_eq!(
            serde_json::to_value(&published).unwrap(),
            serde_json::json!([
                {
                    "code": "sidecar_mismatch",
                    "severity": "degraded",
                    "message": "sidecar mismatch (WAL=true, SHM=false)"
                },
                {
                    "code": "orphaned_lock_file",
                    "severity": "degraded",
                    "message": "orphaned lock file (.beads.lock) present"
                }
            ])
        );
    }

    #[test]
    fn anomaly_severity_ordering_is_correct() {
        assert!(WorkspaceHealth::Healthy < WorkspaceHealth::Degraded);
        assert!(WorkspaceHealth::Degraded < WorkspaceHealth::Recoverable);
        assert!(WorkspaceHealth::Recoverable < WorkspaceHealth::Unsafe);
    }

    #[test]
    fn journal_sidecar_detected() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let journal_path = db_path.with_extension("db-journal");
        std::fs::write(&journal_path, b"journal data").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::JournalSidecarPresent))
        );
        assert_eq!(worst_severity(&anomalies), WorkspaceHealth::Degraded);
    }

    #[test]
    fn recent_lock_file_is_not_orphaned() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let lock_path = db_path.parent().unwrap().join(".beads.lock");
        std::fs::write(&lock_path, "pid:12345").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            !anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::OrphanedLockFile))
        );
        assert_eq!(worst_severity(&anomalies), WorkspaceHealth::Healthy);
    }

    #[test]
    fn stale_lock_modified_time_is_orphaned() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_hours(1);
        let stale_modified = now
            .checked_sub(ORPHANED_LOCK_FILE_STALE_AFTER + Duration::from_secs(1))
            .unwrap();
        let recent_age = ORPHANED_LOCK_FILE_STALE_AFTER.saturating_sub(Duration::from_secs(1));
        let recent_modified = now.checked_sub(recent_age).unwrap();
        let future_modified = now + Duration::from_secs(1);

        assert!(lock_modified_time_is_stale(stale_modified, now));
        assert!(!lock_modified_time_is_stale(recent_modified, now));
        assert!(!lock_modified_time_is_stale(future_modified, now));
    }

    /// Every variant, its code and its severity, in one place.
    ///
    /// Exhaustive on purpose: adding a variant without deciding how bad it is
    /// should not compile, and the codes are what `br sync --status --json`
    /// publishes in `reliability_audit`, so changing one silently is a change
    /// to a machine-readable output.
    #[test]
    fn every_anomaly_has_a_stable_code_and_a_decided_severity() {
        let all = [
            AnomalyClass::DatabaseMissing,
            AnomalyClass::DatabaseNotSqlite,
            AnomalyClass::SidecarMismatch {
                has_wal: true,
                has_shm: false,
            },
            AnomalyClass::TruncatedWal,
            AnomalyClass::JsonlConflictMarkers,
            AnomalyClass::JournalSidecarPresent,
            AnomalyClass::OrphanedLockFile,
        ];

        let observed: Vec<(&str, WorkspaceHealth)> = all
            .iter()
            .map(|anomaly| (anomaly.code(), anomaly.severity()))
            .collect();

        assert_eq!(
            observed,
            vec![
                ("database_missing", WorkspaceHealth::Recoverable),
                ("database_not_sqlite", WorkspaceHealth::Recoverable),
                ("sidecar_mismatch", WorkspaceHealth::Degraded),
                ("truncated_wal", WorkspaceHealth::Recoverable),
                ("jsonl_conflict_markers", WorkspaceHealth::Unsafe),
                ("journal_sidecar_present", WorkspaceHealth::Degraded),
                ("orphaned_lock_file", WorkspaceHealth::Degraded),
            ]
        );
    }
}
