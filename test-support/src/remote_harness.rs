//! Shared helpers for the `br remote` tests.
//!
//! This used to hold `adopt_everything`, a hand-assembled copy of the pipeline
//! `br remote pull` would eventually run — fetch, pair, classify, order,
//! create parent-first, import links in a second pass — because the verbs were
//! stubs and three test files each needed that pipeline from somewhere.
//!
//! The verbs have landed, so the copy is gone: every test that used it now
//! drives the real `br remote pull` as a subprocess, which is the only way to
//! prove the CLI assembles those calls in the right order rather than proving
//! that a second implementation of the order agrees with the first. What
//! remains here is the small stuff several of those tests still share.

use beads::model::Issue;
use beads::remote::http::{HttpClient, RetryPolicy, Token};
use beads::storage::SqliteStorage;
use beads::util::id::IdConfig;
use std::path::Path;

/// An `HttpClient` pointed at a loopback mock, with retries off so a test
/// never waits on a backoff.
#[must_use]
pub fn client(base_url: &str) -> HttpClient {
    HttpClient::new(base_url, Token::new("t"), RetryPolicy::none())
}

/// The id settings `br init --prefix <prefix>` leaves behind.
#[must_use]
pub fn id_config(prefix: &str) -> IdConfig {
    IdConfig {
        prefix: prefix.to_string(),
        min_hash_length: 3,
        max_hash_length: 8,
        max_collision_prob: 0.25,
    }
}

/// The workspace database `br init` created under `beads_dir`.
///
/// # Panics
/// Panics if the database cannot be opened.
#[must_use]
pub fn open_storage(beads_dir: &Path) -> SqliteStorage {
    SqliteStorage::open(&beads_dir.join("beads.db")).expect("open workspace db")
}

/// Every issue with its relations, labels and comments attached.
///
/// `get_all_issues_for_export` returns bare rows — no `dependencies`, no
/// `labels`, no `comments` — and each consumer is only as good as what it is
/// handed: the link differ reports every mirrored link as locally absent
/// without the relations, and the comment count gate misfires on every issue
/// without the comments. Mirrors `cli::commands::remote::hydrated_issues`.
///
/// # Panics
/// Panics if any of the four export reads fails.
#[must_use]
pub fn hydrated_issues(storage: &SqliteStorage) -> Vec<Issue> {
    let mut issues = storage.get_all_issues_for_export().expect("issues");
    let mut dependencies = storage.get_all_dependency_records().expect("dependencies");
    let mut labels = storage.get_labels_for_export().expect("labels");
    let ids: Vec<String> = issues.iter().map(|issue| issue.id.clone()).collect();
    let mut comments = storage.get_comments_for_issues(&ids).expect("comments");
    for issue in &mut issues {
        if let Some(rows) = dependencies.remove(&issue.id) {
            issue.dependencies = rows;
        }
        if let Some(names) = labels.remove(&issue.id) {
            issue.labels = names;
        }
        if let Some(rows) = comments.remove(&issue.id) {
            issue.comments = rows;
        }
    }
    issues
}
