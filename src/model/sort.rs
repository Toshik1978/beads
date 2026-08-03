//! Multi-key sort specifications for `br list` and `br search`.
//!
//! A `SortSpec` is parsed once at the CLI boundary and then rendered two ways:
//! into a SQL `ORDER BY` clause, and into an in-memory comparator. Both read
//! `resolved()`, so the two engines cannot disagree about the effective order.

use std::str::FromStr;

use crate::error::{BeadsError, Result};

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

impl SortField {
    /// The direction a bare field takes, matching what that field already did
    /// as a single key before multi-key specs existed.
    #[must_use]
    pub const fn natural_direction(self) -> SortDirection {
        match self {
            Self::CreatedAt | Self::UpdatedAt => SortDirection::Desc,
            _ => SortDirection::Asc,
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "priority" => Self::Priority,
            "status" => Self::Status,
            "type" => Self::Type,
            "assignee" => Self::Assignee,
            "created_at" | "created" => Self::CreatedAt,
            "updated_at" | "updated" => Self::UpdatedAt,
            "title" => Self::Title,
            _ => return None,
        })
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
    #[allow(unused_imports)]
    use super::*;

    fn spec(input: &str) -> SortSpec {
        input.parse().expect("valid spec")
    }

    fn fields(keys: &[SortKey]) -> Vec<(SortField, SortDirection)> {
        keys.iter().map(|k| (k.field, k.direction)).collect()
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
}
