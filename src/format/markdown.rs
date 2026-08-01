//! Markdown rendering for descriptions and comments.
//!
//! [`render_markdown_text`] is the single entry point: it splits `content`
//! into top-level blocks via `split_top_level_blocks`, renders each through
//! `rich_rust`'s `Markdown` renderable, re-wraps prose at the caller's width
//! with `wrap_text_with_indent`, and leaves fixed blocks (tables, fenced code,
//! rules) exactly as the renderer laid them out.
//!
//! # Example
//!
//! ```ignore
//! use beads::format::markdown::render_markdown_text;
//!
//! let content = "# Heading\n\nThis is **bold** and *italic*.";
//! let rendered = render_markdown_text(content, 80);
//! ```

use pulldown_cmark::{Event, Options, Parser, Tag};
use rich_rust::Text;
use rich_rust::renderables::markdown::Markdown;
use std::borrow::Cow;

/// Whether a rendered block may be re-wrapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockKind {
    /// Paragraph, heading, list, blockquote — safe to wrap.
    Prose,
    /// Table, fenced code, horizontal rule — must keep the renderer's own
    /// layout. Wrapping one of these mangles it.
    Fixed,
}

/// A top-level markdown block, borrowed from the original source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceBlock<'a> {
    pub kind: BlockKind,
    pub source: &'a str,
}

/// Split `content` into top-level blocks, each tagged with whether it may be
/// re-wrapped.
///
/// Relies on two properties of `pulldown_cmark`'s offset iterator, both pinned
/// by the tests in this module: a `Start` event seen at nesting depth 0 carries
/// the whole element's source range, and `Rule` arrives unwrapped at depth 0.
pub(crate) fn split_top_level_blocks(content: &str) -> Vec<SourceBlock<'_>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    let mut blocks = Vec::new();
    let mut depth = 0usize;

    for (event, range) in Parser::new_ext(content, options).into_offset_iter() {
        match event {
            Event::Start(ref tag) => {
                if depth == 0 {
                    blocks.push(SourceBlock {
                        kind: classify_block(tag),
                        source: &content[range],
                    });
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::Rule if depth == 0 => blocks.push(SourceBlock {
                kind: BlockKind::Fixed,
                source: &content[range],
            }),
            _ => {}
        }
    }

    blocks
}

/// Tables and code blocks carry layout the renderer computed; everything else
/// is prose this crate re-wraps itself.
fn classify_block(tag: &Tag<'_>) -> BlockKind {
    match tag {
        Tag::Table(_) | Tag::CodeBlock(_) => BlockKind::Fixed,
        _ => BlockKind::Prose,
    }
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
/// **No emitted line exceeds `width`.** Prose is wrapped to it by
/// [`wrap_text_with_indent`]. Fixed blocks (tables, fenced code, rules) are
/// never wrapped — `rich_rust` does not clamp a fixed block's width, so a
/// table wider than requested still renders at its own natural width — so
/// instead each of their lines is *cropped* to `width`. Wrapping a table row
/// would scatter its cells across several lines and destroy it; cropping
/// keeps a row recognizable as a row, and either way an over-wide line would
/// break a panel's border, so cropping is the least-bad option.
#[must_use]
pub fn render_markdown_text(content: &str, width: usize) -> Text {
    let width = width.max(1);
    let definitions = collect_link_definitions(content);

    let mut out = Text::new("");
    let mut emitted = 0usize;

    for block in split_top_level_blocks(content) {
        let mut rendered = match block.kind {
            BlockKind::Fixed => {
                let rendered = trim_leading_blank_lines(&render_block(block.source, width));
                crop_fixed_lines(&trim_end(&rendered), width)
            }
            BlockKind::Prose => {
                let source = if definitions.is_empty() {
                    Cow::Borrowed(block.source)
                } else {
                    // A blank line, not a single one: a paragraph's own
                    // range never includes its trailing newline (it is the
                    // *last* thing pulldown-cmark emits for that block), so
                    // one `\n` would make the definition a lazy continuation
                    // of the paragraph instead of a block of its own.
                    Cow::Owned(format!("{}\n\n{definitions}", block.source.trim_end()))
                };
                let rendered = trim_leading_blank_lines(&render_block(&source, width));
                trim_end(&wrap_text_with_indent(&trim_line_ends(&rendered), width))
            }
        };

        if rendered.plain().trim().is_empty() && !block.source.trim().is_empty() {
            // The renderer produced nothing for this block alone — raw HTML
            // and a handful of other constructs do this. Fall back to the
            // block's own source rather than silently dropping it; the
            // whole-document fallback below only catches this when it
            // happens to be the only block.
            rendered = Text::new(block.source.trim_end());
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
        return Text::new(content);
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
/// guarantee hold for fixed blocks. Character-wise, using the same display
/// width accounting `wrap_text_with_indent` uses, so a cut never lands inside
/// a wide character; `Text::slice` re-maps style spans onto the cropped
/// range, so no span is disturbed by cutting mid-run.
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
///
/// No caller yet — bds-jgk.4.1 wires this into the `--context` search snippet.
#[allow(dead_code)]
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
/// The plain-text twin of this is `wrap_body` in `cli::commands::show`; the two
/// must agree, because the same issue body is rendered through one or the other
/// depending on output mode. Existing line breaks are preserved and a single
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
    fn split_top_level_blocks_classifies_each_block() {
        let src = "# Head\n\nPara one **bold**.\n\n- a\n- b\n\n```rust\nfn x() {}\n```\n\n| a | b |\n| - | - |\n| 1 | 2 |\n\n---\n\n> quote\n\nLast para.\n";

        let blocks = split_top_level_blocks(src);
        let kinds: Vec<BlockKind> = blocks.iter().map(|b| b.kind).collect();

        assert_eq!(
            kinds,
            vec![
                BlockKind::Prose, // heading
                BlockKind::Prose, // paragraph
                BlockKind::Prose, // list
                BlockKind::Fixed, // fenced code
                BlockKind::Fixed, // table
                BlockKind::Fixed, // rule
                BlockKind::Prose, // blockquote
                BlockKind::Prose, // paragraph
            ]
        );

        // The slices must be the real source text, not reconstructed markdown.
        assert_eq!(blocks[0].source, "# Head\n");
        assert_eq!(blocks[3].source, "```rust\nfn x() {}\n```");
        assert!(blocks[4].source.contains("| 1 | 2 |"));
    }

    #[test]
    fn split_top_level_blocks_handles_empty_and_whitespace_input() {
        assert!(split_top_level_blocks("").is_empty());
        assert!(split_top_level_blocks("   \n\n  \n").is_empty());
    }

    #[test]
    fn split_top_level_blocks_keeps_nested_lists_as_one_block() {
        let src = "- outer\n  - inner\n  - inner two\n- outer two\n";
        let blocks = split_top_level_blocks(src);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Prose);
        assert!(blocks[0].source.contains("inner two"));
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

    /// A table whose rendered width (44 columns, verified with an
    /// out-of-repo probe against this `rich_rust` version) exceeds the width
    /// requested below. `rich_rust` does not clamp a table's width to what
    /// was asked for, so a narrower table would make the `Fixed`/`Prose`
    /// routing untestable: both paths are a no-op when nothing needs to be
    /// wrapped or cropped.
    const WIDE_TABLE_SRC: &str = "| left column header | right column header |\n| ------------------- | -------------------- |\n| left value here | right value here |\n";

    #[test]
    fn render_markdown_text_does_not_break_a_table_row() {
        let out = render_markdown_text(WIDE_TABLE_SRC, 30);
        let lines: Vec<&str> = out.plain().lines().collect();

        // One physical line per table row (top border, header, separator,
        // value, bottom border). If `Fixed` blocks were routed through the
        // prose path instead, `wrap_text_with_indent` would break each
        // 44-column row at its internal spaces, growing this to 7 lines.
        assert_eq!(
            lines.len(),
            5,
            "a table row was split across lines: {:?}",
            out.plain()
        );

        let header = lines
            .iter()
            .find(|l| l.contains("left column header"))
            .expect("header row missing");
        assert!(
            header.starts_with('│'),
            "the header row lost its left border: {header:?}"
        );
    }

    #[test]
    fn render_markdown_text_crops_lines_of_an_over_wide_table_to_the_requested_width() {
        // `rich_rust` renders this table at its own natural width (44
        // columns) regardless of the narrower width requested here, so
        // `render_markdown_text` must crop each line itself, or a caller
        // (a panel) that assumes no emitted line exceeds the requested width
        // gets hard evidence to the contrary the first time it draws a
        // border around one.
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

        // A non-final fixed-block line keeps the trailing background-fill
        // padding `rich_rust` renders it with -- the contract for `Fixed`
        // blocks is "emitted verbatim, including any background fill",
        // and per-line trimming is `Prose`-only behaviour. If `BlockKind::Fixed`
        // were routed through the prose path instead, `trim_line_ends` would
        // strip that padding from every line, not just the block's last one.
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(lines[0]),
            40,
            "the code line lost its background-fill padding, or was wrapped: {:?}",
            lines[0]
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
