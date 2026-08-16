//! The link differ: three dependency types against the mirror's links.
//!
//! **A removal must carry the internal id.** [`LinkChange::Remove`] holds
//! `target_internal_id` for exactly that reason — a removal addressed by
//! `idReadable` 404s (see `youtrack::links::link_remove`), and the id is only
//! available because the fetch requests `issues(id,idReadable)`.
//!
//! **A rename needs no special handling here.** Reparenting changes the
//! bead's parent, the differ sees a `Subtask` link present on one side and
//! absent on the other, and emits one removal and one addition. The tombstone
//! rule only has to re-point `external_ref`; the link churn falls out of this
//! differ for free.
//!
//! Each kind is mirrored in exactly **one** direction, and the other
//! direction of the same type is ignored rather than diffed. A parent link
//! appears on the child as `INWARD` and on the parent as `OUTWARD`; diffing
//! both would have each end try to remove what the other end just added. The
//! child owns the parent link, the blocker owns the `Depend` link, and
//! `Relates` — undirected, and therefore present on both ends as `BOTH` — is
//! mirrored symmetrically from a symmetric local set.
//!
//! **The read and the write use the same identifier, by construction.** A
//! link is selected here by `LinkTypes::link_id(kind, mirrored_direction(kind))`
//! — the very string the executor puts in the `{linkID}` path segment — and
//! not by the `direction` word YouTrack reports alongside it. That closes the
//! one failure mode this module could otherwise have: if the suffix↔direction
//! correspondence (`…t` ↔ `INWARD`) were ever wrong, a differ reading by
//! `direction` and an executor writing by `link_id` would disagree forever,
//! each run adding a link the next run removes. Reading by the same id makes
//! that class of bug unrepresentable — a wrong `mirrored_direction` then
//! yields stable, obviously-wrong data (parent and child swapped) rather than
//! silent churn.

use crate::remote::model::RemoteIssue;
use crate::remote::youtrack::links::{Direction, LinkKind, LinkTypes, LinkedIssue};
use std::collections::BTreeSet;

/// One bead's relations, as bead ids.
#[derive(Debug, Clone, Default)]
pub struct BeadLinks {
    /// The bead this one is a subtask of.
    pub parent: Option<String>,
    /// Beads this one blocks — it is required for them.
    pub blocks: Vec<String>,
    /// Beads related to this one. Symmetric: if `a` lists `b`, `b` lists `a`.
    pub related: Vec<String>,
}

/// One link to create or destroy.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LinkChange {
    Add {
        kind: LinkKind,
        target_readable: String,
    },
    Remove {
        kind: LinkKind,
        /// The internal database id, e.g. `"3-20"`. A removal addressed by
        /// `idReadable` 404s as if the link were already gone.
        target_internal_id: String,
        /// Carried for the printed plan only.
        target_readable: String,
    },
}

/// The direction each kind is mirrored in.
///
/// This is the single definition the differ and the executor share: the
/// executor turns it into a `{linkID}` path segment with
/// `LinkTypes::link_id(kind, mirrored_direction(kind))`.
#[must_use]
pub const fn mirrored_direction(kind: LinkKind) -> Direction {
    match kind {
        // "subtask of": the linked issue is this one's parent.
        LinkKind::Subtask => Direction::TargetToSource,
        // "is required for": this issue blocks the linked one.
        LinkKind::Depend => Direction::SourceToTarget,
        LinkKind::Relates => Direction::Undirected,
    }
}

/// The `direction` string a fetched link carries for `direction`.
///
/// The differ does **not** use this — it selects by `link_id` so that read and
/// write share one identifier. It is kept because it states the correspondence
/// the whole scheme rests on, and
/// `link_id_and_direction_tag_describe_the_same_link` pins the two together.
#[must_use]
pub const fn direction_tag(direction: Direction) -> &'static str {
    match direction {
        Direction::SourceToTarget => "OUTWARD",
        Direction::TargetToSource => "INWARD",
        Direction::Undirected => "BOTH",
    }
}

/// The name a kind is printed under.
#[must_use]
pub const fn kind_name(kind: LinkKind) -> &'static str {
    match kind {
        LinkKind::Subtask => "parent",
        LinkKind::Depend => "blocks",
        LinkKind::Relates => "relates",
    }
}

/// Diff one bead's relations against its mirror's links.
///
/// `types` is the instance's resolved link type ids; it is what makes the
/// selection here and the write in the executor use one identifier — see the
/// module doc.
///
/// `resolve` maps a bead id onto its mirror's `idReadable`. A target it
/// cannot map has no mirror yet, so the link is **skipped, not guessed**: it
/// is emitted on a later run once both ends exist. Emitting it now would need
/// an id nobody has.
#[must_use]
pub fn diff_links(
    bead_deps: &BeadLinks,
    remote: &RemoteIssue,
    types: &LinkTypes,
    resolve: &dyn Fn(&str) -> Option<String>,
) -> Vec<LinkChange> {
    let parent: Vec<String> = bead_deps.parent.iter().cloned().collect();
    let mut changes = Vec::new();
    for (kind, local) in [
        (LinkKind::Subtask, parent.as_slice()),
        (LinkKind::Depend, bead_deps.blocks.as_slice()),
        (LinkKind::Relates, bead_deps.related.as_slice()),
    ] {
        diff_one_kind(&mut changes, kind, local, remote, types, resolve);
    }
    changes
}

fn diff_one_kind(
    changes: &mut Vec<LinkChange>,
    kind: LinkKind,
    local_bead_ids: &[String],
    remote: &RemoteIssue,
    types: &LinkTypes,
    resolve: &dyn Fn(&str) -> Option<String>,
) {
    let wanted: BTreeSet<String> = local_bead_ids
        .iter()
        .filter_map(|bead_id| resolve(bead_id))
        .collect();

    // The same string the executor will put in the `{linkID}` path segment.
    let link_id = types.link_id(kind, mirrored_direction(kind));
    let present: Vec<&LinkedIssue> = remote
        .links
        .iter()
        .filter(|link| link.link_id == link_id)
        .flat_map(|link| link.issues.iter())
        .collect();
    let present_readable: BTreeSet<&str> = present
        .iter()
        .map(|issue| issue.id_readable.as_str())
        .collect();

    for target in &wanted {
        if !present_readable.contains(target.as_str()) {
            changes.push(LinkChange::Add {
                kind,
                target_readable: target.clone(),
            });
        }
    }
    for issue in present {
        if !wanted.contains(&issue.id_readable) {
            changes.push(LinkChange::Remove {
                kind,
                target_internal_id: issue.id.clone(),
                target_readable: issue.id_readable.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::youtrack::links::RemoteLink;

    fn remote_with_parent(parent_internal: &str, parent_readable: &str) -> RemoteIssue {
        let mut issue = RemoteIssue::for_test("EM-2");
        issue.links = vec![RemoteLink {
            link_id: "173-3t".into(),
            kind: LinkKind::Subtask,
            direction: "INWARD".into(),
            issues: vec![LinkedIssue {
                id: parent_internal.into(),
                id_readable: parent_readable.into(),
            }],
        }];
        issue
    }

    /// The reference instance's link type ids. Ids live in tests, never in
    /// `src/` — see `links::tests`.
    fn types() -> LinkTypes {
        LinkTypes {
            subtask: "173-3".into(),
            depend: "173-1".into(),
            relates: "173-0".into(),
        }
    }

    /// Map a bead id onto its mirror's readable id.
    fn resolver(pairs: Vec<(&'static str, &'static str)>) -> impl Fn(&str) -> Option<String> {
        move |bead_id: &str| {
            pairs
                .iter()
                .find(|(b, _)| *b == bead_id)
                .map(|(_, r)| (*r).to_string())
        }
    }

    #[test]
    fn a_local_only_parent_emits_one_addition() {
        let links = BeadLinks {
            parent: Some("bds-1".into()),
            blocks: vec![],
            related: vec![],
        };
        let remote = RemoteIssue::for_test("EM-2");
        let changes = diff_links(
            &links,
            &remote,
            &types(),
            &resolver(vec![("bds-1", "EM-1")]),
        );
        assert_eq!(changes.len(), 1);
        assert!(
            matches!(
                &changes[0],
                LinkChange::Add { kind: LinkKind::Subtask, target_readable } if target_readable == "EM-1"
            ),
            "got {:?}",
            changes[0]
        );
    }

    #[test]
    fn a_remote_only_parent_emits_a_removal_carrying_the_internal_id() {
        let links = BeadLinks::default();
        let remote = remote_with_parent("3-20", "EM-1");
        let changes = diff_links(&links, &remote, &types(), &resolver(vec![]));
        match &changes[0] {
            LinkChange::Remove {
                target_internal_id,
                target_readable,
                ..
            } => {
                assert_eq!(target_internal_id, "3-20", "a removal by idReadable 404s");
                assert_eq!(
                    target_readable, "EM-1",
                    "the readable id is for the printed plan"
                );
            }
            other @ LinkChange::Add { .. } => panic!("expected Remove, got {other:?}"),
        }
    }

    #[test]
    fn an_agreeing_parent_emits_nothing() {
        let links = BeadLinks {
            parent: Some("bds-1".into()),
            blocks: vec![],
            related: vec![],
        };
        let remote = remote_with_parent("3-20", "EM-1");
        let changes = diff_links(
            &links,
            &remote,
            &types(),
            &resolver(vec![("bds-1", "EM-1")]),
        );
        assert!(changes.is_empty(), "got {changes:?}");
    }

    #[test]
    fn a_reparent_emits_one_removal_and_one_addition_and_no_create() {
        let links = BeadLinks {
            parent: Some("bds-9".into()),
            blocks: vec![],
            related: vec![],
        };
        let remote = remote_with_parent("3-20", "EM-1");
        let changes = diff_links(
            &links,
            &remote,
            &types(),
            &resolver(vec![("bds-9", "EM-7")]),
        );

        assert_eq!(changes.len(), 2);
        assert!(
            changes
                .iter()
                .any(|c| matches!(c, LinkChange::Remove { .. }))
        );
        assert!(changes.iter().any(|c| matches!(c, LinkChange::Add { .. })));
    }

    #[test]
    fn an_unpaired_link_target_is_skipped_not_guessed() {
        // The target bead has no mirror yet — the link is emitted on a later
        // run, once both ends exist. Emitting it now would need an id nobody has.
        let links = BeadLinks {
            parent: Some("bds-new".into()),
            blocks: vec![],
            related: vec![],
        };
        let remote = RemoteIssue::for_test("EM-2");
        let changes = diff_links(&links, &remote, &types(), &resolver(vec![]));
        assert!(changes.is_empty(), "got {changes:?}");
    }

    #[test]
    fn the_other_direction_of_the_same_type_is_ignored_not_removed() {
        // The parent's own issue carries the OUTWARD half of the same link.
        // Diffing it here would make the two ends fight: the child adds the
        // link, the parent removes it, forever.
        let mut remote = RemoteIssue::for_test("EM-1");
        remote.links = vec![RemoteLink {
            link_id: "173-3s".into(),
            kind: LinkKind::Subtask,
            direction: "OUTWARD".into(),
            issues: vec![LinkedIssue {
                id: "3-21".into(),
                id_readable: "EM-2".into(),
            }],
        }];
        let changes = diff_links(&BeadLinks::default(), &remote, &types(), &resolver(vec![]));
        assert!(changes.is_empty(), "got {changes:?}");
    }

    #[test]
    fn each_kind_is_diffed_in_its_own_direction() {
        let mut remote = RemoteIssue::for_test("EM-2");
        remote.links = vec![
            RemoteLink {
                link_id: "173-1s".into(),
                kind: LinkKind::Depend,
                direction: "OUTWARD".into(),
                issues: vec![LinkedIssue {
                    id: "3-30".into(),
                    id_readable: "EM-3".into(),
                }],
            },
            RemoteLink {
                link_id: "173-0".into(),
                kind: LinkKind::Relates,
                direction: "BOTH".into(),
                issues: vec![LinkedIssue {
                    id: "3-40".into(),
                    id_readable: "EM-4".into(),
                }],
            },
        ];
        let links = BeadLinks {
            parent: None,
            blocks: vec!["bds-3".into()],
            related: vec!["bds-4".into()],
        };
        let changes = diff_links(
            &links,
            &remote,
            &types(),
            &resolver(vec![("bds-3", "EM-3"), ("bds-4", "EM-4")]),
        );
        assert!(changes.is_empty(), "got {changes:?}");
    }

    /// The one correspondence the whole scheme rests on, stated once.
    ///
    /// The differ selects by `link_id` and the executor writes by `link_id`,
    /// so the two cannot disagree — but a fetched link also carries a
    /// `direction` word, and a reader of this module needs to know which
    /// suffix goes with which word. If YouTrack ever returned `OUTWARD` for a
    /// link created through a `…t` id, this table is where that would be
    /// recorded.
    #[test]
    fn link_id_and_direction_tag_describe_the_same_link() {
        let types = types();
        for (kind, expected_id, expected_tag) in [
            (LinkKind::Subtask, "173-3t", "INWARD"),
            (LinkKind::Depend, "173-1s", "OUTWARD"),
            (LinkKind::Relates, "173-0", "BOTH"),
        ] {
            let direction = mirrored_direction(kind);
            assert_eq!(
                types.link_id(kind, direction),
                expected_id,
                "{} writes through {expected_id}",
                kind_name(kind)
            );
            assert_eq!(
                direction_tag(direction),
                expected_tag,
                "{} reads back as {expected_tag}",
                kind_name(kind)
            );
        }
    }

    /// A link whose `direction` word disagrees with its id is still matched by
    /// id. This is the guard finding 1 asked for: read and write share one
    /// identifier, so a wrong suffix↔word table cannot produce churn.
    #[test]
    fn selection_follows_the_id_not_the_direction_word() {
        let mut remote = RemoteIssue::for_test("EM-2");
        remote.links = vec![RemoteLink {
            link_id: "173-3t".into(),
            kind: LinkKind::Subtask,
            direction: "SOMETHING-ELSE".into(),
            issues: vec![LinkedIssue {
                id: "3-20".into(),
                id_readable: "EM-1".into(),
            }],
        }];
        let links = BeadLinks {
            parent: Some("bds-1".into()),
            blocks: vec![],
            related: vec![],
        };
        let changes = diff_links(
            &links,
            &remote,
            &types(),
            &resolver(vec![("bds-1", "EM-1")]),
        );
        assert!(
            changes.is_empty(),
            "the link is the one we would write; its direction word is not consulted: {changes:?}"
        );
    }
}
