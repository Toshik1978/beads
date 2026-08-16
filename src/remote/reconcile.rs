//! Pairing a workspace against a fetched mirror.
//!
//! The engine is statelessly full-reconciling: there is no watermark and no
//! sidecar, so every run re-derives the whole picture from the workspace and
//! the remote. Identity is single-sided — a bead's `external_ref` holds the
//! remote `idReadable`, and nothing is stored on the remote side that br has
//! to trust.
//!
//! Scoping is the whole of identity's isolation. `external_ref` carries a
//! *global* partial UNIQUE index and is a general-purpose field, so the
//! pairing is scoped by configuration rather than by a stored marker:
//! `idReadable` is `<shortName>-<n>` and `remote.yaml` names the project, so
//! `br remote` considers exactly those beads whose `external_ref` matches
//! `^EM-\d+$` for project `EM`. A ref written by another importer is
//! invisible to every verb, which is what lets one workspace be mirrored into
//! two trackers without either clobbering the other's ref.

use crate::model::{Issue, Status};
use crate::remote::config::RemoteConfig;
use crate::remote::model::RemoteIssue;
use std::collections::HashMap;

/// A bead paired with the remote issue its `external_ref` names.
#[derive(Debug, Clone)]
pub struct PairedPair {
    pub bead_id: String,
    pub remote: RemoteIssue,
}

/// Everything one reconciliation run needs to know about who is who.
///
/// The three outcomes that matter are `paired`, `dangling_local` (a bead
/// whose ref names an issue that no longer exists — reported, never silently
/// re-created) and `unpaired_remote` (an adoption candidate).
#[derive(Debug, Clone, Default)]
pub struct Pairing {
    /// Beads whose in-scope `external_ref` names an issue that exists.
    pub paired: Vec<PairedPair>,
    /// Bead ids whose in-scope `external_ref` names an issue that does not.
    pub dangling_local: Vec<String>,
    /// Remote issues no bead claims — adoption candidates.
    pub unpaired_remote: Vec<RemoteIssue>,
    /// Live bead ids with no `external_ref` at all — create candidates.
    pub unpaired_local: Vec<String>,
    /// Bead ids whose `external_ref` belongs to some other tracker.
    /// Informational: deliberately untouched by every verb.
    pub out_of_scope: Vec<String>,
}

/// Whether `external_ref` names an issue in the configured project.
///
/// `external_ref` is a general-purpose field with a global UNIQUE index, so
/// scoping is what keeps one workspace's two mirrors from seeing each other's
/// pairings. Exact prefix, then an all-digits suffix — no regex needed.
#[must_use]
pub fn is_in_scope(cfg: &RemoteConfig, external_ref: &str) -> bool {
    let prefix = cfg.issue_prefix();
    let Some(suffix) = external_ref.strip_prefix(&prefix) else {
        return false;
    };
    !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit())
}

/// Pair `beads` against `remote`, scoped by `cfg`'s project prefix.
#[must_use]
pub fn pair_workspace(cfg: &RemoteConfig, beads: &[Issue], remote: Vec<RemoteIssue>) -> Pairing {
    // Index by readable id, keeping the fetch order for whatever is left over
    // so the printed plan is stable run to run.
    let mut by_readable: HashMap<String, usize> = HashMap::with_capacity(remote.len());
    for (index, issue) in remote.iter().enumerate() {
        by_readable.insert(issue.id_readable.clone(), index);
    }
    let mut slots: Vec<Option<RemoteIssue>> = remote.into_iter().map(Some).collect();

    let mut pairing = Pairing::default();
    for bead in beads {
        match bead.external_ref.as_deref() {
            Some(reference) if !is_in_scope(cfg, reference) => {
                // Another importer's ref. Not dangling, not a create
                // candidate, not ours to touch.
                pairing.out_of_scope.push(bead.id.clone());
            }
            Some(reference) => match by_readable
                .get(reference)
                .and_then(|index| slots[*index].take())
            {
                Some(issue) => pairing.paired.push(PairedPair {
                    bead_id: bead.id.clone(),
                    remote: issue,
                }),
                // Reported, never re-created: re-creating would silently
                // duplicate an issue a human deleted on purpose.
                //
                // One caveat, deliberately not designed around. `take()`
                // claims the issue for the first bead that names it, so if
                // two beads somehow carried the *same* `external_ref` the
                // second would land here and be described as naming an issue
                // that is gone, which would be false — it was claimed, not
                // deleted. `external_ref` carries a partial UNIQUE index in
                // SQLite, so a workspace br wrote cannot reach this; a
                // hand-edited `issues.jsonl` could. A distinct outcome would
                // cost a third case in every consumer to describe a state the
                // database forbids, so the note is the fix.
                None => pairing.dangling_local.push(bead.id.clone()),
            },
            // The tombstone rule owns tombstones; creating one here would
            // mirror a deletion as a brand-new issue.
            None if bead.status == Status::Tombstone => {}
            None => pairing.unpaired_local.push(bead.id.clone()),
        }
    }

    pairing.unpaired_remote = slots.into_iter().flatten().collect();
    pairing
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RemoteConfig {
        RemoteConfig::from_yaml_str(include_str!("../../tests/fixtures/remote_em.yaml"))
            .expect("valid config")
    }

    fn bead(id: &str, external_ref: Option<&str>, status: Status) -> Issue {
        Issue {
            id: id.to_string(),
            title: "t".to_string(),
            external_ref: external_ref.map(str::to_string),
            status,
            ..Issue::default()
        }
    }

    fn remote(id_readable: &str) -> RemoteIssue {
        RemoteIssue::for_test(id_readable)
    }

    #[test]
    fn a_ref_from_another_importer_is_out_of_scope_and_untouched() {
        let cfg = config();
        let beads = vec![bead("bds-1", Some("JIRA-123"), Status::Open)];
        let pairing = pair_workspace(&cfg, &beads, vec![]);

        assert!(pairing.paired.is_empty());
        assert!(
            pairing.dangling_local.is_empty(),
            "an out-of-scope ref is not dangling"
        );
        assert!(
            pairing.unpaired_local.is_empty(),
            "nor is it a create candidate"
        );
        assert_eq!(pairing.out_of_scope, ["bds-1"]);
    }

    #[test]
    fn a_ref_naming_a_missing_issue_is_dangling_never_recreated() {
        let beads = vec![bead("bds-1", Some("EM-9"), Status::Open)];
        let pairing = pair_workspace(&config(), &beads, vec![]);

        assert_eq!(pairing.dangling_local, ["bds-1"]);
        assert!(
            pairing.unpaired_local.is_empty(),
            "a dangling bead must not fall through into the create set"
        );
    }

    #[test]
    fn an_unclaimed_issue_is_an_adoption_candidate() {
        let pairing = pair_workspace(&config(), &[], vec![remote("EM-4")]);
        assert_eq!(pairing.unpaired_remote.len(), 1);
        assert_eq!(pairing.unpaired_remote[0].id_readable, "EM-4");
    }

    #[test]
    fn a_live_bead_without_a_ref_is_a_create_candidate() {
        let beads = vec![bead("bds-1", None, Status::Open)];
        let pairing = pair_workspace(&config(), &beads, vec![]);
        assert_eq!(pairing.unpaired_local, ["bds-1"]);
    }

    #[test]
    fn a_tombstone_is_not_a_create_candidate() {
        let beads = vec![bead("bds-1", None, Status::Tombstone)];
        let pairing = pair_workspace(&config(), &beads, vec![]);
        assert!(
            pairing.unpaired_local.is_empty(),
            "the tombstone rule owns tombstones; creating one would mirror a deletion as an issue"
        );
    }

    #[test]
    fn a_matching_pair_is_paired() {
        let beads = vec![bead("bds-1", Some("EM-4"), Status::Open)];
        let pairing = pair_workspace(&config(), &beads, vec![remote("EM-4")]);
        assert_eq!(pairing.paired.len(), 1);
        assert_eq!(pairing.paired[0].bead_id, "bds-1");
        assert!(pairing.unpaired_remote.is_empty());
    }

    #[test]
    fn scoping_requires_the_exact_prefix_and_a_numeric_suffix() {
        let cfg = config();
        assert!(is_in_scope(&cfg, "EM-1"));
        assert!(is_in_scope(&cfg, "EM-1234"));
        assert!(
            !is_in_scope(&cfg, "EMX-1"),
            "a longer short name must not match"
        );
        assert!(!is_in_scope(&cfg, "EM-1a"), "the suffix must be all digits");
        assert!(
            !is_in_scope(&cfg, "em-1"),
            "YouTrack short names are case-sensitive"
        );
        assert!(!is_in_scope(&cfg, "EM-"));
    }

    #[test]
    fn unpaired_remote_keeps_the_fetch_order() {
        let beads = vec![bead("bds-1", Some("EM-2"), Status::Open)];
        let mirror = vec![remote("EM-1"), remote("EM-2"), remote("EM-3")];
        let pairing = pair_workspace(&config(), &beads, mirror);
        let left: Vec<&str> = pairing
            .unpaired_remote
            .iter()
            .map(|i| i.id_readable.as_str())
            .collect();
        assert_eq!(left, ["EM-1", "EM-3"]);
    }
}
