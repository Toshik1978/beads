//! `br merge-driver` — a git merge driver for `.beads/issues.jsonl`.
//!
//! Git calls it with the ancestor, ours and theirs (`%O %A %B`) and takes the
//! file left at ours as the result: exit 0 is a clean merge, anything else a
//! conflict. It touches no database; the next `br` command's auto-import sees
//! the merged file. The merge itself is [`crate::sync::merge_driver`].

use crate::cli::MergeDriverArgs;
use crate::error::Result;
use crate::sync::merge_driver::{merge_jsonl_files, write_merged_file};

/// Execute the merge-driver command.
///
/// # Errors
///
/// Returns an error, leaving ours untouched, if any side cannot be trusted
/// (unreadable, conflict markers, invalid or duplicate records) or the result
/// cannot be written.
pub fn execute(args: &MergeDriverArgs) -> Result<()> {
    let merged = merge_jsonl_files(&args.base, &args.ours, &args.theirs)?;
    for note in &merged.notes {
        eprintln!("br merge-driver: {note}");
    }
    write_merged_file(&args.ours, &merged.content)
}
