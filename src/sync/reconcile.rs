//! Reconciling the database with the JSONL file.
//!
//! [`merge_db_with_jsonl`] is the workspace-level three-way merge behind
//! `br sync --merge`: it reads both sides, merges against a base, applies the
//! result to the database, re-exports, and records the new agreement.

use crate::error::{BeadsError, Result};
use crate::model::{Issue, Status};
use crate::storage::SqliteStorage;
use crate::sync::agreement::{ExportAgreement, trusted_base};
use crate::sync::history::HistoryConfig;
use crate::sync::{
    ConflictResolution, ExportConfig, ImportConfig, METADATA_JSONL_CONTENT_HASH, MergeContext,
    MergeReport, compute_staleness_refreshing_witnesses, ensure_no_conflict_markers,
    export_to_jsonl_with_policy, finalize_export, import_from_jsonl, read_issues_from_jsonl,
    three_way_merge, validate_sync_path_with_external,
};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::Path;
use tracing::{debug, info};

/// Three-way merge the database with the JSONL file and make them agree.
///
/// `base` is the common ancestor. A caller with no trusted base passes an
/// empty map, under which every disagreement is a conflict for
/// [`ConflictResolution::Manual`] (the merge never guesses a side).
///
/// On conflicts nothing is written and `BeadsError::SyncConflict` is
/// returned, listing the conflicting issues; how to resolve them depends on
/// the command, so the caller adds the hint. Otherwise deletions, upserts and merge notes are applied, caches
/// are rebuilt, the JSONL is exported (forced) and the agreement is recorded.
///
/// # Errors
///
/// Returns an error on conflicts, on a JSONL containing conflict markers or
/// unparseable lines, or if any database or file operation fails.
#[allow(clippy::implicit_hasher)]
pub fn merge_db_with_jsonl(
    storage: &mut SqliteStorage,
    beads_dir: &Path,
    jsonl_path: &Path,
    base: HashMap<String, Issue>,
    strategy: ConflictResolution,
    export_config: &ExportConfig,
) -> Result<MergeReport> {
    debug!(base_count = base.len(), "Merging with base snapshot");

    // Left: the local database, with deps/labels/comments attached.
    let mut left_issues = storage.get_all_issues_for_export()?;
    let all_deps = storage.get_all_dependency_records()?;
    let all_labels = storage.get_all_labels()?;
    let all_comments = storage.get_all_comments()?;

    for issue in &mut left_issues {
        if let Some(deps) = all_deps.get(&issue.id) {
            issue.dependencies = deps.clone();
        }
        if let Some(labels) = all_labels.get(&issue.id) {
            issue.labels = labels.clone();
        }
        if let Some(comments) = all_comments.get(&issue.id) {
            issue.comments = comments.clone();
        }
    }
    let left: HashMap<String, Issue> = left_issues
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect();
    debug!(left_count = left.len(), "Loaded local state (DB)");

    // Right: the JSONL.
    let mut right = HashMap::new();
    if jsonl_path.exists() {
        // A line-by-line parse of a file with unresolved merge-conflict
        // markers gives a generic "Invalid JSON" error; scan for markers
        // first so the user gets the helpful one.
        ensure_no_conflict_markers(jsonl_path)?;
        for issue in read_issues_from_jsonl(jsonl_path)? {
            right.insert(issue.id.clone(), issue);
        }
    }
    debug!(right_count = right.len(), "Loaded external state (JSONL)");

    let context = MergeContext::new(base, left, right);
    let local_tombstones: HashSet<String> = context
        .left
        .values()
        .filter(|issue| issue.status == Status::Tombstone)
        .map(|issue| issue.id.clone())
        .collect();
    let tombstones = if local_tombstones.is_empty() {
        None
    } else {
        Some(&local_tombstones)
    };

    let report = three_way_merge(&context, strategy, tombstones);
    info!(
        kept = report.kept.len(),
        deleted = report.deleted.len(),
        conflicts = report.conflicts.len(),
        "Merge calculated"
    );

    if report.has_conflicts() {
        let mut message = String::from("Merge conflicts detected:\n");
        for (id, kind) in &report.conflicts {
            let _ = writeln!(message, "  - {id}: {kind:?}");
        }
        return Err(BeadsError::SyncConflict { message });
    }

    // Base snapshots can lag behind historical ID migrations, so a merge may
    // ask to delete an issue that is already absent; that is a no-op.
    let existing_deleted_ids: HashSet<String> = storage
        .get_issues_by_ids(&report.deleted)?
        .into_iter()
        .map(|i| i.id)
        .collect();
    for id in &report.deleted {
        if existing_deleted_ids.contains(id) {
            storage.delete_issue(id, "system", "merge deletion", Some(chrono::Utc::now()))?;
        } else {
            debug!(
                issue_id = %id,
                "Skipping merge deletion for issue already absent from local database"
            );
        }
    }

    for issue in &report.kept {
        storage.upsert_issue_for_import(issue)?;
        storage.sync_labels_for_import(&issue.id, &issue.labels)?;
        storage.sync_dependencies_for_import(&issue.id, &issue.dependencies)?;
        storage.sync_comments_for_import(&issue.id, &issue.comments)?;
    }

    for (id, note) in &report.notes {
        if let Err(e) = storage.add_comment(id, "br-sync", note) {
            tracing::warn!(issue_id = %id, error = %e, "Failed to add merge note to issue");
        } else {
            info!(issue_id = %id, note = %note, "Added merge resolution note");
        }
    }

    storage.rebuild_blocked_cache(true)?;
    // Merge can introduce hierarchical IDs via upsert; refresh counters before
    // the next child-ID allocation trusts them.
    storage.rebuild_child_counters_in_tx()?;

    // Export so the JSONL matches the DB, then record the agreement.
    info!(path = %jsonl_path.display(), "Writing merged issues.jsonl");
    let (export_result, _) = export_to_jsonl_with_policy(storage, jsonl_path, export_config)?;
    finalize_export(
        storage,
        &export_result,
        Some(&export_result.issue_hashes),
        jsonl_path,
        beads_dir,
        ExportAgreement::Full,
    )?;

    Ok(report)
}

/// What [`reconcile_before_command`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// The JSONL is what the database last agreed with (or does not exist).
    Unchanged,
    /// The JSONL changed and the database had no local changes: imported.
    Imported,
    /// Both sides changed and merged cleanly.
    Merged { kept: usize, deleted: usize },
    /// Read-only caller: the sides disagree and the stale database is served.
    ServedStale { reason: String },
}

/// Inputs for [`reconcile_before_command`].
#[derive(Debug, Clone)]
pub struct ReconcileOptions<'a> {
    pub expected_prefix: Option<&'a str>,
    pub allow_external_jsonl: bool,
    /// Never refuse: report [`ReconcileOutcome::ServedStale`] instead.
    pub read_only: bool,
    /// The workspace's tombstone retention, as `br sync --merge` exports with,
    /// so an automatic merge writes the same JSONL an explicit one would.
    pub retention_days: Option<u64>,
    /// The workspace's history-backup settings, for the same reason.
    pub history: HistoryConfig,
}

/// How to resolve a refused reconcile: the one set of hints every refusal
/// carries. `--force-db` / `--force-jsonl` are `--merge` modifiers.
pub(crate) const RESOLVE_HINT: &str = "Run `br sync --merge` to reconcile, or pick a side with \
                            `br sync --merge --force-db` (keep the database) or \
                            `br sync --merge --force-jsonl` (keep the JSONL).";

/// Whether this is what `br init` leaves: a database that has never agreed
/// with any JSONL, beside a JSONL that holds no records.
///
/// The JSONL counts as changed (there is no agreement to match), but it has
/// no content a write could overwrite or a merge could misattribute, so the
/// precondition need not refuse. An agreement on record means an empty file
/// is news (every issue deleted elsewhere), and this is false.
///
/// # Errors
///
/// Returns an error if the metadata cannot be read.
pub(crate) fn never_agreed_over_an_empty_jsonl(
    storage: &SqliteStorage,
    jsonl_path: &Path,
) -> Result<bool> {
    // Anything longer than a few blank lines holds a record.
    const MAX_BLANK_FILE_LEN: u64 = 64;
    if storage.get_metadata(METADATA_JSONL_CONTENT_HASH)?.is_some() {
        return Ok(false);
    }
    let Ok(metadata) = std::fs::metadata(jsonl_path) else {
        return Ok(false);
    };
    if !metadata.is_file() || metadata.len() > MAX_BLANK_FILE_LEN {
        return Ok(false);
    }
    Ok(std::fs::read(jsonl_path).is_ok_and(|bytes| bytes.iter().all(u8::is_ascii_whitespace)))
}

/// Whether the database holds changes the JSONL has not seen: dirty rows, or a
/// pending flush (for example after a hard delete).
///
/// # Errors
///
/// Returns an error if the dirty count or metadata cannot be read.
pub fn has_local_changes(storage: &SqliteStorage) -> Result<bool> {
    Ok(storage.get_dirty_issue_count()? > 0
        || storage.get_metadata("needs_flush")?.as_deref() == Some("true"))
}

/// The precondition for a command: make the database and JSONL agree, or say
/// why they cannot.
///
/// Unchanged JSONL: nothing. Changed JSONL and no local changes: import.
/// Changed JSONL and local changes: three-way merge against the trusted base;
/// a conflict, or no trusted base, is a `SyncConflict` naming `br sync
/// --merge`, except with `read_only`, which serves the stale database.
///
/// # Errors
///
/// Returns an error on a refused reconcile, or if the probe, import or merge
/// fails.
pub fn reconcile_before_command(
    storage: &mut SqliteStorage,
    beads_dir: &Path,
    jsonl_path: &Path,
    opts: &ReconcileOptions,
) -> Result<ReconcileOutcome> {
    if jsonl_path.exists() {
        validate_sync_path_with_external(jsonl_path, beads_dir, opts.allow_external_jsonl)?;
    }
    let staleness = compute_staleness_refreshing_witnesses(storage, jsonl_path)?;
    if !staleness.jsonl_newer {
        return Ok(ReconcileOutcome::Unchanged);
    }

    if !has_local_changes(storage)? {
        let import_config = ImportConfig {
            // The configured prefix is the default for new IDs, not a
            // project-wide invariant; preserve mixed-prefix workspaces.
            skip_prefix_validation: true,
            beads_dir: Some(beads_dir.to_path_buf()),
            allow_external_jsonl: opts.allow_external_jsonl,
            show_progress: false,
            ..Default::default()
        };
        let result = import_from_jsonl(storage, jsonl_path, &import_config, opts.expected_prefix)?;
        debug!(imported_count = result.imported_count, "Reconcile imported");
        return Ok(ReconcileOutcome::Imported);
    }

    let base = match trusted_base(storage, beads_dir)? {
        Some(base) => Some(base),
        // Nothing on the JSONL side to lose or misread: every local row is an
        // addition, which is exactly what an empty base says.
        None if never_agreed_over_an_empty_jsonl(storage, jsonl_path)? => Some(HashMap::new()),
        None => None,
    };
    let refusal = match base {
        None => BeadsError::SyncConflict {
            message: format!(
                "The JSONL changed while the database has local changes, and there is no \
                 trusted base to merge against.\n{RESOLVE_HINT}"
            ),
        },
        Some(base) => {
            let export_config = ExportConfig {
                force: true,
                retention_days: opts.retention_days,
                beads_dir: Some(beads_dir.to_path_buf()),
                allow_external_jsonl: opts.allow_external_jsonl,
                history: opts.history.clone(),
                ..ExportConfig::default()
            };
            match merge_db_with_jsonl(
                storage,
                beads_dir,
                jsonl_path,
                base,
                ConflictResolution::Manual,
                &export_config,
            ) {
                Ok(report) => {
                    return Ok(ReconcileOutcome::Merged {
                        kept: report.kept.len(),
                        deleted: report.deleted.len(),
                    });
                }
                Err(BeadsError::SyncConflict { message }) => BeadsError::SyncConflict {
                    message: format!(
                        "The JSONL and the database both changed.\n{message}{RESOLVE_HINT}"
                    ),
                },
                Err(other) => return Err(other),
            }
        }
    };

    if opts.read_only {
        let BeadsError::SyncConflict { message } = refusal else {
            return Err(refusal);
        };
        return Ok(ReconcileOutcome::ServedStale { reason: message });
    }
    Err(refusal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::agreement::trusted_base;
    use crate::sync::{ExportErrorPolicy, export_to_jsonl};
    use tempfile::TempDir;

    fn issue(id: &str, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            ..Issue::default()
        }
    }

    fn config(beads_dir: &Path) -> ExportConfig {
        ExportConfig {
            force: true,
            error_policy: ExportErrorPolicy::Strict,
            beads_dir: Some(beads_dir.to_path_buf()),
            ..ExportConfig::default()
        }
    }

    fn jsonl_issues(path: &Path) -> HashMap<String, Issue> {
        read_issues_from_jsonl(path)
            .unwrap()
            .into_iter()
            .map(|i| (i.id.clone(), i))
            .collect()
    }

    #[test]
    fn merge_without_a_base_turns_every_disagreement_into_a_conflict() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl = beads_dir.join("issues.jsonl");

        let mut other = SqliteStorage::open_memory().unwrap();
        other
            .create_issue(&issue("bd-1", "from jsonl"), "t")
            .unwrap();
        export_to_jsonl(&other, &jsonl, &ExportConfig::default()).unwrap();

        let mut storage = SqliteStorage::open_memory().unwrap();
        storage
            .create_issue(&issue("bd-1", "from db"), "t")
            .unwrap();

        let err = merge_db_with_jsonl(
            &mut storage,
            &beads_dir,
            &jsonl,
            HashMap::new(),
            ConflictResolution::Manual,
            &config(&beads_dir),
        )
        .unwrap_err();
        let BeadsError::SyncConflict { message } = err else {
            panic!("expected SyncConflict, got {err:?}");
        };
        assert!(message.contains("bd-1"), "{message}");
        assert!(
            !message.contains("--force"),
            "the caller adds the hint for its command: {message}"
        );
        assert_eq!(
            storage.get_issue("bd-1").unwrap().unwrap().title,
            "from db",
            "a conflict writes nothing"
        );
    }

    #[test]
    fn clean_merge_records_agreement() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl = beads_dir.join("issues.jsonl");

        // v1: both issues, in the JSONL (the base) and the DB.
        let mut storage = SqliteStorage::open_memory().unwrap();
        storage.create_issue(&issue("bd-a", "a1"), "t").unwrap();
        storage.create_issue(&issue("bd-b", "b1"), "t").unwrap();
        export_to_jsonl(&storage, &jsonl, &ExportConfig::default()).unwrap();
        let base = jsonl_issues(&jsonl);

        // JSONL v2 edits b; the DB edits a.
        let mut remote = SqliteStorage::open_memory().unwrap();
        remote.create_issue(&issue("bd-a", "a1"), "t").unwrap();
        remote.create_issue(&issue("bd-b", "b2"), "t").unwrap();
        export_to_jsonl(&remote, &jsonl, &ExportConfig::default()).unwrap();
        let mut edited = issue("bd-a", "a2");
        edited.updated_at = chrono::Utc::now() + chrono::Duration::seconds(5);
        storage.upsert_issue_for_import(&edited).unwrap();

        merge_db_with_jsonl(
            &mut storage,
            &beads_dir,
            &jsonl,
            base,
            ConflictResolution::Manual,
            &config(&beads_dir),
        )
        .unwrap();

        assert_eq!(storage.get_issue("bd-a").unwrap().unwrap().title, "a2");
        assert_eq!(storage.get_issue("bd-b").unwrap().unwrap().title, "b2");
        let merged = jsonl_issues(&jsonl);
        assert_eq!(merged["bd-a"].title, "a2");
        assert_eq!(merged["bd-b"].title, "b2");
        let trusted = trusted_base(&storage, &beads_dir)
            .unwrap()
            .expect("trusted");
        assert_eq!(trusted["bd-a"].title, "a2");
        assert_eq!(trusted["bd-b"].title, "b2");
    }

    struct Workspace {
        _temp: TempDir,
        beads_dir: std::path::PathBuf,
        jsonl: std::path::PathBuf,
        storage: SqliteStorage,
    }

    fn write_jsonl(path: &Path, issues: &[(&str, &str)]) {
        let mut other = SqliteStorage::open_memory().unwrap();
        for (id, title) in issues {
            other.create_issue(&issue(id, title), "t").unwrap();
        }
        export_to_jsonl(&other, path, &ExportConfig::default()).unwrap();
    }

    /// A database that has imported `issues` from the JSONL: agreement recorded.
    fn agreed_workspace(issues: &[(&str, &str)]) -> Workspace {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl = beads_dir.join("issues.jsonl");
        write_jsonl(&jsonl, issues);
        let mut storage = SqliteStorage::open_memory().unwrap();
        let cfg = ImportConfig {
            skip_prefix_validation: true,
            beads_dir: Some(beads_dir.clone()),
            ..Default::default()
        };
        import_from_jsonl(&mut storage, &jsonl, &cfg, None).unwrap();
        Workspace {
            _temp: temp,
            beads_dir,
            jsonl,
            storage,
        }
    }

    fn opts(read_only: bool) -> ReconcileOptions<'static> {
        ReconcileOptions {
            expected_prefix: None,
            allow_external_jsonl: false,
            read_only,
            retention_days: None,
            history: HistoryConfig::default(),
        }
    }

    fn reconcile(ws: &mut Workspace, read_only: bool) -> Result<ReconcileOutcome> {
        reconcile_before_command(&mut ws.storage, &ws.beads_dir, &ws.jsonl, &opts(read_only))
    }

    fn retitle(ws: &mut Workspace, id: &str, title: &str) {
        let update = crate::storage::IssueUpdate {
            title: Some(title.to_string()),
            ..Default::default()
        };
        ws.storage.update_issue(id, &update, "t").unwrap();
    }

    fn title(ws: &Workspace, id: &str) -> String {
        ws.storage.get_issue(id).unwrap().unwrap().title
    }

    #[test]
    fn unchanged_jsonl_is_unchanged() {
        let mut ws = agreed_workspace(&[("bd-a", "a1")]);
        assert_eq!(
            reconcile(&mut ws, false).unwrap(),
            ReconcileOutcome::Unchanged
        );
    }

    #[test]
    fn changed_jsonl_without_local_changes_imports() {
        let mut ws = agreed_workspace(&[("bd-a", "a1")]);
        write_jsonl(&ws.jsonl, &[("bd-a", "a2")]);
        assert_eq!(
            reconcile(&mut ws, false).unwrap(),
            ReconcileOutcome::Imported
        );
        assert_eq!(title(&ws, "bd-a"), "a2");
    }

    #[test]
    fn fresh_clone_without_a_base_imports() {
        let mut ws = agreed_workspace(&[("bd-a", "a1")]);
        std::fs::remove_file(ws.beads_dir.join("beads.base.jsonl")).unwrap();
        write_jsonl(&ws.jsonl, &[("bd-a", "a2")]);
        assert_eq!(
            reconcile(&mut ws, false).unwrap(),
            ReconcileOutcome::Imported
        );
        assert_eq!(title(&ws, "bd-a"), "a2");
    }

    #[test]
    fn changed_jsonl_with_local_changes_merges_cleanly() {
        let mut ws = agreed_workspace(&[("bd-a", "a1"), ("bd-b", "b1")]);
        write_jsonl(&ws.jsonl, &[("bd-a", "a1"), ("bd-b", "b2")]);
        retitle(&mut ws, "bd-a", "a2");
        let outcome = reconcile(&mut ws, false).unwrap();
        assert!(
            matches!(outcome, ReconcileOutcome::Merged { .. }),
            "{outcome:?}"
        );
        assert_eq!(title(&ws, "bd-a"), "a2");
        assert_eq!(title(&ws, "bd-b"), "b2");
    }

    #[test]
    fn conflicting_changes_refuse() {
        let mut ws = agreed_workspace(&[("bd-a", "a1")]);
        write_jsonl(&ws.jsonl, &[("bd-a", "from jsonl")]);
        let jsonl_before = std::fs::read_to_string(&ws.jsonl).unwrap();
        retitle(&mut ws, "bd-a", "from db");
        let BeadsError::SyncConflict { message } = reconcile(&mut ws, false).unwrap_err() else {
            panic!("expected SyncConflict");
        };
        assert!(message.contains("bd-a"), "{message}");
        assert_eq!(message.matches("--force-db").count(), 1, "{message}");
        assert_eq!(message.matches("--force-jsonl").count(), 1, "{message}");
        assert!(message.contains("br sync --merge`"), "{message}");
        assert!(
            !message.contains("--force ") && !message.contains("--force`"),
            "never advertises bare --force: {message}"
        );
        assert_eq!(title(&ws, "bd-a"), "from db", "a refusal writes nothing");
        assert_eq!(std::fs::read_to_string(&ws.jsonl).unwrap(), jsonl_before);
    }

    #[test]
    fn automatic_merge_exports_with_the_workspace_history_config() {
        let mut ws = agreed_workspace(&[("bd-a", "a1"), ("bd-b", "b1")]);
        write_jsonl(&ws.jsonl, &[("bd-a", "a1"), ("bd-b", "b2")]);
        retitle(&mut ws, "bd-a", "a2");
        let history_dir = ws.beads_dir.join(".br_history");
        assert!(!history_dir.exists());

        let mut options = opts(false);
        options.history.enabled = false;
        let outcome =
            reconcile_before_command(&mut ws.storage, &ws.beads_dir, &ws.jsonl, &options).unwrap();

        assert!(
            matches!(outcome, ReconcileOutcome::Merged { .. }),
            "{outcome:?}"
        );
        assert!(
            !history_dir.exists(),
            "history disabled in the workspace config: the merge's export takes no backup"
        );
    }

    #[test]
    fn local_changes_without_a_trusted_base_refuse() {
        let mut ws = agreed_workspace(&[("bd-a", "a1"), ("bd-b", "b1")]);
        std::fs::remove_file(ws.beads_dir.join("beads.base.jsonl")).unwrap();
        write_jsonl(&ws.jsonl, &[("bd-a", "a1"), ("bd-b", "b2")]);
        retitle(&mut ws, "bd-a", "a2");
        let BeadsError::SyncConflict { message } = reconcile(&mut ws, false).unwrap_err() else {
            panic!("expected SyncConflict");
        };
        assert!(message.contains("br sync --merge"), "{message}");
    }

    #[test]
    fn local_changes_over_a_freshly_initialised_jsonl_merge() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl = beads_dir.join("issues.jsonl");
        std::fs::write(&jsonl, "").unwrap();
        let mut storage = SqliteStorage::open_memory().unwrap();
        storage.create_issue(&issue("bd-a", "a1"), "t").unwrap();

        let outcome =
            reconcile_before_command(&mut storage, &beads_dir, &jsonl, &opts(false)).unwrap();

        assert!(
            matches!(outcome, ReconcileOutcome::Merged { .. }),
            "{outcome:?}"
        );
        assert_eq!(jsonl_issues(&jsonl)["bd-a"].title, "a1");
    }

    #[test]
    fn an_emptied_jsonl_after_an_agreement_is_not_a_fresh_init() {
        let mut ws = agreed_workspace(&[("bd-a", "a1")]);
        std::fs::remove_file(ws.beads_dir.join("beads.base.jsonl")).unwrap();
        std::fs::write(&ws.jsonl, "").unwrap();
        retitle(&mut ws, "bd-a", "a2");
        assert!(!never_agreed_over_an_empty_jsonl(&ws.storage, &ws.jsonl).unwrap());
        assert!(matches!(
            reconcile(&mut ws, false),
            Err(BeadsError::SyncConflict { .. })
        ));
    }

    #[test]
    fn a_pending_hard_delete_counts_as_a_local_change() {
        let mut ws = agreed_workspace(&[("bd-a", "a1"), ("bd-b", "b1")]);
        ws.storage.purge_issue("bd-a").unwrap();
        assert!(has_local_changes(&ws.storage).unwrap());
        write_jsonl(&ws.jsonl, &[("bd-a", "a1"), ("bd-b", "b2")]);
        let outcome = reconcile(&mut ws, false).unwrap();
        assert!(
            matches!(outcome, ReconcileOutcome::Merged { .. }),
            "{outcome:?}"
        );
        assert!(ws.storage.get_issue("bd-a").unwrap().is_none());
        assert_eq!(title(&ws, "bd-b"), "b2");
    }

    #[test]
    fn read_only_conflict_serves_stale() {
        let mut ws = agreed_workspace(&[("bd-a", "a1")]);
        write_jsonl(&ws.jsonl, &[("bd-a", "from jsonl")]);
        retitle(&mut ws, "bd-a", "from db");
        let outcome = reconcile(&mut ws, true).unwrap();
        let ReconcileOutcome::ServedStale { reason } = outcome else {
            panic!("expected ServedStale");
        };
        assert!(reason.contains("br sync --merge"), "{reason}");
        assert_eq!(title(&ws, "bd-a"), "from db");
    }

    #[test]
    fn missing_jsonl_is_unchanged() {
        let mut ws = agreed_workspace(&[("bd-a", "a1")]);
        std::fs::remove_file(&ws.jsonl).unwrap();
        assert_eq!(
            reconcile(&mut ws, false).unwrap(),
            ReconcileOutcome::Unchanged
        );
    }
}
