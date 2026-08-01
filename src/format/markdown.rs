//! Markdown rendering for descriptions and comments.
//!
//! [`render_markdown_text`] is the single entry point: it splits `content`
//! into blocks via `split_blocks`, renders each through `rich_rust`, re-wraps
//! prose at the caller's width with `wrap_text_with_indent`, and gives code
//! blocks, tables and rules a layout that is never re-wrapped.
//!
//! # Example
//!
//! ```
//! use beads::format::markdown::render_markdown_text;
//!
//! let content = "# Heading\n\nThis is **bold** and *italic*.";
//! let rendered = render_markdown_text(content, 80);
//! assert!(rendered.plain().contains("Heading"));
//! assert!(rendered.plain().contains("bold"));
//! ```

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use rich_rust::Text;
use rich_rust::renderables::markdown::Markdown;
use rich_rust::renderables::table::{Cell, Column, Row, Table};
use std::borrow::Cow;
use std::ops::Range;

/// One renderable unit of a markdown document.
///
/// Code blocks and tables carry their **parsed content** rather than a slice
/// of the source, and are lifted out of whatever container they sit in — a
/// list item, a blockquote, or the top level. That is deliberate:
/// `pulldown_cmark` strips a container's prefix from a nested block's *first*
/// line only, so the source range of a fenced block inside a blockquote is
/// ``"```sh\n> echo hi\n> ```"`` — the surviving `> ` markers make the slice
/// invalid as standalone markdown. The parser's own events are already
/// de-prefixed, so they are the source of truth for these two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Block<'a> {
    /// Markdown source that may be re-wrapped to the caller's width.
    Prose(&'a str),
    /// Markdown source rendered as-is and never re-wrapped: horizontal rules,
    /// whose source is a single line and needs no de-prefixing.
    Fixed(&'a str),
    /// A code block's exact content, with the info string from its fence.
    Code { language: String, text: String },
    /// A table's cells. Rendered through `rich_rust`'s `Table`, which honours
    /// a maximum width; the `Markdown` renderable's own table path ignores the
    /// width it is given entirely.
    Table {
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
}

fn parser_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options
}

/// Split `content` into blocks, lifting every code block and table out of its
/// container so each can be rendered with a layout of its own.
///
/// Prose keeps its original source slice, which preserves context a
/// reconstruction would lose — notably a list's numbering, since the slice
/// following a lifted code block still begins `2.` and `rich_rust` honours an
/// ordered list's start number.
pub(crate) fn split_blocks(content: &str) -> Vec<Block<'_>> {
    let events: Vec<(Event<'_>, Range<usize>)> = Parser::new_ext(content, parser_options())
        .into_offset_iter()
        .collect();

    let mut blocks = Vec::new();
    let mut i = 0usize;

    while i < events.len() {
        match &events[i].0 {
            // A rule arrives unwrapped, with no matching `End`.
            Event::Rule => {
                blocks.push(Block::Fixed(&content[events[i].1.clone()]));
                i += 1;
            }
            Event::Start(_) => {
                let end = matching_end(&events, i);
                split_element(content, &events[i..=end], &mut blocks);
                i = end + 1;
            }
            _ => i += 1,
        }
    }

    blocks
}

/// Index of the `End` event closing the `Start` at `start`.
fn matching_end(events: &[(Event<'_>, Range<usize>)], start: usize) -> usize {
    let mut depth = 0usize;
    for (offset, (event, _)) in events[start..].iter().enumerate() {
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return start + offset;
                }
            }
            _ => {}
        }
    }
    events.len() - 1
}

/// Emit one top-level element as a run of blocks, splitting it around any code
/// block or table nested anywhere inside it.
///
/// An element with no fixed descendant yields exactly one `Prose` block
/// covering the whole element, which is the common case.
fn split_element<'a>(
    content: &'a str,
    events: &[(Event<'a>, Range<usize>)],
    out: &mut Vec<Block<'a>>,
) {
    let element = events[0].1.clone();

    // The element is itself a code block or table — the top-level case.
    if let Event::Start(tag) = &events[0].0
        && let Some(block) = extract_fixed(tag, events)
    {
        out.push(block);
        return;
    }

    let mut cursor = element.start;
    let mut i = 1usize;

    while i < events.len() {
        let Event::Start(tag) = &events[i].0 else {
            i += 1;
            continue;
        };

        let inner_end = matching_end(events, i);
        if let Some(block) = extract_fixed(tag, &events[i..=inner_end]) {
            let range = events[i].1.clone();
            push_prose(content, cursor..range.start, out);
            out.push(block);
            cursor = range.end;
            i = inner_end + 1;
        } else {
            i += 1;
        }
    }

    push_prose(content, cursor..element.end, out);
}

/// Push `content[range]` as prose, unless it holds nothing a reader would see.
///
/// Lifting a table out of a blockquote leaves `"> "` behind as the slice before
/// it; emitting that would render an empty quote marker, and — worse — trip
/// `render_markdown_text`'s raw-source fallback and print the `>` literally.
fn push_prose<'a>(content: &'a str, range: Range<usize>, out: &mut Vec<Block<'a>>) {
    if range.start >= range.end {
        return;
    }
    let slice = &content[range];
    if has_visible_content(slice) {
        out.push(Block::Prose(slice));
    }
}

/// Whether `source` contains anything that renders to visible output.
///
/// Used both to drop structural leftovers (see [`push_prose`]) and to decide
/// whether a block the renderer produced nothing for is worth falling back to
/// its raw source. Raw HTML counts as visible: it is exactly the case the
/// fallback exists for.
fn has_visible_content(source: &str) -> bool {
    Parser::new_ext(source, parser_options()).any(|event| match event {
        Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) => {
            !text.trim().is_empty()
        }
        Event::Rule | Event::FootnoteReference(_) => true,
        _ => false,
    })
}

/// Pull a code block's or table's content out of its events, or `None` if this
/// element is neither.
fn extract_fixed<'a>(tag: &Tag<'a>, events: &[(Event<'a>, Range<usize>)]) -> Option<Block<'a>> {
    match tag {
        Tag::CodeBlock(kind) => {
            let language = match kind {
                CodeBlockKind::Fenced(info) => {
                    info.split_whitespace().next().unwrap_or("").to_string()
                }
                CodeBlockKind::Indented => String::new(),
            };
            let text = events
                .iter()
                .filter_map(|(event, _)| match event {
                    Event::Text(text) => Some(text.as_ref()),
                    _ => None,
                })
                .collect::<String>();
            Some(Block::Code { language, text })
        }
        Tag::Table(_) => Some(extract_table(events)),
        _ => None,
    }
}

/// Collect a table's cells as plain strings.
///
/// Inline markup inside a cell is flattened to its text: `` `code` `` arrives
/// as an `Event::Code` and contributes `code`, without the backticks the
/// `Markdown` renderable leaves behind.
fn extract_table<'a>(events: &[(Event<'a>, Range<usize>)]) -> Block<'a> {
    let mut header = Vec::new();
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut in_cell = false;

    for (event, _) in events {
        match event {
            Event::Start(Tag::TableCell) => {
                in_cell = true;
                cell.clear();
            }
            Event::Text(text) | Event::Code(text) if in_cell => cell.push_str(text),
            Event::SoftBreak | Event::HardBreak if in_cell => cell.push(' '),
            Event::End(TagEnd::TableCell) => {
                in_cell = false;
                row.push(std::mem::take(&mut cell));
            }
            Event::End(TagEnd::TableHead) => header = std::mem::take(&mut row),
            Event::End(TagEnd::TableRow) => rows.push(std::mem::take(&mut row)),
            _ => {}
        }
    }

    Block::Table { header, rows }
}

/// Render `content` as styled, width-aware terminal markdown.
///
/// `width` is the **content** width — a panel's inner width, not the terminal
/// width.
///
/// This function does not sanitize. Callers pass text that has already been
/// through `sanitize_terminal_text`, so the only escape sequences in the result
/// are the ones this renderer produced.
///
/// **No emitted line exceeds `width`.** How each kind of block honours that
/// differs, because only some can be reflowed without being destroyed:
///
/// - Prose is wrapped to `width` by `wrap_text_with_indent`.
/// - A table is laid out to fit by `rich_rust`'s `Table`, which wraps cell
///   content within each column. A table narrower than `width` keeps its
///   natural size.
/// - A code block is broken at the width boundary, keeping every character,
///   the way a terminal soft-wraps. It is never reflowed at spaces: that
///   would alter the code.
/// - A horizontal rule is *cropped*, having nothing worth preserving past
///   the width.
///
/// The crop is also applied to both fallback paths below (a block the renderer
/// produced nothing for, and a document the renderer produced nothing for at
/// all), so the guarantee holds there too. It matters: a caller that draws a
/// border at `width` — every panel in this crate — is broken by a single line
/// that exceeds it.
#[must_use]
pub fn render_markdown_text(content: &str, width: usize) -> Text {
    let width = width.max(1);
    let definitions = collect_link_definitions(content);

    let mut out = Text::new("");
    let mut emitted = 0usize;

    for block in split_blocks(content) {
        // Only a block that still holds a usable slice of the original source
        // can fall back to showing it. A lifted code block or table carries
        // parsed content instead, and its slice may not be valid markdown.
        let (mut rendered, fallback) = match &block {
            Block::Fixed(source) => {
                let rendered = trim_leading_blank_lines(&render_block(source, width));
                (crop_fixed_lines(&trim_end(&rendered), width), Some(*source))
            }
            Block::Prose(source) => {
                let with_definitions = if definitions.is_empty() {
                    Cow::Borrowed(*source)
                } else {
                    // A blank line, not a single one: a paragraph's own
                    // range never includes its trailing newline (it is the
                    // *last* thing pulldown-cmark emits for that block), so
                    // one `\n` would make the definition a lazy continuation
                    // of the paragraph instead of a block of its own.
                    Cow::Owned(format!("{}\n\n{definitions}", source.trim_end()))
                };
                let rendered = trim_leading_blank_lines(&render_block(&with_definitions, width));
                (
                    trim_end(&wrap_text_with_indent(&trim_line_ends(&rendered), width)),
                    Some(*source),
                )
            }
            Block::Code { language, text } => (render_code_block(language, text, width), None),
            Block::Table { header, rows } => (render_table_block(header, rows, width), None),
        };

        if rendered.plain().trim().is_empty()
            && let Some(source) = fallback.filter(|source| has_visible_content(source))
        {
            // The renderer produced nothing for this block alone — raw HTML
            // and a handful of other constructs do this. Fall back to the
            // block's own source rather than silently dropping it; the
            // whole-document fallback below only catches this when it
            // happens to be the only block.
            rendered = crop_fixed_lines(&Text::new(source.trim_end()), width);
        }

        if rendered.plain().trim().is_empty() {
            continue;
        }
        if emitted > 0 {
            out.append("\n\n");
        }
        out.append_text(&rendered);
        emitted += 1;
    }

    if emitted == 0 && !content.trim().is_empty() {
        // The renderer produced nothing for non-empty input — a lone link
        // definition does this. Show the source rather than an empty panel.
        return crop_fixed_lines(&Text::new(content), width);
    }

    out
}

/// Run one block through `rich_rust`'s markdown renderable, collecting its
/// segments into a styled `Text`.
fn render_block(source: &str, width: usize) -> Text {
    let mut text = Text::new("");
    for segment in Markdown::new(source).hyperlinks(true).render(width) {
        match segment.style {
            Some(style) => text.append_styled(&segment.text, style),
            None => text.append(&segment.text),
        }
    }
    text
}

/// Render a lifted code block by re-fencing its exact content.
///
/// The fence is grown past the longest backtick run in the content, so a code
/// block that itself contains a fence cannot terminate its own.
fn render_code_block(language: &str, text: &str, width: usize) -> Text {
    let longest_run = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest_run.max(2) + 1);

    let body = if text.is_empty() || text.ends_with('\n') {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!("{text}\n"))
    };

    let rendered = trim_leading_blank_lines(&render_block(
        &format!("{fence}{language}\n{body}{fence}"),
        width,
    ));
    wrap_code_lines(&trim_end(&rendered), width)
}

/// Break every over-wide line of a code block at exactly `width`.
///
/// Code is not reflowed the way prose is — a break at a space would be a
/// break inside the code, and dropping that space (which the prose wrapper
/// does, so a broken line does not start with one) would silently alter it.
/// So this cuts at the width boundary and keeps every character, the way a
/// terminal soft-wraps.
///
/// Cropping would be the alternative, and was what code blocks got before:
/// it reads more tidily right up until the truncated line is a command
/// someone needs to run. Losing the tail of `cargo nextest run --workspace
/// --no-fail-fast --status-level all` is worse than wrapping it.
fn wrap_code_lines(text: &Text, width: usize) -> Text {
    let mut out = Text::new("");

    for (idx, line) in text.split_lines().iter().enumerate() {
        if idx > 0 {
            out.append("\n");
        }
        if line.cell_len() <= width {
            out.append_text(line);
            continue;
        }

        let chars: Vec<char> = line.plain().chars().collect();
        let mut start = 0usize;
        let mut piece = 0usize;

        while start < chars.len() {
            if piece > 0 {
                out.append("\n");
            }
            let end = cut_at_width(&chars, start, width);
            out.append_text(&line.slice(start, end));
            start = end;
            piece += 1;
        }
    }

    out
}

/// Exclusive character index at which `chars[start..]` reaches `width` display
/// columns. Always greater than `start`, so a caller's loop cannot spin.
fn cut_at_width(chars: &[char], start: usize, width: usize) -> usize {
    let mut acc = 0usize;
    for i in start..chars.len() {
        let w = char_width(chars[i]);
        if acc + w > width {
            return i.max(start + 1);
        }
        acc += w;
    }
    chars.len()
}

/// Render a lifted table through `rich_rust`'s `Table` renderable.
///
/// `Table::render` treats its argument as a true maximum: it wraps cell
/// content to fit and leaves a table narrower than `width` at its natural
/// size. The `Markdown` renderable's own table path does neither — it lays
/// columns out at their content width and ignores the width it was given, so
/// an over-wide table could only be cropped, which discarded most of it
/// (bds-jgk.6).
fn render_table_block(header: &[String], rows: &[Vec<String>], width: usize) -> Text {
    if header.is_empty() {
        return Text::new("");
    }

    let mut table = Table::new().with_columns(header.iter().map(|h| Column::new(h.as_str())));

    for row in rows {
        // A malformed row can carry a different cell count than the header;
        // pad and truncate so the table stays rectangular.
        let cells = (0..header.len())
            .map(|i| Cell::new(row.get(i).map_or("", String::as_str)))
            .collect();
        table = table.with_row(Row::new(cells));
    }

    let mut text = Text::new("");
    for segment in table.render(width) {
        match segment.style {
            Some(style) => text.append_styled(&segment.text, style),
            None => text.append(&segment.text),
        }
    }

    crop_fixed_lines(&trim_end(&text), width)
}

/// Strip the right-padding `rich_rust` adds to every rendered line.
///
/// Those trailing spaces count toward `cell_len`, so leaving them in would make
/// every line look over-long and force spurious wraps.
fn trim_line_ends(text: &Text) -> Text {
    let mut out = Text::new("");
    for (idx, line) in text.split_lines().iter().enumerate() {
        if idx > 0 {
            out.append("\n");
        }
        let chars: Vec<char> = line.plain().chars().collect();
        let end = chars.iter().rposition(|c| *c != ' ').map_or(0, |i| i + 1);
        out.append_text(&line.slice(0, end));
    }
    out
}

/// Drop a block's trailing whitespace without disturbing its spans.
fn trim_end(text: &Text) -> Text {
    let chars: Vec<char> = text.plain().chars().collect();
    let end = chars
        .iter()
        .rposition(|c| !c.is_whitespace())
        .map_or(0, |i| i + 1);
    text.slice(0, end)
}

/// Drop leading lines that are entirely whitespace.
///
/// `rich_rust` pads short lines out to the render width, so a leading blank
/// line in a block's own layout — a horizontal rule renders one above the
/// rule itself — would otherwise survive as a row of spaces once blocks are
/// joined by `render_markdown_text`'s own blank-line separator.
fn trim_leading_blank_lines(text: &Text) -> Text {
    let lines = text.split_lines();
    let start = lines
        .iter()
        .position(|line| !line.plain().trim().is_empty())
        .unwrap_or(lines.len());

    let mut out = Text::new("");
    for (idx, line) in lines[start..].iter().enumerate() {
        if idx > 0 {
            out.append("\n");
        }
        out.append_text(line);
    }
    out
}

/// Crop every over-wide line of a fixed block down to `width`, leaving
/// shorter lines untouched.
///
/// `rich_rust` does not clamp a table's rendered width to what was asked for,
/// so this is what makes `render_markdown_text`'s no-line-exceeds-`width`
/// guarantee hold for fixed blocks — and, since `render_markdown_text` also
/// runs its two fallback paths (uncropped source text) through this
/// function, for those too. Character-wise, using the same display width
/// accounting `wrap_text_with_indent` uses, so a cut never lands inside a
/// wide character; `Text::slice` re-maps style spans onto the cropped range,
/// so no span is disturbed by cutting mid-run.
fn crop_fixed_lines(text: &Text, width: usize) -> Text {
    let mut out = Text::new("");
    for (idx, line) in text.split_lines().iter().enumerate() {
        if idx > 0 {
            out.append("\n");
        }

        if line.cell_len() <= width {
            out.append_text(line);
            continue;
        }

        let chars: Vec<char> = line.plain().chars().collect();
        let mut acc = 0usize;
        let mut end = chars.len();
        for (i, &c) in chars.iter().enumerate() {
            if acc + char_width(c) > width {
                end = i;
                break;
            }
            acc += char_width(c);
        }
        out.append_text(&line.slice(0, end));
    }
    out
}

/// Collect link reference definitions (`[label]: url`) from the whole source.
///
/// Blocks are rendered in isolation, so a paragraph containing `[text][label]`
/// would otherwise lose its target: the definition is a different block, and
/// `pulldown_cmark` emits no event for it at all. Re-appending the definitions
/// to each prose block keeps references resolvable.
///
/// Skips lines inside fenced code, and skips anything that merely looks like
/// a definition but is not one by CommonMark's rules (see
/// [`is_link_definition`]) — otherwise `collect_link_definitions` would pull
/// an ordinary paragraph line into every other block's rendering input and it
/// would be rendered again, visibly, everywhere.
fn collect_link_definitions(content: &str) -> String {
    let mut in_code_block = false;
    content
        .lines()
        .filter(|line| {
            if is_fence_line(line) {
                in_code_block = !in_code_block;
                return false;
            }
            !in_code_block && is_link_definition(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A fenced code block delimiter (` ``` ` or `~~~`), ignoring leading
/// indentation.
fn is_fence_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// Whether `line` is a CommonMark link reference definition: `[label]:
/// destination` with an optional quoted title, and nothing else. CommonMark
/// rejects unquoted trailing text after the destination — `[Note]: remember
/// to check this` is an ordinary paragraph, not a definition — and this must
/// agree, or a plain paragraph that merely starts with `[word]:` gets
/// collected and re-rendered as a duplicate, visible line in every other
/// block.
fn is_link_definition(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix('[') else {
        return false;
    };
    let Some(close) = rest.find("]:") else {
        return false;
    };
    if rest[..close].trim().is_empty() {
        return false;
    }

    let remainder = rest[close + 2..].trim();
    let mut parts = remainder.splitn(2, char::is_whitespace);
    let Some(destination) = parts.next() else {
        return false;
    };
    if destination.is_empty() {
        return false;
    }

    match parts.next().map(str::trim) {
        None | Some("") => true,
        Some(title) => is_quoted_title(title),
    }
}

/// Whether `s` is a link title wrapped in one of the three delimiter pairs
/// CommonMark allows: `"..."`, `'...'`, or `(...)`.
fn is_quoted_title(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 2
        && matches!(
            (bytes[0], bytes[bytes.len() - 1]),
            (b'"', b'"') | (b'\'', b'\'') | (b'(', b')')
        )
}

/// Strip markdown formatting and return plain text.
///
/// Removes markdown syntax while preserving the underlying text content.
pub(crate) fn strip_markdown(content: &str) -> String {
    let mut result = String::new();
    let mut in_code_block = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Handle fenced code blocks
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }

        // Inside code block, preserve as-is (indented)
        if in_code_block {
            result.push_str("    ");
            result.push_str(line);
            result.push('\n');
            continue;
        }

        // Check for horizontal rules first
        if is_horizontal_rule(trimmed) {
            result.push_str("---\n");
            continue;
        }

        // Process the line to strip markdown
        let processed = strip_line_markdown(line);
        result.push_str(&processed);
        result.push('\n');
    }

    // Remove trailing newline
    result.trim_end().to_string()
}

/// Check if a line is a horizontal rule.
fn is_horizontal_rule(trimmed: &str) -> bool {
    let hr_stripped: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    (hr_stripped.chars().all(|c| c == '-') || hr_stripped.chars().all(|c| c == '*'))
        && hr_stripped.len() >= 3
}

/// Strip markdown formatting from a single line.
fn strip_line_markdown(line: &str) -> String {
    let mut processed = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut in_inline_code = false;

    while i < chars.len() {
        let c = chars[i];

        // Handle inline code
        if c == '`' {
            in_inline_code = !in_inline_code;
            i += 1;
            continue;
        }

        // Skip markdown formatting characters (only outside inline code)
        if !in_inline_code && let Some(skip) = try_skip_formatting(&chars, i, &mut processed) {
            i = skip;
            continue;
        }

        processed.push(c);
        i += 1;
    }

    processed
}

/// Try to skip markdown formatting at the current position.
/// Returns the new index if formatting was skipped, None otherwise.
fn try_skip_formatting(chars: &[char], i: usize, processed: &mut String) -> Option<usize> {
    let c = chars[i];

    // Bold/italic markers
    if c == '*' || c == '_' {
        // Check for double markers (always strip)
        let is_double = i + 1 < chars.len() && chars[i + 1] == c;

        if !is_double {
            // Don't strip single underscore if it's intra-word (e.g., snake_case)
            if c == '_' && i > 0 && i + 1 < chars.len() {
                let prev = chars[i - 1];
                let next = chars[i + 1];
                if prev.is_alphanumeric() && next.is_alphanumeric() {
                    return None;
                }
            }

            // Don't strip single asterisk if surrounded by spaces (e.g., math or bullet)
            // or if it opens emphasis but has no matching closing asterisk (e.g., "pointer *p")
            if c == '*' {
                let prev_space = i == 0 || chars[i - 1].is_whitespace();
                let next_space = i + 1 >= chars.len() || chars[i + 1].is_whitespace();
                if prev_space && next_space {
                    return None;
                }
                if prev_space && !next_space {
                    // Look for a right-flanking closing * (preceded by non-space)
                    let has_closing = chars[i + 1..].iter().enumerate().any(|(k, &ch)| {
                        ch == '*' && k > 0 && !chars[i + 1 + k - 1].is_whitespace()
                    });
                    if !has_closing {
                        return None;
                    }
                }
            }
        }

        let mut j = i;
        // Skip all identical contiguous markers
        while j < chars.len() && chars[j] == c {
            j += 1;
        }
        return Some(j);
    }

    // Strikethrough
    if c == '~' && i + 1 < chars.len() && chars[i + 1] == '~' {
        return Some(i + 2);
    }

    // Headers at start of line
    if processed.is_empty() && c == '#' {
        let mut j = i;
        while j < chars.len() && chars[j] == '#' {
            j += 1;
        }
        // Skip space after header markers
        if j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        return Some(j);
    }

    // Links: [text](url) -> text
    if c == '['
        && let Some(new_i) = try_extract_link(chars, i, processed)
    {
        return Some(new_i);
    }

    // Image: ![alt](url) -> [Image: alt]
    if c == '!'
        && i + 1 < chars.len()
        && chars[i + 1] == '['
        && let Some(new_i) = try_extract_image(chars, i, processed)
    {
        return Some(new_i);
    }

    // Blockquote marker at start
    if processed.is_empty() && c == '>' {
        let mut j = i + 1;
        // Skip space after >
        if j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        processed.push_str("  "); // Indent blockquotes
        return Some(j);
    }

    None
}

/// Try to extract link text from [text](url) format.
fn try_extract_link(chars: &[char], i: usize, processed: &mut String) -> Option<usize> {
    let start = i + 1;
    let bracket_end = find_matching_bracket(chars, start)?;

    // Extract link text
    let text: String = chars[start..bracket_end].iter().collect();
    processed.push_str(&text);

    // Skip past ](url)
    let mut j = bracket_end + 1;
    if j < chars.len() && chars[j] == '(' {
        j = skip_parentheses(chars, j);
    }
    Some(j)
}

/// Try to extract image alt text from ![alt](url) format.
fn try_extract_image(chars: &[char], i: usize, processed: &mut String) -> Option<usize> {
    let start = i + 2; // Skip ![
    let bracket_end = find_closing_bracket(chars, start)?;

    let alt: String = chars[start..bracket_end].iter().collect();
    if !alt.is_empty() {
        processed.push_str("[Image: ");
        processed.push_str(&alt);
        processed.push(']');
    }

    // Skip past ](url)
    let mut j = bracket_end + 1;
    if j < chars.len() && chars[j] == '(' {
        j = skip_parentheses(chars, j);
    }
    Some(j)
}

/// Find the matching closing bracket for an opening bracket.
fn find_matching_bracket(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 1;
    for (offset, &ch) in chars[start..].iter().enumerate() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// Find the closing bracket (simple, no nesting).
fn find_closing_bracket(chars: &[char], start: usize) -> Option<usize> {
    for (offset, &ch) in chars[start..].iter().enumerate() {
        if ch == ']' {
            return Some(start + offset);
        }
    }
    None
}

/// Skip over parentheses including nested ones.
fn skip_parentheses(chars: &[char], start: usize) -> usize {
    let mut j = start + 1;
    let mut paren_depth = 1;
    while j < chars.len() && paren_depth > 0 {
        if chars[j] == '(' {
            paren_depth += 1;
        } else if chars[j] == ')' {
            paren_depth -= 1;
        }
        j += 1;
    }
    j
}

/// Soft-wrap `text` to `width` columns, carrying each line's leading
/// indentation onto its continuation lines.
///
/// The plain-text twin of this is `wrap_body` in `cli::commands::show`, which
/// wraps the raw, unrendered issue body for plain-mode output. The two are
/// *not* required to agree: rich mode renders the body as markdown through
/// this module and wraps the result with this function, while plain mode
/// wraps the untouched source with `wrap_body` — that split is deliberate,
/// not an invariant to hold. Existing line breaks are preserved and a single
/// over-long word is never split.
///
/// Style-preserving by construction: every cut goes through `Text::slice`,
/// which re-maps spans onto the sliced range.
pub(crate) fn wrap_text_with_indent(text: &Text, width: usize) -> Text {
    let width = width.max(1);
    let mut out = Text::new("");

    for (line_idx, line) in text.split_lines().iter().enumerate() {
        if line_idx > 0 {
            out.append("\n");
        }

        if line.cell_len() <= width {
            out.append_text(line);
            continue;
        }

        let chars: Vec<char> = line.plain().chars().collect();
        let indent_len = chars.iter().take_while(|c| c.is_whitespace()).count();
        let indent: String = chars[..indent_len].iter().collect();
        let avail = width.saturating_sub(str_width(&indent)).max(1);

        let mut start = indent_len;
        let mut piece = 0usize;

        while start < chars.len() {
            if piece > 0 {
                out.append("\n");
            }
            out.append(&indent);

            let end = wrap_point(&chars, start, avail);
            out.append_text(&line.slice(start, end));

            // Skip the space we broke on so it does not open the next line.
            start = end;
            while start < chars.len() && chars[start] == ' ' {
                start += 1;
            }
            piece += 1;
        }
    }

    out
}

/// Exclusive character index to cut at, breaking on the last space that fits.
///
/// Always returns a value greater than `start`, so the caller's loop cannot
/// spin. When no break point fits, the over-long word is returned whole and
/// allowed to overflow rather than being split.
// This is a genuine index walk with lookahead to track last_space; an iterator chain cannot hold that state.
#[allow(clippy::needless_range_loop)]
fn wrap_point(chars: &[char], start: usize, avail: usize) -> usize {
    let mut width = 0usize;
    let mut last_space: Option<usize> = None;

    for i in start..chars.len() {
        let w = char_width(chars[i]);

        if chars[i] == ' ' {
            last_space = Some(i);
        }

        if width + w > avail {
            if let Some(space) = last_space.filter(|space| *space > start) {
                return space;
            }
            return chars[i..]
                .iter()
                .position(|c| *c == ' ')
                .map_or(chars.len(), |offset| i + offset)
                .max(start + 1);
        }

        width += w;
    }

    chars.len()
}

fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

fn str_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rich_rust::{Style, Text};

    #[test]
    fn test_strip_markdown_headers() {
        assert!(strip_markdown("# H1").contains("H1"));
        assert!(strip_markdown("## H2").contains("H2"));
        assert!(strip_markdown("### H3").contains("H3"));
        assert!(!strip_markdown("# H1").contains('#'));
    }

    #[test]
    fn test_strip_markdown_emphasis() {
        assert_eq!(strip_markdown("**bold**"), "bold");
        assert_eq!(strip_markdown("*italic*"), "italic");
        assert_eq!(strip_markdown("__bold__"), "bold");
        assert_eq!(strip_markdown("_italic_"), "italic");
        assert_eq!(strip_markdown("~~strikethrough~~"), "strikethrough");
    }

    #[test]
    fn test_strip_markdown_preserves_snake_case() {
        assert_eq!(strip_markdown("my_variable_name"), "my_variable_name");
        assert_eq!(
            strip_markdown("some_function(with_args)"),
            "some_function(with_args)"
        );
    }

    #[test]
    fn test_strip_markdown_preserves_math_asterisks() {
        assert_eq!(strip_markdown("2 * 3 = 6"), "2 * 3 = 6");
        assert_eq!(strip_markdown("pointer *p"), "pointer *p");
    }

    #[test]
    fn test_strip_markdown_links() {
        assert_eq!(strip_markdown("[text](https://example.com)"), "text");
        assert!(strip_markdown("[link text](url)").contains("link text"));
        assert!(!strip_markdown("[link](url)").contains("url"));
    }

    #[test]
    fn test_strip_markdown_code() {
        let result = strip_markdown("`inline code`");
        assert!(result.contains("inline code"));
        assert!(!result.contains('`'));
    }

    #[test]
    fn test_strip_markdown_code_blocks() {
        let content = "```rust\nfn main() {}\n```";
        let result = strip_markdown(content);
        assert!(result.contains("fn main()"));
        assert!(!result.contains("```"));
    }

    #[test]
    fn test_strip_markdown_blockquotes() {
        let result = strip_markdown("> quoted text");
        assert!(result.contains("quoted text"));
        // Blockquotes are indented
        assert!(result.starts_with("  "));
    }

    #[test]
    fn test_strip_markdown_horizontal_rule() {
        assert!(strip_markdown("---").contains("---"));
        assert!(strip_markdown("***").contains("---"));
    }

    #[test]
    fn test_strip_markdown_images() {
        let result = strip_markdown("![alt text](image.png)");
        assert!(result.contains("[Image: alt text]"));
        assert!(!result.contains("image.png"));
    }

    #[test]
    fn test_strip_markdown_nested_formatting() {
        let content = "**bold with *italic* inside**";
        let result = strip_markdown(content);
        assert!(result.contains("bold with"));
        assert!(result.contains("italic"));
        assert!(result.contains("inside"));
    }

    #[test]
    fn test_strip_markdown_empty() {
        assert!(strip_markdown("").is_empty());
    }

    #[test]
    fn split_blocks_classifies_each_block() {
        let src = "# Head\n\nPara one **bold**.\n\n- a\n- b\n\n```rust\nfn x() {}\n```\n\n| a | b |\n| - | - |\n| 1 | 2 |\n\n---\n\n> quote\n\nLast para.\n";

        let blocks = split_blocks(src);

        // Prose keeps the real source text, not reconstructed markdown.
        assert_eq!(blocks[0], Block::Prose("# Head\n"));
        assert!(matches!(blocks[1], Block::Prose(s) if s.contains("**bold**")));
        assert!(matches!(blocks[2], Block::Prose(s) if s.contains("- b")));
        assert_eq!(
            blocks[3],
            Block::Code {
                language: "rust".to_string(),
                text: "fn x() {}\n".to_string(),
            }
        );
        assert_eq!(
            blocks[4],
            Block::Table {
                header: vec!["a".to_string(), "b".to_string()],
                rows: vec![vec!["1".to_string(), "2".to_string()]],
            }
        );
        assert!(matches!(blocks[5], Block::Fixed(_)));
        assert!(matches!(blocks[6], Block::Prose(s) if s.contains("quote")));
        assert!(matches!(blocks[7], Block::Prose(s) if s.contains("Last para.")));
        assert_eq!(blocks.len(), 8);
    }

    #[test]
    fn split_blocks_handles_empty_and_whitespace_input() {
        assert!(split_blocks("").is_empty());
        assert!(split_blocks("   \n\n  \n").is_empty());
    }

    #[test]
    fn split_blocks_keeps_nested_lists_as_one_block() {
        let src = "- outer\n  - inner\n  - inner two\n- outer two\n";
        let blocks = split_blocks(src);
        assert_eq!(blocks.len(), 1);
        assert!(matches!(blocks[0], Block::Prose(s) if s.contains("inner two")));
    }

    #[test]
    fn split_blocks_lifts_a_code_block_out_of_a_list_item() {
        // bds-jgk.7: the container used to be classified whole, so the code
        // inside it went through the prose wrapper and got shredded.
        let src = "1. First step:\n\n   ```rust\n   let x = 1;\n   ```\n\n2. Second step\n";
        let blocks = split_blocks(src);

        assert_eq!(
            blocks[1],
            Block::Code {
                language: "rust".to_string(),
                // De-prefixed by the parser: the source slice would still
                // carry the list item's three-space indent.
                text: "let x = 1;\n".to_string(),
            }
        );
        // The prose either side keeps its own source, so the list numbering
        // survives the split.
        assert!(matches!(blocks[0], Block::Prose(s) if s.starts_with("1. First step:")));
        assert!(matches!(blocks[2], Block::Prose(s) if s.contains("2. Second step")));
        assert_eq!(blocks.len(), 3);
    }

    #[test]
    fn split_blocks_lifts_a_table_out_of_a_blockquote() {
        // bds-jgk.7, the other half. The `> ` left over in front of the table
        // must not survive into the block, and the bare `> ` preceding it must
        // not be emitted as a prose block of its own.
        let src = "> | column alpha | column beta |\n> | --- | --- |\n> | one | two |\n";
        let blocks = split_blocks(src);

        assert_eq!(
            blocks,
            vec![Block::Table {
                header: vec!["column alpha".to_string(), "column beta".to_string()],
                rows: vec![vec!["one".to_string(), "two".to_string()]],
            }]
        );
    }

    #[test]
    fn split_blocks_flattens_inline_markup_in_a_table_cell() {
        let src = "| head `code` | **bold** head |\n| --- | --- |\n| a `b` | *c* |\n";
        let blocks = split_blocks(src);

        assert_eq!(
            blocks,
            vec![Block::Table {
                header: vec!["head code".to_string(), "bold head".to_string()],
                rows: vec![vec!["a b".to_string(), "c".to_string()]],
            }]
        );
    }

    #[test]
    fn wrap_text_with_indent_wraps_a_long_paragraph() {
        let text = Text::new("aaa bbb ccc ddd eee");
        let wrapped = wrap_text_with_indent(&text, 11);
        assert_eq!(wrapped.plain(), "aaa bbb ccc\nddd eee");
    }

    #[test]
    fn wrap_text_with_indent_carries_the_indent_onto_continuations() {
        let text = Text::new("  - alpha beta gamma delta");
        let wrapped = wrap_text_with_indent(&text, 14);
        // Every continuation line starts with the original two-space indent.
        for line in wrapped.plain().lines().skip(1) {
            assert!(
                line.starts_with("  "),
                "continuation lost its indent: {line:?}"
            );
        }
        assert!(wrapped.plain().contains('\n'));
    }

    #[test]
    fn wrap_text_with_indent_never_splits_an_over_long_word() {
        let text = Text::new("aaa supercalifragilistic bbb");
        let wrapped = wrap_text_with_indent(&text, 8);
        assert!(
            wrapped.plain().contains("supercalifragilistic"),
            "an over-long word must be emitted whole, got {:?}",
            wrapped.plain()
        );
    }

    #[test]
    fn wrap_text_with_indent_leaves_short_lines_alone() {
        let text = Text::new("short\nalso short");
        let wrapped = wrap_text_with_indent(&text, 40);
        assert_eq!(wrapped.plain(), "short\nalso short");
    }

    #[test]
    fn render_markdown_text_renders_a_heading_without_its_markers() {
        let out = render_markdown_text("## Heading two\n", 40);
        assert!(out.plain().contains("Heading two"));
        assert!(!out.plain().contains("##"), "got {:?}", out.plain());
        assert!(!out.spans().is_empty(), "heading was not styled");
    }

    #[test]
    fn render_markdown_text_wraps_a_long_paragraph() {
        let src =
            "This is a deliberately long paragraph of plain prose with no links or emphasis at all
        so that wrapping is observable.\n";
        let out = render_markdown_text(src, 40);
        for line in out.plain().lines() {
            assert!(
                unicode_width::UnicodeWidthStr::width(line) <= 40,
                "line exceeded the width: {line:?}"
            );
        }
        assert!(out.plain().lines().count() > 1);
    }

    #[test]
    fn render_markdown_text_leaves_no_trailing_padding_on_prose() {
        let out = render_markdown_text("# H\n\nshort para\n", 60);
        for line in out.plain().lines() {
            assert_eq!(line, line.trim_end(), "line kept its padding: {line:?}");
        }
    }

    /// A table whose natural width (44 columns) exceeds the width requested
    /// below, so it can only be shown by reflowing it. A narrower table would
    /// make these tests vacuous.
    const WIDE_TABLE_SRC: &str = "| left column header | right column header |\n| ------------------- | -------------------- |\n| left value here | right value here |\n";

    #[test]
    fn render_markdown_text_keeps_a_table_a_table() {
        let out = render_markdown_text(WIDE_TABLE_SRC, 30);

        // Every line belongs to the table's frame: a border or a cell row.
        // If a table were routed through the prose wrapper instead, its rows
        // would break at their internal spaces into borderless fragments.
        for line in out.plain().lines() {
            let first = line.chars().next().expect("blank line inside a table");
            assert!(
                "┏┃┡│└├┌".contains(first),
                "a table line lost its left border: {line:?}"
            );
        }
    }

    #[test]
    fn render_markdown_text_reflows_an_over_wide_table_without_losing_content() {
        // bds-jgk.6: this table used to be cropped to the requested width,
        // which kept the row structure but discarded most of the text — the
        // second column came out as five characters. `rich_rust`'s `Table`
        // wraps cell content to fit instead, so every word survives.
        let out = render_markdown_text(WIDE_TABLE_SRC, 30);
        let plain = out.plain();
        let flattened: String = plain.split_whitespace().collect::<Vec<_>>().join(" ");

        for word in ["left", "column", "header", "right", "value", "here"] {
            assert!(
                flattened.contains(word),
                "the table lost {word:?}: {plain:?}"
            );
        }
    }

    #[test]
    fn render_markdown_text_fits_an_over_wide_table_within_the_requested_width() {
        // A caller (a panel) that assumes no emitted line exceeds the
        // requested width gets hard evidence to the contrary the first time
        // it draws a border around one.
        let out = render_markdown_text(WIDE_TABLE_SRC, 30);
        for line in out.plain().lines() {
            assert!(
                unicode_width::UnicodeWidthStr::width(line) <= 30,
                "line exceeded the requested width: {line:?}"
            );
        }
    }

    #[test]
    fn render_markdown_text_preserves_a_fenced_code_block() {
        let src = "```rust\nfn a() {}\nfn b() {}\n```\n";
        let out = render_markdown_text(src, 40);
        let lines: Vec<&str> = out.plain().lines().collect();

        assert!(lines[0].contains("fn a() {}"), "got {:?}", out.plain());
        assert!(lines[1].contains("fn b() {}"), "got {:?}", out.plain());

        // A non-final code line keeps the trailing background-fill padding
        // `rich_rust` renders it with -- the contract for a code block is
        // "emitted verbatim, including any background fill", and per-line
        // trimming is prose-only behaviour. If a code block were routed
        // through the prose path instead, `trim_line_ends` would strip that
        // padding from every line, not just the block's last one.
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(lines[0]),
            40,
            "the code line lost its background-fill padding, or was wrapped: {:?}",
            lines[0]
        );
    }

    #[test]
    fn render_markdown_text_keeps_a_code_block_inside_a_list_item_intact() {
        // bds-jgk.7: the list was classified whole as prose, so this code
        // line -- longer than the requested width -- was broken at its spaces
        // into three lines.
        let src = "1. Run this:\n\n   ```sh\n   cargo nextest run --workspace --no-fail-fast\n   ```\n\n2. Then check the output\n";
        let out = render_markdown_text(src, 40);
        let plain = out.plain();

        // Every character of the command survives. The prose wrapper broke it
        // at spaces and dropped them; the code path breaks at the width
        // boundary and keeps them, so re-joining the pieces reproduces the
        // original exactly.
        // Joining without a separator is what makes this an assertion about
        // the *code* path: it breaks at the width boundary and keeps the
        // spaces, so the pieces re-join into the original. The prose wrapper
        // breaks at a space and drops it, which would yield
        // `--workspace--no-fail-fast` here.
        let rejoined = plain.replace('\n', "");
        assert!(
            rejoined.contains("cargo nextest run --workspace --no-fail-fast"),
            "the code was altered by wrapping: {plain:?}"
        );

        for line in plain.lines() {
            assert!(
                unicode_width::UnicodeWidthStr::width(line) <= 40,
                "a code line exceeded the width: {line:?}"
            );
        }

        // Splitting the list around the code block must not cost it its
        // numbering -- the prose either side keeps its own source, and
        // `rich_rust` honours an ordered list's start number.
        assert!(plain.contains("1."), "lost the first marker: {plain:?}");
        assert!(plain.contains("2."), "lost the second marker: {plain:?}");
    }

    #[test]
    fn render_markdown_text_does_not_wrap_a_table_inside_a_blockquote() {
        // bds-jgk.7, the other half: this came out as `| column alpha |
        // column beta |` / `column gamma |` -- exactly the row-shredding the
        // fixed-block handling exists to prevent.
        let src = "> | column alpha | column beta |\n> | --- | --- |\n> | column gamma | column delta |\n";
        let out = render_markdown_text(src, 40);

        for line in out.plain().lines() {
            let first = line.chars().next().expect("blank line inside a table");
            assert!(
                "┏┃┡│└├┌".contains(first),
                "a table line lost its left border: {line:?}"
            );
        }
        assert!(
            !out.plain().contains('>'),
            "a blockquote marker leaked into the table: {:?}",
            out.plain()
        );
    }

    #[test]
    fn render_markdown_text_resolves_reference_links_across_blocks() {
        // The definition is a separate top-level block from the paragraph that
        // uses it. Rendering blocks in isolation would lose the target.
        let src = "See [the docs][ref] for more.\n\n[ref]: https://example.com\n";
        let out = render_markdown_text(src, 60);
        assert!(out.plain().contains("the docs"), "got {:?}", out.plain());
        assert!(
            !out.plain().contains("[ref]"),
            "the reference was left unresolved: {:?}",
            out.plain()
        );
    }

    #[test]
    fn render_markdown_text_resolves_reference_links_when_the_final_block_has_no_trailing_newline()
    {
        // A block's own source range from `pulldown_cmark` does not include a
        // trailing newline when it is the *last* thing in the document, so
        // appending the collected definitions with a single `\n` (rather than
        // a full blank line) would make them a lazy continuation of this
        // paragraph instead of a reference block of their own -- the
        // definition would stay unresolved and its raw text would leak into
        // the rendered prose. This shape is not an edge case: 14 of 25 prose
        // fields in this repo's own `.beads/issues.jsonl` have no trailing
        // newline.
        let src = "Intro.\n\n[ref]: https://example.com\n\nSee [the docs][ref].";
        let out = render_markdown_text(src, 60);
        assert!(out.plain().contains("the docs"), "got {:?}", out.plain());
        assert!(
            !out.plain().contains("[ref]"),
            "the reference was left unresolved: {:?}",
            out.plain()
        );
        assert!(
            !out.plain().contains("example.com"),
            "the raw definition leaked into the rendered prose: {:?}",
            out.plain()
        );
    }

    #[test]
    fn render_markdown_text_does_not_duplicate_a_malformed_link_definition() {
        // CommonMark rejects a "definition" with unquoted trailing text after
        // the destination -- `[Note]: remember to check this` stays an
        // ordinary paragraph. If `is_link_definition` accepted it anyway, the
        // collector would re-append it to every other prose block in the
        // document, and it would render as a duplicate, standalone line
        // everywhere but where it was actually written.
        let src = "Some prose line\n[Note]: remember to check this\n\nSecond paragraph.\n";
        let out = render_markdown_text(src, 60);
        let occurrences = out.plain().matches("remember to check this").count();
        assert_eq!(
            occurrences,
            1,
            "the malformed definition was duplicated: {:?}",
            out.plain()
        );
    }

    #[test]
    fn render_markdown_text_ignores_a_link_definition_shaped_line_inside_fenced_code() {
        // A line inside a fenced code block that merely looks like a
        // definition must not be collected and re-appended to every other
        // prose block either.
        let src = "```\n[Note]: remember to check this\n```\n\nSecond paragraph.\n";
        let out = render_markdown_text(src, 60);
        let occurrences = out.plain().matches("remember to check this").count();
        assert_eq!(
            occurrences,
            1,
            "the code-fenced line leaked out as a collected definition: {:?}",
            out.plain()
        );
    }

    #[test]
    fn render_markdown_text_falls_back_per_block_for_raw_html() {
        // `rich_rust`'s markdown renderable emits nothing for a raw HTML
        // block. The whole-document fallback only fires when the entire
        // document renders empty, so without a per-block fallback too, this
        // block would simply vanish even though its neighbours rendered fine.
        let src = "Para one.\n\n<div>raw html</div>\n\nPara two.\n";
        let out = render_markdown_text(src, 40);
        assert!(
            out.plain().contains("<div>raw html</div>"),
            "got {:?}",
            out.plain()
        );
        assert!(out.plain().contains("Para one"), "got {:?}", out.plain());
        assert!(out.plain().contains("Para two"), "got {:?}", out.plain());
    }

    #[test]
    fn render_markdown_text_has_no_leading_blank_line_before_a_rule() {
        // `trim_end` only trims a block's *trailing* whitespace, and a `Fixed`
        // block (a horizontal rule among them) skips the per-line right-trim
        // that would otherwise catch this: `rich_rust` renders a blank,
        // padded-to-width line above the rule itself, and without stripping
        // leading blank lines that survives as a row of spaces on top of the
        // rule.
        let out = render_markdown_text("---\n", 20);
        assert_eq!(out.plain(), "─".repeat(20), "got {:?}", out.plain());
    }

    #[test]
    fn render_markdown_text_returns_empty_for_empty_input() {
        assert_eq!(render_markdown_text("", 40).plain(), "");
        assert_eq!(render_markdown_text("   \n\n", 40).plain(), "");
    }

    #[test]
    fn render_markdown_text_falls_back_when_the_renderer_yields_nothing() {
        // A lone link definition produces no rendered block at all.
        let src = "[ref]: https://example.com\n";
        let out = render_markdown_text(src, 40);
        assert_eq!(out.plain(), src, "expected the source as a fallback");
    }

    #[test]
    fn render_markdown_text_crops_the_whole_document_fallback_to_width() {
        // Same shape as the fallback above, but the source line itself is
        // wider than the requested width. The fallback returns source text
        // verbatim, so without cropping this would violate the "no emitted
        // line exceeds width" guarantee documented on `render_markdown_text`.
        let src = "[ref]: https://example.com/a/very/long/path/over/twenty/columns\n";
        let out = render_markdown_text(src, 20);
        for line in out.plain().lines() {
            assert!(
                unicode_width::UnicodeWidthStr::width(line) <= 20,
                "fallback line exceeded the requested width: {line:?}"
            );
        }
    }

    #[test]
    fn render_markdown_text_crops_the_per_block_fallback_to_width() {
        // Raw HTML falls back to source text per-block (see the raw-HTML
        // test above); make that source line wider than the requested width
        // and confirm the fallback is cropped, not emitted verbatim.
        let src = "Para one.\n\n<div>raw html that is definitely over twenty columns wide</div>\n\nPara two.\n";
        let out = render_markdown_text(src, 20);
        for line in out.plain().lines() {
            assert!(
                unicode_width::UnicodeWidthStr::width(line) <= 20,
                "fallback line exceeded the requested width: {line:?}"
            );
        }
    }

    #[test]
    fn wrap_text_with_indent_preserves_styles_across_a_break() {
        let mut text = Text::new("");
        text.append("plain ");
        text.append_styled("styled words here", Style::new().bold());

        let wrapped = wrap_text_with_indent(&text, 10);

        assert!(wrapped.plain().contains('\n'), "expected a wrap");
        assert!(
            !wrapped.spans().is_empty(),
            "wrapping dropped every style span"
        );
        // The styled run must still be styled after being cut.
        let styled_text: String = wrapped
            .spans()
            .iter()
            .map(|s| {
                wrapped
                    .plain()
                    .chars()
                    .skip(s.start)
                    .take(s.end - s.start)
                    .collect::<String>()
            })
            .collect();
        assert!(styled_text.contains("styled"), "got {styled_text:?}");
    }
}
