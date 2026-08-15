//! Status-workflow and repository-capacity policy.
//!
//! Loads `.beads/policy.yaml` (when present) into a typed [`PolicyDocument`].
//! Every section is opt-in; with no policy file every consumer sees the
//! permissive default (no status vocabulary enforced, no transition rules,
//! no capacity limits).
//!
//! This module used to also own closure-time gates (`close_policy:`,
//! required close reasons, acceptance-criteria/typed-reference checks, and
//! the `--bypass-policy` escape hatch, issue #274 Phase 1). Evidence that
//! those flags were never used in practice (bds-b4f.4.1) led to their
//! removal; `Workflow` and `CapacityPolicy` below remain live and are
//! consumed by `br create`/`update`/`close`/`epic`/`reopen`/`ready`/
//! `vocabulary` and the storage layer's atomic-update path, so they were
//! kept.

use crate::error::{BeadsError, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Default file name for the policy document inside `.beads/`.
pub const POLICY_FILE_NAME: &str = "policy.yaml";

/// Top-level policy document loaded from `.beads/policy.yaml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyDocument {
    /// Status-workflow policy (issue #311). Optional; when the `workflow:`
    /// section is absent the default is permissive (no enforcement), so
    /// existing repos are unaffected.
    pub workflow: Workflow,
}

/// Status-workflow policy (issue #311).
///
/// The `workflow:` namespace owns the allowed status set, the permitted
/// transitions between statuses, and the fields a transition requires. It is
/// deliberately a self-contained section. It once also owned a
/// `gates:` block; bds-04l.23 removed that engine, so a `gates:` key is now
/// reported as an unknown field rather than honoured.
///
/// ```yaml
/// workflow:
///   strict: true
///   statuses: [open, in_progress, blocked, deferred, draft, closed]
///   transitions:                       # issue #312, layer 1
///     open: [in_progress, deferred, closed]
///     in_progress: [in_review, blocked, open]
///     in_review: [closed, in_progress]
///     blocked: [open, in_progress]
///     deferred: [open]
///     # initial: [open, draft]         # statuses allowed when current is unknown
///     # any: [closed]                  # to-statuses allowed from EVERY from-status
/// ```
///
/// When the section is absent the default (`strict: false`, empty `statuses`,
/// empty `transitions`) means no enforcement, matching pre-#311 behavior
/// exactly.
///
/// Transition enforcement (issue #312, layer 1) is opt-in via a non-empty
/// `transitions:` map and is independently gated on `strict`: when
/// `strict: true` *and* `transitions` is non-empty, a status change whose
/// `from -> to` pair is not listed is rejected on `br update`. Absent or
/// empty `transitions` leaves transition behavior exactly as before.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workflow {
    /// When `true` *and* `statuses` is non-empty, a status outside the
    /// configured set is rejected on `create`/`update` and flagged by
    /// reporting. When `false`, `statuses` is advisory only (no
    /// enforcement). The same flag gates transition enforcement (see
    /// `transitions`).
    pub strict: bool,
    /// The allowed status set. Each entry is a canonical-or-custom status
    /// string (e.g. `open`, `in_progress`, `closed`, or a project-specific
    /// value). Empty disables enforcement even when `strict` is `true`.
    #[serde(default)]
    pub statuses: Vec<String>,
    /// Allowed status transitions (issue #312, layer 1). A map of
    /// `from-status -> [allowed to-statuses]`. Two reserved keys widen the
    /// rules:
    ///
    /// - `any` — its to-statuses are allowed *from every* from-status (a
    ///   wildcard source; e.g. allow `closed` from anywhere).
    /// - `initial` — the to-statuses allowed when there is no recorded
    ///   current status (e.g. a `create`, or an issue whose status the caller
    ///   could not resolve). Absent `initial` means any initial status is
    ///   accepted, since there is no `from` state to validate against.
    ///
    /// Comparison is case-insensitive, mirroring `statuses`. A no-op
    /// transition (`from == to`) is always allowed and never consults the
    /// map. Empty map disables transition enforcement entirely (backward
    /// compatible).
    #[serde(default)]
    pub transitions: std::collections::BTreeMap<String, Vec<String>>,
    /// Fields that must be supplied or satisfied for a status transition
    /// (GitHub #388). Keys may be an exact `"from -> to"` transition or a
    /// bare target status that applies to every transition entering that
    /// status. Exact and target rules compose, with duplicate fields removed.
    ///
    /// `transition_comment` is deliberately request-scoped: callers must
    /// supply a non-empty comment with the status mutation, and storage writes
    /// that comment in the same transaction as the transition. Historical
    /// comments never satisfy this requirement.
    #[serde(default)]
    pub required_fields: std::collections::BTreeMap<String, Vec<TransitionRequiredField>>,
    /// Named status groups (issue #354). Currently only `ready` is consumed —
    /// it defines which statuses `br ready` (and the scheduler) treat as
    /// actionable work. When the `status_groups:` block (or the `ready:` key
    /// inside it) is absent, the ready group defaults to `[open]`, preserving
    /// the pre-#354 behavior exactly.
    ///
    /// ```yaml
    /// workflow:
    ///   status_groups:
    ///     ready: [open, rework]
    /// ```
    #[serde(default)]
    pub status_groups: StatusGroups,
    /// Atomic status-capacity and cross-queue admission policy (GitHub #384).
    ///
    /// Capacity is deliberately independent from `workflow.strict`: projects
    /// may keep transition enumeration advisory while still enforcing a hard
    /// repository admission ceiling.  Once any capacity rule is configured,
    /// however, every referenced status must be declared in
    /// `workflow.statuses`; [`Workflow::validate_capacity`] rejects ambiguous
    /// or misspelled names before storage is opened.
    #[serde(default)]
    pub capacity: CapacityPolicy,
}

/// Repository-level workflow capacity policy (GitHub #384, phase 1).
///
/// The first phase intentionally implements the race-sensitive core: hard
/// limits for individual statuses and named groups, plus transition-scoped
/// admission rules that inspect other queues.  Hierarchy-aware counting,
/// exemptions, and actor/harness/subtree scopes are represented by later
/// phases of the tracked epic rather than silently approximated here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityPolicy {
    /// Per-status soft/hard limits, keyed by a declared workflow status.
    pub statuses: std::collections::BTreeMap<String, CapacityLimit>,
    /// Named multi-status capacity pools.
    pub groups: std::collections::BTreeMap<String, CapacityGroup>,
    /// Transition-scoped cross-queue admission guards.
    pub admission: Vec<CapacityAdmissionRule>,
}

impl CapacityPolicy {
    /// Whether any capacity behavior is configured.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.statuses.is_empty() || !self.groups.is_empty() || !self.admission.is_empty()
    }
}

/// Soft and hard occupancy thresholds for one capacity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityLimit {
    /// Advisory threshold. Phase 2 exposes successful-transition warnings.
    pub soft: Option<u32>,
    /// Enforced threshold. A prospective count equal to this value is allowed;
    /// a transition that would exceed it is rejected atomically.
    pub hard: Option<u32>,
}

/// A named pool spanning multiple workflow statuses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityGroup {
    /// Declared workflow statuses included in this pool.
    pub statuses: Vec<String>,
    /// Advisory threshold.
    pub soft: Option<u32>,
    /// Enforced threshold.
    pub hard: Option<u32>,
}

impl CapacityGroup {
    /// View the group's thresholds through the common limit type.
    #[must_use]
    pub const fn limit(&self) -> CapacityLimit {
        CapacityLimit {
            soft: self.soft,
            hard: self.hard,
        }
    }
}

/// Source/target matcher for one admission rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityTransitionMatcher {
    /// Source statuses to which the rule applies.
    pub from: Vec<String>,
    /// Target statuses to which the rule applies.
    pub to: Vec<String>,
}

/// Queues that must remain below configured thresholds for an admission rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityRequirements {
    /// Status name -> exclusive upper bound (`count < threshold`).
    pub statuses: std::collections::BTreeMap<String, u32>,
    /// Capacity group name -> exclusive upper bound (`count < threshold`).
    pub groups: std::collections::BTreeMap<String, u32>,
}

/// A transition-scoped cross-queue admission rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityAdmissionRule {
    /// Stable operator-facing name used in diagnostics and policy paths.
    pub name: String,
    /// Source and target statuses matched by this rule.
    pub transitions: CapacityTransitionMatcher,
    /// Every referenced queue must have a prospective count below its bound.
    pub require_below: CapacityRequirements,
}

/// Structured evidence for a hard workflow-capacity rejection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowCapacityViolation {
    pub issue_id: String,
    pub from_status: Option<String>,
    pub to_status: String,
    pub capacity_kind: String,
    pub capacity_name: String,
    pub scope: String,
    pub counting_mode: String,
    pub current: u32,
    pub prospective: u32,
    pub soft_limit: Option<u32>,
    pub hard_limit: u32,
    pub policy_path: String,
}

/// Structured evidence emitted after a successful transition reaches or
/// exceeds an advisory workflow-capacity threshold.
///
/// Warnings deliberately mirror the hard-rejection evidence so human and
/// machine consumers can use one stable vocabulary. `hard_limit` remains
/// optional because a project may configure an advisory ceiling without a
/// corresponding hard stop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowCapacityWarning {
    pub issue_id: String,
    pub from_status: Option<String>,
    pub to_status: String,
    pub capacity_kind: String,
    pub capacity_name: String,
    pub scope: String,
    pub counting_mode: String,
    pub current: u32,
    pub prospective: u32,
    pub soft_limit: u32,
    pub hard_limit: Option<u32>,
    pub policy_path: String,
}

impl std::fmt::Display for WorkflowCapacityWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let from = self.from_status.as_deref().unwrap_or("<initial>");
        write!(
            f,
            "transitioned {} from {} to {}; repository {} capacity '{}' has reached or exceeded its soft limit (current: {}, prospective: {}, soft: {}; policy: {}). Drain existing work before admitting more",
            self.issue_id,
            from,
            self.to_status,
            self.capacity_kind,
            self.capacity_name,
            self.current,
            self.prospective,
            self.soft_limit,
            self.policy_path,
        )
    }
}

impl std::fmt::Display for WorkflowCapacityViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let from = self.from_status.as_deref().unwrap_or("<initial>");
        if self.capacity_kind.starts_with("admission_") {
            return write!(
                f,
                "cannot transition {} from {} to {}: repository admission guard '{}' requires the observed queue to remain below {} (current: {}, prospective: {}; policy: {})",
                self.issue_id,
                from,
                self.to_status,
                self.capacity_name,
                self.hard_limit,
                self.current,
                self.prospective,
                self.policy_path,
            );
        }
        write!(
            f,
            "cannot transition {} from {} to {}: repository {} capacity '{}' would exceed its hard limit (current: {}, prospective: {}, hard: {}; policy: {})",
            self.issue_id,
            from,
            self.to_status,
            self.capacity_kind,
            self.capacity_name,
            self.current,
            self.prospective,
            self.hard_limit,
            self.policy_path,
        )
    }
}

/// Named status groups under `workflow.status_groups` (issue #354).
///
/// The `ready` group is the set of statuses `br ready` surfaces as actionable
/// work. An empty/absent `ready` list means "use the default" — see
/// [`Workflow::ready_status_group`], which substitutes `[open]` so existing
/// repos behave exactly as before this field existed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusGroups {
    /// Statuses treated as "ready to work on" by `br ready`. Empty means the
    /// default group (`[open]`).
    #[serde(default)]
    pub ready: Vec<String>,
}

/// The canonical default ready status group when none is configured.
pub const DEFAULT_READY_STATUS: &str = "open";

/// A field that workflow policy may require for a status transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionRequiredField {
    /// The issue must have non-empty acceptance criteria with no unchecked
    /// checklist items after applying the prospective update.
    AcceptanceCriteria,
    /// The transition request must carry a new, non-empty comment that is
    /// committed atomically with the status change.
    TransitionComment,
}

impl Workflow {
    /// Compute required fields for one transition. Exact `from -> to` rules
    /// and bare target-status rules compose; each field appears at most once.
    #[must_use]
    pub fn required_fields_for(
        &self,
        from: Option<&str>,
        to: &str,
    ) -> Vec<TransitionRequiredField> {
        let mut required = Vec::new();
        for (key, fields) in &self.required_fields {
            let matches = if let Some((rule_from, rule_to)) = parse_transition_key(key) {
                from.is_some_and(|actual_from| {
                    rule_from.eq_ignore_ascii_case(actual_from) && rule_to.eq_ignore_ascii_case(to)
                })
            } else {
                key.trim().eq_ignore_ascii_case(to)
            };
            if !matches {
                continue;
            }
            for field in fields {
                if !required.contains(field) {
                    required.push(*field);
                }
            }
        }
        required
    }

    /// Validate transition-required-field policy at load time.
    ///
    /// # Errors
    ///
    /// Returns a validation error for an empty key/list, malformed transition,
    /// or a status outside a configured strict status vocabulary.
    pub fn validate_required_fields(&self) -> Result<()> {
        for (raw_key, fields) in &self.required_fields {
            let key = raw_key.trim();
            if key.is_empty() {
                return Err(BeadsError::validation(
                    "workflow.required_fields",
                    "required-field rule keys must not be empty",
                ));
            }
            if fields.is_empty() {
                return Err(BeadsError::validation(
                    "workflow.required_fields",
                    format!("required-field rule '{key}' must name at least one field"),
                ));
            }

            if key.match_indices("->").count() > 1 {
                return Err(BeadsError::validation(
                    "workflow.required_fields",
                    format!(
                        "malformed required-field transition '{key}' (expected exactly one 'from -> to' separator or a bare target status)"
                    ),
                ));
            }

            if let Some((from, to)) = parse_transition_key(key) {
                if self.is_enforced() && (!self.allows(from) || !self.allows(to)) {
                    return Err(BeadsError::validation(
                        "workflow.required_fields",
                        format!(
                            "required-field transition '{key}' references a status outside the configured workflow: {}",
                            self.allowed_list()
                        ),
                    ));
                }
            } else {
                if key.contains("->") {
                    return Err(BeadsError::validation(
                        "workflow.required_fields",
                        format!(
                            "malformed required-field transition '{key}' (expected 'from -> to' or a bare target status)"
                        ),
                    ));
                }
                if self.is_enforced() && !self.allows(key) {
                    return Err(BeadsError::validation(
                        "workflow.required_fields",
                        format!(
                            "required-field target '{key}' is outside the configured workflow: {}",
                            self.allowed_list()
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Evaluate policy-required fields against the prospective issue state and
/// request-scoped transition comment.
#[must_use]
pub fn evaluate_transition_required_fields(
    workflow: &Workflow,
    issue_id: &str,
    from: Option<&str>,
    to: &str,
    acceptance_criteria: Option<&str>,
    transition_comment: Option<&str>,
) -> Vec<PolicyViolation> {
    let required = workflow.required_fields_for(from, to);
    let mut violations = Vec::new();

    for field in required {
        match field {
            TransitionRequiredField::AcceptanceCriteria => {
                let criteria = acceptance_criteria.map(str::trim).unwrap_or_default();
                if criteria.is_empty() {
                    violations.push(PolicyViolation {
                        gate: "transition_acceptance_criteria_missing".to_string(),
                        message: format!(
                            "transition '{} -> {to}' requires non-empty acceptance criteria for {issue_id}",
                            from.unwrap_or("initial")
                        ),
                        detail: Some(serde_json::json!({
                            "issue_id": issue_id,
                            "from": from,
                            "to": to,
                            "required_field": "acceptance_criteria",
                            "reason": "missing",
                        })),
                    });
                    continue;
                }

                // This value comes from the dedicated acceptance_criteria
                // column, so every checklist item in it is a criterion even
                // when operators organize the field with custom headings.
                let unchecked = find_unchecked_checklist_items(criteria);
                if !unchecked.is_empty() {
                    violations.push(PolicyViolation {
                        gate: "transition_acceptance_criteria_unchecked".to_string(),
                        message: format!(
                            "transition '{} -> {to}' requires all acceptance criteria to be satisfied for {issue_id}; {} unchecked item(s) remain",
                            from.unwrap_or("initial"),
                            unchecked.len()
                        ),
                        detail: Some(serde_json::json!({
                            "issue_id": issue_id,
                            "from": from,
                            "to": to,
                            "required_field": "acceptance_criteria",
                            "reason": "unchecked",
                            "unchecked": unchecked,
                        })),
                    });
                }
            }
            TransitionRequiredField::TransitionComment => {
                if transition_comment.map(str::trim).is_none_or(str::is_empty) {
                    violations.push(PolicyViolation {
                        gate: "transition_comment_missing".to_string(),
                        message: format!(
                            "transition '{} -> {to}' requires a new non-empty transition comment for {issue_id}",
                            from.unwrap_or("initial")
                        ),
                        detail: Some(serde_json::json!({
                            "issue_id": issue_id,
                            "from": from,
                            "to": to,
                            "required_field": "transition_comment",
                            "reason": "missing",
                        })),
                    });
                }
            }
        }
    }
    violations
}

/// Parse a `"from -> to"` transition key into its two sides, trimming
/// whitespace. Returns `None` when the key has no `->` separator or either
/// side is empty.
fn parse_transition_key(key: &str) -> Option<(&str, &str)> {
    let (from, to) = key.split_once("->")?;
    let from = from.trim();
    let to = to.trim();
    if from.is_empty() || to.is_empty() {
        return None;
    }
    Some((from, to))
}

/// Reserved `transitions` key whose to-statuses are allowed from every
/// from-status (wildcard source).
pub const TRANSITION_ANY_FROM: &str = "any";
/// Reserved `transitions` key whose to-statuses are allowed when there is no
/// recorded current status (e.g. a create, or an unresolved current status).
pub const TRANSITION_INITIAL: &str = "initial";

impl Workflow {
    /// True when strict enforcement is configured: `strict` is on *and* at
    /// least one allowed status is listed. Enforcement short-circuits on
    /// `false`.
    #[must_use]
    pub fn is_enforced(&self) -> bool {
        self.strict && !self.statuses.is_empty()
    }

    /// Resolve the configured ready status group (issue #354), substituting the
    /// default `[open]` when nothing is configured. Returns lowercased,
    /// source-order, de-duplicated status names so the query layer can build a
    /// stable `status IN (...)` clause. The default is returned whenever
    /// `workflow.status_groups.ready` is empty, which is the unconfigured case —
    /// preserving pre-#354 behavior exactly.
    #[must_use]
    pub fn ready_status_group(&self) -> Vec<String> {
        let configured = &self.status_groups.ready;
        let source: Vec<String> = if configured.is_empty() {
            vec![DEFAULT_READY_STATUS.to_string()]
        } else {
            configured.clone()
        };
        let mut out: Vec<String> = Vec::with_capacity(source.len());
        for status in source {
            let normalized = status.trim().to_lowercase();
            if normalized.is_empty() {
                continue;
            }
            if !out.contains(&normalized) {
                out.push(normalized);
            }
        }
        if out.is_empty() {
            out.push(DEFAULT_READY_STATUS.to_string());
        }
        out
    }

    /// Validate the configured ready status group (issue #354). When
    /// `workflow.strict` is set *and* `workflow.statuses` is non-empty, every
    /// member of the configured ready group must appear in the allowed status
    /// set; an out-of-vocabulary member is rejected with a clear error. When
    /// not strict (or no `statuses` configured, or no `ready` group
    /// configured), the group is accepted as-is.
    ///
    /// # Errors
    ///
    /// Returns a validation error when strict enforcement is configured and a
    /// member of the ready group is not in `workflow.statuses`.
    pub fn validate_ready_status_group(&self) -> Result<()> {
        // Only validate an explicitly-configured group; the implicit `[open]`
        // default is never rejected.
        if self.status_groups.ready.is_empty() {
            return Ok(());
        }
        if !self.is_enforced() {
            return Ok(());
        }
        let unknown: Vec<String> = self
            .ready_status_group()
            .into_iter()
            .filter(|status| !self.allows(status))
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        Err(BeadsError::validation(
            "workflow.status_groups.ready",
            format!(
                "ready status group contains status(es) not permitted by the project workflow \
                 policy (.beads/policy.yaml workflow.strict): {}. Allowed statuses: {}.",
                unknown.join(", "),
                self.allowed_list()
            ),
        ))
    }

    /// Validate every configured repository-capacity reference and threshold.
    ///
    /// Capacity misspellings must fail closed: unlike advisory workflow
    /// extensions, a silently ignored queue name would make an operator believe
    /// admission is bounded when it is not.  Validation is case-insensitive in
    /// the same way as status parsing and transition matching.
    ///
    /// # Errors
    ///
    /// Returns a validation error when a referenced status/group is unknown,
    /// a name/list is empty, thresholds are zero or inverted, or admission rule
    /// names collide case-insensitively.
    pub fn validate_capacity(&self) -> Result<()> {
        if !self.capacity.is_active() {
            return Ok(());
        }
        if self.statuses.is_empty() {
            return Err(capacity_validation_error(
                "capacity requires workflow.statuses to declare every status it references",
            ));
        }

        let declared_statuses = declared_capacity_statuses(&self.statuses)?;
        validate_status_capacities(&self.capacity.statuses, &declared_statuses)?;
        let group_names = validate_capacity_groups(&self.capacity.groups, &declared_statuses)?;
        validate_capacity_admission_rules(
            &self.capacity.admission,
            &declared_statuses,
            &group_names,
        )?;
        Ok(())
    }

    /// True when `status` (case-insensitively) is in the configured set.
    /// Comparison mirrors [`crate::model::Status`] parsing: canonical names
    /// are matched case-insensitively, so a config entry of `In_Progress`
    /// still admits the canonical `in_progress`.
    #[must_use]
    pub fn allows(&self, status: &str) -> bool {
        let target = status.to_lowercase();
        self.statuses
            .iter()
            .any(|allowed| allowed.to_lowercase() == target)
    }

    /// Comma-separated, source-order list of the allowed statuses for error
    /// messages. Empty string when nothing is configured.
    #[must_use]
    pub fn allowed_list(&self) -> String {
        self.statuses.join(", ")
    }

    /// Validate a target status against the workflow policy. Returns `Ok(())`
    /// when enforcement is off, the status set is empty, or the status is in
    /// the set. Returns a [`BeadsError::Validation`] naming the allowed values
    /// otherwise.
    ///
    /// This is the *only* surface that rejects a status, and it is deliberately
    /// project-specific: [`Status::Custom`] exists because accepting an
    /// arbitrary status is a feature, so the only thing that can make one wrong
    /// is a `policy.yaml` that enumerates the permitted set. A generic
    /// "invalid status" error naming a built-in vocabulary once existed
    /// alongside this and was removed unconstructed (bds-npo): in a strict
    /// workspace its list would have been the wrong list, and in an ordinary
    /// one there is nothing to reject.
    ///
    /// # Errors
    ///
    /// Returns a validation error when strict enforcement is configured and
    /// `status` is not in the allowed set.
    pub fn validate_status(&self, status: &str) -> Result<()> {
        if !self.is_enforced() || self.allows(status) {
            return Ok(());
        }
        Err(BeadsError::validation(
            "status",
            format!(
                "status '{status}' is not permitted by the project workflow policy \
                 (.beads/policy.yaml workflow.strict). Allowed statuses: {}.",
                self.allowed_list()
            ),
        ))
    }

    /// True when transition enforcement is configured: `strict` is on *and*
    /// at least one `from -> [to...]` rule is listed. Enforcement
    /// short-circuits on `false`.
    #[must_use]
    pub fn transitions_enforced(&self) -> bool {
        self.strict && !self.transitions.is_empty()
    }

    /// Case-insensitive lookup of the to-statuses listed for `from`. Returns
    /// `None` when `from` has no entry in the map.
    fn transitions_from(&self, from: &str) -> Option<&Vec<String>> {
        let target = from.to_lowercase();
        self.transitions
            .iter()
            .find(|(key, _)| key.to_lowercase() == target)
            .map(|(_, tos)| tos)
    }

    /// Collect every to-status reachable from `from`, merging the explicit
    /// `from` entry with the wildcard `any` entry. Returns the source-order,
    /// de-duplicated list used for error messages and membership checks.
    #[must_use]
    pub fn allowed_targets_from(&self, from: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let push_unique = |list: &Vec<String>, out: &mut Vec<String>| {
            for value in list {
                if !out.iter().any(|seen| seen.eq_ignore_ascii_case(value)) {
                    out.push(value.clone());
                }
            }
        };
        if let Some(explicit) = self.transitions_from(from) {
            push_unique(explicit, &mut out);
        }
        if let Some(wildcard) = self.transitions_from(TRANSITION_ANY_FROM) {
            push_unique(wildcard, &mut out);
        }
        out
    }

    /// True when moving `from -> to` is permitted by the configured
    /// transition rules. A no-op (`from == to`, case-insensitive) is always
    /// allowed. Otherwise the target must appear either under the explicit
    /// `from` key or under the wildcard `any` key.
    #[must_use]
    pub fn allows_transition(&self, from: &str, to: &str) -> bool {
        if from.eq_ignore_ascii_case(to) {
            return true;
        }
        self.allowed_targets_from(from)
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(to))
    }

    /// True when `to` is permitted as an initial status (no recorded `from`).
    /// When no `initial` key is configured, any initial status is accepted —
    /// there is no prior state to validate against.
    #[must_use]
    pub fn allows_initial(&self, to: &str) -> bool {
        match self.transitions_from(TRANSITION_INITIAL) {
            None => true,
            Some(allowed) => allowed.iter().any(|s| s.eq_ignore_ascii_case(to)),
        }
    }

    /// Validate a status *change* against the workflow transition rules.
    ///
    /// `from` is the issue's current status, or `None` when there is no
    /// recorded current status (a create, or an unresolved current status —
    /// validated against the reserved `initial` key).
    ///
    /// Returns `Ok(())` when transition enforcement is off, the move is a
    /// no-op, or the move is permitted. Returns a [`BeadsError::Validation`]
    /// naming the current status, the attempted status, and the valid next
    /// statuses otherwise — mirroring the `validate_status` error style.
    ///
    /// # Errors
    ///
    /// Returns a validation error when transition enforcement is configured
    /// and the `from -> to` move is not in the allowed set.
    pub fn validate_transition(&self, from: Option<&str>, to: &str) -> Result<()> {
        if !self.transitions_enforced() {
            return Ok(());
        }

        match from {
            None => {
                if self.allows_initial(to) {
                    return Ok(());
                }
                let allowed = self
                    .transitions_from(TRANSITION_INITIAL)
                    .map(|tos| tos.join(", "))
                    .unwrap_or_default();
                Err(BeadsError::validation(
                    "status",
                    format!(
                        "initial status '{to}' is not permitted by the project workflow policy \
                         (.beads/policy.yaml workflow.transitions, key 'initial'). \
                         Allowed initial statuses: {allowed}."
                    ),
                ))
            }
            Some(from) => {
                if self.allows_transition(from, to) {
                    return Ok(());
                }
                let allowed = self.allowed_targets_from(from);
                let allowed_list = if allowed.is_empty() {
                    "(none)".to_string()
                } else {
                    allowed.join(", ")
                };
                Err(BeadsError::validation(
                    "status",
                    format!(
                        "transition '{from}' -> '{to}' is not permitted by the project workflow \
                         policy (.beads/policy.yaml workflow.transitions). \
                         Valid next statuses from '{from}': {allowed_list}."
                    ),
                ))
            }
        }
    }
}

fn capacity_validation_error(reason: impl Into<String>) -> BeadsError {
    BeadsError::validation("workflow.capacity", reason)
}

fn declared_capacity_statuses(statuses: &[String]) -> Result<std::collections::HashSet<String>> {
    let declared: std::collections::HashSet<String> = statuses
        .iter()
        .map(|status| status.trim().to_lowercase())
        .filter(|status| !status.is_empty())
        .collect();
    if declared.len() != statuses.len() {
        return Err(capacity_validation_error(
            "workflow.statuses must be non-empty and unique (case-insensitive) when capacity is configured",
        ));
    }
    Ok(declared)
}

fn validate_status_capacities(
    capacities: &std::collections::BTreeMap<String, CapacityLimit>,
    declared_statuses: &std::collections::HashSet<String>,
) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for (status, limit) in capacities {
        validate_capacity_name(status, "status")?;
        validate_declared_capacity_status(status, declared_statuses)?;
        if !names.insert(status.trim().to_lowercase()) {
            return Err(capacity_validation_error(format!(
                "capacity status '{status}' is duplicated case-insensitively"
            )));
        }
        validate_capacity_limit(limit, &format!("statuses.{status}"), true)?;
    }
    Ok(())
}

fn validate_capacity_groups(
    groups: &std::collections::BTreeMap<String, CapacityGroup>,
    declared_statuses: &std::collections::HashSet<String>,
) -> Result<std::collections::HashSet<String>> {
    let mut names = std::collections::HashSet::new();
    for (name, group) in groups {
        validate_capacity_name(name, "group")?;
        if !names.insert(name.trim().to_lowercase()) {
            return Err(capacity_validation_error(format!(
                "capacity group name '{name}' is duplicated case-insensitively"
            )));
        }
        if group.statuses.is_empty() {
            return Err(capacity_validation_error(format!(
                "capacity group '{name}' must include at least one status"
            )));
        }
        let mut members = std::collections::HashSet::new();
        for status in &group.statuses {
            validate_declared_capacity_status(status, declared_statuses)?;
            if !members.insert(status.trim().to_lowercase()) {
                return Err(capacity_validation_error(format!(
                    "capacity group '{name}' repeats status '{status}'"
                )));
            }
        }
        validate_capacity_limit(&group.limit(), &format!("groups.{name}"), false)?;
    }
    Ok(names)
}

fn validate_capacity_admission_rules(
    rules: &[CapacityAdmissionRule],
    declared_statuses: &std::collections::HashSet<String>,
    group_names: &std::collections::HashSet<String>,
) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for rule in rules {
        validate_capacity_name(&rule.name, "admission rule")?;
        if !names.insert(rule.name.trim().to_lowercase()) {
            return Err(capacity_validation_error(format!(
                "capacity admission rule name '{}' is duplicated case-insensitively",
                rule.name
            )));
        }
        validate_capacity_admission_rule(rule, declared_statuses, group_names)?;
    }
    Ok(())
}

fn validate_capacity_admission_rule(
    rule: &CapacityAdmissionRule,
    declared_statuses: &std::collections::HashSet<String>,
    group_names: &std::collections::HashSet<String>,
) -> Result<()> {
    if rule.transitions.from.is_empty() || rule.transitions.to.is_empty() {
        return Err(capacity_validation_error(format!(
            "capacity admission rule '{}' requires non-empty transitions.from and transitions.to lists",
            rule.name
        )));
    }
    for status in rule
        .transitions
        .from
        .iter()
        .chain(&rule.transitions.to)
        .chain(rule.require_below.statuses.keys())
    {
        validate_declared_capacity_status(status, declared_statuses)?;
    }
    if rule.require_below.statuses.is_empty() && rule.require_below.groups.is_empty() {
        return Err(capacity_validation_error(format!(
            "capacity admission rule '{}' must inspect at least one status or group",
            rule.name
        )));
    }
    validate_capacity_admission_statuses(rule)?;
    validate_capacity_admission_groups(rule, group_names)
}

fn validate_capacity_admission_statuses(rule: &CapacityAdmissionRule) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for (status, threshold) in &rule.require_below.statuses {
        if !names.insert(status.trim().to_lowercase()) {
            return Err(capacity_validation_error(format!(
                "capacity admission rule '{}' repeats required status '{status}' case-insensitively",
                rule.name
            )));
        }
        if *threshold == 0 {
            return Err(capacity_validation_error(format!(
                "capacity admission rule '{}' has zero threshold for status '{status}'",
                rule.name
            )));
        }
    }
    Ok(())
}

fn validate_capacity_admission_groups(
    rule: &CapacityAdmissionRule,
    group_names: &std::collections::HashSet<String>,
) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for (group, threshold) in &rule.require_below.groups {
        let canonical = group.trim().to_lowercase();
        if !names.insert(canonical.clone()) {
            return Err(capacity_validation_error(format!(
                "capacity admission rule '{}' repeats required group '{group}' case-insensitively",
                rule.name
            )));
        }
        if !group_names.contains(&canonical) {
            return Err(capacity_validation_error(format!(
                "capacity admission rule '{}' references undeclared group '{group}'",
                rule.name
            )));
        }
        if *threshold == 0 {
            return Err(capacity_validation_error(format!(
                "capacity admission rule '{}' has zero threshold for group '{group}'",
                rule.name
            )));
        }
    }
    Ok(())
}

fn validate_capacity_name(name: &str, kind: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(capacity_validation_error(format!(
            "capacity {kind} name cannot be empty"
        )));
    }
    Ok(())
}

fn validate_declared_capacity_status(
    status: &str,
    declared_statuses: &std::collections::HashSet<String>,
) -> Result<()> {
    let canonical = status.trim().to_lowercase();
    if canonical.is_empty() || !declared_statuses.contains(&canonical) {
        return Err(capacity_validation_error(format!(
            "capacity references undeclared workflow status '{status}'"
        )));
    }
    Ok(())
}

fn validate_capacity_limit(
    limit: &CapacityLimit,
    path: &str,
    require_threshold: bool,
) -> Result<()> {
    if require_threshold && limit.soft.is_none() && limit.hard.is_none() {
        return Err(capacity_validation_error(format!(
            "capacity {path} must configure soft and/or hard"
        )));
    }
    if limit.soft == Some(0) || limit.hard == Some(0) {
        return Err(capacity_validation_error(format!(
            "capacity {path} thresholds must be greater than zero"
        )));
    }
    if let (Some(soft), Some(hard)) = (limit.soft, limit.hard)
        && soft > hard
    {
        return Err(capacity_validation_error(format!(
            "capacity {path} soft limit ({soft}) cannot exceed hard limit ({hard})"
        )));
    }
    Ok(())
}

/// A single policy violation discovered while evaluating gates. Shared by
/// the workflow required-fields gate below (`evaluate_transition_required_fields`)
/// and the storage-layer `enforce_workflow_transition_batch_in_tx` check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyViolation {
    /// Stable machine identifier for the gate (e.g. `transition_comment_missing`).
    pub gate: String,
    /// Human-readable explanation. Always present.
    pub message: String,
    /// Optional structured detail (counts, items, expected vs actual).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// Locate every unchecked markdown checklist item outside fenced code. Used
/// by [`evaluate_transition_required_fields`] to check the dedicated
/// `acceptance_criteria` field, where the entire value is criteria by
/// definition (no section-heading scanning needed).
fn find_unchecked_checklist_items(body: &str) -> Vec<String> {
    let mut fence_marker = None;
    let mut out = Vec::new();
    for line in body.lines() {
        if update_code_fence(line, &mut fence_marker) || fence_marker.is_some() {
            continue;
        }
        if let Some(item) = parse_unchecked_box(line.trim_start()) {
            out.push(item);
        }
    }
    out
}

fn update_code_fence(line: &str, fence_marker: &mut Option<char>) -> bool {
    let trimmed = line.trim_start();
    let Some(marker @ ('`' | '~')) = trimmed.chars().next() else {
        return false;
    };
    let marker_len = trimmed.chars().take_while(|ch| *ch == marker).count();
    if marker_len < 3 {
        return false;
    }

    if fence_marker.is_some_and(|open_marker| open_marker == marker) {
        *fence_marker = None;
    } else if fence_marker.is_none() {
        *fence_marker = Some(marker);
    }
    true
}

/// Parse a single line for an unchecked checkbox. Accepts `- [ ]`, `* [ ]`,
/// `+ [ ]`, optional leading whitespace, and any space (or lack thereof)
/// between the marker and the bracket.
fn parse_unchecked_box(line: &str) -> Option<String> {
    let mut chars = line.chars().peekable();
    let bullet = chars.next()?;
    if !matches!(bullet, '-' | '*' | '+') {
        return None;
    }
    // Skip whitespace between bullet and `[`.
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
    if chars.next()? != '[' {
        return None;
    }
    // Inner char must be whitespace/empty for "unchecked".
    let inner = chars.next()?;
    let inner_is_unchecked = inner.is_whitespace() || inner == ' ';
    if !inner_is_unchecked {
        return None;
    }
    if chars.next()? != ']' {
        return None;
    }
    let rest: String = chars.collect();
    let rest = rest.trim().to_string();
    Some(rest)
}

/// Load the policy document from `.beads/policy.yaml`. Returns the default
/// (no enforcement) when the file does not exist. Returns an error only if
/// the file exists but cannot be read or parsed — never silently downgrades
/// a broken config to "permissive."
///
/// # Unknown fields (beads#302)
///
/// The policy struct tree deliberately accepts unknown fields rather than
/// hard-failing the parse: a typo or project-local experimental key used to
/// take down every command that consults the policy for every operator on
/// the project, with no recovery path. We warn instead of erroring; the
/// trade is loss of typo-at-parse-time detection, but the cost (a full
/// project outage from one typo) was much worse.
///
/// Unknown fields are surfaced exactly once per load via
/// [`detect_unknown_policy_fields`] and emitted as a `tracing::warn!`
/// event. The warning lists every unknown path with a dotted scope
/// (e.g. `workflow.status_groups.readi`) so operators can find typos
/// without re-reading the file.
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_for_beads_dir(beads_dir: &Path) -> Result<PolicyDocument> {
    let path = beads_dir.join(POLICY_FILE_NAME);
    if !path.exists() {
        return Ok(PolicyDocument::default());
    }
    let raw = fs::read_to_string(&path).map_err(BeadsError::from)?;
    let document: PolicyDocument = serde_yml::from_str(&raw).map_err(|err| {
        BeadsError::Config(format!("failed to parse {}: {}", path.display(), err))
    })?;
    document.workflow.validate_capacity()?;
    document.workflow.validate_required_fields()?;

    // Re-parse the raw YAML into a free-form value tree so we can diff it
    // against the typed schema and surface unknown fields without failing
    // the load. Failure to re-parse as a `Value` is impossible here (the
    // typed parse above already succeeded), but if it ever did, we'd
    // rather skip the warning than spurious-error the load — that's the
    // whole point of #302.
    if let Ok(raw_value) = serde_yml::from_str::<serde_yml::Value>(&raw) {
        let unknown = detect_unknown_policy_fields(&raw_value);
        if !unknown.is_empty() {
            tracing::warn!(
                policy_path = %path.display(),
                unknown_fields = ?unknown,
                "policy.yaml contains {} unknown field(s); \
                 these were ignored (beads#302). Check for typos: {}",
                unknown.len(),
                unknown.join(", "),
            );
        }
    }

    Ok(document)
}

/// Walk a parsed `policy.yaml` value tree and collect dotted paths to any
/// keys not recognized by the typed policy schema.
///
/// We use a hard-coded recursive walk (rather than `serde(flatten)` with
/// `extras` fields on every struct) so the typed public API stays simple
/// and the extras maps don't leak into round-trip serialization. Adding
/// a new canonical field becomes a one-line update in [`PolicyNode`].
///
/// Returns a sorted, de-duplicated list of dotted paths
/// (e.g. `["workflow.status_groups.readi"]`). Empty when
/// the document only uses canonical fields.
#[must_use]
pub fn detect_unknown_policy_fields(root: &serde_yml::Value) -> Vec<String> {
    let mut unknown = Vec::new();
    walk_policy_node(root, PolicyNode::Document, "", &mut unknown);
    unknown.sort();
    unknown.dedup();
    unknown
}

/// Schema-tree node used by [`detect_unknown_policy_fields`] to recognise
/// which keys are canonical at each depth of `policy.yaml`. Leaves (`Scalar`)
/// terminate the walk; mappings descend per the `key -> child-node` table.
#[derive(Clone, Copy, Debug)]
enum PolicyNode {
    /// Top-level `policy.yaml` mapping.
    Document,
    /// `workflow:` block (issue #311).
    Workflow,
    /// `workflow.status_groups:` block (issue #354).
    StatusGroups,
    /// Terminal scalar / list — descent stops here.
    Scalar,
}

impl PolicyNode {
    /// Canonical keys at this depth, plus the child node each key descends
    /// into. Keys absent from this table are reported as unknown.
    const fn child_table(self) -> &'static [(&'static str, Self)] {
        match self {
            Self::Document => &[("workflow", Self::Workflow)],
            Self::Workflow => &[
                ("strict", Self::Scalar),
                ("statuses", Self::Scalar),
                ("transitions", Self::Scalar),
                // `gates` is deliberately absent: bds-04l.23 removed the
                // workflow-gate engine, so a `gates:` block in an existing
                // policy.yaml is now an unknown field. That is the intended
                // outcome -- it surfaces as a `tracing::warn!` telling the
                // operator the key no longer does anything, rather than
                // silently accepting a key with no effect.
                ("required_fields", Self::Scalar),
                ("status_groups", Self::StatusGroups),
                // Capacity owns a strict typed schema with
                // `deny_unknown_fields`; its nested keys are validated by
                // serde plus `Workflow::validate_capacity` rather than the
                // legacy permissive unknown-field walker.
                ("capacity", Self::Scalar),
            ],
            Self::StatusGroups => &[("ready", Self::Scalar)],
            Self::Scalar => &[],
        }
    }
}

fn walk_policy_node(
    value: &serde_yml::Value,
    node: PolicyNode,
    scope: &str,
    out: &mut Vec<String>,
) {
    if matches!(node, PolicyNode::Scalar) {
        return;
    }
    let Some(map) = value.as_mapping() else {
        return;
    };
    let table = node.child_table();
    for (key, sub) in map {
        let Some(key_str) = key.as_str() else {
            continue;
        };
        let path = if scope.is_empty() {
            key_str.to_string()
        } else {
            format!("{scope}.{key_str}")
        };
        match table.iter().find(|(k, _)| *k == key_str) {
            Some((_, child)) => walk_policy_node(sub, *child, &path, out),
            None => out.push(path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_status_group_defaults_to_open() {
        // #354: unconfigured workflow → ready group is [open].
        let workflow = Workflow::default();
        assert_eq!(workflow.ready_status_group(), vec!["open".to_string()]);
    }

    #[test]
    fn ready_status_group_uses_configured_values_normalized() {
        // #354: configured group is normalized (lowercased, trimmed, de-duped,
        // source order preserved).
        let mut workflow = Workflow::default();
        workflow.status_groups.ready = vec![
            "Open".to_string(),
            "  rework ".to_string(),
            "open".to_string(),
        ];
        assert_eq!(
            workflow.ready_status_group(),
            vec!["open".to_string(), "rework".to_string()]
        );
    }

    #[test]
    fn transition_required_fields_compose_exact_and_target_rules() {
        // Required fields are opt-in by the presence of the map itself. They
        // intentionally do not depend on strict status-vocabulary enforcement.
        let mut workflow = Workflow::default();
        workflow.required_fields.insert(
            "in_review".to_string(),
            vec![TransitionRequiredField::TransitionComment],
        );
        workflow.required_fields.insert(
            "in_progress -> in_review".to_string(),
            vec![
                TransitionRequiredField::AcceptanceCriteria,
                TransitionRequiredField::TransitionComment,
            ],
        );

        assert_eq!(
            workflow.required_fields_for(Some("IN_PROGRESS"), "In_Review"),
            vec![
                TransitionRequiredField::AcceptanceCriteria,
                TransitionRequiredField::TransitionComment,
            ]
        );
        assert_eq!(
            workflow.required_fields_for(Some("open"), "in_review"),
            vec![TransitionRequiredField::TransitionComment]
        );
        assert!(
            workflow
                .required_fields_for(Some("open"), "closed")
                .is_empty()
        );
    }

    #[test]
    fn transition_required_fields_report_missing_and_unchecked_values() {
        let mut workflow = Workflow::default();
        workflow.required_fields.insert(
            "in_progress -> in_review".to_string(),
            vec![
                TransitionRequiredField::AcceptanceCriteria,
                TransitionRequiredField::TransitionComment,
            ],
        );

        let missing = evaluate_transition_required_fields(
            &workflow,
            "bd-1",
            Some("in_progress"),
            "in_review",
            None,
            Some("  "),
        );
        assert_eq!(missing.len(), 2);
        assert_eq!(missing[0].gate, "transition_acceptance_criteria_missing");
        assert_eq!(missing[1].gate, "transition_comment_missing");

        let unchecked = evaluate_transition_required_fields(
            &workflow,
            "bd-1",
            Some("in_progress"),
            "in_review",
            Some("## Phase one\n- [x] Built\n## Phase two\n- [ ] Verified\n"),
            Some("Ready for a fresh review"),
        );
        assert_eq!(unchecked.len(), 1);
        assert_eq!(
            unchecked[0].gate,
            "transition_acceptance_criteria_unchecked"
        );
        assert_eq!(
            unchecked[0].detail.as_ref().unwrap()["unchecked"][0],
            "Verified"
        );

        assert!(
            evaluate_transition_required_fields(
                &workflow,
                "bd-1",
                Some("in_progress"),
                "in_review",
                Some("- [x] Built\n- [X] Verified\n"),
                Some("Ready for a fresh review"),
            )
            .is_empty()
        );
    }

    #[test]
    fn transition_required_fields_reject_malformed_multi_arrow_rule() {
        let mut workflow = Workflow {
            strict: true,
            ..Default::default()
        };
        workflow.required_fields.insert(
            "open -> review -> closed".to_string(),
            vec![TransitionRequiredField::TransitionComment],
        );
        let error = workflow.validate_required_fields().unwrap_err();
        assert!(error.to_string().contains("exactly one"));
    }

    #[test]
    fn validate_ready_group_accepts_subset_when_strict() {
        // #354: strict mode, group is a subset of statuses → ok.
        let mut workflow = Workflow {
            strict: true,
            statuses: vec![
                "open".to_string(),
                "rework".to_string(),
                "closed".to_string(),
            ],
            ..Workflow::default()
        };
        workflow.status_groups.ready = vec!["open".to_string(), "rework".to_string()];
        assert!(workflow.validate_ready_status_group().is_ok());
    }

    #[test]
    fn validate_ready_group_rejects_out_of_vocab_when_strict() {
        // #354: strict mode, group has a status outside statuses → rejected with
        // a clear error naming the offending value.
        let mut workflow = Workflow {
            strict: true,
            statuses: vec!["open".to_string(), "closed".to_string()],
            ..Workflow::default()
        };
        workflow.status_groups.ready = vec!["open".to_string(), "rework".to_string()];
        let err = workflow
            .validate_ready_status_group()
            .expect_err("out-of-vocab group must be rejected under strict");
        let msg = err.to_string();
        assert!(
            msg.contains("rework"),
            "error should name the bad status: {msg}"
        );
    }

    #[test]
    fn validate_ready_group_accepts_anything_when_not_strict() {
        // #354: without strict (or without a statuses vocabulary), the group is
        // accepted as-is.
        let mut workflow = Workflow {
            strict: false,
            statuses: vec!["open".to_string()],
            ..Workflow::default()
        };
        workflow.status_groups.ready = vec!["open".to_string(), "rework".to_string()];
        assert!(workflow.validate_ready_status_group().is_ok());

        // strict but empty statuses → enforcement off → accepted.
        workflow.strict = true;
        workflow.statuses = vec![];
        assert!(workflow.validate_ready_status_group().is_ok());
    }

    #[test]
    fn status_groups_parse_from_yaml_and_not_flagged_unknown() {
        // #354: the new keys deserialize and are recognized by the
        // unknown-field detector.
        let raw = "workflow:\n  status_groups:\n    ready: [open, rework]\n";
        let doc: PolicyDocument = serde_yml::from_str(raw).unwrap();
        assert_eq!(
            doc.workflow.status_groups.ready,
            vec!["open".to_string(), "rework".to_string()]
        );
        let value: serde_yml::Value = serde_yml::from_str(raw).unwrap();
        let unknown = detect_unknown_policy_fields(&value);
        assert!(
            unknown.is_empty(),
            "status_groups.ready must not be flagged unknown: {unknown:?}"
        );
    }

    #[test]
    fn loader_returns_default_when_file_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = load_for_beads_dir(dir.path()).expect("load");
        assert_eq!(policy, PolicyDocument::default());
        assert!(!policy.workflow.is_enforced());
    }

    /// beads#302: unknown fields used to hard-fail and take down every
    /// command that consults the policy, project-wide. They are now
    /// tolerated — the parse succeeds and the unknown keys surface via
    /// [`detect_unknown_policy_fields`].
    #[test]
    fn loader_tolerates_unknown_top_level_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml = "unknown_key: 1\nworkflow:\n  strict: true\n";
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("load must succeed");
        assert!(policy.workflow.strict, "known fields must still parse");

        let raw: serde_yml::Value = serde_yml::from_str(yaml).unwrap();
        let unknown = detect_unknown_policy_fields(&raw);
        assert_eq!(unknown, vec!["unknown_key".to_string()]);
    }

    #[test]
    fn loader_accepts_empty_document() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(POLICY_FILE_NAME), "{}\n").unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("load");
        assert_eq!(policy, PolicyDocument::default());
    }

    #[test]
    fn parse_unchecked_box_recognises_variants() {
        assert_eq!(
            parse_unchecked_box("- [ ] todo item").as_deref(),
            Some("todo item")
        );
        assert_eq!(
            parse_unchecked_box("* [ ] starred").as_deref(),
            Some("starred")
        );
        assert_eq!(parse_unchecked_box("+ [ ] plus").as_deref(), Some("plus"));
        assert!(parse_unchecked_box("- [x] checked").is_none());
        assert!(parse_unchecked_box("- [X] checked").is_none());
        assert!(parse_unchecked_box("plain text").is_none());
        assert!(parse_unchecked_box("- not a box").is_none());
    }

    /// Drift guard for beads#302: `PolicyNode::child_table()` is a
    /// hand-maintained mirror of the typed policy struct fields. If a
    /// new field is added to one of the structs without also being added
    /// to the table, `detect_unknown_policy_fields` will fire a
    /// **false-positive** "unknown field" warning on every canonical
    /// document containing that field. The owner explicitly acknowledged
    /// this sync hazard in the commit message — this test makes the drift
    /// impossible to ship: it serialises a `Default` instance of every
    /// policy struct and asserts the produced key set is a subset of
    /// the corresponding `PolicyNode`'s `child_table()` keys.
    ///
    /// We assert "subset" rather than "equality" because table keys may
    /// intentionally list `Option<T>` fields that serialise to nothing in
    /// the default form (none today, but future-proofing).
    #[test]
    fn policy_node_child_table_covers_every_typed_struct_field() {
        fn field_names_of<T: serde::Serialize + Default>() -> Vec<String> {
            let value =
                serde_yml::to_value(T::default()).expect("default struct must serialise to value");
            let mapping = value
                .as_mapping()
                .expect("default struct must serialise as a mapping");
            mapping
                .iter()
                .filter_map(|(k, _)| k.as_str().map(String::from))
                .collect()
        }

        fn assert_table_covers(node: PolicyNode, struct_fields: &[String], struct_name: &str) {
            let table_keys: std::collections::HashSet<&'static str> =
                node.child_table().iter().map(|(k, _)| *k).collect();
            for field in struct_fields {
                assert!(
                    table_keys.contains(field.as_str()),
                    "PolicyNode::{node:?}::child_table() is missing key `{field}` declared on \
                     struct `{struct_name}`. `detect_unknown_policy_fields` would emit a \
                     FALSE-POSITIVE 'unknown field' warning on every canonical policy.yaml that \
                     uses this field. Add the entry to `child_table()` (see beads#302).",
                );
            }
        }

        assert_table_covers(
            PolicyNode::Document,
            &field_names_of::<PolicyDocument>(),
            "PolicyDocument",
        );
        assert_table_covers(
            PolicyNode::Workflow,
            &field_names_of::<Workflow>(),
            "Workflow",
        );
    }

    /// Inverse drift guard: every key listed in `PolicyNode::child_table()`
    /// must correspond to an actual field on the typed struct. Otherwise a
    /// stale entry would silently SUPPRESS the unknown-field warning for a
    /// field that no longer exists (false negative: typo in YAML matches a
    /// dead table entry → no warning, but the field is also not honoured by
    /// the typed parse).
    ///
    /// `regex: Option<String>` is in the default serialised mapping as a
    /// null entry, so it counts as "present" for this check.
    #[test]
    fn policy_node_child_table_has_no_stale_entries() {
        fn field_names_of<T: serde::Serialize + Default>() -> std::collections::HashSet<String> {
            let value =
                serde_yml::to_value(T::default()).expect("default struct must serialise to value");
            let mapping = value
                .as_mapping()
                .expect("default struct must serialise as a mapping");
            mapping
                .iter()
                .filter_map(|(k, _)| k.as_str().map(String::from))
                .collect()
        }

        fn assert_no_stale(
            node: PolicyNode,
            struct_fields: &std::collections::HashSet<String>,
            struct_name: &str,
        ) {
            for (key, _) in node.child_table() {
                assert!(
                    struct_fields.contains(*key),
                    "PolicyNode::{node:?}::child_table() lists key `{key}` that does not exist \
                     on struct `{struct_name}`. A typo of this key in policy.yaml would NOT be \
                     reported as unknown even though it is silently ignored by the typed parse \
                     (see beads#302).",
                );
            }
        }

        assert_no_stale(
            PolicyNode::Document,
            &field_names_of::<PolicyDocument>(),
            "PolicyDocument",
        );
        assert_no_stale(
            PolicyNode::Workflow,
            &field_names_of::<Workflow>(),
            "Workflow",
        );
    }

    // =========================================================================
    // Status-workflow policy (issue #311)
    // =========================================================================

    fn strict_workflow() -> Workflow {
        Workflow {
            strict: true,
            statuses: vec![
                "open".to_string(),
                "in_progress".to_string(),
                "closed".to_string(),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn workflow_default_is_not_enforced() {
        let workflow = Workflow::default();
        assert!(!workflow.is_enforced());
        // With enforcement off every status is permitted.
        assert!(workflow.validate_status("anything-at-all").is_ok());
    }

    #[test]
    fn workflow_strict_but_empty_statuses_is_not_enforced() {
        let workflow = Workflow {
            strict: true,
            statuses: vec![],
            ..Default::default()
        };
        assert!(!workflow.is_enforced());
        assert!(workflow.validate_status("bogus").is_ok());
    }

    #[test]
    fn workflow_rejects_status_outside_the_set() {
        let workflow = strict_workflow();
        let err = workflow
            .validate_status("completed")
            .expect_err("out-of-set status must be rejected");
        let message = err.to_string();
        assert!(message.contains("completed"), "{message}");
        // The error names the allowed values so the user can self-correct.
        assert!(message.contains("open"), "{message}");
        assert!(message.contains("in_progress"), "{message}");
        assert!(message.contains("closed"), "{message}");
    }

    #[test]
    fn workflow_allows_status_in_the_set() {
        let workflow = strict_workflow();
        assert!(workflow.validate_status("open").is_ok());
        assert!(workflow.validate_status("in_progress").is_ok());
        assert!(workflow.validate_status("closed").is_ok());
    }

    #[test]
    fn workflow_status_match_is_case_insensitive() {
        let workflow = Workflow {
            strict: true,
            statuses: vec!["In_Progress".to_string()],
            ..Default::default()
        };
        assert!(workflow.allows("in_progress"));
        assert!(workflow.validate_status("in_progress").is_ok());
    }

    #[test]
    fn workflow_supports_custom_statuses() {
        let workflow = Workflow {
            strict: true,
            statuses: vec!["open".to_string(), "in_review".to_string()],
            ..Default::default()
        };
        assert!(workflow.validate_status("in_review").is_ok());
        assert!(workflow.validate_status("blocked").is_err());
    }

    #[test]
    fn loader_parses_workflow_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml = r#"
workflow:
  strict: true
  statuses: ["open", "in_progress", "closed"]
"#;
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("load");
        assert!(policy.workflow.is_enforced());
        assert_eq!(
            policy.workflow.statuses,
            vec![
                "open".to_string(),
                "in_progress".to_string(),
                "closed".to_string()
            ]
        );
        assert!(policy.workflow.validate_status("open").is_ok());
        assert!(policy.workflow.validate_status("completed").is_err());
    }

    #[test]
    fn loader_absent_workflow_section_is_permissive() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A policy.yaml that configures close gates but NO workflow section
        // must not enforce any status set.
        let yaml = "close_policy:\n  require_acceptance_criteria_satisfied:\n    enabled: true\n";
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("load");
        assert!(!policy.workflow.is_enforced());
        assert!(policy.workflow.validate_status("whatever").is_ok());
    }

    #[test]
    fn detect_unknown_policy_fields_walks_workflow_typos() {
        let yaml = r#"
workflow:
  strict: true
  statusses: ["open"]   # typo: should be statuses
"#;
        let raw: serde_yml::Value = serde_yml::from_str(yaml).unwrap();
        let unknown = detect_unknown_policy_fields(&raw);
        assert_eq!(unknown, vec!["workflow.statusses".to_string()]);
    }

    #[test]
    fn detect_unknown_policy_fields_accepts_canonical_workflow() {
        let yaml = r#"
workflow:
  strict: true
  statuses: ["open", "closed"]
"#;
        let raw: serde_yml::Value = serde_yml::from_str(yaml).unwrap();
        assert!(detect_unknown_policy_fields(&raw).is_empty());
    }

    #[test]
    fn loader_parses_and_validates_repository_capacity_policy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml = r"
workflow:
  statuses: [open, in_progress, in_review, rework, closed]
  capacity:
    statuses:
      in_progress:
        soft: 2
        hard: 3
    groups:
      active_work:
        statuses: [in_progress, in_review, rework]
        hard: 5
    admission:
      - name: drain_downstream
        transitions:
          from: [open]
          to: [in_progress]
        require_below:
          statuses:
            in_review: 2
          groups:
            active_work: 5
";
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("capacity policy must load");
        assert!(policy.workflow.capacity.is_active());
        assert_eq!(
            policy.workflow.capacity.statuses["in_progress"].hard,
            Some(3)
        );
        assert_eq!(
            policy.workflow.capacity.groups["active_work"].statuses,
            vec!["in_progress", "in_review", "rework"]
        );
        assert_eq!(
            policy.workflow.capacity.admission[0].name,
            "drain_downstream"
        );
    }

    #[test]
    fn capacity_rejects_undeclared_status_and_inverted_thresholds() {
        let workflow: Workflow = serde_yml::from_str(
            r"
statuses: [open, in_progress]
capacity:
  statuses:
    in_review:
      soft: 4
      hard: 2
",
        )
        .unwrap();
        let error = workflow.validate_capacity().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("undeclared workflow status 'in_review'"),
            "{error}"
        );

        let workflow: Workflow = serde_yml::from_str(
            r"
statuses: [open, in_progress]
capacity:
  statuses:
    in_progress:
      soft: 4
      hard: 2
",
        )
        .unwrap();
        let error = workflow.validate_capacity().unwrap_err();
        assert!(error.to_string().contains("cannot exceed"), "{error}");
    }

    #[test]
    fn capacity_rejects_unknown_nested_policy_keys() {
        let error = serde_yml::from_str::<Workflow>(
            r"
statuses: [open, in_progress]
capacity:
  statuses:
    in_progress:
      herd: 2
",
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn capacity_rejects_case_ambiguous_map_keys() {
        let workflow: Workflow = serde_yml::from_str(
            r"
statuses: [open, in_progress]
capacity:
  statuses:
    in_progress:
      hard: 1
    IN_PROGRESS:
      hard: 2
",
        )
        .unwrap();
        let error = workflow.validate_capacity().unwrap_err();
        assert!(
            error.to_string().contains("duplicated case-insensitively"),
            "{error}"
        );
    }

    // =========================================================================
    // Status-transition rules (issue #312, layer 1)
    // =========================================================================

    fn transition_workflow() -> Workflow {
        let mut transitions = std::collections::BTreeMap::new();
        transitions.insert(
            "open".to_string(),
            vec!["in_progress".to_string(), "closed".to_string()],
        );
        transitions.insert(
            "in_progress".to_string(),
            vec!["in_review".to_string(), "blocked".to_string()],
        );
        transitions.insert("in_review".to_string(), vec!["closed".to_string()]);
        Workflow {
            strict: true,
            statuses: vec![],
            transitions,
            ..Default::default()
        }
    }

    #[test]
    fn transitions_default_is_not_enforced() {
        let workflow = Workflow::default();
        assert!(!workflow.transitions_enforced());
        // With enforcement off every transition is permitted.
        assert!(workflow.validate_transition(Some("open"), "bogus").is_ok());
        assert!(workflow.validate_transition(None, "bogus").is_ok());
    }

    #[test]
    fn transitions_present_but_not_strict_is_not_enforced() {
        let mut workflow = transition_workflow();
        workflow.strict = false;
        assert!(!workflow.transitions_enforced());
        // Not strict => transitions advisory only, nothing rejected.
        assert!(
            workflow
                .validate_transition(Some("open"), "deferred")
                .is_ok()
        );
    }

    #[test]
    fn transitions_strict_but_empty_map_is_not_enforced() {
        let workflow = Workflow {
            strict: true,
            statuses: vec![],
            transitions: std::collections::BTreeMap::new(),
            ..Default::default()
        };
        assert!(!workflow.transitions_enforced());
        assert!(
            workflow
                .validate_transition(Some("open"), "blocked")
                .is_ok()
        );
    }

    #[test]
    fn transition_valid_move_is_allowed() {
        let workflow = transition_workflow();
        assert!(workflow.allows_transition("open", "in_progress"));
        assert!(
            workflow
                .validate_transition(Some("open"), "in_progress")
                .is_ok()
        );
        assert!(
            workflow
                .validate_transition(Some("in_progress"), "in_review")
                .is_ok()
        );
        assert!(
            workflow
                .validate_transition(Some("in_review"), "closed")
                .is_ok()
        );
    }

    #[test]
    fn transition_invalid_move_is_rejected_with_actionable_error() {
        let workflow = transition_workflow();
        let err = workflow
            .validate_transition(Some("open"), "in_review")
            .expect_err("open -> in_review is not configured");
        let message = err.to_string();
        // Names current, attempted, and the valid next statuses.
        assert!(message.contains("'open'"), "{message}");
        assert!(message.contains("'in_review'"), "{message}");
        assert!(message.contains("in_progress"), "{message}");
        assert!(message.contains("closed"), "{message}");
        assert!(message.contains("workflow.transitions"), "{message}");
    }

    #[test]
    fn transition_from_status_with_no_rule_rejects_with_none_targets() {
        let workflow = transition_workflow();
        // `blocked` has no entry and there is no `any` wildcard.
        let err = workflow
            .validate_transition(Some("blocked"), "open")
            .expect_err("blocked has no allowed targets");
        let message = err.to_string();
        assert!(message.contains("(none)"), "{message}");
    }

    #[test]
    fn transition_no_op_is_always_allowed() {
        let workflow = transition_workflow();
        // Same status, even when there is no explicit self-loop rule.
        assert!(workflow.validate_transition(Some("open"), "open").is_ok());
        // Even for a status with no rule at all.
        assert!(
            workflow
                .validate_transition(Some("blocked"), "blocked")
                .is_ok()
        );
    }

    #[test]
    fn transition_is_case_insensitive() {
        let workflow = transition_workflow();
        assert!(
            workflow
                .validate_transition(Some("OPEN"), "In_Progress")
                .is_ok()
        );
    }

    #[test]
    fn transition_any_wildcard_allows_target_from_every_status() {
        let mut workflow = transition_workflow();
        workflow
            .transitions
            .insert("any".to_string(), vec!["deferred".to_string()]);
        // `deferred` now allowed from any from-status, including ones with
        // their own rules and ones with none.
        assert!(
            workflow
                .validate_transition(Some("open"), "deferred")
                .is_ok()
        );
        assert!(
            workflow
                .validate_transition(Some("blocked"), "deferred")
                .is_ok()
        );
        // Wildcard targets are merged into the error message's "valid next".
        let err = workflow
            .validate_transition(Some("open"), "bogus")
            .expect_err("bogus is not reachable");
        assert!(err.to_string().contains("deferred"), "{err}");
    }

    #[test]
    fn transition_initial_key_restricts_creates() {
        let mut workflow = transition_workflow();
        workflow.transitions.insert(
            "initial".to_string(),
            vec!["open".to_string(), "draft".to_string()],
        );
        // No prior status => validated against `initial`.
        assert!(workflow.validate_transition(None, "open").is_ok());
        assert!(workflow.validate_transition(None, "draft").is_ok());
        let err = workflow
            .validate_transition(None, "in_progress")
            .expect_err("in_progress is not an allowed initial status");
        let message = err.to_string();
        assert!(message.contains("initial"), "{message}");
        assert!(message.contains("'in_progress'"), "{message}");
        assert!(message.contains("open"), "{message}");
        assert!(message.contains("draft"), "{message}");
    }

    #[test]
    fn transition_absent_initial_key_accepts_any_initial_status() {
        // `transition_workflow()` has no `initial` key — any starting status
        // is accepted since there is no prior state to validate against.
        let workflow = transition_workflow();
        assert!(workflow.validate_transition(None, "open").is_ok());
        assert!(workflow.validate_transition(None, "anything").is_ok());
    }

    #[test]
    fn allowed_targets_from_dedupes_and_merges_wildcard() {
        let mut workflow = transition_workflow();
        // Overlap between explicit `open` targets and the wildcard.
        workflow.transitions.insert(
            "any".to_string(),
            vec!["closed".to_string(), "deferred".to_string()],
        );
        let targets = workflow.allowed_targets_from("open");
        // explicit: in_progress, closed; wildcard adds deferred (closed deduped).
        assert_eq!(
            targets,
            vec![
                "in_progress".to_string(),
                "closed".to_string(),
                "deferred".to_string()
            ]
        );
    }

    #[test]
    fn loader_parses_workflow_transitions_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let yaml = r"
workflow:
  strict: true
  transitions:
    open: [in_progress, deferred, closed]
    in_progress: [in_review, blocked, open]
    any: [closed]
    initial: [open, draft]
";
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("load");
        let workflow = &policy.workflow;
        assert!(workflow.transitions_enforced());
        assert!(
            workflow
                .validate_transition(Some("open"), "in_progress")
                .is_ok()
        );
        assert!(
            workflow
                .validate_transition(Some("open"), "in_review")
                .is_err()
        );
        // `any: [closed]` allows close from a from-status without an explicit rule.
        assert!(
            workflow
                .validate_transition(Some("blocked"), "closed")
                .is_ok()
        );
        // `initial` gates creates.
        assert!(workflow.validate_transition(None, "open").is_ok());
        assert!(workflow.validate_transition(None, "closed").is_err());
    }

    #[test]
    fn loader_absent_transitions_is_not_enforced() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Strict status enforcement but NO transitions map: status moves are
        // unconstrained (backward compatible with #311-only configs).
        let yaml = "workflow:\n  strict: true\n  statuses: [open, in_progress, closed]\n";
        std::fs::write(dir.path().join(POLICY_FILE_NAME), yaml).unwrap();
        let policy = load_for_beads_dir(dir.path()).expect("load");
        assert!(policy.workflow.is_enforced());
        assert!(!policy.workflow.transitions_enforced());
        assert!(
            policy
                .workflow
                .validate_transition(Some("closed"), "open")
                .is_ok()
        );
    }

    #[test]
    fn detect_unknown_policy_fields_accepts_canonical_transitions() {
        let yaml = r"
workflow:
  strict: true
  transitions:
    open: [in_progress]
";
        let raw: serde_yml::Value = serde_yml::from_str(yaml).unwrap();
        assert!(detect_unknown_policy_fields(&raw).is_empty());
    }

    // =========================================================================
    // Workflow gate engine (issue #312, layer 2 / beads#319)
    // =========================================================================
}
