//! The single agreement point between the SQLite database and the JSONL file.
//!
//! "Agreement" is the fact that the database and `issues.jsonl` describe the
//! same issues, together with the evidence for it: the JSONL content hash and
//! file witness (mtime, size), plus a base snapshot (`beads.base.jsonl`) whose
//! bytes hash to the same value. This module owns the helpers that read and
//! write that evidence.

use crate::error::{BeadsError, Result};
use crate::model::Issue;
use crate::storage::SqliteStorage;
use crate::sync::{
    METADATA_JSONL_CONTENT_HASH, METADATA_JSONL_MTIME, METADATA_JSONL_SIZE,
    METADATA_LAST_EXPORT_TIME, METADATA_LAST_IMPORT_TIME, TempFileGuard, compute_jsonl_hash,
    create_base_snapshot_temp_file, ensure_no_conflict_markers, load_base_snapshot,
    require_safe_sync_overwrite_path,
};
use chrono::Utc;
use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;

/// Metadata key for the content hash of the base snapshot as last recorded.
pub const METADATA_BASE_CONTENT_HASH: &str = "base_content_hash";

pub(crate) const BASE_SNAPSHOT_FILE: &str = "beads.base.jsonl";

/// Which direction produced the agreement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgreementSource {
    Import,
    Export,
}

/// How an export records agreement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportAgreement {
    /// Content hash, witness, export time and a fresh base snapshot.
    Full,
    /// A partial export: the file is the database's own write, so its hash,
    /// witness and export time are recorded, but the base file and
    /// `base_content_hash` are left alone. A partial file never becomes a merge
    /// base.
    WithoutBase,
    /// Nothing is recorded: the storage is not the workspace database
    /// (`--no-db`).
    Skip,
}

/// Exact observed mtime and size of the JSONL file.
#[derive(Debug, Clone)]
pub(crate) struct JsonlWitness {
    pub(crate) mtime: std::time::SystemTime,
    pub(crate) mtime_witness: String,
    pub(crate) size: u64,
}

/// An agreement whose base file is already on disk, awaiting its metadata
/// commit.
#[derive(Debug, Clone)]
pub struct PreparedAgreement {
    pub content_hash: String,
    witness: JsonlWitness,
    records_base: bool,
}

pub(crate) fn observed_jsonl_witness(jsonl_path: &Path) -> Result<JsonlWitness> {
    let metadata = fs::symlink_metadata(jsonl_path)?;
    let jsonl_mtime = metadata.modified()?;
    Ok(JsonlWitness {
        mtime: jsonl_mtime,
        mtime_witness: chrono::DateTime::<Utc>::from(jsonl_mtime).to_rfc3339(),
        size: metadata.len(),
    })
}

fn record_observed_jsonl_witness_in_tx(
    storage: &SqliteStorage,
    observed: &JsonlWitness,
) -> Result<()> {
    storage.set_metadata_in_tx(METADATA_JSONL_MTIME, &observed.mtime_witness)?;
    storage.set_metadata_in_tx(METADATA_JSONL_SIZE, &observed.size.to_string())
}

/// Copy the exact bytes of the JSONL to `beads_dir/beads.base.jsonl`.
///
/// This is a byte copy (not a parse and re-serialize) so the base matches the
/// on-disk file exactly. The write goes through a validated temp file and a
/// durable rename, and a symlinked base is refused rather than followed.
fn write_base_snapshot(jsonl_path: &Path, beads_dir: &Path) -> Result<()> {
    ensure_no_conflict_markers(jsonl_path)?;
    let bytes = fs::read(jsonl_path)?;
    let snapshot_path = beads_dir.join(BASE_SNAPSHOT_FILE);
    let (temp_path, temp_file) = create_base_snapshot_temp_file(&snapshot_path, beads_dir)?;
    let mut temp_guard = TempFileGuard::new(temp_path.clone());
    let mut writer = BufWriter::new(temp_file);
    writer.write_all(&bytes).map_err(BeadsError::Io)?;
    writer.flush()?;
    writer
        .into_inner()
        .map_err(|e| BeadsError::Io(e.into_error()))?
        .sync_all()?;
    require_safe_sync_overwrite_path(&temp_path, beads_dir, false, "rename base snapshot")?;
    require_safe_sync_overwrite_path(&snapshot_path, beads_dir, false, "overwrite base snapshot")?;
    crate::util::durable_rename(&temp_path, &snapshot_path)?;
    temp_guard.persist();
    Ok(())
}

/// Hash and stat the JSONL, then copy its exact bytes to
/// `beads_dir/beads.base.jsonl`.
///
/// The base is written before any metadata, so a crash before the metadata
/// commit leaves [`trusted_base`] returning `None` rather than a wrong base.
///
/// # Errors
///
/// Returns an error if the JSONL cannot be read, the base cannot be written,
/// or the JSONL changed while it was being copied (base hash != content hash).
pub fn prepare_agreement(jsonl_path: &Path, beads_dir: &Path) -> Result<PreparedAgreement> {
    let content_hash = compute_jsonl_hash(jsonl_path)?;
    let witness = observed_jsonl_witness(jsonl_path)?;
    write_base_snapshot(jsonl_path, beads_dir)?;
    let base_hash = compute_jsonl_hash(&beads_dir.join(BASE_SNAPSHOT_FILE))?;
    if base_hash != content_hash {
        return Err(BeadsError::Config(
            "JSONL changed while recording sync agreement; retry".to_string(),
        ));
    }
    Ok(PreparedAgreement {
        content_hash,
        witness,
        records_base: true,
    })
}

/// Prepare the agreement for an export according to `mode`; `None` for
/// [`ExportAgreement::Skip`].
///
/// # Errors
///
/// As [`prepare_agreement`] for `Full`; for `WithoutBase`, if the JSONL cannot
/// be read or stat'ed.
pub fn prepare_export_agreement(
    jsonl_path: &Path,
    beads_dir: &Path,
    mode: ExportAgreement,
) -> Result<Option<PreparedAgreement>> {
    match mode {
        ExportAgreement::Full => prepare_agreement(jsonl_path, beads_dir).map(Some),
        ExportAgreement::WithoutBase => Ok(Some(PreparedAgreement {
            content_hash: compute_jsonl_hash(jsonl_path)?,
            witness: observed_jsonl_witness(jsonl_path)?,
            records_base: false,
        })),
        ExportAgreement::Skip => Ok(None),
    }
}

/// Write the agreement metadata inside the caller's write transaction.
///
/// # Errors
///
/// Returns an error if any metadata write fails.
pub fn commit_agreement_in_tx(
    storage: &SqliteStorage,
    prepared: &PreparedAgreement,
    source: AgreementSource,
) -> Result<()> {
    storage.set_metadata_in_tx(METADATA_JSONL_CONTENT_HASH, &prepared.content_hash)?;
    if prepared.records_base {
        storage.set_metadata_in_tx(METADATA_BASE_CONTENT_HASH, &prepared.content_hash)?;
    }
    record_observed_jsonl_witness_in_tx(storage, &prepared.witness)?;
    let time_key = match source {
        AgreementSource::Import => METADATA_LAST_IMPORT_TIME,
        AgreementSource::Export => METADATA_LAST_EXPORT_TIME,
    };
    storage.set_metadata_in_tx(time_key, &Utc::now().to_rfc3339())
}

/// Prepare an agreement and commit it in its own transaction.
///
/// # Errors
///
/// Returns an error if preparation or the metadata transaction fails.
pub fn record_agreement(
    storage: &mut SqliteStorage,
    jsonl_path: &Path,
    beads_dir: &Path,
    source: AgreementSource,
) -> Result<()> {
    let prepared = prepare_agreement(jsonl_path, beads_dir)?;
    storage.with_write_transaction(|storage| commit_agreement_in_tx(storage, &prepared, source))
}

/// The base snapshot, but only if it is the one last recorded: the file
/// exists and its hash equals the stored `base_content_hash`.
///
/// # Errors
///
/// Returns an error if the base exists, matches, and then fails to parse, or
/// on metadata read failure.
pub fn trusted_base(
    storage: &SqliteStorage,
    beads_dir: &Path,
) -> Result<Option<HashMap<String, Issue>>> {
    let Some(stored) = storage.get_metadata(METADATA_BASE_CONTENT_HASH)? else {
        return Ok(None);
    };
    let base_path = beads_dir.join(BASE_SNAPSHOT_FILE);
    if !base_path.exists() {
        return Ok(None);
    }
    match compute_jsonl_hash(&base_path) {
        Ok(actual) if actual == stored => load_base_snapshot(beads_dir).map(Some),
        _ => Ok(None),
    }
}

/// Record the full agreement, but only when the JSONL's hash already equals the
/// stored `jsonl_content_hash`. For a caller that has established there are no
/// local changes, this restores a missing or untrusted base without blessing a
/// file the database has not seen. Returns whether it recorded.
///
/// # Errors
///
/// Returns an error if the file cannot be read or the record fails.
pub fn record_agreement_if_hash_matches(
    storage: &mut SqliteStorage,
    jsonl_path: &Path,
    beads_dir: &Path,
    source: AgreementSource,
) -> Result<bool> {
    let Some(stored) = storage.get_metadata(METADATA_JSONL_CONTENT_HASH)? else {
        return Ok(false);
    };
    if compute_jsonl_hash(jsonl_path)? != stored {
        return Ok(false);
    }
    record_agreement(storage, jsonl_path, beads_dir, source)?;
    Ok(true)
}

/// Rewrite the mtime/size witness from a probe's observation, provided the file
/// still has the observed mtime and size. This does not hash the file; the
/// probe's own gate vouches for the content.
///
/// # Errors
///
/// Returns an error if the file cannot be stat'ed or the write fails.
pub(crate) fn refresh_observed_witness(
    storage: &mut SqliteStorage,
    jsonl_path: &Path,
    observed: &JsonlWitness,
) -> Result<()> {
    let current = observed_jsonl_witness(jsonl_path)?;
    if current.mtime != observed.mtime || current.size != observed.size {
        return Ok(());
    }
    storage.with_write_transaction(|storage| record_observed_jsonl_witness_in_tx(storage, &current))
}

/// Rewrite only the mtime/size witness, and only when the file's full hash
/// equals the stored `jsonl_content_hash`. Returns whether it wrote.
///
/// # Errors
///
/// Returns an error if the file cannot be read or the write fails.
pub fn refresh_witness_if_hash_matches(
    storage: &mut SqliteStorage,
    jsonl_path: &Path,
) -> Result<bool> {
    let Some(stored) = storage.get_metadata(METADATA_JSONL_CONTENT_HASH)? else {
        return Ok(false);
    };
    let witness = observed_jsonl_witness(jsonl_path)?;
    if compute_jsonl_hash(jsonl_path)? != stored {
        return Ok(false);
    }
    storage
        .with_write_transaction(|storage| record_observed_jsonl_witness_in_tx(storage, &witness))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (SqliteStorage, TempDir, std::path::PathBuf) {
        let storage = SqliteStorage::open_memory().unwrap();
        let dir = TempDir::new().unwrap();
        let jsonl = dir.path().join("issues.jsonl");
        fs::write(
            &jsonl,
            "{\"id\":\"bd-1\",\"title\":\"one\",\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:00:00Z\"}\n{\"id\":\"bd-2\",\"title\":\"two\",\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:00:00Z\"}\n",
        )
        .unwrap();
        (storage, dir, jsonl)
    }

    #[test]
    fn record_agreement_writes_every_key_and_the_base() {
        let (mut storage, dir, jsonl) = setup();
        record_agreement(&mut storage, &jsonl, dir.path(), AgreementSource::Import).unwrap();
        for key in [
            METADATA_JSONL_CONTENT_HASH,
            METADATA_JSONL_MTIME,
            METADATA_JSONL_SIZE,
            METADATA_BASE_CONTENT_HASH,
            METADATA_LAST_IMPORT_TIME,
        ] {
            assert!(storage.get_metadata(key).unwrap().is_some(), "{key} unset");
        }
        assert_eq!(
            storage.get_metadata(METADATA_LAST_EXPORT_TIME).unwrap(),
            None
        );
        assert_eq!(
            fs::read(dir.path().join(BASE_SNAPSHOT_FILE)).unwrap(),
            fs::read(&jsonl).unwrap()
        );
        assert_eq!(
            storage.get_metadata(METADATA_BASE_CONTENT_HASH).unwrap(),
            storage.get_metadata(METADATA_JSONL_CONTENT_HASH).unwrap()
        );
    }

    #[test]
    fn export_source_sets_last_export_time_only() {
        let (mut storage, dir, jsonl) = setup();
        record_agreement(&mut storage, &jsonl, dir.path(), AgreementSource::Export).unwrap();
        assert!(
            storage
                .get_metadata(METADATA_LAST_EXPORT_TIME)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            storage.get_metadata(METADATA_LAST_IMPORT_TIME).unwrap(),
            None
        );
    }

    #[test]
    fn trusted_base_is_some_after_agreement() {
        let (mut storage, dir, jsonl) = setup();
        record_agreement(&mut storage, &jsonl, dir.path(), AgreementSource::Import).unwrap();
        assert!(trusted_base(&storage, dir.path()).unwrap().is_some());
    }

    #[test]
    fn trusted_base_is_none_when_the_base_file_is_missing() {
        let (mut storage, dir, jsonl) = setup();
        record_agreement(&mut storage, &jsonl, dir.path(), AgreementSource::Import).unwrap();
        fs::remove_file(dir.path().join(BASE_SNAPSHOT_FILE)).unwrap();
        assert!(trusted_base(&storage, dir.path()).unwrap().is_none());
    }

    #[test]
    fn trusted_base_is_none_when_the_base_was_rewritten() {
        let (mut storage, dir, jsonl) = setup();
        record_agreement(&mut storage, &jsonl, dir.path(), AgreementSource::Import).unwrap();
        fs::write(dir.path().join(BASE_SNAPSHOT_FILE), "{\"id\":\"bd-9\"}\n").unwrap();
        assert!(trusted_base(&storage, dir.path()).unwrap().is_none());
    }

    #[test]
    fn trusted_base_is_none_before_any_agreement() {
        let (storage, dir, jsonl) = setup();
        fs::copy(&jsonl, dir.path().join(BASE_SNAPSHOT_FILE)).unwrap();
        assert!(trusted_base(&storage, dir.path()).unwrap().is_none());
    }

    #[test]
    fn witness_refresh_requires_a_matching_hash() {
        let (mut storage, dir, jsonl) = setup();
        record_agreement(&mut storage, &jsonl, dir.path(), AgreementSource::Import).unwrap();
        let before = storage.get_metadata(METADATA_JSONL_MTIME).unwrap();

        // Same bytes, new mtime: witness is refreshed.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
        fs::File::options()
            .write(true)
            .open(&jsonl)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(refresh_witness_if_hash_matches(&mut storage, &jsonl).unwrap());
        let after = storage.get_metadata(METADATA_JSONL_MTIME).unwrap();
        assert_ne!(before, after);

        // One byte changed: nothing is written.
        fs::write(&jsonl, "{\"id\":\"bd-1\"}\n{\"id\":\"bd-3\"}\n").unwrap();
        assert!(!refresh_witness_if_hash_matches(&mut storage, &jsonl).unwrap());
        assert_eq!(storage.get_metadata(METADATA_JSONL_MTIME).unwrap(), after);
    }

    #[test]
    fn base_write_skips_a_stale_regular_temp_file() {
        let (mut storage, dir, jsonl) = setup();
        let base = dir.path().join(BASE_SNAPSHOT_FILE);
        fs::write(&base, "old-snapshot\n").unwrap();
        let stale = crate::sync::export_temp_path_for_attempt(&base, 0);
        fs::write(&stale, "stale temp\n").unwrap();

        record_agreement(&mut storage, &jsonl, dir.path(), AgreementSource::Export).unwrap();

        assert_eq!(fs::read(&base).unwrap(), fs::read(&jsonl).unwrap());
        assert_eq!(fs::read_to_string(&stale).unwrap(), "stale temp\n");
    }

    #[cfg(unix)]
    #[test]
    fn base_write_rejects_an_existing_temp_symlink() {
        let (mut storage, dir, jsonl) = setup();
        let base = dir.path().join(BASE_SNAPSHOT_FILE);
        fs::write(&base, "old-snapshot\n").unwrap();
        let target = dir.path().join("captured.txt");
        fs::write(&target, "do-not-touch").unwrap();
        let pid = std::process::id();
        std::os::unix::fs::symlink(
            &target,
            dir.path().join(format!("{BASE_SNAPSHOT_FILE}.{pid}.tmp")),
        )
        .unwrap();

        let result = record_agreement(&mut storage, &jsonl, dir.path(), AgreementSource::Export);

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&base).unwrap(), "old-snapshot\n");
        assert_eq!(fs::read_to_string(&target).unwrap(), "do-not-touch");
    }
}
