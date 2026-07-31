// Tests for the storage engine — the SQLite database, the JSONL mirror, and
// the sync between them.
//
// These were 11 separate integration binaries. Each linked the whole
// dependency graph — including rusqlite's bundled C SQLite — and the link steps
// cost far more than compiling the tests did. As modules of one binary they link
// once. Nothing was deleted and nothing was renamed beyond dropping the
// redundant `storage_` prefix that `tests/storage/` now carries.
//
// `#[path]` because this file is the crate root, where a bare `mod x;` would
// resolve to `tests/x.rs` and claim a sibling binary's name.
//
// Any `#![allow(...)]` at the top of a module file below stays valid and now
// scopes to that module instead of a whole binary, which is strictly narrower.

extern crate test_support as common;

#[path = "storage/blocked_cache.rs"]
mod blocked_cache;
#[path = "storage/crud.rs"]
mod crud;
#[path = "storage/deps.rs"]
mod deps;
#[path = "storage/export_atomic.rs"]
mod export_atomic;
#[path = "storage/golden_snapshot.rs"]
mod golden_snapshot;
#[path = "storage/history.rs"]
mod history;
#[path = "storage/id_hash_parity.rs"]
mod id_hash_parity;
#[path = "storage/invariants.rs"]
mod invariants;
#[path = "storage/list_filters.rs"]
mod list_filters;
#[path = "storage/ready.rs"]
mod ready;
#[path = "storage/schema_shape.rs"]
mod schema_shape;
