//! Multi-key sort specifications for `br list` and `br search`.
//!
//! A `SortSpec` is parsed once at the CLI boundary and then rendered two ways:
//! into a SQL `ORDER BY` clause, and into an in-memory comparator. Both read
//! `resolved()`, so the two engines cannot disagree about the effective order.

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::str::FromStr;

use crate::error::{BeadsError, Result};
use crate::model::{Issue, IssueType, Status};

/// A column the result set can be ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortField {
    Priority,
    Status,
    Type,
    Assignee,
    CreatedAt,
    UpdatedAt,
    Title,
    /// The implicit terminator appended by [`SortSpec::resolved`]. Not
    /// parseable: `--sort id` is rejected like any other unknown field.
    Id,
}

/// What one sort field expands to, in order. Two slots because `status`,
/// `type` and `assignee` each need a pair and nothing needs more; the shorter
/// fields leave the second `None`.
///
/// A fixed array rather than a `Vec` for two reasons. It costs no allocation,
/// which matters because [`SortField::compare_fragments`] runs inside a
/// comparator. And it is the same shape for both renderings, so the two stay
/// visibly arm-for-arm — the property the whole design rests on.
type Fragments<T> = [Option<(T, FragmentDirection)>; 2];

impl SortField {
    /// Every user-facing field, in the order the documentation lists them.
    /// `Id` is deliberately absent: it is the implicit terminator, not a key
    /// anyone can ask for.
    ///
    /// [`Self::parse`] reads this list rather than matching the enum
    /// directly, so a variant absent from `ALL` is simply not parseable — do
    /// not "simplify" `parse` back into a standalone match, or a new field
    /// could become usable from `--sort` without ever being offered by
    /// completion or documented.
    pub const ALL: &'static [Self] = &[
        Self::Priority,
        Self::Status,
        Self::Type,
        Self::Assignee,
        Self::CreatedAt,
        Self::UpdatedAt,
        Self::Title,
    ];

    /// The canonical spelling, i.e. the non-alias name.
    #[must_use]
    pub const fn canonical_name(self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::Status => "status",
            Self::Type => "type",
            Self::Assignee => "assignee",
            Self::CreatedAt => "created_at",
            Self::UpdatedAt => "updated_at",
            Self::Title => "title",
            Self::Id => "id",
        }
    }

    /// The direction a bare field takes, matching what that field already did
    /// as a single key before multi-key specs existed.
    #[must_use]
    pub const fn natural_direction(self) -> SortDirection {
        match self {
            Self::CreatedAt | Self::UpdatedAt => SortDirection::Desc,
            _ => SortDirection::Asc,
        }
    }

    /// Parses a `--sort` key name against [`Self::ALL`], so a variant not
    /// listed there cannot be reached this way regardless of what arm this
    /// function's `match` might otherwise have. `created`/`updated` are the
    /// only aliases — spellings that do not equal any [`Self::canonical_name`]
    /// — so they are handled explicitly before the lookup.
    fn parse(name: &str) -> Option<Self> {
        match name {
            "created" => return Some(Self::CreatedAt),
            "updated" => return Some(Self::UpdatedAt),
            _ => {}
        }
        Self::ALL
            .iter()
            .copied()
            .find(|field| field.canonical_name() == name)
    }

    /// The SQL fragments this field orders by, in order.
    ///
    /// `status` and `type` emit a `CASE` plus the raw column: the raw column
    /// only ever breaks ties inside the custom bucket, since every known value
    /// has a distinct rank, so it cannot disturb the documented order.
    fn sql_fragments(self) -> Fragments<String> {
        match self {
            Self::Priority => [
                Some(("priority".to_string(), FragmentDirection::Follow)),
                None,
            ],
            Self::Status => [
                Some((
                    case_expression("status", Status::SORT_RANKS, Status::CUSTOM_SORT_RANK),
                    FragmentDirection::Follow,
                )),
                Some(("status".to_string(), FragmentDirection::Follow)),
            ],
            Self::Type => [
                Some((
                    case_expression(
                        "issue_type",
                        IssueType::SORT_RANKS,
                        IssueType::CUSTOM_SORT_RANK,
                    ),
                    FragmentDirection::Follow,
                )),
                Some(("issue_type".to_string(), FragmentDirection::Follow)),
            ],
            Self::Assignee => [
                Some((
                    "CASE WHEN NULLIF(assignee,'') IS NULL THEN 1 ELSE 0 END".to_string(),
                    FragmentDirection::ForceAsc,
                )),
                Some(("NULLIF(assignee,'')".to_string(), FragmentDirection::Follow)),
            ],
            Self::CreatedAt => [
                Some(("created_at".to_string(), FragmentDirection::Follow)),
                None,
            ],
            Self::UpdatedAt => [
                Some(("updated_at".to_string(), FragmentDirection::Follow)),
                None,
            ],
            Self::Title => [
                Some((
                    "title COLLATE NOCASE".to_string(),
                    FragmentDirection::Follow,
                )),
                None,
            ],
            Self::Id => [Some(("id".to_string(), FragmentDirection::Follow)), None],
        }
    }

    /// The comparisons this field orders by, in the same order and with the
    /// same direction modes as [`Self::sql_fragments`]. Keeping the two in
    /// lockstep is what makes the SQL and in-memory orderings agree, which is
    /// also why both return the same fixed-size shape: an arm that emits one
    /// fragment here must emit one there, and a reader can check that by
    /// looking rather than by counting.
    ///
    /// Every arm is allocation-free. `sort_by` calls this once per key per
    /// comparison — O(n log n) times — so a `Vec` or a `String` here is a
    /// `Vec` or a `String` n log n times.
    fn compare_fragments(self, left: &Issue, right: &Issue) -> Fragments<Ordering> {
        match self {
            Self::Priority => [
                Some((
                    left.priority.cmp(&right.priority),
                    FragmentDirection::Follow,
                )),
                None,
            ],
            Self::Status => [
                Some((
                    left.status.sort_rank().cmp(&right.status.sort_rank()),
                    FragmentDirection::Follow,
                )),
                Some((
                    left.status.as_str().cmp(right.status.as_str()),
                    FragmentDirection::Follow,
                )),
            ],
            Self::Type => [
                Some((
                    left.issue_type
                        .sort_rank()
                        .cmp(&right.issue_type.sort_rank()),
                    FragmentDirection::Follow,
                )),
                Some((
                    left.issue_type.as_str().cmp(right.issue_type.as_str()),
                    FragmentDirection::Follow,
                )),
            ],
            Self::Assignee => {
                // Matches NULLIF(assignee,''): only the empty string folds,
                // not whitespace. A nested `fn` rather than a closure so the
                // borrow of each issue ends with the call.
                fn fold(issue: &Issue) -> Option<&str> {
                    issue.assignee.as_deref().filter(|value| !value.is_empty())
                }
                let (left_name, right_name) = (fold(left), fold(right));
                [
                    Some((
                        u8::from(left_name.is_none()).cmp(&u8::from(right_name.is_none())),
                        FragmentDirection::ForceAsc,
                    )),
                    Some((
                        left_name
                            .unwrap_or_default()
                            .cmp(right_name.unwrap_or_default()),
                        FragmentDirection::Follow,
                    )),
                ]
            }
            Self::CreatedAt => [
                Some((
                    left.created_at.cmp(&right.created_at),
                    FragmentDirection::Follow,
                )),
                None,
            ],
            Self::UpdatedAt => [
                Some((
                    left.updated_at.cmp(&right.updated_at),
                    FragmentDirection::Follow,
                )),
                None,
            ],
            // ASCII-only fold, matching SQLite's COLLATE NOCASE. Comparing the
            // folded bytes lazily is byte-for-byte what comparing two
            // `to_ascii_lowercase()` `String`s would give — `str` orders by
            // bytes and the fold only ever rewrites ASCII ones — without
            // allocating either of them.
            Self::Title => [
                Some((
                    left.title
                        .bytes()
                        .map(|byte| byte.to_ascii_lowercase())
                        .cmp(right.title.bytes().map(|byte| byte.to_ascii_lowercase())),
                    FragmentDirection::Follow,
                )),
                None,
            ],
            Self::Id => [
                Some((left.id.cmp(&right.id), FragmentDirection::Follow)),
                None,
            ],
        }
    }
}

/// Ascending or descending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortDirection {
    Asc,
    Desc,
}

impl SortDirection {
    #[must_use]
    pub const fn flipped(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }

    #[must_use]
    pub const fn sql(self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }
}

/// How a rendered fragment takes its direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FragmentDirection {
    /// Use the key's direction.
    Follow,
    /// Always ascending, whatever the key says. Used for the assignee
    /// null-rank so unassigned sorts last in both directions.
    ForceAsc,
}

/// Build `CASE <column> WHEN 'a' THEN 0 … ELSE <custom> END` from a rank table.
fn case_expression(column: &str, ranks: &[(&str, u8)], custom_rank: u8) -> String {
    let mut sql = format!("CASE {column}");
    for (name, rank) in ranks {
        let _ = write!(sql, " WHEN '{name}' THEN {rank}");
    }
    let _ = write!(sql, " ELSE {custom_rank} END");
    sql
}

/// A field paired with a concrete direction. Produced by resolution; never
/// parsed directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey {
    pub field: SortField,
    pub direction: SortDirection,
}

/// One parsed segment, before natural directions are filled in. `direction`
/// is `None` when the user wrote a bare field — which is what the legacy
/// `priority` carve-out keys off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedKey {
    field: SortField,
    direction: Option<SortDirection>,
}

/// A parsed `--sort` specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortSpec {
    keys: Vec<ParsedKey>,
}

fn invalid(reason: String) -> BeadsError {
    BeadsError::Validation {
        field: "sort".to_string(),
        reason,
    }
}

impl SortSpec {
    /// The effective ordering: natural directions applied, the legacy
    /// `priority` carve-out expanded, `reverse` folded in, and the `id ASC`
    /// terminator appended.
    ///
    /// This is the only place the effective order is derived. SQL rendering,
    /// in-memory comparison, and the index fast-path guard all read it.
    #[must_use]
    pub fn resolved(&self, reverse: bool) -> Vec<SortKey> {
        let mut keys: Vec<SortKey> = if self.is_legacy_priority() {
            vec![
                SortKey {
                    field: SortField::Priority,
                    direction: SortDirection::Asc,
                },
                SortKey {
                    field: SortField::CreatedAt,
                    direction: SortDirection::Desc,
                },
            ]
        } else {
            self.keys
                .iter()
                .map(|parsed| SortKey {
                    field: parsed.field,
                    direction: parsed
                        .direction
                        .unwrap_or_else(|| parsed.field.natural_direction()),
                })
                .collect()
        };

        if reverse {
            for key in &mut keys {
                key.direction = key.direction.flipped();
            }
        }

        // Always last, always ascending: the tiebreaker that makes output
        // deterministic. `--reverse` does not flip it.
        keys.push(SortKey {
            field: SortField::Id,
            direction: SortDirection::Asc,
        });
        keys
    }

    /// A single bare `priority` keeps the `created_at DESC` tiebreaker it has
    /// always had. Any explicit direction or any second key makes the spec
    /// literal.
    fn is_legacy_priority(&self) -> bool {
        matches!(
            self.keys.as_slice(),
            [ParsedKey {
                field: SortField::Priority,
                direction: None
            }]
        )
    }

    /// The ordering applied when no `--sort` was given. Identical to a bare
    /// `priority` spec, so the default and `--sort priority` cannot drift.
    #[must_use]
    pub fn default_order() -> Self {
        Self {
            keys: vec![ParsedKey {
                field: SortField::Priority,
                direction: None,
            }],
        }
    }

    /// Render the effective ordering as a SQL `ORDER BY` clause, with a leading
    /// space so it concatenates onto a partially built statement.
    #[must_use]
    pub fn order_by_sql(&self, reverse: bool) -> String {
        let mut sql = String::from(" ORDER BY ");
        let mut first = true;
        for key in self.resolved(reverse) {
            for (expression, mode) in key.field.sql_fragments().into_iter().flatten() {
                if !first {
                    sql.push_str(", ");
                }
                first = false;
                let direction = match mode {
                    FragmentDirection::ForceAsc => SortDirection::Asc,
                    FragmentDirection::Follow => key.direction,
                };
                let _ = write!(sql, "{expression} {}", direction.sql());
            }
        }
        sql
    }

    /// A comparator over this spec, for sorting a collection.
    ///
    /// [`Self::resolved`] is walked once, here, and the result captured — so
    /// the returned closure allocates nothing at all. Prefer this to
    /// [`Self::compare`] for anything larger than a handful of comparisons:
    /// `sort_by` calls its closure O(n log n) times, so work done per call is
    /// work done n log n times.
    #[must_use]
    pub fn comparator(&self, reverse: bool) -> impl Fn(&Issue, &Issue) -> Ordering + use<> {
        let keys = self.resolved(reverse);
        move |left, right| compare_resolved(&keys, left, right)
    }

    /// Compare two issues under this spec. The in-memory counterpart of
    /// [`Self::order_by_sql`]; both walk [`Self::resolved`].
    ///
    /// Resolves the spec on every call. To sort a collection use
    /// [`Self::comparator`], which resolves once.
    #[must_use]
    pub fn compare(&self, left: &Issue, right: &Issue, reverse: bool) -> Ordering {
        compare_resolved(&self.resolved(reverse), left, right)
    }
}

/// The whole in-memory ordering, over an already-resolved key list.
///
/// Both [`SortSpec::compare`] and [`SortSpec::comparator`] end here, so there
/// is exactly one implementation of the comparison and the cheap path cannot
/// drift from the convenient one.
fn compare_resolved(keys: &[SortKey], left: &Issue, right: &Issue) -> Ordering {
    for key in keys {
        for (ordering, mode) in key
            .field
            .compare_fragments(left, right)
            .into_iter()
            .flatten()
        {
            let direction = match mode {
                FragmentDirection::ForceAsc => SortDirection::Asc,
                FragmentDirection::Follow => key.direction,
            };
            let ordering = match direction {
                SortDirection::Asc => ordering,
                SortDirection::Desc => ordering.reverse(),
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
    }
    Ordering::Equal
}

impl FromStr for SortSpec {
    type Err = BeadsError;

    fn from_str(input: &str) -> Result<Self> {
        if input.trim().is_empty() {
            return Err(invalid("sort spec is empty".to_string()));
        }

        let mut keys: Vec<ParsedKey> = Vec::new();
        for segment in input.split(',') {
            let segment = segment.trim();
            if segment.is_empty() {
                return Err(invalid(format!(
                    "empty sort key in '{input}' — check for a stray comma"
                )));
            }

            let (direction, name) = match segment.strip_prefix('-') {
                Some(rest) => (Some(SortDirection::Desc), rest),
                None => match segment.strip_prefix('+') {
                    Some(rest) => (Some(SortDirection::Asc), rest),
                    None => (None, segment),
                },
            };

            let Some(field) = SortField::parse(name) else {
                return Err(invalid(format!("invalid sort field '{name}'")));
            };

            if keys.iter().any(|existing| existing.field == field) {
                return Err(invalid(format!(
                    "duplicate sort field '{name}' in '{input}'"
                )));
            }

            keys.push(ParsedKey { field, direction });
        }

        Ok(Self { keys })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, Priority, Status};
    use chrono::{TimeZone, Utc};
    use std::cmp::Ordering;

    fn spec(input: &str) -> SortSpec {
        input.parse().expect("valid spec")
    }

    fn issue(id: &str) -> Issue {
        let mut issue = Issue::default();
        issue.id = id.to_string();
        issue.created_at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        issue.updated_at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        issue
    }

    /// Orders through `comparator`, the path the CLI takes, and asserts on the
    /// way past that `compare` — the single-shot convenience — would have said
    /// the same thing. Every ordering test below therefore checks both.
    fn order(spec_str: &str, reverse: bool, mut issues: Vec<Issue>) -> Vec<String> {
        let parsed = spec(spec_str);

        let mut one_shot = issues.clone();
        one_shot.sort_by(|left, right| parsed.compare(left, right, reverse));

        issues.sort_by(parsed.comparator(reverse));

        assert_eq!(
            issues.iter().map(|i| &i.id).collect::<Vec<_>>(),
            one_shot.iter().map(|i| &i.id).collect::<Vec<_>>(),
            "'{spec_str}' reverse={reverse}: compare disagreed with comparator"
        );
        issues.into_iter().map(|i| i.id).collect()
    }

    fn fields(keys: &[SortKey]) -> Vec<(SortField, SortDirection)> {
        keys.iter().map(|k| (k.field, k.direction)).collect()
    }

    #[test]
    fn compares_by_priority_then_falls_through_to_id() {
        let mut low = issue("b");
        low.priority = Priority(3);
        let mut high = issue("a");
        high.priority = Priority(0);
        let mut tie = issue("c");
        tie.priority = Priority(0);

        assert_eq!(
            order("+priority", false, vec![low, high, tie]),
            vec!["a", "c", "b"]
        );
    }

    #[test]
    fn status_orders_by_rank_not_alphabetically() {
        // Ids run counter to rank order on purpose. Were the status key
        // dropped, the implicit `id ASC` terminator would produce [a, m, z] —
        // so this pins the key doing work, not merely that it is not
        // alphabetical by status name.
        let mut open = issue("z");
        open.status = Status::Open;
        let mut blocked = issue("m");
        blocked.status = Status::Blocked;
        let mut closed = issue("a");
        closed.status = Status::Closed;

        // Alphabetically by status this would be blocked, closed, open.
        assert_eq!(
            order("status", false, vec![closed, blocked, open]),
            vec!["z", "m", "a"]
        );
    }

    #[test]
    fn custom_statuses_sort_last_and_alphabetically_among_themselves() {
        // Ids contradict the expected order twice over: dropping the rank
        // fragment leaves [a, m, z] by the id terminator, and dropping only
        // the name fragment leaves the two customs tied on rank, so `zeta`
        // ("a") would precede `alpha` ("m"). Both fragments have to work.
        let mut known = issue("z");
        known.status = Status::Pinned;
        let mut zeta = issue("a");
        zeta.status = Status::Custom("zeta".to_string());
        let mut alpha = issue("m");
        alpha.status = Status::Custom("alpha".to_string());

        assert_eq!(
            order("status", false, vec![zeta, alpha, known]),
            vec!["z", "m", "a"]
        );
    }

    #[test]
    fn unassigned_sorts_last_in_both_directions() {
        let mut anna = issue("a");
        anna.assignee = Some("anna".to_string());
        let mut zoe = issue("z");
        zoe.assignee = Some("zoe".to_string());
        let nobody = issue("n"); // assignee: None
        let mut blank = issue("e");
        blank.assignee = Some(String::new()); // folded to unassigned like NULLIF

        let ascending = order(
            "assignee",
            false,
            vec![zoe.clone(), nobody.clone(), blank.clone(), anna.clone()],
        );
        assert_eq!(ascending, vec!["a", "z", "e", "n"]);

        let descending = order("-assignee", false, vec![zoe, nobody, blank, anna]);
        assert_eq!(descending, vec!["z", "a", "e", "n"]);
    }

    #[test]
    fn title_folds_ascii_case_only() {
        let mut upper = issue("a");
        upper.title = "Beta".to_string();
        let mut lower = issue("b");
        lower.title = "alpha".to_string();

        assert_eq!(order("title", false, vec![upper, lower]), vec!["b", "a"]);
    }

    #[test]
    fn reverse_flips_keys_but_id_still_breaks_ties_ascending() {
        let mut first = issue("a");
        first.priority = Priority(1);
        let mut second = issue("b");
        second.priority = Priority(1);

        assert_eq!(
            order("+priority", true, vec![second, first]),
            vec!["a", "b"]
        );
    }

    #[test]
    fn multi_key_falls_through_in_order() {
        let mut a = issue("a");
        a.priority = Priority(1);
        a.updated_at = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap();
        let mut b = issue("b");
        b.priority = Priority(1);
        b.updated_at = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let mut c = issue("c");
        c.priority = Priority(0);
        c.updated_at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

        // priority ASC first, then updated_at DESC within the priority-1 band.
        assert_eq!(
            order("priority,updated", false, vec![a, b, c]),
            vec!["c", "b", "a"]
        );
    }

    #[test]
    fn an_exhausted_spec_reports_equal_only_for_the_same_id() {
        let left = issue("a");
        let right = issue("a");
        assert_eq!(spec("title").compare(&left, &right, false), Ordering::Equal);
    }

    #[test]
    fn bare_fields_take_their_natural_direction() {
        assert_eq!(
            fields(&spec("updated").resolved(false)),
            vec![
                (SortField::UpdatedAt, SortDirection::Desc),
                (SortField::Id, SortDirection::Asc),
            ]
        );
        assert_eq!(
            fields(&spec("title").resolved(false)),
            vec![
                (SortField::Title, SortDirection::Asc),
                (SortField::Id, SortDirection::Asc),
            ]
        );
    }

    #[test]
    fn aliases_resolve() {
        assert_eq!(spec("created"), spec("created_at"));
        assert_eq!(spec("updated"), spec("updated_at"));
    }

    #[test]
    fn direction_prefixes_override_the_natural_direction() {
        assert_eq!(
            fields(&spec("-priority,+updated").resolved(false)),
            vec![
                (SortField::Priority, SortDirection::Desc),
                (SortField::UpdatedAt, SortDirection::Asc),
                (SortField::Id, SortDirection::Asc),
            ]
        );
    }

    #[test]
    fn multi_key_specs_are_literal() {
        assert_eq!(
            fields(&spec("priority,updated").resolved(false)),
            vec![
                (SortField::Priority, SortDirection::Asc),
                (SortField::UpdatedAt, SortDirection::Desc),
                (SortField::Id, SortDirection::Asc),
            ]
        );
    }

    #[test]
    fn a_single_bare_priority_keeps_the_legacy_created_at_tiebreaker() {
        assert_eq!(
            fields(&spec("priority").resolved(false)),
            vec![
                (SortField::Priority, SortDirection::Asc),
                (SortField::CreatedAt, SortDirection::Desc),
                (SortField::Id, SortDirection::Asc),
            ]
        );
    }

    #[test]
    fn an_explicit_direction_disables_the_legacy_carve_out() {
        assert_eq!(
            fields(&spec("+priority").resolved(false)),
            vec![
                (SortField::Priority, SortDirection::Asc),
                (SortField::Id, SortDirection::Asc),
            ]
        );
    }

    #[test]
    fn reverse_flips_every_key_but_not_the_id_terminator() {
        assert_eq!(
            fields(&spec("priority,updated").resolved(true)),
            vec![
                (SortField::Priority, SortDirection::Desc),
                (SortField::UpdatedAt, SortDirection::Asc),
                (SortField::Id, SortDirection::Asc),
            ]
        );
    }

    #[test]
    fn reverse_flips_the_legacy_carve_outs_inherited_key_too() {
        // Matches today's `--sort priority --reverse` at sqlite.rs:4055.
        assert_eq!(
            fields(&spec("priority").resolved(true)),
            vec![
                (SortField::Priority, SortDirection::Desc),
                (SortField::CreatedAt, SortDirection::Asc),
                (SortField::Id, SortDirection::Asc),
            ]
        );
    }

    #[test]
    fn every_documented_field_parses() {
        for name in [
            "priority",
            "status",
            "type",
            "assignee",
            "created_at",
            "updated_at",
            "title",
        ] {
            assert!(name.parse::<SortSpec>().is_ok(), "{name} must parse");
        }
    }

    #[test]
    fn invalid_specs_are_rejected() {
        for input in [
            "",                   // empty
            "   ",                // blank
            "nonsense",           // unknown field
            "id",                 // internal terminator, not user-facing
            "priority,,title",    // empty segment
            ",priority",          // leading comma
            "priority,",          // trailing comma
            "priority,-priority", // duplicate field
            "created,created_at", // duplicate via alias
            "-",                  // prefix with no field
        ] {
            assert!(
                input.parse::<SortSpec>().is_err(),
                "{input:?} must be rejected"
            );
        }
    }

    #[test]
    fn the_error_names_the_sort_field() {
        let err = "nonsense".parse::<SortSpec>().unwrap_err();
        let rendered = err.to_string();
        assert!(
            rendered.contains("nonsense"),
            "error should quote the input: {rendered}"
        );
    }

    #[test]
    fn renders_a_single_key_with_the_id_terminator() {
        assert_eq!(
            spec("title").order_by_sql(false),
            " ORDER BY title COLLATE NOCASE ASC, id ASC"
        );
    }

    #[test]
    fn renders_the_legacy_priority_ordering_exactly_as_today() {
        // Byte-identical to the hand-written clause at sqlite.rs:4058.
        assert_eq!(
            spec("priority").order_by_sql(false),
            " ORDER BY priority ASC, created_at DESC, id ASC"
        );
        assert_eq!(
            spec("priority").order_by_sql(true),
            " ORDER BY priority DESC, created_at ASC, id ASC"
        );
    }

    #[test]
    fn the_no_sort_default_renders_the_same_clause_as_bare_priority() {
        assert_eq!(
            SortSpec::default_order().order_by_sql(false),
            " ORDER BY priority ASC, created_at DESC, id ASC"
        );
        assert_eq!(
            SortSpec::default_order().order_by_sql(true),
            " ORDER BY priority DESC, created_at ASC, id ASC"
        );
    }

    #[test]
    fn renders_multiple_keys_in_order() {
        assert_eq!(
            spec("priority,updated").order_by_sql(false),
            " ORDER BY priority ASC, updated_at DESC, id ASC"
        );
    }

    #[test]
    fn status_renders_a_case_expression_then_the_raw_column() {
        let sql = spec("status").order_by_sql(false);
        assert_eq!(
            sql,
            " ORDER BY CASE status WHEN 'open' THEN 0 WHEN 'in_progress' THEN 1 \
             WHEN 'blocked' THEN 2 WHEN 'deferred' THEN 3 WHEN 'draft' THEN 4 \
             WHEN 'closed' THEN 5 WHEN 'tombstone' THEN 6 WHEN 'pinned' THEN 7 \
             ELSE 99 END ASC, status ASC, id ASC"
        );
    }

    #[test]
    fn type_renders_against_the_issue_type_column() {
        let sql = spec("type").order_by_sql(false);
        assert!(sql.contains("CASE issue_type WHEN 'task' THEN 0"), "{sql}");
        assert!(
            sql.contains("ELSE 99 END ASC, issue_type ASC, id ASC"),
            "{sql}"
        );
    }

    #[test]
    fn assignee_keeps_unassigned_last_in_both_directions() {
        assert_eq!(
            spec("assignee").order_by_sql(false),
            " ORDER BY CASE WHEN NULLIF(assignee,'') IS NULL THEN 1 ELSE 0 END ASC, \
             NULLIF(assignee,'') ASC, id ASC"
        );
        // The null-rank fragment stays ASC; only the name flips.
        assert_eq!(
            spec("-assignee").order_by_sql(false),
            " ORDER BY CASE WHEN NULLIF(assignee,'') IS NULL THEN 1 ELSE 0 END ASC, \
             NULLIF(assignee,'') DESC, id ASC"
        );
        assert_eq!(
            spec("assignee").order_by_sql(true),
            " ORDER BY CASE WHEN NULLIF(assignee,'') IS NULL THEN 1 ELSE 0 END ASC, \
             NULLIF(assignee,'') DESC, id ASC"
        );
    }

    #[test]
    fn the_id_terminator_never_flips() {
        for input in ["priority,updated", "status", "assignee", "title"] {
            assert!(
                spec(input).order_by_sql(true).ends_with(", id ASC"),
                "{input} should still end with id ASC under reverse"
            );
        }
    }
}
