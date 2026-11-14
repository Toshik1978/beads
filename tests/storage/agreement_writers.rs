//! Every writer of the sync-state keys goes through `sync::agreement`.
//!
//! The guard test reads the source tree, so a new writer added anywhere else
//! fails here by `file:line`. The behaviour test proves the routing worked: after
//! each way of reaching agreement the base is trusted and matches the JSONL.

use crate::common;

use beads::model::Issue;
use beads::storage::SqliteStorage;
use beads::sync::agreement::{ExportAgreement, trusted_base};
use beads::sync::{
    ExportConfig, ImportConfig, auto_flush, export_to_jsonl, finalize_export, import_from_jsonl,
    read_issues_from_jsonl,
};
use common::fixtures;
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const AGREEMENT_FILE: &str = "src/sync/agreement.rs";

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn only_the_agreement_module_writes_sync_state() {
    let root = common::repo_root();
    // A metadata write naming a sync-state key. `\s*` spans line breaks, so a
    // wrapped call is still caught. (`KNOWN_METADATA_DEFAULTS` in storage seeds
    // empty values for these keys by name, not through `set_metadata`, and is
    // deliberately outside this rule.)
    let key_writes = Regex::new(concat!(
        r#"set_metadata(_in_tx)?\(\s*(METADATA_(JSONL_CONTENT_HASH|JSONL_MTIME|JSONL_SIZE|"#,
        r#"LAST_IMPORT_TIME|LAST_EXPORT_TIME|BASE_CONTENT_HASH)|"(jsonl_content_hash|"#,
        r#"jsonl_mtime|jsonl_size|last_import_time|last_export_time|base_content_hash)")"#,
    ))
    .unwrap();
    // The base file's name as a string literal. Real writers bind the path
    // first and write through the binding, so the literal itself is the signal:
    // it may appear only in `agreement.rs`, plus the listed non-write uses.
    let base_literal = r#""beads.base.jsonl""#;
    let allowed_base_literals = [("src/config/mod.rs", r#""beads.base.jsonl","#)];

    let test_module = Regex::new(r"\n#\[cfg\(test\)\]\n(#\[[^\n]*\]\n)*mod \w+ \{").unwrap();

    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    let mut offenders = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if relative == AGREEMENT_FILE {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap();
        // Only production code: everything before the unit-test module. An
        // item-level `#[cfg(test)]` earlier in the file does not end it.
        let production = test_module
            .find(&text)
            .map_or(text.as_str(), |found| &text[..found.start()]);
        for found in key_writes.find_iter(production) {
            let line = production[..found.start()].matches('\n').count() + 1;
            offenders.push(format!("{relative}:{line}: sync-state key write"));
        }
        for (start, _) in production.match_indices(base_literal) {
            let line = production[..start].matches('\n').count() + 1;
            let text = production.lines().nth(line - 1).unwrap().trim();
            if allowed_base_literals.contains(&(relative.as_str(), text)) {
                continue;
            }
            offenders.push(format!("{relative}:{line}: base file name literal"));
        }
    }
    assert!(
        offenders.is_empty(),
        "sync state must be written only by {AGREEMENT_FILE}; offenders:\n{}",
        offenders.join("\n")
    );
}

fn workspace() -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    let jsonl = beads_dir.join("issues.jsonl");
    (temp, beads_dir, jsonl)
}

fn assert_base_matches_jsonl(storage: &SqliteStorage, beads_dir: &Path, jsonl: &Path, step: &str) {
    let base = trusted_base(storage, beads_dir)
        .unwrap()
        .unwrap_or_else(|| panic!("{step}: base is not trusted"));
    let expected: HashMap<String, Issue> = read_issues_from_jsonl(jsonl)
        .unwrap()
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect();
    let mut base_ids: Vec<_> = base.keys().collect();
    let mut expected_ids: Vec<_> = expected.keys().collect();
    base_ids.sort();
    expected_ids.sort();
    assert_eq!(base_ids, expected_ids, "{step}: base ids differ from JSONL");
    for (id, issue) in &expected {
        assert_eq!(base[id].title, issue.title, "{step}: title of {id}");
    }
}

fn issue(id: &str, title: &str) -> Issue {
    let mut issue = fixtures::issue(id);
    issue.id = id.to_string();
    issue.title = title.to_string();
    issue
}

#[test]
fn every_agreement_refreshes_the_base() {
    // Import.
    let (_temp, beads_dir, jsonl) = workspace();
    let mut source = common::test_db();
    source
        .create_issue(&issue("bd-1", "one"), "tester")
        .unwrap();
    export_to_jsonl(&source, &jsonl, &ExportConfig::default()).unwrap();
    let mut storage = common::test_db();
    let config = ImportConfig {
        beads_dir: Some(beads_dir.clone()),
        ..ImportConfig::default()
    };
    import_from_jsonl(&mut storage, &jsonl, &config, Some("bd-")).unwrap();
    assert_base_matches_jsonl(&storage, &beads_dir, &jsonl, "import");

    // Export + finalize.
    storage
        .create_issue(&issue("bd-2", "two"), "tester")
        .unwrap();
    let export_config = ExportConfig {
        beads_dir: Some(beads_dir.clone()),
        ..ExportConfig::default()
    };
    let result = export_to_jsonl(&storage, &jsonl, &export_config).unwrap();
    finalize_export(
        &mut storage,
        &result,
        Some(&result.issue_hashes),
        &jsonl,
        &beads_dir,
        ExportAgreement::Full,
    )
    .unwrap();
    assert_base_matches_jsonl(&storage, &beads_dir, &jsonl, "export");

    // Incremental auto-flush of a dirty issue.
    storage
        .create_issue(&issue("bd-3", "three"), "tester")
        .unwrap();
    let flushed = auto_flush(&mut storage, &beads_dir, &jsonl, false).unwrap();
    assert!(flushed.flushed);
    assert_base_matches_jsonl(&storage, &beads_dir, &jsonl, "auto-flush");
}

fn seeded_pair() -> (TempDir, PathBuf, PathBuf, SqliteStorage) {
    let (temp, beads_dir, jsonl) = workspace();
    let mut storage = common::test_db();
    storage
        .create_issue(&issue("bd-1", "one"), "tester")
        .unwrap();
    let result = export_to_jsonl(&storage, &jsonl, &ExportConfig::default()).unwrap();
    finalize_export(
        &mut storage,
        &result,
        Some(&result.issue_hashes),
        &jsonl,
        &beads_dir,
        ExportAgreement::Full,
    )
    .unwrap();
    (temp, beads_dir, jsonl, storage)
}

#[test]
fn a_partial_export_records_the_hash_but_keeps_the_base() {
    let (_temp, beads_dir, jsonl, mut storage) = seeded_pair();
    let base_path = beads_dir.join("beads.base.jsonl");
    let base_before = fs::read(&base_path).unwrap();
    let base_hash_before = storage.get_metadata("base_content_hash").unwrap();

    storage
        .create_issue(&issue("bd-2", "two"), "tester")
        .unwrap();
    let result = export_to_jsonl(&storage, &jsonl, &ExportConfig::default()).unwrap();
    finalize_export(
        &mut storage,
        &result,
        Some(&result.issue_hashes),
        &jsonl,
        &beads_dir,
        ExportAgreement::WithoutBase,
    )
    .unwrap();

    assert_eq!(
        storage.get_metadata("jsonl_content_hash").unwrap(),
        Some(result.content_hash),
        "the DB's own file is recorded"
    );
    assert!(storage.get_metadata("last_export_time").unwrap().is_some());
    assert_eq!(fs::read(&base_path).unwrap(), base_before);
    assert_eq!(
        storage.get_metadata("base_content_hash").unwrap(),
        base_hash_before
    );
    let base = trusted_base(&storage, &beads_dir).unwrap().unwrap();
    assert_eq!(base.len(), 1, "the old base stays the trusted base");
}

#[test]
fn an_import_that_skips_agreement_leaves_the_base_alone() {
    let (_temp, beads_dir, jsonl, _source) = seeded_pair();
    let base_path = beads_dir.join("beads.base.jsonl");
    fs::write(&base_path, "sentinel\n").unwrap();
    let mut memory = common::test_db();
    let config = ImportConfig {
        beads_dir: Some(beads_dir),
        skip_agreement: true,
        ..ImportConfig::default()
    };
    import_from_jsonl(&mut memory, &jsonl, &config, Some("bd-")).unwrap();

    assert_eq!(fs::read_to_string(&base_path).unwrap(), "sentinel\n");
    assert_eq!(memory.get_metadata("jsonl_content_hash").unwrap(), None);
}

fn cli_workspace() -> common::cli::BrWorkspace {
    let workspace = common::cli::BrWorkspace::new();
    let init = common::cli::run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create = common::cli::run_br(&workspace, ["create", "Original title"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let flush = common::cli::run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);
    workspace
}

#[test]
fn a_no_db_command_does_not_write_the_base() {
    let workspace = cli_workspace();
    let base_path = workspace.root.join(".beads").join("beads.base.jsonl");
    fs::write(&base_path, "sentinel\n").unwrap();

    let list = common::cli::run_br(&workspace, ["--no-db", "list"], "no_db_list");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    assert_eq!(fs::read_to_string(&base_path).unwrap(), "sentinel\n");
}

#[test]
fn a_no_op_flush_does_not_bless_an_unimported_jsonl() {
    let workspace = cli_workspace();
    let beads = workspace.root.join(".beads");
    // A workspace whose base is not trusted, then a JSONL edited behind br's
    // back (a pull) that keeps every id.
    fs::remove_file(beads.join("beads.base.jsonl")).unwrap();
    let jsonl = beads.join("issues.jsonl");
    let edited = fs::read_to_string(&jsonl)
        .unwrap()
        .replace("Original title", "Edited elsewhere");
    fs::write(&jsonl, edited).unwrap();

    let flush = common::cli::run_br(&workspace, ["sync", "--flush-only"], "noop_flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let list = common::cli::run_br(&workspace, ["list", "--json"], "list_after_noop_flush");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    assert!(
        list.stdout.contains("Edited elsewhere"),
        "the edit must still be auto-imported: {}",
        list.stdout
    );
}

#[test]
fn a_no_op_flush_restores_a_missing_base_when_the_hash_matches() {
    let workspace = cli_workspace();
    let base = workspace.root.join(".beads").join("beads.base.jsonl");
    fs::remove_file(&base).unwrap();

    let flush = common::cli::run_br(&workspace, ["sync", "--flush-only"], "noop_restore");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let jsonl = workspace.root.join(".beads").join("issues.jsonl");
    assert_eq!(fs::read(&base).unwrap(), fs::read(&jsonl).unwrap());
}

/// A flush over a JSONL that differs from the agreement imports it first
/// (the database has no local changes), so the base it restores is the file
/// the database now agrees with — never a file it has not read.
#[test]
fn a_flush_over_a_changed_jsonl_imports_it_before_restoring_the_base() {
    let workspace = cli_workspace();
    let beads = workspace.root.join(".beads");
    fs::remove_file(beads.join("beads.base.jsonl")).unwrap();
    let jsonl = beads.join("issues.jsonl");
    let edited = fs::read_to_string(&jsonl)
        .unwrap()
        .replace("Original title", "Edited elsewhere");
    fs::write(&jsonl, &edited).unwrap();

    let flush = common::cli::run_br(&workspace, ["sync", "--flush-only"], "flush_imports");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let list = common::cli::run_br(
        &workspace,
        ["--allow-stale", "list", "--json"],
        "list_after_flush",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);
    assert!(list.stdout.contains("Edited elsewhere"), "{}", list.stdout);
    assert_eq!(
        fs::read_to_string(beads.join("beads.base.jsonl")).unwrap(),
        fs::read_to_string(&jsonl).unwrap()
    );
}
