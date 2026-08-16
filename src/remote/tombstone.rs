//! Renames, tombstones, and genuine deletions — and the rule that keeps
//! `br remote` from ever deleting a mirrored issue.
//!
//! **`br remote` never deletes a remote issue.** YouTrack deletion is
//! permanent and permission-gated, and a mirror that can permanently destroy
//! the thing it mirrors is a worse tool than one that cannot. Every tombstone
//! this module sees resolves to one of two actions, and neither is a
//! `DELETE`:
//!
//! - **Forward.** The tombstone's id appears in some live issue's
//!   `former_ids` — it was renamed, and the live issue carries the identity
//!   forward. The only work here is a local repair: point `external_ref` at
//!   the surviving issue so ordinary reconciliation (`build_plan`,
//!   bds-4r2.4) picks the pairing back up. Nothing is written to the mirror.
//! - **Delete.** No live issue claims the tombstone's id — it is gone for
//!   real. The mirror's own record of the deletion is a `State` change to
//!   `deleted_state` plus a `[br] deleted in beads` comment, never a removed
//!   issue.
//!
//! The classification keys off `former_ids` membership, not `delete_reason`
//! prose: `rename_issue` (`src/storage/sqlite.rs`) writes
//! `delete_reason: "renamed to {new_id}"` on the tombstone it leaves behind,
//! but that string is free text for a human to read, not a contract this
//! module parses. `former_ids` is the append-only, exact record of every id
//! an issue has ever held, so membership is exact where prose could drift.
//!
//! ## Why this module does not touch `ReconcilePlan::tombstoned`
//!
//! `build_plan` (`src/remote/plan.rs`) already names every tombstoned bead
//! still paired with a live remote issue in `ReconcilePlan::tombstoned`, and
//! deliberately does not diff it. That pairing runs on `external_ref`: a
//! tombstone reaches it only when the tombstone's *own* row still carries an
//! `external_ref` naming a remote issue — which is exactly what a genuine
//! delete (`SqliteStorage::delete_issue`) leaves behind, since it flips
//! `status` to `tombstone` in place and touches nothing else. A rename's
//! tombstone (`SqliteStorage::insert_rename_tombstone`) is a different shape:
//! it is a *fresh* row at the vacated id, and `external_ref` is
//! **unconditionally** cleared on it — not as a matter of the common case,
//! but because `idx_issues_external_ref_unique` has no status predicate, so a
//! tombstone that kept the value would collide with the live row it was just
//! renamed to and the whole rename would roll back. The live row kept the
//! value under the new id automatically, by virtue of `rewrite_issue_id`
//! moving the same row's primary key rather than copying it. So a plain
//! rename's tombstone never carries an `external_ref`, is invisible to
//! `pair_workspace`, and needs no forwarding: the pairing already followed
//! the identity on its own.
//!
//! `plan_tombstones` here is therefore intentionally broader and shallower
//! than `ReconcilePlan::tombstoned`: it classifies every tombstone in the
//! workspace by `former_ids` membership alone, independent of whether that
//! tombstone happens to be paired with a remote issue right now. The two are
//! meant to be joined by an executor, not merged into one type: a `Forward`
//! action is a pure local `external_ref` repair that never needs a
//! `remote_id` at all, while a `Delete` action needs the pairing from
//! [`crate::remote::plan::ReconcilePlan::tombstoned`] — join its entries onto
//! this function's output by `bead_id` — to know which remote issue to
//! update. Nothing here changes `ReconcilePlan` or `build_plan`; `tombstoned`
//! stays exactly the plain report section bds-4r2.4 left it as.

use crate::model::Issue;

/// What to do about one tombstoned bead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TombstoneAction {
    /// The tombstone's id survives under a new identity. `destination_bead_id`
    /// is the live bead whose `former_ids` names it; the only work is
    /// re-pointing `external_ref` onto that bead, locally, with zero writes
    /// to the mirror.
    Forward { destination_bead_id: String },
    /// No live bead claims this id — it is gone for real. The mirror's
    /// record of that is a `State` change to `deleted_state` plus a comment,
    /// never a `DELETE`. `reason` carries the tombstone's own
    /// `delete_reason`, if any, into that comment.
    Delete { reason: Option<String> },
}

/// Classify one tombstone by `former_ids` membership alone.
///
/// Scans `live` for any issue whose `former_ids` contains `tombstone_id`; the
/// first match wins (an id can be renamed away from at most once — a second
/// rename onto the same vacated id is refused by `rename_issue`'s
/// `id_exists` check, which counts tombstones as taken). Found → `Forward`
/// onto that issue's id; otherwise → `Delete`, with no reason attached (the
/// caller — `plan_tombstones` — supplies the tombstone's own `delete_reason`).
#[must_use]
pub fn classify_tombstone(tombstone_id: &str, live: &[Issue]) -> TombstoneAction {
    live.iter()
        .find(|issue| issue.former_ids.iter().any(|id| id == tombstone_id))
        .map_or(TombstoneAction::Delete { reason: None }, |issue| {
            TombstoneAction::Forward {
                destination_bead_id: issue.id.clone(),
            }
        })
}

/// Classify every tombstone in `beads` against the rest of the workspace.
///
/// Each tombstone is classified against the *whole* bead list, `beads`
/// itself included — a tombstone's own `former_ids` are irrelevant to its
/// own classification (they were reset to empty when it was created, see the
/// module doc), so including it in the scan changes nothing but costs
/// nothing to allow. Order matches `beads`' own tombstone order, not sorted.
///
/// This returns bare bead ids, not remote ids: a `Delete` entry needs a
/// remote issue to act on, and that comes from joining this output against
/// [`crate::remote::plan::ReconcilePlan::tombstoned`] by `bead_id` — see that
/// field's doc comment for the same cross-reference in the other direction.
#[must_use]
pub fn plan_tombstones(beads: &[Issue]) -> Vec<(String, TombstoneAction)> {
    beads
        .iter()
        .filter(|issue| issue.status == crate::model::Status::Tombstone)
        .map(|tombstone| {
            let action = match classify_tombstone(&tombstone.id, beads) {
                forward @ TombstoneAction::Forward { .. } => forward,
                TombstoneAction::Delete { .. } => TombstoneAction::Delete {
                    reason: tombstone.delete_reason.clone(),
                },
            };
            (tombstone.id.clone(), action)
        })
        .collect()
}

/// The comment text a genuine deletion leaves on the mirrored issue.
///
/// Prefixed `[br]` so a later run recognises it as its own echo rather than a
/// human comment (the same convention comment sync uses elsewhere in this
/// epic). `reason` is the tombstone's own `delete_reason`, when it has one.
#[must_use]
pub fn deleted_comment_text(reason: Option<&str>) -> String {
    reason.map_or_else(
        || "[br]\ndeleted in beads".to_string(),
        |reason| format!("[br]\ndeleted in beads: {reason}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    fn live(id: &str, former: &[&str]) -> Issue {
        Issue {
            id: id.to_string(),
            title: "live".to_string(),
            former_ids: former.iter().map(|s| (*s).to_string()).collect(),
            ..Issue::default()
        }
    }

    fn tombstone(id: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: "gone".to_string(),
            status: crate::model::Status::Tombstone,
            ..Issue::default()
        }
    }

    #[test]
    fn the_two_real_tombstones_are_forwarding_pointers() {
        // Verified against this workspace: both tombstones are renames.
        let live_issues = vec![
            live("bds-is1", &["bds-b4f.7"]),
            live("bds-j1m", &["bds-b4f.8"]),
        ];

        assert_eq!(
            classify_tombstone("bds-b4f.7", &live_issues),
            TombstoneAction::Forward {
                destination_bead_id: "bds-is1".into()
            }
        );
        assert_eq!(
            classify_tombstone("bds-b4f.8", &live_issues),
            TombstoneAction::Forward {
                destination_bead_id: "bds-j1m".into()
            }
        );
    }

    #[test]
    fn a_tombstone_in_no_former_ids_is_a_genuine_deletion() {
        let live_issues = vec![live("bds-is1", &["bds-b4f.7"])];
        assert!(matches!(
            classify_tombstone("bds-gone", &live_issues),
            TombstoneAction::Delete { .. }
        ));
    }

    #[test]
    fn the_delete_comment_names_the_reason_and_carries_the_marker() {
        let text = deleted_comment_text(Some("superseded by the new engine"));
        assert!(
            text.starts_with("[br]"),
            "must be recognised as our own echo: {text}"
        );
        assert!(text.contains("deleted in beads"), "{text}");
        assert!(text.contains("superseded by the new engine"), "{text}");
    }

    #[test]
    fn the_delete_comment_omits_a_missing_reason_rather_than_saying_none() {
        let text = deleted_comment_text(None);
        assert!(text.starts_with("[br]"), "{text}");
        assert!(text.contains("deleted in beads"), "{text}");
        assert!(
            !text.contains(':'),
            "no reason means no trailing colon: {text}"
        );
    }

    #[test]
    fn classification_ignores_delete_reason_prose() {
        // A rename worded differently must still classify as a forward.
        let mut tomb = tombstone("bds-old");
        tomb.delete_reason = Some("moved under the new epic".into());
        let live_issues = vec![live("bds-new", &["bds-old"])];
        assert_eq!(
            classify_tombstone(&tomb.id, &live_issues),
            TombstoneAction::Forward {
                destination_bead_id: "bds-new".into()
            },
            "membership in former_ids decides, not the wording of delete_reason"
        );
    }

    #[test]
    fn plan_tombstones_classifies_every_tombstone_and_carries_the_delete_reason() {
        let mut renamed_away = tombstone("bds-old");
        renamed_away.delete_reason = Some("renamed to bds-new".into());
        let mut genuinely_gone = tombstone("bds-gone");
        genuinely_gone.delete_reason = Some("no longer needed".into());
        let destination = live("bds-new", &["bds-old"]);
        let untouched = Issue {
            id: "bds-live".into(),
            title: "still here".into(),
            ..Issue::default()
        };

        let beads = vec![renamed_away, genuinely_gone, destination, untouched];
        let plan = plan_tombstones(&beads);

        assert_eq!(plan.len(), 2, "{plan:?}");
        let by_id: std::collections::HashMap<_, _> = plan.into_iter().collect();
        assert_eq!(
            by_id["bds-old"],
            TombstoneAction::Forward {
                destination_bead_id: "bds-new".into()
            }
        );
        assert_eq!(
            by_id["bds-gone"],
            TombstoneAction::Delete {
                reason: Some("no longer needed".into())
            }
        );
    }

    #[test]
    fn plan_tombstones_is_empty_over_a_workspace_with_no_tombstones() {
        let beads = vec![Issue {
            id: "bds-1".into(),
            title: "live".into(),
            ..Issue::default()
        }];
        assert!(plan_tombstones(&beads).is_empty());
    }

    /// Whether `text` contains a `.delete(...)` call whose `entity` argument
    /// is the literal `"issue"`.
    ///
    /// Broader than a fixed-path needle on purpose: `HttpClient::delete_issue_link`
    /// already narrows the client to one path-building call
    /// (`src/remote/http.rs`), but this guard exists as defence in depth for
    /// the day someone re-adds a general `delete(path, entity)` and calls it
    /// on an issue path from somewhere else. `entity` is already how this
    /// codebase tells an issue from an issue *link* apart at every call site
    /// (`"issue"` vs `"issue link"`), so it is the discriminator here too —
    /// and unlike a literal path fragment, it does not care what sits between
    /// `.delete(` and that argument, so building the path into a local first
    /// (the exact idiom `link_remove` itself used to write the link path)
    /// does not evade it.
    fn calls_delete_on_an_issue(text: &str) -> bool {
        let forbidden = Regex::new(r#"\.delete\([^)]*"issue"\)"#).expect("valid regex");
        forbidden.is_match(text)
    }

    #[test]
    fn the_guard_catches_a_path_built_before_the_call() {
        let snippet = "let path = format!(\"/api/issues/{remote_id}\");\n\
                        http.delete(&path, \"issue\")";
        assert!(
            calls_delete_on_an_issue(snippet),
            "must catch the format!-then-delete idiom, not just an inline path literal"
        );
    }

    #[test]
    fn the_guard_does_not_flag_a_legitimate_issue_link_delete() {
        let snippet = "let path = format!(\"/api/issues/{from}/links/{link_id}/issues/{to}\");\n\
                        http.delete(&path, \"issue link\")";
        assert!(
            !calls_delete_on_an_issue(snippet),
            "issue link deletion is legitimate and must not trip the guard"
        );
    }

    #[test]
    fn no_remote_code_path_deletes_an_issue() {
        // A mirror that can permanently destroy what it mirrors is a worse
        // tool than one that cannot. Pin the absence.
        for entry in walk_remote_sources() {
            let text = std::fs::read_to_string(&entry).expect("read source");
            assert!(
                !calls_delete_on_an_issue(&text),
                "{} calls .delete(..., \"issue\"); br must never destroy a mirrored issue",
                entry.display()
            );
        }
    }

    /// Every `.rs` file under `src/remote/`, except this one.
    ///
    /// Excluding `src/remote/tombstone.rs` by name — the same move
    /// `tests/licensing.rs` makes for itself via `ALLOWED_FILES` — is what
    /// lets `calls_delete_on_an_issue`'s needle stay a plain, visibly-correct
    /// regex instead of one obfuscated to survive scanning its own source:
    /// this file's two tests above spell out the exact forbidden call shape
    /// in plain text, which would otherwise make this test fail on itself.
    fn walk_remote_sources() -> Vec<std::path::PathBuf> {
        let self_path = std::path::Path::new("src/remote/tombstone.rs");
        let mut found = Vec::new();
        let mut stack = vec![std::path::PathBuf::from("src/remote")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read_dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") && path != self_path {
                    found.push(path);
                }
            }
        }
        found
    }
}
