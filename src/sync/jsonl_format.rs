//! The generation marker on `.beads/issues.jsonl`, and the rules for reading a
//! file written by a different one.
//!
//! # Why the interchange format needs this and the database does not
//!
//! `.beads/*.db` is gitignored and derived; changing a column there costs, at
//! worst, dropping the file and re-importing, and `PRAGMA user_version` plus
//! [`crate::storage::schema`]'s migration gates already handle the rest.
//! `issues.jsonl` is the opposite: it is committed, it is merged, it is read by
//! tools that are not this one, and until this module existed it carried no way
//! to say which generation of the format wrote it. That, not the database, was
//! the real obstacle to changing the data model — a field could not be added
//! without breaking every reader at once, because no reader could tell an old
//! file from a new one.
//!
//! # Where the marker lives, and why per record
//!
//! Every record carries `format_version` as its first key. The alternatives
//! were a header record or `.beads/metadata.json`, and both fail the same two
//! tests this format is actually subjected to: JSONL records get concatenated,
//! and they get three-way merged (this repository ships
//! `beads.{base,left,right}.jsonl` handling for exactly that). A header line
//! survives neither. `metadata.json` is per workspace, so it says nothing about
//! a file that arrived from somewhere else — which is the case the marker is
//! for.
//!
//! The cost is one key per line and a deliberate change to the field set pinned
//! by `tests/storage/schema_shape.rs`. That test is right to be strict and stays
//! strict; the entry was added to it on purpose.
//!
//! # The three cases a reader can face
//!
//! - **Older.** Migrated forward on import by [`migrate_record`], counted, and
//!   reported. The next flush writes the file back at the current generation,
//!   so the upgrade is announced once rather than on every run.
//! - **Current.** Nothing happens.
//! - **Newer.** Refused, by [`ensure_readable`]. This is the case that
//!   justifies the whole mechanism, so it is worth being clear about why
//!   refusal beats a best-effort read: a newer generation may *reinterpret* a
//!   key this build already knows, not merely add ones it does not. No amount
//!   of care with unrecognised fields protects against that, and a best-effort
//!   read would write its misreading back over a committed file. This mirrors
//!   [`crate::BeadsError::SchemaMismatch`], which refuses a database newer than
//!   the binary for the same reason.
//!
//! # Unknown keys are dropped, deliberately
//!
//! Within a generation this build understands, a key it does not recognise is
//! not a future field — the version marker is what future fields travel behind.
//! It is a foreign field, from a tracker this fork does not model, and this
//! fork's premise is a local single-user tracker rather than a federation hub.
//! Carrying such a key would mean storing it in the derived database and
//! letting it take part in `sync_equals`, `content_hash` and the three-way
//! merge — four decisions made on data with no meaning attached.
//!
//! So they are dropped, which is also what happened before this module existed;
//! what is new is that it is now a decision with a reason and a test
//! (`tests/storage/jsonl_format_version.rs`) rather than an accident. Forward
//! compatibility is provided by the refusal above, which is stronger: it
//! protects the keys a future generation *reinterprets*, not only the ones it
//! adds.

use serde::{Deserialize, Serialize};
use std::io::Write;

use crate::error::{BeadsError, Result};
use crate::model::Issue;

/// The generation this build writes and is the newest it can read.
///
/// Bump this when the meaning or shape of an exported record changes, and add
/// the matching arm to [`migrate_record`] in the same commit — the two are a
/// pair, and a bump without a migration turns every existing file into one this
/// build silently mis-reads.
pub const CURRENT_JSONL_FORMAT_VERSION: u32 = 1;

/// The generation assigned to a record with no `format_version` key: every
/// file written before the marker existed.
pub const UNVERSIONED_JSONL_FORMAT: u32 = 0;

/// The wire name of the marker. Named here so the export wrapper, the probe and
/// the tests cannot drift apart.
pub const FORMAT_VERSION_KEY: &str = "format_version";

/// An issue as it appears on the wire: the marker, then the issue's own fields
/// in declaration order.
///
/// `flatten` is what puts the marker *outside* [`Issue`] while keeping it
/// *inside* the record. The alternative — a real field on `Issue` — would have
/// meant a value that must read 0 when absent from a file but write the current
/// generation on export, which is two different defaults for one field, and it
/// would have put a property of the file format into the model of an issue.
#[derive(Serialize)]
struct VersionedIssue<'a> {
    format_version: u32,
    #[serde(flatten)]
    issue: &'a Issue,
}

/// Just the marker. Every other key deserializes into `IgnoredAny` and is
/// skipped without allocating, so this is a lexing pass rather than a second
/// full parse.
#[derive(Deserialize)]
struct FormatVersionProbe {
    #[serde(default)]
    format_version: u32,
}

/// Serialize one issue as a JSONL line's worth of JSON, stamped with the
/// current generation.
///
/// # Errors
///
/// Returns an error if the issue does not serialize.
pub fn to_line(issue: &Issue) -> serde_json::Result<String> {
    serde_json::to_string(&VersionedIssue {
        format_version: CURRENT_JSONL_FORMAT_VERSION,
        issue,
    })
}

/// [`to_line`] straight into a writer, for the export path's reused buffer.
///
/// # Errors
///
/// Returns an error if the issue does not serialize or the write fails.
pub fn write_line<W: Write>(writer: W, issue: &Issue) -> serde_json::Result<()> {
    serde_json::to_writer(
        writer,
        &VersionedIssue {
            format_version: CURRENT_JSONL_FORMAT_VERSION,
            issue,
        },
    )
}

/// Read the generation a record declares, or [`UNVERSIONED_JSONL_FORMAT`] when
/// it declares none.
///
/// # Errors
///
/// Returns an error if the line is not a JSON object.
pub fn read_format_version(line: &str) -> serde_json::Result<u32> {
    serde_json::from_str::<FormatVersionProbe>(line).map(|probe| probe.format_version)
}

/// Refuse a record from a generation this build does not know.
///
/// # Errors
///
/// Returns [`BeadsError::JsonlFormatTooNew`] when `version` is ahead of
/// [`CURRENT_JSONL_FORMAT_VERSION`].
pub const fn ensure_readable(version: u32) -> Result<()> {
    if version > CURRENT_JSONL_FORMAT_VERSION {
        return Err(BeadsError::JsonlFormatTooNew {
            found: version,
            supported: CURRENT_JSONL_FORMAT_VERSION,
        });
    }
    Ok(())
}

/// Bring one record forward from `from` to [`CURRENT_JSONL_FORMAT_VERSION`],
/// returning whether anything had to change.
///
/// Modelled on the database's `user_version < N` gates rather than on the
/// probe-the-table style beside them: each step is keyed on the generation it
/// upgrades *from*, runs at most once, and leaves a record already at the
/// current generation untouched. That is what makes a second import a no-op.
///
/// There is exactly one step so far, and it moves no data:
/// [`UNVERSIONED_JSONL_FORMAT`] and generation 1 describe the same field set,
/// and the only difference between a file at each is whether the marker is
/// present. It is still routed through here rather than special-cased, because
/// the first real migration should be an added arm and nothing else.
pub fn migrate_record(from: u32, _issue: &mut Issue) -> bool {
    let mut version = from;

    if version == UNVERSIONED_JSONL_FORMAT {
        // 0 -> 1: stamp only. No field moved, was renamed, or changed meaning.
        version = 1;
    }

    debug_assert!(
        version == CURRENT_JSONL_FORMAT_VERSION,
        "migrate_record left a record at generation {version}, not \
         {CURRENT_JSONL_FORMAT_VERSION}: CURRENT_JSONL_FORMAT_VERSION was bumped \
         without adding the matching step"
    );

    from < CURRENT_JSONL_FORMAT_VERSION
}

/// What an import did about generations, for the operator-facing report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FormatUpgradeTally {
    /// Records that arrived at an older generation and were migrated.
    pub upgraded: usize,
    /// The oldest generation seen in the file, when anything was upgraded.
    pub oldest_seen: Option<u32>,
}

impl FormatUpgradeTally {
    /// Fold one record's declared generation into the tally.
    pub fn record(&mut self, version: u32, upgraded: bool) {
        if upgraded {
            self.upgraded += 1;
            self.oldest_seen = Some(self.oldest_seen.map_or(version, |seen| seen.min(version)));
        }
    }

    /// True when the file needs rewriting to carry the current generation.
    #[must_use]
    pub const fn needs_restamp(&self) -> bool {
        self.upgraded > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_issue() -> Issue {
        Issue {
            id: "bd-fmt".to_string(),
            title: "probe".to_string(),
            ..Issue::default()
        }
    }

    #[test]
    fn an_exported_line_leads_with_the_current_generation() {
        let line = to_line(&probe_issue()).expect("serializes");
        assert!(
            line.starts_with(&format!(
                "{{\"{FORMAT_VERSION_KEY}\":{CURRENT_JSONL_FORMAT_VERSION},"
            )),
            "the marker must be the first key so a reader can tell the \
             generation before interpreting anything else: {line}"
        );
    }

    #[test]
    fn an_exported_line_still_carries_the_issue_fields_in_declaration_order() {
        let line = to_line(&probe_issue()).expect("serializes");
        let id_at = line.find("\"id\"").expect("id present");
        let title_at = line.find("\"title\"").expect("title present");
        assert!(
            id_at < title_at,
            "flatten must preserve the struct's field order, not sort it: {line}"
        );
    }

    #[test]
    fn a_record_without_the_key_reads_as_unversioned() {
        let version = read_format_version(r#"{"id":"bd-1","title":"legacy"}"#).expect("probes");
        assert_eq!(version, UNVERSIONED_JSONL_FORMAT);
    }

    #[test]
    fn the_probe_ignores_every_other_key_including_unknown_ones() {
        let version = read_format_version(
            r#"{"id":"bd-1","format_version":1,"whatever":{"nested":[1,2,3]}}"#,
        )
        .expect("probes");
        assert_eq!(version, 1);
    }

    #[test]
    fn a_newer_generation_is_refused_rather_than_read_best_effort() {
        let error = ensure_readable(CURRENT_JSONL_FORMAT_VERSION + 1)
            .expect_err("a newer generation must not be readable");
        assert!(
            matches!(error, BeadsError::JsonlFormatTooNew { found, supported }
                if found == CURRENT_JSONL_FORMAT_VERSION + 1
                    && supported == CURRENT_JSONL_FORMAT_VERSION),
            "unexpected error: {error:?}"
        );
        assert!(ensure_readable(CURRENT_JSONL_FORMAT_VERSION).is_ok());
        assert!(ensure_readable(UNVERSIONED_JSONL_FORMAT).is_ok());
    }

    #[test]
    fn migrating_is_a_no_op_at_the_current_generation() {
        let mut issue = probe_issue();
        let before = issue.clone();
        assert!(!migrate_record(CURRENT_JSONL_FORMAT_VERSION, &mut issue));
        assert_eq!(issue, before, "a current record must not be rewritten");
    }

    #[test]
    fn migrating_an_unversioned_record_reports_the_upgrade_and_changes_no_data() {
        let mut issue = probe_issue();
        let before = issue.clone();
        assert!(migrate_record(UNVERSIONED_JSONL_FORMAT, &mut issue));
        assert_eq!(
            issue, before,
            "0 -> 1 is a stamp; no field moved or changed meaning"
        );
    }

    #[test]
    fn the_tally_remembers_the_oldest_generation_it_upgraded() {
        let mut tally = FormatUpgradeTally::default();
        assert!(!tally.needs_restamp());

        tally.record(CURRENT_JSONL_FORMAT_VERSION, false);
        assert_eq!(tally, FormatUpgradeTally::default());

        tally.record(UNVERSIONED_JSONL_FORMAT, true);
        tally.record(UNVERSIONED_JSONL_FORMAT, true);
        assert_eq!(tally.upgraded, 2);
        assert_eq!(tally.oldest_seen, Some(UNVERSIONED_JSONL_FORMAT));
        assert!(tally.needs_restamp());
    }
}
