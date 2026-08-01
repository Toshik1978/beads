//! # Output Module
//!
//! This module provides rich terminal output using the [`rich_rust`] library.
//! It automatically detects the output mode and renders accordingly.
//!
//! ## Mode Detection
//!
//! Output mode is determined by the following priority:
//!
//! 1. `--json` flag → **JSON mode** (machine-readable)
//! 2. `--quiet` flag → **Quiet mode** (minimal output)
//! 3. `NO_COLOR` env or `--no-color` → **Plain mode** (no ANSI codes)
//! 4. Non-TTY stdout → **Plain mode** (piped output)
//! 5. Otherwise → **Rich mode** (colors, tables, panels)
//!
//! ## Usage
//!
//! ```rust,ignore
//! use crate::output::{OutputContext, OutputMode};
//!
//! // Create from CLI args
//! let ctx = OutputContext::from_args(&cli);
//!
//! // Or from flags directly
//! let ctx = OutputContext::from_flags(json, quiet, no_color);
//!
//! // Mode-aware output
//! ctx.success("Operation completed");
//! ctx.json(&data);  // Only outputs in JSON mode
//!
//! // Rich rendering (only in Rich mode)
//! ctx.render(&table);
//! ctx.render(&panel);
//! ```
//!
//! ## Submodules
//!
//! - [`context`]: Core [`OutputContext`] struct and [`OutputMode`] enum
//! - [`theme`]: Visual styling with [`Theme`] struct (colors, borders)
//! - [`components`]: Reusable output components (tables, panels, etc.)
//!
//! ## Design Principles
//!
//! - **Zero overhead in JSON/Quiet modes**: Console and theme are lazy-initialized
//! - **Automatic mode detection**: No manual configuration needed
//! - **Graceful degradation**: Rich → Plain → JSON → Quiet fallback chain
//! - **Consistent styling**: Theme provides unified look across commands

pub mod components;
pub mod context;
pub mod theme;

pub use components::*;
pub(crate) use context::JsonArrayPageMeta;
pub use context::{
    OutputContext, OutputMode, record_pending_exit_code, take_output_serialization_failure,
    take_pending_exit_code,
};
pub use theme::Theme;

/// Build a `rich_rust` `Console` with its dimensions already pinned.
///
/// **Always construct consoles through this.** `Console` resolves its own size
/// lazily — `Console::width()` falls back to `rich_rust`'s
/// `get_terminal_width()`, which calls `crossterm::terminal::size()`, which
/// opens `/dev/tty`. Opening a controlling terminal that has already hung up
/// never returns, and because [`crate::shutdown::install`] handles SIGHUP (so
/// `SqliteStorage::Drop` can flush the WAL, #270) `br` survives the hangup and
/// walks straight into that open — then hangs forever holding
/// `.beads/.write.lock`, blocking every later `br` in the repository
/// (bds-h2z).
///
/// `ConsoleBuilder::build` starts from `Console::new()` and overrides only the
/// fields that were set, so this differs from a bare `Console::new()` in
/// exactly one way: the size is known up front, measured by
/// [`crate::format::terminal_width`] from a descriptor this process
/// already holds. `tests/repro/tty_hangup_width.rs` fails if a raw
/// `Console::new()`/`Console::default()` reappears.
#[must_use]
pub fn console() -> rich_rust::console::Console {
    rich_rust::console::Console::builder()
        .width(crate::format::terminal_width())
        .height(crate::format::terminal_height())
        .build()
}
