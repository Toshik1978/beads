use crate::format::{
    format_status_label, format_type_label, sanitize_terminal_inline, sanitize_terminal_text,
    truncate_title,
};
use crate::model::Issue;
use crate::output::Theme;
use crate::util::time::to_local;
use regex::{Regex, RegexBuilder};
use rich_rust::prelude::*;
use rich_rust::renderables::Cell;
use std::collections::HashMap;

/// Renders a list of issues as a beautiful table.
pub struct IssueTable<'a> {
    issues: &'a [Issue],
    theme: &'a Theme,
    columns: IssueTableColumns,
    title: Option<String>,
    highlight_query: Option<String>,
    context_snippets: Option<HashMap<String, String>>,
    width: Option<usize>,
    wrap: bool,
}

#[derive(Default, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct IssueTableColumns {
    pub id: bool,
    pub priority: bool,
    pub status: bool,
    pub issue_type: bool,
    pub title: bool,
    pub labels: bool,
    pub created: bool,
    pub updated: bool,
    pub context: bool,
}

impl IssueTableColumns {
    #[must_use]
    pub fn standard() -> Self {
        Self {
            id: true,
            priority: true,
            status: true,
            issue_type: true,
            title: true,
            ..Default::default()
        }
    }
}

impl<'a> IssueTable<'a> {
    #[must_use]
    pub fn new(issues: &'a [Issue], theme: &'a Theme) -> Self {
        Self {
            issues,
            theme,
            columns: IssueTableColumns::standard(),
            title: None,
            highlight_query: None,
            context_snippets: None,
            width: None,
            wrap: false,
        }
    }

    #[must_use]
    pub fn width(mut self, width: Option<usize>) -> Self {
        self.width = width;
        self
    }

    #[must_use]
    pub fn wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    #[must_use]
    pub fn columns(mut self, columns: IssueTableColumns) -> Self {
        self.columns = columns;
        self
    }

    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    #[must_use]
    pub fn highlight_query(mut self, query: impl Into<String>) -> Self {
        let query = query.into();
        if !query.trim().is_empty() {
            self.highlight_query = Some(query);
        }
        self
    }

    #[must_use]
    pub fn context_snippets(mut self, snippets: HashMap<String, String>) -> Self {
        if !snippets.is_empty() {
            self.context_snippets = Some(snippets);
        }
        self
    }

    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn build(&self) -> Table {
        let highlight_regex = self
            .highlight_query
            .as_deref()
            .and_then(build_highlight_regex);

        // Reserve ~100 chars for other columns (conservative) or min 60.
        let title_max_width = self.width.map_or(60, |w| w.saturating_sub(100).max(60));

        let mut table = Table::new()
            .box_style(self.theme.box_style)
            .border_style(self.theme.table_border.clone())
            .header_style(self.theme.table_header.clone());

        if let Some(ref title) = self.title {
            table = table.title(Text::new(sanitize_terminal_inline(title).into_owned()));
        }

        // Add columns based on config
        if self.columns.id {
            table = table.with_column(Column::new("ID").min_width(10));
        }
        if self.columns.priority {
            table = table.with_column(Column::new("P").justify(JustifyMethod::Center).width(3));
        }
        if self.columns.status {
            table = table.with_column(Column::new("Status").min_width(8));
        }
        if self.columns.issue_type {
            table = table.with_column(Column::new("Type").min_width(7));
        }
        if self.columns.title {
            table = table.with_column(
                Column::new("Title")
                    .min_width(20)
                    .max_width(title_max_width),
            );
        }
        if self.columns.labels {
            table = table.with_column(Column::new("Labels").max_width(30));
        }
        if self.columns.created {
            table = table.with_column(Column::new("Created").width(10));
        }
        if self.columns.updated {
            table = table.with_column(Column::new("Updated").width(10));
        }
        if self.columns.context {
            table = table.with_column(Column::new("Context").min_width(20).max_width(60));
        }

        // Add rows
        for issue in self.issues {
            let mut cells: Vec<Cell> = vec![];

            if self.columns.id {
                cells.push(
                    Cell::new(Text::new(sanitize_terminal_inline(&issue.id).into_owned()))
                        .style(self.theme.issue_id.clone()),
                );
            }
            if self.columns.priority {
                cells.push(
                    Cell::new(Text::new(format!("P{}", issue.priority.0)))
                        .style(self.theme.priority_style(issue.priority)),
                );
            }
            if self.columns.status {
                cells.push(
                    Cell::new(Text::new(format_status_label(&issue.status, false)))
                        .style(self.theme.status_style(&issue.status)),
                );
            }
            if self.columns.issue_type {
                cells.push(
                    Cell::new(Text::new(format_type_label(&issue.issue_type)))
                        .style(self.theme.type_style(&issue.issue_type)),
                );
            }
            if self.columns.title {
                let title = if self.wrap {
                    sanitize_terminal_inline(&issue.title).into_owned()
                } else {
                    truncate_title(&issue.title, title_max_width)
                };
                let title_text = highlight_text(&title, highlight_regex.as_ref(), self.theme);
                cells.push(Cell::new(title_text).style(self.theme.issue_title.clone()));
            }
            if self.columns.labels {
                cells.push(
                    Cell::new(Text::new(
                        issue
                            .labels
                            .iter()
                            .map(|label| sanitize_terminal_inline(label).into_owned())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ))
                    .style(self.theme.label.clone()),
                );
            }
            if self.columns.created {
                cells.push(
                    Cell::new(Text::new(
                        to_local(issue.created_at).format("%Y-%m-%d").to_string(),
                    ))
                    .style(self.theme.timestamp.clone()),
                );
            }
            if self.columns.updated {
                cells.push(
                    Cell::new(Text::new(
                        to_local(issue.updated_at).format("%Y-%m-%d").to_string(),
                    ))
                    .style(self.theme.timestamp.clone()),
                );
            }
            if self.columns.context {
                let snippet = self
                    .context_snippets
                    .as_ref()
                    .and_then(|snippets| snippets.get(&issue.id))
                    .map_or("", String::as_str);
                let plaintext_snippet = plain_snippet(snippet);
                let snippet_text = highlight_text(
                    plaintext_snippet.as_ref(),
                    highlight_regex.as_ref(),
                    self.theme,
                );
                cells.push(Cell::new(snippet_text).style(self.theme.muted.clone()));
            }

            table.add_row(Row::new(cells));
        }

        table
    }
}

fn build_highlight_regex(query: &str) -> Option<Regex> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }
    let pattern = regex::escape(trimmed);
    RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .build()
        .ok()
}

fn highlight_text(text: &str, regex: Option<&Regex>, theme: &Theme) -> Text {
    let Some(regex) = regex else {
        return Text::new(text);
    };

    let mut rich_text = Text::new("");
    let mut last = 0;
    let mut found = false;

    for matched in regex.find_iter(text) {
        found = true;
        let start = matched.start();
        let end = matched.end();
        if start > last {
            rich_text.append(&text[last..start]);
        }
        rich_text.append_styled(&text[start..end], theme.highlight.clone());
        last = end;
    }

    if !found {
        return Text::new(text);
    }
    if last < text.len() {
        rich_text.append(&text[last..]);
    }

    rich_text
}

/// Flatten a search snippet for a one-line table cell.
///
/// The snippet arrives already stripped of markdown by `build_context_snippets`
/// — including its raw-text fallback, which is chosen precisely because
/// stripping loses the match, so re-stripping here would erase the very
/// fallback that produced it. `strip_markdown` is also not idempotent
/// (`[[x]]` strips to `[x]`, and a second pass strips that to `x`), so a
/// second call here could mangle content the first pass already produced
/// correctly. This function therefore only sanitizes terminal escapes and
/// flattens whitespace to a single line; it must not strip markdown again.
///
/// Sanitizing must run first, and always — it is a terminal-injection
/// defence and must apply to every snippet regardless of origin.
fn plain_snippet(snippet: &str) -> String {
    let sanitized = sanitize_terminal_text(snippet);
    // Collapse any remaining whitespace to single spaces (snippet should
    // already be one line from normalize_whitespace in snippet_around_match).
    sanitized.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::{IssueTable, IssueTableColumns, plain_snippet};
    use crate::format::truncate_title;
    use crate::model::{Issue, IssueType, Status};
    use crate::output::Theme;

    #[test]
    fn test_table_truncation_safe() {
        let title = "😊".repeat(60); // 240 bytes, 60 chars, 120 visual width

        let truncated = truncate_title(&title, 60);

        // Should be safe and shorter than original
        assert!(truncated.chars().count() < 60);
        assert!(truncated.starts_with("😊"));
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn table_sanitizes_custom_status_and_type_cells() {
        let issue = Issue {
            id: "bd-table\x1b]52;c;bad\x07".to_string(),
            title: "safe title".to_string(),
            status: Status::Custom("state\x1b[2J".to_string()),
            issue_type: IssueType::Custom("kind\x07bell".to_string()),
            ..Issue::default()
        };
        let theme = Theme::default();
        let rendered = IssueTable::new(std::slice::from_ref(&issue), &theme)
            .columns(IssueTableColumns {
                id: true,
                status: true,
                issue_type: true,
                title: true,
                ..Default::default()
            })
            .build()
            .render_plain(120);

        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x07'));
        assert!(rendered.contains("bd-table\\u{1b}]52;c;bad\\u{7}"));
        assert!(rendered.contains("\\u{1b}[2J"));
        assert!(rendered.contains("\\u{7}bell"));
    }

    #[test]
    fn table_sanitizes_title_controls() {
        let issue = Issue {
            id: "bd-table".to_string(),
            title: "safe title".to_string(),
            ..Issue::default()
        };
        let theme = Theme::default();
        let rendered = IssueTable::new(std::slice::from_ref(&issue), &theme)
            .title("Search: \x1b]52;c;bad\x07\rreset")
            .build()
            .render_plain(120);

        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x07'));
        assert!(!rendered.contains('\r'));
        assert!(rendered.contains("Search: \\u{1b}]52;c;bad\\u{7}\\rreset"));
    }

    #[test]
    fn plain_snippet_flattens_multiline_text_to_one_line() {
        let snippet = "Heading\n\nSome bold and code text.";
        let out = plain_snippet(snippet);

        assert!(
            !out.contains('\n'),
            "a table cell must be one line: {out:?}"
        );
        assert!(out.contains("Heading"));
        assert!(out.contains("bold"));
    }

    #[test]
    fn plain_snippet_does_not_strip_markdown_a_second_time() {
        // `build_context_snippets` already strips markdown before a snippet
        // reaches here, including on its raw-text fallback path (chosen
        // precisely because stripping loses the search match). A second
        // `strip_markdown` pass here would erase that fallback, and
        // `strip_markdown` is not idempotent (`[[x]]` -> `[x]` -> `x`), so a
        // second pass can also mangle content the first pass produced
        // correctly. `plain_snippet` must leave markdown markers alone.
        let snippet = "## Heading\n\nSome **bold** and `code` text.";
        let out = plain_snippet(snippet);

        assert!(out.contains('#'), "got {out:?}");
        assert!(out.contains('*'), "got {out:?}");
        assert!(
            !out.contains('\n'),
            "a table cell must be one line: {out:?}"
        );
    }

    #[test]
    fn plain_snippet_sanitizes_terminal_escapes() {
        let out = plain_snippet("danger \x1b[2J here");
        assert!(!out.contains('\x1b'), "raw escape survived: {out:?}");
    }
}
