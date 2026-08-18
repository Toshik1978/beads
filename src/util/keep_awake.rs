//! Holding the machine awake for the length of a long network run.
//!
//! A laptop that idle-sleeps mid-`br remote sync` drops every open TCP
//! connection, and the run sees `Connection reset by peer` — which is
//! indistinguishable from a request that never left, and so costs a retry
//! this crate deliberately will not perform (`crate::remote::http::may_retry`).
//! Nothing about running in a terminal prevents that: a terminal emulator
//! holds no power assertion on behalf of the processes it runs, which
//! surprises approximately everyone.
//!
//! **This spawns `caffeinate` rather than calling IOKit.** The assertion API
//! is `IOPMAssertionCreateWithName`, which is C and therefore `unsafe`, and
//! `src/lib.rs` forbids `unsafe_code` outright. `/usr/bin/caffeinate` is the
//! same assertion behind a process boundary, ships with every macOS, and
//! needs no dependency at all.
//!
//! **`-w <our pid>` is what makes this leak-proof.** `caffeinate` releases
//! the assertion when the process it was told to watch exits, so an abnormal
//! end — a panic, a `SIGKILL`, a pulled power cable — cannot strand an
//! assertion holding the machine awake indefinitely. [`Drop`] kills the child
//! anyway, so the assertion ends with the verb rather than with the process,
//! but correctness does not depend on `Drop` running.
//!
//! **It prevents *idle* sleep, and only that.** Closing the lid still sleeps
//! the machine regardless of any assertion, unless it is on AC power with an
//! external display attached. This covers walking away from a long push; it
//! is not a promise that the run survives a closed laptop.

/// A held assertion, released when this is dropped.
///
/// Constructed by [`KeepAwake::hold`]. On every platform but macOS, and
/// whenever the caller opts out, this holds nothing and does nothing.
#[derive(Debug)]
pub struct KeepAwake {
    #[cfg(target_os = "macos")]
    child: Option<std::process::Child>,
}

/// The binary, by absolute path: this is a fixed part of macOS, and resolving
/// it through `PATH` would let a shadowing binary of the same name be run
/// instead.
#[cfg(target_os = "macos")]
const CAFFEINATE: &str = "/usr/bin/caffeinate";

impl KeepAwake {
    /// Hold the machine awake until the returned guard is dropped.
    ///
    /// `enabled` is the caller's opt-out — `--no-keep-awake` passes `false`,
    /// and the guard then holds nothing.
    ///
    /// **Never fails.** A missing or unrunnable `caffeinate` yields a guard
    /// that holds nothing, because a machine that might sleep is a far better
    /// outcome than a `br remote sync` that refuses to run.
    #[must_use]
    pub fn hold(enabled: bool) -> Self {
        #[cfg(target_os = "macos")]
        {
            use std::process::{Command, Stdio};
            let child = enabled
                .then(|| {
                    Command::new(CAFFEINATE)
                        .args(caffeinate_args(std::process::id()))
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .ok()
                })
                .flatten();
            Self { child }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = enabled;
            Self {}
        }
    }

    /// Whether an assertion is actually being held.
    ///
    /// False when the caller opted out, when `caffeinate` could not be run,
    /// and on every platform that has no `caffeinate` to run.
    #[must_use]
    pub const fn is_held(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.child.is_some()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
}

/// The arguments the child is given: prevent idle sleep, and release when
/// `pid` exits.
///
/// Pulled out as a pure function so the shape can be asserted without
/// spawning anything. `-i` and not `-s`: `-s` is documented as valid only on
/// AC power, so on a laptop running on battery — the case this exists for —
/// it does nothing at all.
#[cfg(target_os = "macos")]
fn caffeinate_args(pid: u32) -> [String; 3] {
    ["-i".to_string(), "-w".to_string(), pid.to_string()]
}

#[cfg(target_os = "macos")]
impl Drop for KeepAwake {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Both ignored deliberately: the child may already have exited,
            // and `-w` has released the assertion either way. `wait` is here
            // to reap, not to check.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opting_out_holds_nothing() {
        let guard = KeepAwake::hold(false);
        assert!(!guard.is_held());
    }

    /// The real spawn, on the one platform that has the binary. It asserts
    /// only that the child started: whether the assertion took effect is the
    /// kernel's business, and `pmset -g assertions` is how a human checks it.
    #[cfg(target_os = "macos")]
    #[test]
    fn holding_spawns_the_child() {
        let guard = KeepAwake::hold(true);
        assert!(
            guard.is_held(),
            "{CAFFEINATE} should be present on every macOS"
        );
    }

    /// `-s` would be the wrong flag (AC power only) and a missing `-w` would
    /// outlive the run, so the argument shape is worth pinning.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_child_is_told_to_watch_this_process() {
        let args = caffeinate_args(4321);
        assert_eq!(args, ["-i", "-w", "4321"]);
    }

    /// Dropping releases and reaps without panicking, twice over — the guard
    /// is created and dropped once per remote verb.
    #[test]
    fn dropping_is_quiet() {
        drop(KeepAwake::hold(true));
        drop(KeepAwake::hold(true));
    }
}
