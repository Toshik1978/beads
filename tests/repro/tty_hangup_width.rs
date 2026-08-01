//! bds-h2z: `br` hung forever after its controlling terminal hung up.
//!
//! `format::text::terminal_width()` used `crossterm::terminal::size()`, whose
//! unix path opens `/dev/tty` before falling back to `STDOUT_FILENO`. Opening
//! a *hung-up* controlling terminal never returns, and crossterm's fallback
//! only fires when the open *fails* — a blocking open never does. Because
//! `shutdown::install()` handles SIGHUP (so `SqliteStorage::Drop` can flush
//! the WAL, #270), `br` survives the hangup that would kill an ordinary
//! process and reaches that open instead of dying. The process then never
//! exits and never releases `.beads/.write.lock`, so every later `br` in that
//! repository times out waiting for it.
//!
//! **Why this is a source guard rather than a hang test.** Reproducing the
//! hang needs a pty whose master is closed mid-run, and giving a child a
//! controlling terminal requires `setsid` + `TIOCSCTTY` between fork and exec
//! — `Command::pre_exec`, which is `unsafe`. This crate sets
//! `unsafe_code = "forbid"`, and `[lints]` applies to test targets too, so
//! that test cannot be written here. What is checkable, and what actually
//! matters, is the invariant: the width path must never open `/dev/tty`.
//! The manual reproduction is recorded on bds-h2z.

use std::fs;
use std::path::Path;

/// Calls that resolve the terminal size by opening `/dev/tty`.
///
/// `crossterm::terminal::size()` delegates to `window_size()`, which opens it;
/// both are named so that reaching for either is caught.
const BLOCKING_SIZE_CALLS: &[&str] = &[
    "crossterm::terminal::size",
    "crossterm::terminal::window_size",
];

/// Put this on the line above a call to exempt it, with a reason.
///
/// The one legitimate use is the Windows branch of `terminal_dimensions`: there
/// is no `/dev/tty` on Windows, so crossterm's console-API path cannot block.
/// Following `tests/no_id_pinning.rs`, which exempts by annotation for the
/// same reason — a guard with no escape hatch gets deleted the first time it
/// is wrong.
const EXEMPTION: &str = "tty-hangup-exempt:";

fn rust_sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// A `Console` with no explicit size resolves it lazily through
/// `rich_rust::terminal::get_terminal_width`, which is
/// `crossterm::terminal::size()` — the same blocking open, reached from every
/// rich render rather than from `terminal_width`. `crate::output::console()`
/// pins the dimensions up front, so it is the only place allowed to build one.
const RAW_CONSOLE_CTORS: &[&str] = &["Console::new()", "Console::default()"];

#[test]
fn consoles_are_built_with_pinned_dimensions() {
    let mut sources = Vec::new();
    rust_sources(Path::new("src"), &mut sources);

    let mut offenders = Vec::new();
    for path in &sources {
        // The chokepoint itself, which builds the one sized Console.
        if path.ends_with("output/mod.rs") {
            continue;
        }
        let text = fs::read_to_string(path).expect("read source");
        for (index, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for ctor in RAW_CONSOLE_CTORS {
                if line.contains(ctor) {
                    offenders.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these build a Console that will resolve its own size by opening \
         /dev/tty, which blocks forever once the controlling terminal has hung \
         up (bds-h2z). Use crate::output::console() instead:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn width_detection_never_opens_dev_tty() {
    let mut sources = Vec::new();
    rust_sources(Path::new("src"), &mut sources);
    assert!(!sources.is_empty(), "found no sources to scan");

    let mut offenders = Vec::new();
    for path in &sources {
        // This file names the calls in order to forbid them.
        if path.ends_with("tty_hangup_width.rs") {
            continue;
        }
        let text = fs::read_to_string(path).expect("read source");
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            // An exemption is the nearest preceding comment line, so the
            // reason sits with the call rather than in a list elsewhere.
            let exempt = index
                .checked_sub(1)
                .is_some_and(|previous| lines[previous].contains(EXEMPTION));

            for call in BLOCKING_SIZE_CALLS {
                if line.contains(call) && !exempt {
                    offenders.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these open /dev/tty to measure the terminal, which blocks forever once \
         the controlling terminal has hung up (bds-h2z). Read the winsize from a \
         descriptor already held instead:\n{}",
        offenders.join("\n")
    );
}
