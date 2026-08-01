//! Progress indicator utilities for long-running operations.
//!
//! Provides:
//! - Determinate progress bars for known-count operations
//! - Spinners for indeterminate operations
//! - Conditional display based on terminal detection

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::time::Duration;

/// Create a determinate progress bar for operations with known total count.
///
/// # Arguments
/// * `total` - Total number of items to process
/// * `message` - Initial message to display
/// * `show` - Whether to actually show the progress bar (use `should_show_progress()`)
///
/// # Panics
/// Panics if the progress bar template string is invalid.
///
/// # Example
/// ```ignore
/// let pb = create_progress_bar(issues.len() as u64, "Exporting issues", should_show_progress());
/// for issue in issues {
///     // ... process issue
///     pb.inc(1);
/// }
/// pb.finish_with_message("Export complete");
/// ```
#[must_use]
pub fn create_progress_bar(total: u64, message: &str, show: bool) -> ProgressBar {
    let pb = ProgressBar::new(total);

    if show {
        let style = ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=>-");

        pb.set_style(style);
        pb.set_message(message.to_string());
    } else {
        pb.set_draw_target(ProgressDrawTarget::hidden());
    }

    pb
}

/// Create a spinner for indeterminate operations.
///
/// # Arguments
/// * `message` - Message to display alongside the spinner
/// * `show` - Whether to actually show the spinner (use `should_show_progress()`)
///
/// # Panics
/// Panics if the spinner template string is invalid.
///
/// # Example
/// ```ignore
/// let spinner = create_spinner("Scanning git history...", should_show_progress());
/// // ... long operation
/// spinner.finish_with_message("Scan complete");
/// ```
#[must_use]
pub fn create_spinner(message: &str, show: bool) -> ProgressBar {
    let pb = ProgressBar::new_spinner();

    if show {
        let style = ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner());

        pb.set_style(style);
        pb.set_message(message.to_string());
        pb.enable_steady_tick(Duration::from_millis(100));
    } else {
        pb.set_draw_target(ProgressDrawTarget::hidden());
    }

    pb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_bar_hidden_when_not_terminal() {
        let pb = create_progress_bar(100, "Test", false);
        assert_eq!(pb.length(), Some(100));
        assert_eq!(pb.position(), 0);

        pb.inc(50);
        assert_eq!(pb.position(), 50);

        pb.finish();
        assert!(pb.is_finished());
    }

    #[test]
    fn test_spinner_hidden_when_not_terminal() {
        let spinner = create_spinner("Testing...", false);
        assert_eq!(spinner.length(), None);
        assert_eq!(spinner.position(), 0);

        spinner.finish();
        assert!(spinner.is_finished());
    }
}
