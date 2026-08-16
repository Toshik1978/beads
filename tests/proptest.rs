// Property-based tests: invariants that must hold for every generated input,
// as opposed to the specific cases the e2e and repro suites pin.
//
// These were 11 separate integration binaries. Each linked the whole
// dependency graph — including rusqlite's bundled C SQLite — and the link steps
// cost far more than compiling the tests did. As modules of one binary they link
// once. Nothing was deleted and nothing was renamed beyond dropping the
// redundant `proptest_` prefix that `tests/proptest/` now carries.
//
// `#[path]` because this file is the crate root, where a bare `mod x;` would
// resolve to `tests/x.rs` and claim a sibling binary's name.
//
// Any `#![allow(...)]` at the top of a module file below stays valid and now
// scopes to that module instead of a whole binary, which is strictly narrower.

extern crate test_support as common;

#[path = "proptest/claim_exclusion.rs"]
mod claim_exclusion;
#[path = "proptest/hash.rs"]
mod hash;
#[path = "proptest/id.rs"]
mod id;
#[path = "proptest/jsonl_roundtrip.rs"]
mod jsonl_roundtrip;
#[path = "proptest/merge.rs"]
mod merge;
#[path = "proptest/model_roundtrip.rs"]
mod model_roundtrip;
#[path = "proptest/parent_child.rs"]
mod parent_child;
#[path = "proptest/remote_comment_echo.rs"]
mod remote_comment_echo;
#[path = "proptest/sort_spec_agreement.rs"]
mod sort_spec_agreement;
#[path = "proptest/status_partition.rs"]
mod status_partition;
#[path = "proptest/sync_path.rs"]
mod sync_path;
#[path = "proptest/time.rs"]
mod time;
#[path = "proptest/validation.rs"]
mod validation;
