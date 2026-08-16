// End-to-end tests: each one drives the `br` binary as a subprocess and asserts
// on what a user or an agent would actually observe.
//
// These were 46 separate integration binaries. Each linked the whole
// dependency graph — including rusqlite's bundled C SQLite — and the link steps
// cost far more than compiling the tests did. As modules of one binary they link
// once. Nothing was deleted and nothing was renamed beyond dropping the
// redundant `e2e_` prefix that `tests/e2e/` now carries.
//
// `#[path]` because this file is the crate root, where a bare `mod x;` would
// resolve to `tests/x.rs` and claim a sibling binary's name.
//
// Any `#![allow(...)]` at the top of a module file below stays valid and now
// scopes to that module instead of a whole binary, which is strictly narrower.

extern crate test_support as common;

#[path = "e2e/basic_lifecycle.rs"]
mod basic_lifecycle;
#[path = "e2e/cas_guards.rs"]
mod cas_guards;
#[path = "e2e/comments.rs"]
mod comments;
#[path = "e2e/comments_stdin.rs"]
mod comments_stdin;
#[path = "e2e/completions.rs"]
mod completions;
#[path = "e2e/concurrency.rs"]
mod concurrency;
#[path = "e2e/config_precedence.rs"]
mod config_precedence;
#[path = "e2e/create_output.rs"]
mod create_output;
#[path = "e2e/date_range_flags.rs"]
mod date_range_flags;
#[path = "e2e/defer.rs"]
mod defer;
#[path = "e2e/dep_tree_mermaid.rs"]
mod dep_tree_mermaid;
#[path = "e2e/detach.rs"]
mod detach;
#[path = "e2e/env_overrides.rs"]
mod env_overrides;
#[path = "e2e/epic.rs"]
mod epic;
#[path = "e2e/ergonomics.rs"]
mod ergonomics;
#[path = "e2e/errors.rs"]
mod errors;
#[path = "e2e/exclusion_flags.rs"]
mod exclusion_flags;
#[path = "e2e/git_safety_full_cli.rs"]
mod git_safety_full_cli;
#[path = "e2e/global_flags.rs"]
mod global_flags;
#[path = "e2e/history.rs"]
mod history;
#[path = "e2e/history_custom_path.rs"]
mod history_custom_path;
#[path = "e2e/history_restore_prune.rs"]
mod history_restore_prune;
#[path = "e2e/issue_252_fresh_bead_lookup.rs"]
mod issue_252_fresh_bead_lookup;
#[path = "e2e/labels.rs"]
mod labels;
#[path = "e2e/list_comprehensive.rs"]
mod list_comprehensive;
#[path = "e2e/list_priority.rs"]
mod list_priority;
#[path = "e2e/list_scenarios.rs"]
mod list_scenarios;
#[path = "e2e/raw_sqlite_rebuilt_lookup.rs"]
mod raw_sqlite_rebuilt_lookup;
#[path = "e2e/read_only_fast_open.rs"]
mod read_only_fast_open;
#[path = "e2e/ready.rs"]
mod ready;
#[path = "e2e/ready_limit.rs"]
mod ready_limit;
#[path = "e2e/relations.rs"]
mod relations;
#[path = "e2e/rename.rs"]
mod rename;
#[path = "e2e/reparent.rs"]
mod reparent;
#[path = "e2e/report_generation.rs"]
mod report_generation;
#[path = "e2e/routing.rs"]
mod routing;
#[path = "e2e/search_scenarios.rs"]
mod search_scenarios;
#[path = "e2e/sort_multi_key.rs"]
mod sort_multi_key;
#[path = "e2e/stale.rs"]
mod stale;
#[path = "e2e/stats.rs"]
mod stats;
#[path = "e2e/sync_artifacts.rs"]
mod sync_artifacts;
#[path = "e2e/sync_failure_injection.rs"]
mod sync_failure_injection;
#[path = "e2e/sync_fuzz_edge_cases.rs"]
mod sync_fuzz_edge_cases;
#[path = "e2e/sync_git_safety.rs"]
mod sync_git_safety;
#[path = "e2e/sync_preflight_integration.rs"]
mod sync_preflight_integration;
#[path = "e2e/sync_status_health.rs"]
mod sync_status_health;
#[path = "e2e/terminal_sanitization.rs"]
mod terminal_sanitization;
#[path = "e2e/undefer.rs"]
mod undefer;
#[path = "e2e/version.rs"]
mod version;
#[path = "e2e/vocabulary.rs"]
mod vocabulary;
#[path = "e2e/workspace_commands.rs"]
mod workspace_commands;
#[path = "e2e/workspace_scenarios.rs"]
mod workspace_scenarios;
#[path = "e2e/wrap.rs"]
mod wrap;
