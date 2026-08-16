//! The value diff and the direction rule.
//!
//! **Diff by value first, direction second.** A field whose values already
//! agree is never a candidate, so clock skew cannot cause a spurious write.
//! It can only mis-order a field edited on *both* sides between two
//! consecutive syncs, which is the narrowest window a design carrying no
//! stored state can offer.
//!
//! **Only three fields can flow back.** `State`, `Priority` and comments —
//! the ones a human actually changes in the web UI. (Comments are diffed
//! separately; they are not a field.) Everything else is local-wins
//! *unconditionally*, so a `design` edited in YouTrack is overwritten on the
//! next push, and that is correct: beads is the source of truth.

use crate::model::Issue;
use crate::remote::config::RemoteConfig;
use crate::remote::model::RemoteIssue;
use serde::Serialize;

/// One mirrored field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Title,
    Description,
    State,
    Priority,
    Type,
    Design,
    AcceptanceCriteria,
    Notes,
    CloseReason,
    Labels,
}

impl Field {
    /// The name this field is printed under.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Description => "description",
            Self::State => "state",
            Self::Priority => "priority",
            Self::Type => "type",
            Self::Design => "design",
            Self::AcceptanceCriteria => "acceptance criteria",
            Self::Notes => "notes",
            Self::CloseReason => "close reason",
            Self::Labels => "labels",
        }
    }
}

/// Which way a difference resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Beads wins: the value is written to the remote.
    Push,
    /// The remote wins — a YouTrack-wins case, always printed.
    Pull,
}

/// The fields a remote edit can flow back through.
///
/// Comments are returnable too, but they are not a `Field`: they are diffed
/// separately.
pub const RETURNABLE_FIELDS: [Field; 2] = [Field::State, Field::Priority];

/// One field that differs, and which way it resolves.
#[derive(Debug, Clone, Serialize)]
pub struct FieldChange {
    pub field: Field,
    pub direction: Direction,
    pub local: String,
    pub remote: String,
}

/// Diff a paired bead against its mirror, field by field.
///
/// `cfg` is taken but unread: `status` and `priority` are compared as *beads*
/// values, because the remote side has already been reversed through the maps
/// by `youtrack::mapping::reverse_fields`. Comparing the mapped strings
/// instead would make an edit to `remote.yaml` read as a field difference on
/// every paired issue.
#[must_use]
pub fn diff_pair(cfg: &RemoteConfig, bead: &Issue, remote: &RemoteIssue) -> Vec<FieldChange> {
    let _ = cfg;
    let mut changes = Vec::new();

    compare(
        &mut changes,
        bead,
        remote,
        Field::Title,
        // Trimmed on both sides. YouTrack has been observed to normalise
        // whitespace in a summary, and an untrimmed comparison would then
        // find a difference every run, push a value that comes back changed,
        // and never settle — the same permanent loop the prose fields avoid
        // by folding `None` and `""` together.
        bead.title.trim().to_string(),
        remote.summary.trim().to_string(),
    );
    compare(
        &mut changes,
        bead,
        remote,
        Field::Description,
        prose(bead.description.as_deref()),
        prose(remote.description.as_deref()),
    );
    compare(
        &mut changes,
        bead,
        remote,
        Field::Type,
        bead.issue_type.as_str().to_string(),
        remote.fields.issue_type.as_str().to_string(),
    );
    compare(
        &mut changes,
        bead,
        remote,
        Field::State,
        bead.status.as_str().to_string(),
        remote.fields.status.as_str().to_string(),
    );
    compare(
        &mut changes,
        bead,
        remote,
        Field::Priority,
        bead.priority.to_string(),
        remote.fields.priority.to_string(),
    );
    compare(
        &mut changes,
        bead,
        remote,
        Field::Design,
        prose(bead.design.as_deref()),
        prose(remote.fields.design.as_deref()),
    );
    compare(
        &mut changes,
        bead,
        remote,
        Field::AcceptanceCriteria,
        prose(bead.acceptance_criteria.as_deref()),
        prose(remote.fields.acceptance_criteria.as_deref()),
    );
    compare(
        &mut changes,
        bead,
        remote,
        Field::Notes,
        prose(bead.notes.as_deref()),
        prose(remote.fields.notes.as_deref()),
    );
    compare(
        &mut changes,
        bead,
        remote,
        Field::CloseReason,
        prose(bead.close_reason.as_deref()),
        prose(remote.fields.close_reason.as_deref()),
    );
    compare(
        &mut changes,
        bead,
        remote,
        Field::Labels,
        label_list(&bead.labels),
        label_list(&remote.labels),
    );

    changes
}

/// Emit a change only when the two rendered values actually differ, then
/// decide direction.
fn compare(
    changes: &mut Vec<FieldChange>,
    bead: &Issue,
    remote: &RemoteIssue,
    field: Field,
    local: String,
    remote_value: String,
) {
    if local == remote_value {
        return;
    }
    changes.push(FieldChange {
        field,
        direction: direction_for(field, bead, remote),
        local,
        remote: remote_value,
    });
}

/// The direction rule, reached only for a field that already differs — which
/// is why skew cannot cause a spurious write.
fn direction_for(field: Field, bead: &Issue, remote: &RemoteIssue) -> Direction {
    if RETURNABLE_FIELDS.contains(&field) {
        // Milliseconds on both sides. YouTrack reports `updated` as epoch
        // millis and the bead's `updated_at` is converted to match; a
        // seconds-resolution comparison would call a one-millisecond lead a
        // tie and pick a side arbitrarily.
        if remote.updated.timestamp_millis() > bead.updated_at.timestamp_millis() {
            Direction::Pull
        } else {
            Direction::Push
        }
    } else {
        Direction::Push
    }
}

/// An absent prose field renders as the empty string, so `None` and `Some("")`
/// compare equal — YouTrack does not reliably distinguish them either.
fn prose(value: Option<&str>) -> String {
    value.unwrap_or_default().to_string()
}

/// Labels compare as a set: order is not meaningful on either side.
fn label_list(labels: &[String]) -> String {
    let mut sorted: Vec<&str> = labels.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    sorted.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn config() -> RemoteConfig {
        RemoteConfig::from_yaml_str(include_str!("../../tests/fixtures/remote_em.yaml"))
            .expect("valid config")
    }

    fn at(millis: i64) -> chrono::DateTime<Utc> {
        Utc.timestamp_millis_opt(millis)
            .single()
            .expect("valid instant")
    }

    /// A bead and a mirror that agree on everything.
    ///
    /// `Priority::MEDIUM` is beads' `P2`; the model has no `P2` constant.
    fn agreeing() -> (Issue, RemoteIssue) {
        let bead = Issue {
            id: "bds-1".into(),
            title: "same title".into(),
            description: Some("same body".into()),
            status: crate::model::Status::Open,
            priority: crate::model::Priority::MEDIUM,
            updated_at: at(1_000),
            ..Issue::default()
        };

        let mut remote = RemoteIssue::for_test("EM-1");
        remote.summary = "same title".into();
        remote.description = Some("same body".into());
        remote.fields.status = crate::model::Status::Open;
        remote.fields.priority = crate::model::Priority::MEDIUM;
        remote.updated = at(9_999_999);

        (bead, remote)
    }

    #[test]
    fn agreement_produces_no_work_however_the_clocks_read() {
        let (bead, remote) = agreeing();
        let changes = diff_pair(&config(), &bead, &remote);
        assert!(
            changes.is_empty(),
            "the mirror is nine thousand seconds newer and it still must not matter: {changes:?}"
        );
    }

    #[test]
    fn a_differing_title_always_pushes() {
        let (mut bead, mut remote) = agreeing();
        bead.title = "local title".into();
        remote.summary = "remote title".into();
        remote.updated = at(9_999_999);
        bead.updated_at = at(1);

        let changes = diff_pair(&config(), &bead, &remote);
        let change = changes
            .iter()
            .find(|c| c.field == Field::Title)
            .expect("title differs");
        assert_eq!(
            change.direction,
            Direction::Push,
            "title is not returnable: local wins even though the remote is far newer"
        );
    }

    #[test]
    fn a_newer_remote_state_is_pulled_and_recorded() {
        let (mut bead, mut remote) = agreeing();
        bead.status = crate::model::Status::Open;
        bead.updated_at = at(1_000);
        remote.fields.status = crate::model::Status::Closed;
        remote.updated = at(2_000);

        let changes = diff_pair(&config(), &bead, &remote);
        let change = changes
            .iter()
            .find(|c| c.field == Field::State)
            .expect("state differs");
        assert_eq!(change.direction, Direction::Pull);
        assert_eq!(change.local, "open");
        assert_eq!(change.remote, "closed");
    }

    #[test]
    fn an_older_remote_state_is_pushed() {
        let (mut bead, mut remote) = agreeing();
        bead.status = crate::model::Status::Closed;
        bead.updated_at = at(5_000);
        remote.fields.status = crate::model::Status::Open;
        remote.updated = at(2_000);

        let changes = diff_pair(&config(), &bead, &remote);
        let change = changes
            .iter()
            .find(|c| c.field == Field::State)
            .expect("state differs");
        assert_eq!(change.direction, Direction::Push);
    }

    #[test]
    fn a_non_returnable_field_pushes_even_when_the_remote_is_newer() {
        let (mut bead, mut remote) = agreeing();
        bead.design = Some("local design".into());
        bead.updated_at = at(1);
        remote.fields.design = Some("remote design".into());
        remote.updated = at(9_999_999);

        let changes = diff_pair(&config(), &bead, &remote);
        let change = changes
            .iter()
            .find(|c| c.field == Field::Design)
            .expect("design differs");
        assert_eq!(
            change.direction,
            Direction::Push,
            "design has no YouTrack equivalent a human edits deliberately; local wins"
        );
    }

    #[test]
    fn the_comparison_uses_milliseconds_on_both_sides() {
        let (mut bead, mut remote) = agreeing();
        // One millisecond apart. If either side were converted to seconds the
        // two would compare equal and the direction would be arbitrary.
        bead.status = crate::model::Status::Open;
        bead.updated_at = at(1_700_000_000_001);
        remote.fields.status = crate::model::Status::Closed;
        remote.updated = at(1_700_000_000_002);

        let changes = diff_pair(&config(), &bead, &remote);
        let change = changes
            .iter()
            .find(|c| c.field == Field::State)
            .expect("state differs");
        assert_eq!(
            change.direction,
            Direction::Pull,
            "a one-millisecond lead must be visible"
        );
    }

    #[test]
    fn a_newer_remote_priority_is_pulled_but_a_newer_remote_type_is_not() {
        let (mut bead, mut remote) = agreeing();
        bead.updated_at = at(1);
        remote.updated = at(2);
        remote.fields.priority = crate::model::Priority::CRITICAL;
        remote.fields.issue_type = crate::model::IssueType::Bug;

        let changes = diff_pair(&config(), &bead, &remote);
        let priority = changes
            .iter()
            .find(|c| c.field == Field::Priority)
            .expect("priority differs");
        let issue_type = changes
            .iter()
            .find(|c| c.field == Field::Type)
            .expect("type differs");
        assert_eq!(priority.direction, Direction::Pull);
        assert_eq!(
            issue_type.direction,
            Direction::Push,
            "only State and Priority are returnable"
        );
    }

    #[test]
    fn labels_compare_as_a_set_not_a_sequence() {
        let (mut bead, mut remote) = agreeing();
        bead.labels = vec!["b".into(), "a".into()];
        remote.labels = vec!["a".into(), "b".into()];
        assert!(
            diff_pair(&config(), &bead, &remote).is_empty(),
            "label order is not a difference"
        );

        remote.labels = vec!["a".into()];
        let changes = diff_pair(&config(), &bead, &remote);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].field, Field::Labels);
        assert_eq!(changes[0].direction, Direction::Push);
    }

    #[test]
    fn a_title_differing_only_in_surrounding_whitespace_is_not_a_difference() {
        let (mut bead, mut remote) = agreeing();
        bead.title = "  same title ".into();
        remote.summary = "same title".into();
        assert!(
            diff_pair(&config(), &bead, &remote).is_empty(),
            "an untrimmed comparison would push forever"
        );
    }

    #[test]
    fn an_absent_prose_field_equals_an_empty_one() {
        let (mut bead, mut remote) = agreeing();
        bead.notes = None;
        remote.fields.notes = Some(String::new());
        assert!(
            diff_pair(&config(), &bead, &remote).is_empty(),
            "None and \"\" are the same absence"
        );
    }
}
