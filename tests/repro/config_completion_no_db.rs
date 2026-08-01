// bds-xtg. Pressing TAB after `br config get` copied the entire SQLite
// database family -- `.db`, `-wal` and `-shm` -- into a temp directory, opened
// the copy and scanned its `config` table, synchronously, in the interactive
// path between the keystroke and the candidates appearing. The result fed
// exactly one consumer: a completer for `br query`, a command this fork does
// not have.
//
// The residue is gone (bds-y6o), so the completion path no longer opens a
// database at all: config keys come from the YAML layers and the environment,
// and the issue-ID completers read `issues.jsonl` line by line.

use crate::common;
use common::cli::{BrWorkspace, run_br, run_br_with_env};

/// The guard that would actually have caught this.
///
/// Every completer lives in `src/cli/mod.rs`, and after this fix none of them
/// touches a database -- so the file mentioning either construct is the
/// regression. A behavioural test cannot see this: `saved_queries_from_db`
/// swallowed every error, so a completion that copied the database and one
/// that did not produced byte-identical candidates. The cost was invisible
/// except under a syscall trace.
///
/// Scoped to this one file on purpose. `src/cli/commands/config.rs` and
/// `info.rs` open snapshots legitimately -- they are commands, not completers.
#[test]
fn the_completion_module_opens_no_database() {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/cli/mod.rs"))
        .expect("read src/cli/mod.rs");

    for construct in [
        "with_database_family_snapshot",
        "Connection::open",
        "SqliteStorage::open",
    ] {
        assert!(
            !source.contains(construct),
            "src/cli/mod.rs holds every shell completer, and completers run on a \
             keystroke -- `{construct}` there puts a database open (bds-xtg: a full \
             copy of the .db/-wal/-shm family) in the interactive path. If a \
             completer genuinely needs stored data, read the JSONL as \
             `build_completion_index` does."
        );
    }
}

/// Config-key completion still works, and works with no database present --
/// the keys come from the config layers alone.
#[test]
fn config_key_completion_returns_keys_without_a_database() {
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    let complete_env = [("COMPLETE", "bash"), ("_CLAP_COMPLETE_INDEX", "3")];
    let with_db = run_br_with_env(
        &workspace,
        ["--", "br", "config", "get", ""],
        complete_env,
        "complete_with_db",
    );
    assert!(
        with_db.status.success(),
        "config-key completion should succeed: {}",
        with_db.stderr
    );
    assert!(
        with_db.stdout.contains("issue_prefix"),
        "expected the config layers' keys, got: {}",
        with_db.stdout
    );

    // Same completion with the database family removed. Identical output is
    // the point: config keys are a property of the YAML layers, so the
    // database was never needed to produce them.
    for entry in std::fs::read_dir(workspace.root.join(".beads")).expect("read .beads") {
        let path = entry.expect("dir entry").path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("beads.db"))
        {
            std::fs::remove_file(&path).expect("remove database file");
        }
    }

    let without_db = run_br_with_env(
        &workspace,
        ["--", "br", "config", "get", ""],
        complete_env,
        "complete_without_db",
    );
    assert_eq!(
        without_db.stdout, with_db.stdout,
        "config-key candidates must not depend on the database"
    );
}
