//! Serialization of one [`Issue`] as a line of `.beads/issues.jsonl`.
//!
//! The record is `Issue`'s derived `Serialize` implementation, nothing more:
//! no wrapper struct, no marker field. `serde_json` carries the
//! `preserve_order` feature, so a record's key order is fixed by `Issue`'s
//! field declaration order, not by anything this module does. Keep it that
//! way — do not build the record through a hand-assembled `serde_json::Map`,
//! which would make key order depend on insertion order instead and silently
//! reorder every line of every workspace.
//!
//! No field may be added to the record here. A field belongs on [`Issue`]
//! itself if it belongs in the interchange format at all.

use std::io::Write;

use crate::model::Issue;

/// Serialize one issue as a JSONL line's worth of JSON.
///
/// # Errors
///
/// Returns an error if the issue does not serialize.
pub fn to_line(issue: &Issue) -> serde_json::Result<String> {
    serde_json::to_string(issue)
}

/// [`to_line`] straight into a writer, for the export path's reused buffer.
///
/// # Errors
///
/// Returns an error if the issue does not serialize or the write fails.
pub fn write_line<W: Write>(writer: W, issue: &Issue) -> serde_json::Result<()> {
    serde_json::to_writer(writer, issue)
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
    fn an_exported_line_carries_the_issue_fields_in_declaration_order() {
        let line = to_line(&probe_issue()).expect("serializes");
        let id_at = line.find("\"id\"").expect("id present");
        let title_at = line.find("\"title\"").expect("title present");
        assert!(
            id_at < title_at,
            "the derive must keep the struct's field order, not sort it: {line}"
        );
    }

    #[test]
    fn write_line_matches_to_line() {
        let issue = probe_issue();
        let mut buf = Vec::new();
        write_line(&mut buf, &issue).expect("writes");
        assert_eq!(
            String::from_utf8(buf).expect("utf8"),
            to_line(&issue).expect("serializes")
        );
    }
}
