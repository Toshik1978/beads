#![allow(dead_code, unused_imports)]

use beads::storage::SqliteStorage;
use std::path::PathBuf;
use std::sync::{Mutex, Once, OnceLock};
use std::time::Instant;
use tempfile::TempDir;
use tracing::info;

pub mod assertions;
pub mod baseline;
pub mod binary_discovery;
pub mod cli;
pub mod dataset_registry;
pub mod fixtures;
pub mod harness;
pub mod mock_http;
pub mod ordering;
pub mod remote_harness;
pub mod report_indexer;
pub mod scenarios;
pub mod youtrack_fixtures;

pub use baseline::{
    BaselineStore, RegressionConfig, RegressionResult, RegressionStatus, RegressionSummary,
    should_update_baseline, update_baselines_from_results,
};
pub use binary_discovery::{BinaryVersion, DiscoveredBinaries, discover_binaries};
pub use dataset_registry::{
    DatasetIntegrityGuard, DatasetMetadata, DatasetOverride, DatasetProvenance, DatasetRegistry,
    IntegrityCheckResult, IsolatedDataset, IsolatedWorkspaceFailureFixture, KnownDataset,
    WorkspaceFailureCommandExpectation, WorkspaceFailureCommandOutcome, WorkspaceFailureFixture,
    WorkspaceFailureFixtureMetadata, isolated_from_override, isolated_workspace_failure_fixture,
    list_workspace_failure_fixtures, run_with_integrity,
};
pub use harness::{ParallelismMode, ResourceGuardrails, RunnerPolicy};
pub use report_indexer::{
    ArtifactIndexer, CommandMetric, FullReport, IndexerConfig, IndexerError, SnapshotMetric,
    SuiteReport, TestReport, generate_html_report, generate_markdown_report, write_reports,
};
pub use scenarios::{
    CompareMode, ExecutionMode, Invariants, NormalizationRules, Scenario, ScenarioCommand,
    ScenarioFilter, ScenarioResult, ScenarioSetup, TagMatchMode,
};

/// The repository root.
///
/// `env!("CARGO_MANIFEST_DIR")` used to be the repo root, because this code
/// lived in `tests/common/` inside the root package. It now points at
/// `test-support/`, so every caller that means "the repository" goes one level
/// up through here rather than re-deriving it and drifting.
#[must_use]
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("test-support/ always has a parent")
        .to_path_buf()
}

/// Locate a binary cargo built for this workspace, from a library crate.
///
/// `assert_cmd::cargo::cargo_bin!` expands to `env!("CARGO_BIN_EXE_<name>")`,
/// which cargo only defines for integration tests *of the package declaring
/// the binary*. This crate is not that package, so the macro fails to compile
/// here. Test binaries live at `<target>/<profile>/deps/<name>-<hash>`, and
/// the built binary sits one directory up.
#[must_use]
pub fn cargo_built_binary(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let profile_dir = if dir.ends_with("deps") {
        dir.parent()?
    } else {
        dir
    };
    let candidate = profile_dir
        .join(name)
        .with_extension(std::env::consts::EXE_EXTENSION);
    candidate.exists().then_some(candidate)
}

static INIT: Once = Once::new();
static WORKSPACE_REPLAY_TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

pub fn workspace_replay_test_guard() -> std::sync::MutexGuard<'static, ()> {
    WORKSPACE_REPLAY_TEST_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn init_test_logging() {
    INIT.call_once(|| {
        beads::logging::init_test_logging();
    });
}

pub struct TestLogGuard {
    name: String,
    start: Instant,
}

impl TestLogGuard {
    fn new(name: &str) -> Self {
        init_test_logging();
        info!("{name}: starting");
        Self {
            name: name.to_string(),
            start: Instant::now(),
        }
    }
}

impl Drop for TestLogGuard {
    fn drop(&mut self) {
        info!(
            "{}: assertions passed (elapsed {:?})",
            self.name,
            self.start.elapsed()
        );
    }
}

pub fn test_log(name: &str) -> TestLogGuard {
    TestLogGuard::new(name)
}

pub fn test_db() -> SqliteStorage {
    init_test_logging();
    SqliteStorage::open_memory().expect("Failed to create test database")
}

pub fn test_db_with_dir() -> (SqliteStorage, TempDir) {
    init_test_logging();
    let dir = TempDir::new().expect("Failed to create temp dir");
    let db_path = dir.path().join(".beads").join("beads.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let storage = SqliteStorage::open(&db_path).expect("Failed to create test database");
    (storage, dir)
}
