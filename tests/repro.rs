// Regression tests, one module per bug that actually happened.
//
// These were 22 separate integration binaries. Every one of them linked the
// whole dependency graph — including a bundled C SQLite — to run an average
// of two tests, and the 22 link steps cost more than the tests themselves.
// As modules of one binary they link once. Nothing was deleted: each module
// keeps the tests it had, under the name it had minus the redundant `repro_`
// prefix the directory now carries.
//
// `#[path]` because this file is the crate root, where a bare `mod list_sort;`
// would resolve to `tests/list_sort.rs` and claim a sibling binary's name.

extern crate test_support as common;

#[path = "repro/auto_flush_inefficiency.rs"]
mod auto_flush_inefficiency;
#[path = "repro/cache_crash.rs"]
mod cache_crash;
#[path = "repro/collision_labels.rs"]
mod collision_labels;
#[path = "repro/config_completion_no_db.rs"]
mod config_completion_no_db;
#[path = "repro/create_output.rs"]
mod create_output;
#[path = "repro/create_path_traversal_check.rs"]
mod create_path_traversal_check;
#[path = "repro/dep_tree.rs"]
mod dep_tree;
#[path = "repro/dotted_id_resolution.rs"]
mod dotted_id_resolution;
#[path = "repro/epic_blocking.rs"]
mod epic_blocking;
#[path = "repro/history_collision.rs"]
mod history_collision;
#[path = "repro/id_collision.rs"]
mod id_collision;
#[path = "repro/import_collision_remap.rs"]
mod import_collision_remap;
#[path = "repro/issue_256_update_diff_target.rs"]
mod issue_256_update_diff_target;
#[path = "repro/issue_not_found_suggestions.rs"]
mod issue_not_found_suggestions;
#[path = "repro/list_sort.rs"]
mod list_sort;
#[path = "repro/list_sort_alias.rs"]
mod list_sort_alias;
#[path = "repro/mergereport_determinism.rs"]
mod mergereport_determinism;
#[path = "repro/parent_blocking.rs"]
mod parent_blocking;
#[path = "repro/pinned_blocker.rs"]
mod pinned_blocker;
#[path = "repro/reparent_orphans_old_epic.rs"]
mod reparent_orphans_old_epic;
#[path = "repro/source_repo_path_leak.rs"]
mod source_repo_path_leak;
#[path = "repro/sync_cycle_fuzz_crashes.rs"]
mod sync_cycle_fuzz_crashes;
#[path = "repro/sync_relations.rs"]
mod sync_relations;
#[path = "repro/three_way_merge_bug.rs"]
mod three_way_merge_bug;
#[path = "repro/time_panic.rs"]
mod time_panic;
#[path = "repro/truncate_width.rs"]
mod truncate_width;
#[path = "repro/tty_hangup_width.rs"]
mod tty_hangup_width;
