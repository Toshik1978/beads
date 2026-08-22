//! Which fields `br search` matches its query against.
//!
//! A `SearchScope` is parsed once at the CLI boundary and then rendered into
//! the SQL predicate every search path shares. It exists for the same reason
//! [`SortSpec`](crate::model::sort::SortSpec) does: the row query, the fast
//! default-visible page and the `COUNT(*)` behind `search --json`'s `total`
//! must be built from one description of the search, or a count will report a
//! truthful-looking total for a different question than the query asked.

use std::fmt;
use std::str::FromStr;

use crate::error::{BeadsError, Result};

/// A column the query text is matched against.
///
/// Every variant is a field `br show` renders, which is the rule that decides
/// membership: if a user can read prose on a bead, `search` can reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchField {
    Id,
    Title,
    Description,
    Design,
    AcceptanceCriteria,
    Notes,
}

impl SearchField {
    /// Every matchable field, and — because [`SearchScope::default_scope`]
    /// returns exactly this list — the default search scope.
    ///
    /// [`Self::parse`] reads this list rather than matching the enum
    /// directly, so a variant absent from `ALL` is simply not parseable. Do
    /// not "simplify" `parse` into a standalone match, or a new field could
    /// become usable from `--in` without being searched by default or offered
    /// by completion.
    pub const ALL: &'static [Self] = &[
        Self::Id,
        Self::Title,
        Self::Description,
        Self::Design,
        Self::AcceptanceCriteria,
        Self::Notes,
    ];

    /// The canonical spelling, i.e. the non-alias name.
    #[must_use]
    pub const fn canonical_name(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Title => "title",
            Self::Description => "description",
            Self::Design => "design",
            Self::AcceptanceCriteria => "acceptance_criteria",
            Self::Notes => "notes",
        }
    }

    /// The `issues` column this field reads.
    ///
    /// Identical to [`Self::canonical_name`] today, and deliberately a
    /// separate function anyway: the user-facing vocabulary and the schema are
    /// free to drift, and a rename on either side should not silently rewrite
    /// the other.
    #[must_use]
    pub const fn column(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Title => "title",
            Self::Description => "description",
            Self::Design => "design",
            Self::AcceptanceCriteria => "acceptance_criteria",
            Self::Notes => "notes",
        }
    }

    /// The heading `br show` prints this field under, used to label a search
    /// snippet so a hit in prose the result line does not display can still
    /// explain itself.
    #[must_use]
    pub const fn heading(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Title => "title",
            Self::Description => "description",
            Self::Design => "design",
            Self::AcceptanceCriteria => "acceptance criteria",
            Self::Notes => "notes",
        }
    }

    /// Resolve a user-typed name. Aliases are spellings that do not equal any
    /// [`Self::canonical_name`], so they are handled before the lookup.
    fn parse(name: &str) -> Option<Self> {
        match name {
            "desc" => return Some(Self::Description),
            "ac" | "acceptance-criteria" | "criteria" => return Some(Self::AcceptanceCriteria),
            _ => {}
        }
        Self::ALL
            .iter()
            .copied()
            .find(|field| field.canonical_name() == name)
    }
}

impl fmt::Display for SearchField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.canonical_name())
    }
}

/// The set of fields one search matches against, in a stable order.
///
/// Always non-empty: an empty scope would match nothing at all, which is never
/// what a user asking to narrow a search meant, so [`FromStr`] rejects it
/// rather than silently returning zero results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchScope(Vec<SearchField>);

impl SearchScope {
    /// Every field in [`SearchField::ALL`] — what a bare `br search` uses.
    #[must_use]
    pub fn default_scope() -> Self {
        Self(SearchField::ALL.to_vec())
    }

    /// The fields to match, deduplicated, in [`SearchField::ALL`] order.
    #[must_use]
    pub fn fields(&self) -> &[SearchField] {
        &self.0
    }

    /// The `WHERE` fragment matching the query text, with one `?` placeholder
    /// per field. Callers must follow it with [`Self::push_params`] to bind
    /// them.
    ///
    /// Case-insensitivity is applied to the column side only, so the needle
    /// must already be lowercased — see [`Self::push_params`].
    #[must_use]
    pub fn match_sql(&self) -> String {
        let clauses: Vec<String> = self
            .0
            .iter()
            .map(|field| format!("instr(lower({}), ?) > 0", field.column()))
            .collect();
        format!("({})", clauses.join(" OR "))
    }

    /// How many `?` placeholders [`Self::match_sql`] emits, i.e. how many
    /// times the caller must bind the needle.
    ///
    /// The needle must already be lowercased — the predicate lowercases only
    /// the column side.
    #[must_use]
    pub fn placeholder_count(&self) -> usize {
        self.0.len()
    }
}

impl Default for SearchScope {
    fn default() -> Self {
        Self::default_scope()
    }
}

impl FromStr for SearchScope {
    type Err = BeadsError;

    fn from_str(input: &str) -> Result<Self> {
        if input.trim().is_empty() {
            return Err(invalid("search scope is empty".to_string()));
        }

        let mut fields: Vec<SearchField> = Vec::new();
        for segment in input.split(',') {
            let segment = segment.trim();
            if segment.is_empty() {
                return Err(invalid(format!(
                    "empty field in '{input}' — check for a stray comma"
                )));
            }

            let lowered = segment.to_ascii_lowercase();
            let Some(field) = SearchField::parse(&lowered) else {
                return Err(invalid(format!(
                    "unknown search field '{segment}'. Valid fields: {}",
                    SearchField::ALL
                        .iter()
                        .map(|f| f.canonical_name())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            };
            if !fields.contains(&field) {
                fields.push(field);
            }
        }

        // Normalise to `ALL` order so two spellings of the same scope produce
        // the same predicate, and therefore the same query plan.
        let mut ordered: Vec<SearchField> = SearchField::ALL
            .iter()
            .copied()
            .filter(|field| fields.contains(field))
            .collect();
        ordered.shrink_to_fit();
        Ok(Self(ordered))
    }
}

fn invalid(reason: String) -> BeadsError {
    BeadsError::Validation {
        field: "--in".to_string(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(input: &str) -> SearchScope {
        input.parse().expect("valid scope")
    }

    #[test]
    fn default_scope_covers_every_field_br_show_renders() {
        assert_eq!(SearchScope::default_scope().fields(), SearchField::ALL);
    }

    #[test]
    fn default_scope_reaches_the_prose_fields_search_used_to_miss() {
        let fields = SearchScope::default_scope();
        for field in [
            SearchField::Design,
            SearchField::AcceptanceCriteria,
            SearchField::Notes,
        ] {
            assert!(
                fields.fields().contains(&field),
                "{field} must be searched by default"
            );
        }
    }

    #[test]
    fn match_sql_emits_one_placeholder_per_field() {
        let sql = scope("title,design").match_sql();
        assert_eq!(
            sql,
            "(instr(lower(title), ?) > 0 OR instr(lower(design), ?) > 0)"
        );
        assert_eq!(sql.matches('?').count(), 2);
    }

    #[test]
    fn placeholder_count_matches_what_match_sql_emits() {
        for input in ["title", "title,design", "id,title,description,design,notes"] {
            let scope = scope(input);
            assert_eq!(
                scope.placeholder_count(),
                scope.match_sql().matches('?').count(),
                "bind count must match placeholder count for '{input}'"
            );
        }
        let all = SearchScope::default_scope();
        assert_eq!(
            all.placeholder_count(),
            all.match_sql().matches('?').count()
        );
    }

    #[test]
    fn aliases_resolve() {
        assert_eq!(scope("desc"), scope("description"));
        assert_eq!(scope("ac"), scope("acceptance_criteria"));
        assert_eq!(scope("acceptance-criteria"), scope("acceptance_criteria"));
        assert_eq!(scope("criteria"), scope("acceptance_criteria"));
    }

    #[test]
    fn parsing_is_case_insensitive() {
        assert_eq!(scope("Title,DESIGN"), scope("title,design"));
    }

    #[test]
    fn duplicates_collapse_and_order_normalises() {
        assert_eq!(scope("design,title,design"), scope("title,design"));
        assert_eq!(scope("notes,id"), scope("id,notes"));
    }

    #[test]
    fn an_empty_scope_is_rejected_rather_than_matching_nothing() {
        assert!("".parse::<SearchScope>().is_err());
        assert!("   ".parse::<SearchScope>().is_err());
        assert!("title,,design".parse::<SearchScope>().is_err());
    }

    #[test]
    fn an_unknown_field_names_the_valid_ones() {
        let err = "assignee".parse::<SearchScope>().unwrap_err().to_string();
        assert!(
            err.contains("assignee"),
            "error must quote the bad name: {err}"
        );
        assert!(
            err.contains("acceptance_criteria"),
            "error must list valid fields: {err}"
        );
    }
}
