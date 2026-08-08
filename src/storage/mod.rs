//! `SQLite` storage layer for `beads`.
//!
//! This module provides the persistence layer using `SQLite` with:
//! - WAL mode for concurrent reads
//! - Transaction discipline for atomic writes
//! - Dirty tracking for JSONL export
//! - Blocked cache for ready/blocked queries
//!
//! # Submodules
//!
//! - [`conn`] - Storage engine adapter over `rusqlite`
//! - [`schema`] - Database schema definitions
//! - [`sqlite`] - Main `SQLite` storage implementation

pub mod conn;
pub mod schema;
pub mod sqlite;

pub(crate) use sqlite::BulkDependencyInsert;
pub use sqlite::{
    ID_BEARING_COLUMNS, IssueUpdate, ListFilters, ReadyFilters, ReadySortPolicy, SqliteStorage,
    StatsIssueRow,
};
// Gated with the accessor that returns it; see `SqliteStorage::get_close_metadata`.
#[cfg(test)]
pub use sqlite::CloseMetadataRow;
