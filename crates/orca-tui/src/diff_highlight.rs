use std::collections::HashMap;
use std::path::Path;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::syntax_highlight::{
    LineHighlighter, MAX_HIGHLIGHT_BYTES, MAX_HIGHLIGHT_LINES, StyledSourceLine, SyntaxTheme,
    content_within_limits, highlighter_for_path,
};
use crate::theme::Theme;

const MAX_RENDERED_DIFF_LINES: usize = 80;
const TRUNCATION_MARKER: &str = "    [... diff truncated ...]";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiffLineKind {
    Context,
    Insert,
    Delete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiffSourceLine {
    pub(crate) kind: DiffLineKind,
    pub(crate) old_line: Option<usize>,
    pub(crate) new_line: Option<usize>,
    pub(crate) content: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DiffHunkEntry {
    Source(DiffSourceLine),
    Metadata(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiffHunk {
    pub(crate) header: String,
    pub(crate) entries: Vec<DiffHunkEntry>,
}

impl DiffHunk {
    pub(crate) fn source_lines(&self) -> impl Iterator<Item = &DiffSourceLine> {
        self.entries.iter().filter_map(|entry| match entry {
            DiffHunkEntry::Source(line) => Some(line),
            DiffHunkEntry::Metadata(_) => None,
        })
    }
}

/// Owns all parsed text so later tasks can transfer it to a background worker.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ParsedDiff {
    pub(crate) destination_path: Option<String>,
    pub(crate) has_multiple_files: bool,
    pub(crate) prelude: Vec<String>,
    pub(crate) hunks: Vec<DiffHunk>,
    pub(crate) aggregate_source_bytes: usize,
    pub(crate) aggregate_source_lines: usize,
}

pub(crate) type RefinedDiffStyles = HashMap<usize, StyledSourceLine>;

/// Worker-facing entry point; callers with a `ParsedDiff` must use this ambiguity guard.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn compute_parsed_diff_file_scoped_styles(
    path: &Path,
    file_text: &str,
    parsed: &ParsedDiff,
    theme: SyntaxTheme,
) -> Option<RefinedDiffStyles> {
    if parsed.has_multiple_files {
        return None;
    }
    compute_file_scoped_styles(path, file_text, &parsed.hunks, theme)
}

fn compute_file_scoped_styles(
    path: &Path,
    file_text: &str,
    hunks: &[DiffHunk],
    theme: SyntaxTheme,
) -> Option<RefinedDiffStyles> {
    compute_file_scoped_styles_with(path, file_text, hunks, theme, |highlighter, text| {
        highlighter.highlight_line(text)
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn compute_file_scoped_styles_with(
    path: &Path,
    file_text: &str,
    hunks: &[DiffHunk],
    theme: SyntaxTheme,
    mut highlight_line: impl FnMut(&mut LineHighlighter, &str) -> Option<StyledSourceLine>,
) -> Option<RefinedDiffStyles> {
    if !content_within_limits(file_text) {
        return None;
    }

    let mut expected = HashMap::new();
    for line in hunks.iter().flat_map(DiffHunk::source_lines) {
        if !matches!(line.kind, DiffLineKind::Context | DiffLineKind::Insert) {
            continue;
        }
        let line_number = line.new_line.filter(|line_number| *line_number > 0)?;
        if let Some(existing) = expected.insert(line_number, line.content.as_str())
            && existing != line.content
        {
            return None;
        }
    }

    if expected.is_empty() {
        return Some(RefinedDiffStyles::new());
    }

    let max_needed = expected.keys().copied().max()?;
    let mut highlighter = highlighter_for_path(path, theme)?;
    let mut refined = RefinedDiffStyles::with_capacity(expected.len());

    for (line_index, source_line) in file_text.split_inclusive('\n').enumerate() {
        let line_number = line_index + 1;
        if line_number > max_needed {
            break;
        }
        let text = source_line
            .strip_suffix('\n')
            .map_or(source_line, |text| text.strip_suffix('\r').unwrap_or(text));
        let expected_text = expected.get(&line_number);
        if expected_text.is_some_and(|expected_text| *expected_text != text) {
            return None;
        }
        let spans = highlight_line(&mut highlighter, text)?;
        if expected_text.is_some() {
            refined.insert(line_number, spans);
        }
        if line_number == max_needed {
            break;
        }
    }

    (refined.len() == expected.len()).then_some(refined)
}

struct HunkBuilder {
    hunk: DiffHunk,
    old_next: usize,
    new_next: usize,
    old_remaining: usize,
    new_remaining: usize,
}

impl HunkBuilder {
    fn new(
        header: String,
        old_start: usize,
        old_count: usize,
        new_start: usize,
        new_count: usize,
    ) -> Self {
        Self {
            hunk: DiffHunk {
                header,
                entries: Vec::new(),
            },
            old_next: old_start,
            new_next: new_start,
            old_remaining: old_count,
            new_remaining: new_count,
        }
    }

    fn is_complete(&self) -> bool {
        self.old_remaining == 0 && self.new_remaining == 0
    }

    fn source_line(&mut self, kind: DiffLineKind, content: String) -> DiffSourceLine {
        match kind {
            DiffLineKind::Context => {
                let line = DiffSourceLine {
                    kind,
                    old_line: Some(self.old_next),
                    new_line: Some(self.new_next),
                    content,
                };
                self.old_next = self.old_next.saturating_add(1);
                self.new_next = self.new_next.saturating_add(1);
                self.old_remaining = self.old_remaining.saturating_sub(1);
                self.new_remaining = self.new_remaining.saturating_sub(1);
                line
            }
            DiffLineKind::Insert => {
                let line = DiffSourceLine {
                    kind,
                    old_line: None,
                    new_line: Some(self.new_next),
                    content,
                };
                self.new_next = self.new_next.saturating_add(1);
                self.new_remaining = self.new_remaining.saturating_sub(1);
                line
            }
            DiffLineKind::Delete => {
                let line = DiffSourceLine {
                    kind,
                    old_line: Some(self.old_next),
                    new_line: None,
                    content,
                };
                self.old_next = self.old_next.saturating_add(1);
                self.old_remaining = self.old_remaining.saturating_sub(1);
                line
            }
        }
    }
}

fn parse_range(token: &str, marker: char) -> Option<(usize, usize)> {
    let coordinates = token.strip_prefix(marker)?;
    let mut parts = coordinates.split(',');
    let start = parts.next()?.parse().ok()?;
    let count = match parts.next() {
        Some(count) => count.parse().ok()?,
        None => 1,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((start, count))
}

fn parse_hunk_coordinates(header: &str) -> Option<(usize, usize, usize, usize)> {
    if !header.starts_with("@@") {
        return None;
    }
    let mut tokens = header.split_whitespace();
    if tokens.next()? != "@@" {
        return None;
    }
    let (old_start, old_count) = parse_range(tokens.next()?, '-')?;
    let (new_start, new_count) = parse_range(tokens.next()?, '+')?;
    if tokens.next()? != "@@" {
        return None;
    }
    Some((old_start, old_count, new_start, new_count))
}

fn parse_header_path(header_value: &str) -> Option<String> {
    let path = header_value
        .split_once('\t')
        .map_or(header_value, |(path, _)| path);
    if path == "/dev/null" {
        return None;
    }
    Some(
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path)
            .to_owned(),
    )
}

fn finish_hunk(current: &mut Option<HunkBuilder>, hunks: &mut Vec<DiffHunk>) {
    if let Some(builder) = current.take() {
        hunks.push(builder.hunk);
    }
}

pub(crate) fn parse_unified_diff(diff: &str) -> ParsedDiff {
    let mut parsed = ParsedDiff::default();
    let mut current: Option<HunkBuilder> = None;
    let mut old_path = None;
    let mut file_sections = 0usize;

    for line in diff.lines() {
        let hunk_is_active = current
            .as_ref()
            .is_some_and(|builder| !builder.is_complete());
        if hunk_is_active {
            let source = match line.as_bytes().first() {
                Some(b' ') => Some((DiffLineKind::Context, &line[1..])),
                Some(b'+') => Some((DiffLineKind::Insert, &line[1..])),
                Some(b'-') => Some((DiffLineKind::Delete, &line[1..])),
                _ => None,
            };
            if let Some((kind, content)) = source {
                let source_line = current
                    .as_mut()
                    .expect("active hunk")
                    .source_line(kind, content.to_owned());
                parsed.aggregate_source_bytes = parsed
                    .aggregate_source_bytes
                    .saturating_add(source_line.content.len());
                parsed.aggregate_source_lines = parsed.aggregate_source_lines.saturating_add(1);
                current
                    .as_mut()
                    .expect("active hunk")
                    .hunk
                    .entries
                    .push(DiffHunkEntry::Source(source_line));
                continue;
            }
        }

        if let Some((old_start, old_count, new_start, new_count)) = parse_hunk_coordinates(line) {
            finish_hunk(&mut current, &mut parsed.hunks);
            current = Some(HunkBuilder::new(
                line.to_owned(),
                old_start,
                old_count,
                new_start,
                new_count,
            ));
            continue;
        }

        if hunk_is_active {
            current
                .as_mut()
                .expect("active hunk")
                .hunk
                .entries
                .push(DiffHunkEntry::Metadata(line.to_owned()));
            continue;
        }

        if let Some(header_value) = line.strip_prefix("--- ") {
            file_sections = file_sections.saturating_add(1);
            parsed.has_multiple_files = file_sections > 1;
            old_path = parse_header_path(header_value);
            parsed.destination_path = old_path.clone();
        } else if let Some(header_value) = line.strip_prefix("+++ ") {
            parsed.destination_path = parse_header_path(header_value).or_else(|| old_path.clone());
        }

        if let Some(builder) = current.as_mut() {
            builder
                .hunk
                .entries
                .push(DiffHunkEntry::Metadata(line.to_owned()));
        } else {
            parsed.prelude.push(line.to_owned());
        }
    }

    finish_hunk(&mut current, &mut parsed.hunks);
    parsed
}

fn source_style(kind: DiffLineKind, theme: &Theme) -> (&'static str, ratatui::style::Color) {
    match kind {
        DiffLineKind::Context => ("     ", theme.muted),
        DiffLineKind::Insert => ("    +", theme.diff_add),
        DiffLineKind::Delete => ("    -", theme.diff_remove),
    }
}

fn unstructured_line_color(line: &str, theme: &Theme) -> ratatui::style::Color {
    if line.starts_with('+') && !line.starts_with("+++") {
        theme.diff_add
    } else if line.starts_with('-') && !line.starts_with("---") {
        theme.diff_remove
    } else if line.starts_with("@@") {
        theme.border
    } else {
        theme.muted
    }
}

fn refined_spans(
    line: &DiffSourceLine,
    refined: Option<&RefinedDiffStyles>,
) -> Option<StyledSourceLine> {
    if line.kind == DiffLineKind::Delete {
        return None;
    }
    let spans = refined?.get(&line.new_line?)?;
    let text = spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    (text == line.content).then(|| spans.clone())
}

fn rendered_source_line(
    line: &DiffSourceLine,
    syntax_spans: Option<StyledSourceLine>,
    refined: Option<&RefinedDiffStyles>,
    theme: &Theme,
) -> Line<'static> {
    let (prefix, fallback_color) = source_style(line.kind, theme);
    let mut spans = vec![Span::styled(prefix, Style::default().fg(fallback_color))];
    spans.extend(
        refined_spans(line, refined)
            .or(syntax_spans)
            .unwrap_or_else(|| {
                vec![Span::styled(
                    line.content.clone(),
                    Style::default().fg(fallback_color),
                )]
            }),
    );
    Line::from(spans)
}

#[derive(Clone, Copy)]
enum DiffSide {
    Old,
    New,
}

fn rendered_metadata_line(line: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        format!("    {line}"),
        Style::default().fg(theme.muted),
    ))
}

fn render_hunk_fallback(hunk: &DiffHunk, entry_budget: usize, theme: &Theme) -> Vec<Line<'static>> {
    hunk.entries
        .iter()
        .take(entry_budget)
        .map(|entry| match entry {
            DiffHunkEntry::Source(line) => rendered_source_line(line, None, None, theme),
            DiffHunkEntry::Metadata(line) => rendered_metadata_line(line, theme),
        })
        .collect()
}

fn render_hunk_with(
    hunk: &DiffHunk,
    entry_budget: usize,
    theme: &Theme,
    refined: Option<&RefinedDiffStyles>,
    mut highlight: impl FnMut(DiffSide, &str) -> Option<StyledSourceLine>,
) -> Vec<Line<'static>> {
    let visible_entries = hunk.entries.iter().take(entry_budget).collect::<Vec<_>>();
    let mut highlighted = Vec::with_capacity(visible_entries.len());
    for entry in &visible_entries {
        let DiffHunkEntry::Source(line) = entry else {
            continue;
        };
        let spans = match line.kind {
            DiffLineKind::Context => {
                if highlight(DiffSide::Old, &line.content).is_none() {
                    return render_hunk_fallback(hunk, entry_budget, theme);
                }
                highlight(DiffSide::New, &line.content)
            }
            DiffLineKind::Insert => highlight(DiffSide::New, &line.content),
            DiffLineKind::Delete => highlight(DiffSide::Old, &line.content),
        };
        let Some(spans) = spans else {
            return render_hunk_fallback(hunk, entry_budget, theme);
        };
        highlighted.push(spans);
    }

    let mut highlighted = highlighted.into_iter();
    visible_entries
        .into_iter()
        .map(|entry| match entry {
            DiffHunkEntry::Source(line) => rendered_source_line(
                line,
                Some(highlighted.next().expect("one style per source line")),
                refined,
                theme,
            ),
            DiffHunkEntry::Metadata(line) => rendered_metadata_line(line, theme),
        })
        .collect()
}

fn parsed_line_count(parsed: &ParsedDiff) -> usize {
    parsed.prelude.len()
        + parsed
            .hunks
            .iter()
            .map(|hunk| 1usize.saturating_add(hunk.entries.len()))
            .sum::<usize>()
}

fn syntax_is_eligible(parsed: &ParsedDiff) -> bool {
    !parsed.has_multiple_files
        && parsed.aggregate_source_bytes <= MAX_HIGHLIGHT_BYTES
        && parsed.aggregate_source_lines <= MAX_HIGHLIGHT_LINES
        && parsed
            .hunks
            .iter()
            .flat_map(DiffHunk::source_lines)
            .all(|line| content_within_limits(&line.content))
}

fn render_parsed_diff_with(
    parsed: &ParsedDiff,
    theme: &Theme,
    mut render_hunk: impl FnMut(&DiffHunk, usize) -> Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let total_lines = parsed_line_count(parsed);
    let mut remaining = MAX_RENDERED_DIFF_LINES;
    let mut rendered = Vec::with_capacity(total_lines.min(MAX_RENDERED_DIFF_LINES) + 1);

    for line in &parsed.prelude {
        if remaining == 0 {
            break;
        }
        rendered.push(Line::from(Span::styled(
            format!("    {line}"),
            Style::default().fg(unstructured_line_color(line, theme)),
        )));
        remaining -= 1;
    }

    if remaining > 0 {
        'hunks: for hunk in &parsed.hunks {
            if remaining == 0 {
                break;
            }
            rendered.push(Line::from(Span::styled(
                format!("    {}", hunk.header),
                Style::default().fg(theme.border),
            )));
            remaining -= 1;
            if remaining == 0 {
                break 'hunks;
            }

            let entry_budget = remaining.min(hunk.entries.len());
            let hunk_lines = render_hunk(hunk, entry_budget);
            assert_eq!(
                hunk_lines.len(),
                entry_budget,
                "bounded hunk renderer must preserve every admitted entry"
            );
            rendered.extend(hunk_lines);
            remaining -= entry_budget;
            if remaining == 0 {
                break 'hunks;
            }
        }
    }

    if total_lines > MAX_RENDERED_DIFF_LINES {
        rendered.push(Line::from(Span::styled(
            TRUNCATION_MARKER,
            Style::default().fg(theme.muted),
        )));
    }
    rendered
}

pub(crate) fn render_parsed_diff(
    parsed: &ParsedDiff,
    theme: &Theme,
    refined: Option<&RefinedDiffStyles>,
) -> Vec<Line<'static>> {
    let syntax_eligible = syntax_is_eligible(parsed);
    let syntax_path = syntax_eligible
        .then(|| parsed.destination_path.as_deref())
        .flatten();
    let refined = syntax_eligible.then_some(refined).flatten();

    render_parsed_diff_with(parsed, theme, |hunk, entry_budget| {
        syntax_path
            .and_then(|path| {
                let mut old = highlighter_for_path(Path::new(path), theme.syntax_theme)?;
                let mut new = highlighter_for_path(Path::new(path), theme.syntax_theme)?;
                Some(render_hunk_with(
                    hunk,
                    entry_budget,
                    theme,
                    refined,
                    |side, content| match side {
                        DiffSide::Old => old.highlight_line(content),
                        DiffSide::New => new.highlight_line(content),
                    },
                ))
            })
            .unwrap_or_else(|| render_hunk_fallback(hunk, entry_budget, theme))
    })
}

pub(crate) fn render_unified_diff(
    diff: &str,
    theme: &Theme,
    refined: Option<&RefinedDiffStyles>,
) -> Vec<Line<'static>> {
    render_parsed_diff(&parse_unified_diff(diff), theme, refined)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::Path;

    use orca_core::config::ThemeName;
    use orca_core::tool_types::FileChangePreview;
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};

    use super::{
        DiffHunk, DiffHunkEntry, DiffLineKind, DiffSourceLine, RefinedDiffStyles,
        compute_file_scoped_styles, compute_parsed_diff_file_scoped_styles, parse_unified_diff,
        render_parsed_diff, render_unified_diff,
    };
    use crate::syntax_highlight::{
        MAX_HIGHLIGHT_BYTES, MAX_HIGHLIGHT_LINE_BYTES, MAX_HIGHLIGHT_LINES, StyledSourceLine,
        highlighter_for_path,
    };
    use crate::theme::Theme;

    const RUST_DIFF: &str = "\
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,3 @@
-fn old() { let value = \"old\"; }
+fn new() { let value = \"new\"; }
 context();
";

    fn dark_theme() -> Theme {
        Theme::named(ThemeName::Dark)
    }

    fn rendered_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn find_rendered_line<'a>(lines: &'a [Line<'static>], needle: &str) -> &'a Line<'static> {
        lines
            .iter()
            .find(|line| rendered_text(line).contains(needle))
            .unwrap_or_else(|| panic!("rendered line containing {needle:?}"))
    }

    fn highlight_sequence(path: &str, source: &[&str], theme: &Theme) -> Vec<StyledSourceLine> {
        let mut highlighter =
            highlighter_for_path(Path::new(path), theme.syntax_theme).expect("known syntax");
        source
            .iter()
            .map(|line| highlighter.highlight_line(line).expect("highlighted line"))
            .collect()
    }

    fn assert_plain_source(
        line: &Line<'_>,
        prefix: &str,
        content: &str,
        color: ratatui::style::Color,
    ) {
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].content.as_ref(), prefix);
        assert_eq!(line.spans[0].style.fg, Some(color));
        assert_eq!(line.spans[1].content.as_ref(), content);
        assert_eq!(line.spans[1].style.fg, Some(color));
    }

    fn padded_rust_line(len: usize) -> String {
        let start = "let value = \"syntax\";";
        format!("{start}{}", " ".repeat(len - start.len()))
    }

    #[test]
    fn parser_tracks_destination_and_old_new_line_numbers() {
        let parsed = parse_unified_diff(RUST_DIFF);

        assert_eq!(parsed.destination_path.as_deref(), Some("src/main.rs"));
        assert_eq!(parsed.hunks.len(), 1);
        let source = parsed.hunks[0].source_lines().collect::<Vec<_>>();
        assert_eq!(
            source
                .iter()
                .map(|line| (
                    line.kind,
                    line.old_line,
                    line.new_line,
                    line.content.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    DiffLineKind::Delete,
                    Some(1),
                    None,
                    "fn old() { let value = \"old\"; }",
                ),
                (
                    DiffLineKind::Insert,
                    None,
                    Some(1),
                    "fn new() { let value = \"new\"; }",
                ),
                (DiffLineKind::Context, Some(2), Some(2), "context();",),
            ]
        );
        assert_eq!(
            parsed.aggregate_source_bytes,
            source.iter().map(|line| line.content.len()).sum::<usize>()
        );
        assert_eq!(parsed.aggregate_source_lines, 3);
    }

    #[test]
    fn generated_context_that_looks_like_hunk_header_stays_source() {
        let preview = orca_tools::file_admission::build_file_change_preview(
            "value.rs",
            Some("before\n@@ -1 +1 @@\nold\n"),
            Some("before\n@@ -1 +1 @@\nnew\n"),
        );
        let FileChangePreview::UnifiedDiff { text: diff, .. } = preview else {
            panic!("small text diff should produce unified diff");
        };
        assert!(
            diff.lines().any(|line| line == " @@ -1 +1 @@"),
            "fixture must contain the hunk-like context line:\n{diff}"
        );

        let parsed = parse_unified_diff(&diff);

        assert_eq!(parsed.hunks.len(), 1, "generated diff:\n{diff}");
        assert_eq!(
            parsed.hunks[0]
                .source_lines()
                .map(|line| (
                    line.kind,
                    line.old_line,
                    line.new_line,
                    line.content.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                (DiffLineKind::Context, Some(1), Some(1), "before"),
                (DiffLineKind::Context, Some(2), Some(2), "@@ -1 +1 @@"),
                (DiffLineKind::Delete, Some(3), None, "old"),
                (DiffLineKind::Insert, None, Some(3), "new"),
            ]
        );
    }

    #[test]
    fn headerless_fragment_keeps_legacy_add_remove_and_hunk_colors() {
        let theme = dark_theme();

        let rendered = render_unified_diff("-old\n+new\n@@ marker", &theme, None);

        assert_eq!(rendered.len(), 3);
        assert_eq!(rendered_text(&rendered[0]), "    -old");
        assert_eq!(rendered[0].spans[0].style.fg, Some(theme.diff_remove));
        assert_eq!(rendered_text(&rendered[1]), "    +new");
        assert_eq!(rendered[1].spans[0].style.fg, Some(theme.diff_add));
        assert_eq!(rendered_text(&rendered[2]), "    @@ marker");
        assert_eq!(rendered[2].spans[0].style.fg, Some(theme.border));
    }

    #[test]
    fn multiple_file_sections_disable_syntax_and_refined_overlays() {
        let theme = dark_theme();
        let first_insert = "fn first() { let value = 1; }";
        let diff = format!(
            "\
--- a/first.rs
+++ b/first.rs
@@ -1 +1 @@
-fn old() {{}}
+{first_insert}
--- a/second.py
+++ b/second.py
@@ -1 +1 @@
-value = 0
+value = 1
"
        );
        let mut refined = RefinedDiffStyles::new();
        refined.insert(
            1,
            vec![Span::styled(
                first_insert,
                Style::default().fg(ratatui::style::Color::Magenta),
            )],
        );

        let rendered = render_unified_diff(&diff, &theme, Some(&refined));
        let first = find_rendered_line(&rendered, "+fn first");
        let second = find_rendered_line(&rendered, "+value = 1");

        assert_plain_source(first, "    +", first_insert, theme.diff_add);
        assert_plain_source(second, "    +", "value = 1", theme.diff_add);
        assert!(
            rendered
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.fg != Some(ratatui::style::Color::Magenta))
        );
        assert_eq!(
            rendered.iter().map(rendered_text).collect::<Vec<_>>(),
            diff.lines()
                .map(|line| format!("    {line}"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn parsed_diff_file_scoped_styles_reject_multiple_files() {
        let theme = dark_theme();
        let file_text = "value = 1\n";
        let parsed = parse_unified_diff(
            "\
--- a/first.py
+++ b/first.py
@@ -1 +1 @@
 value = 1
--- a/second.py
+++ b/second.py
@@ -1 +1 @@
 value = 1
",
        );

        assert!(parsed.has_multiple_files);
        assert!(
            compute_file_scoped_styles(
                Path::new("first.py"),
                file_text,
                &parsed.hunks,
                theme.syntax_theme,
            )
            .is_some(),
            "low-level hunk API intentionally has no ParsedDiff ambiguity guard"
        );
        assert!(
            compute_parsed_diff_file_scoped_styles(
                Path::new("first.py"),
                file_text,
                &parsed,
                theme.syntax_theme,
            )
            .is_none()
        );
    }

    #[test]
    fn destination_extension_wins_for_rename_and_dev_null_uses_real_path() {
        let renamed = parse_unified_diff(
            "\
--- a/src/value.unknown\t2026-07-24
+++ b/src/value.py\t2026-07-24
@@ -1 +1 @@
-old
+new
",
        );
        let added = parse_unified_diff(
            "\
--- /dev/null
+++ b/src/added.rs
@@ -0,0 +1 @@
+fn added() {}
",
        );
        let deleted = parse_unified_diff(
            "\
--- a/src/deleted.py
+++ /dev/null
@@ -1 +0,0 @@
-print('deleted')
",
        );

        assert_eq!(renamed.destination_path.as_deref(), Some("src/value.py"));
        assert_eq!(added.destination_path.as_deref(), Some("src/added.rs"));
        assert_eq!(deleted.destination_path.as_deref(), Some("src/deleted.py"));
    }

    #[test]
    fn file_header_markers_inside_hunks_are_source_lines() {
        let parsed = parse_unified_diff(
            "\
--- a/value.txt
+++ b/value.txt
@@ -1 +1 @@
--- old heading
+++ new heading
",
        );
        let source = parsed.hunks[0].source_lines().collect::<Vec<_>>();

        assert_eq!(source[0].kind, DiffLineKind::Delete);
        assert_eq!(source[0].content, "-- old heading");
        assert_eq!(source[1].kind, DiffLineKind::Insert);
        assert_eq!(source[1].content, "++ new heading");
    }

    #[test]
    fn metadata_lines_remain_in_exact_render_order() {
        let theme = dark_theme();
        let diff = concat!(
            "diff --git a/value.rs b/value.rs\n",
            "--- a/value.rs\n",
            "+++ b/value.rs\n",
            "@@ -1 +1 @@\n",
            "-let value = 1;\n",
            "\\ No newline at end of file\n",
            "+let value = 2;\n",
            "malformed metadata\n",
        );

        let parsed = parse_unified_diff(diff);
        assert!(matches!(
            &parsed.hunks[0].entries[1],
            DiffHunkEntry::Metadata(line) if line == "\\ No newline at end of file"
        ));
        let rendered = render_unified_diff(diff, &theme, None)
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "    diff --git a/value.rs b/value.rs",
                "    --- a/value.rs",
                "    +++ b/value.rs",
                "    @@ -1 +1 @@",
                "    -let value = 1;",
                "    \\ No newline at end of file",
                "    +let value = 2;",
                "    malformed metadata",
            ]
        );
    }

    #[test]
    fn rust_source_render_keeps_exact_prefix_and_tokenizes_content_without_marker() {
        let theme = dark_theme();
        let lines = render_unified_diff(RUST_DIFF, &theme, None);
        let inserted = find_rendered_line(&lines, "+fn new");

        assert_eq!(inserted.spans[0].content.as_ref(), "    +");
        assert_eq!(inserted.spans[0].style.fg, Some(theme.diff_add));
        assert_eq!(
            inserted.spans[1..]
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "fn new() { let value = \"new\"; }"
        );
        assert!(
            !inserted.spans[1..]
                .iter()
                .any(|span| span.content.starts_with('+'))
        );
        let token_foregrounds = inserted.spans[1..]
            .iter()
            .filter_map(|span| span.style.fg)
            .collect::<HashSet<_>>();
        assert!(token_foregrounds.len() >= 2);
    }

    #[test]
    fn old_and_new_hunk_parsers_are_independent() {
        let theme = dark_theme();
        let diff = "\
--- a/item.py
+++ b/item.py
@@ -1,2 +1,2 @@
-\"\"\"old string
-still old
+value = 1
+print(value)
";

        let lines = render_unified_diff(diff, &theme, None);
        let deleted = find_rendered_line(&lines, "-still old");
        let value = find_rendered_line(&lines, "+value = 1");
        let print = find_rendered_line(&lines, "+print(value)");
        let old_expected =
            highlight_sequence("item.py", &["\"\"\"old string", "still old"], &theme);
        let new_expected = highlight_sequence("item.py", &["value = 1", "print(value)"], &theme);
        let leaked = highlight_sequence(
            "item.py",
            &["\"\"\"old string", "still old", "value = 1"],
            &theme,
        );

        assert_eq!(deleted.spans[1..], old_expected[1]);
        assert_eq!(value.spans[1..], new_expected[0]);
        assert_eq!(print.spans[1..], new_expected[1]);
        assert_ne!(value.spans[1..], leaked[2]);
    }

    #[test]
    fn context_advances_both_parsers_before_delete_and_insert() {
        let theme = dark_theme();
        let diff = "\
--- a/item.py
+++ b/item.py
@@ -1,2 +1,2 @@
 \"\"\"shared
-old tail\"\"\"
+new tail\"\"\"
";

        let lines = render_unified_diff(diff, &theme, None);
        let deleted = find_rendered_line(&lines, "-old tail");
        let inserted = find_rendered_line(&lines, "+new tail");
        let old_expected =
            highlight_sequence("item.py", &["\"\"\"shared", "old tail\"\"\""], &theme);
        let new_expected =
            highlight_sequence("item.py", &["\"\"\"shared", "new tail\"\"\""], &theme);
        let fresh_old = highlight_sequence("item.py", &["old tail\"\"\""], &theme);
        let fresh_new = highlight_sequence("item.py", &["new tail\"\"\""], &theme);

        assert_eq!(deleted.spans[1..], old_expected[1]);
        assert_eq!(inserted.spans[1..], new_expected[1]);
        assert_ne!(deleted.spans[1..], fresh_old[0]);
        assert_ne!(inserted.spans[1..], fresh_new[0]);
    }

    #[test]
    fn one_parser_failure_falls_back_the_entire_hunk() {
        let theme = dark_theme();
        let hunk = super::DiffHunk {
            header: "@@ -1 +1 @@".to_owned(),
            entries: vec![
                DiffHunkEntry::Source(super::DiffSourceLine {
                    kind: DiffLineKind::Insert,
                    old_line: None,
                    new_line: Some(1),
                    content: "let new = 2;".to_owned(),
                }),
                DiffHunkEntry::Source(super::DiffSourceLine {
                    kind: DiffLineKind::Delete,
                    old_line: Some(1),
                    new_line: None,
                    content: "let old = 1;".to_owned(),
                }),
            ],
        };
        let mut highlighted_new = false;

        let rendered =
            super::render_hunk_with(&hunk, hunk.entries.len(), &theme, None, |side, content| {
                match side {
                    super::DiffSide::New => {
                        highlighted_new = true;
                        Some(vec![Span::styled(
                            content.to_owned(),
                            Style::default().fg(ratatui::style::Color::Magenta),
                        )])
                    }
                    super::DiffSide::Old => {
                        assert!(
                            highlighted_new,
                            "old-side failure must happen after a successful new-side line"
                        );
                        None
                    }
                }
            });

        assert_plain_source(&rendered[0], "    +", "let new = 2;", theme.diff_add);
        assert_plain_source(&rendered[1], "    -", "let old = 1;", theme.diff_remove);
    }

    #[test]
    fn parsed_render_does_not_highlight_hunk_entries_beyond_visible_budget() {
        let theme = dark_theme();
        let diff = format!(
            "--- a/value.rs\n+++ b/value.rs\n@@ -0,0 +1,100 @@\n{}",
            (0..100)
                .map(|index| format!("+let value_{index} = {index};\n"))
                .collect::<String>()
        );
        let parsed = parse_unified_diff(&diff);
        let mut highlight_calls = 0usize;
        let visible_source_entries =
            super::MAX_RENDERED_DIFF_LINES - parsed.prelude.len() - parsed.hunks.len();

        let rendered = super::render_parsed_diff_with(&parsed, &theme, |hunk, entry_budget| {
            super::render_hunk_with(hunk, entry_budget, &theme, None, |_side, content| {
                highlight_calls += 1;
                Some(vec![Span::raw(content.to_owned())])
            })
        });

        assert_eq!(rendered.len(), 81);
        assert_eq!(rendered_text(&rendered[79]), "    +let value_76 = 76;");
        assert_eq!(rendered_text(&rendered[80]), "    [... diff truncated ...]");
        assert_eq!(highlight_calls, visible_source_entries);
    }

    #[test]
    fn hunk_boundary_resets_multiline_parser_state() {
        let theme = dark_theme();
        let diff = "\
--- a/item.py
+++ b/item.py
@@ -1 +1 @@
 \"\"\"open string
@@ -20,0 +20 @@
+value = 1
";

        let lines = render_unified_diff(diff, &theme, None);
        let inserted = find_rendered_line(&lines, "+value = 1");
        let fresh = highlight_sequence("item.py", &["value = 1"], &theme);
        let leaked = highlight_sequence("item.py", &["\"\"\"open string", "value = 1"], &theme);

        assert_eq!(inserted.spans[1..], fresh[0]);
        assert_ne!(inserted.spans[1..], leaked[1]);
    }

    #[test]
    fn metadata_does_not_advance_parser_state() {
        let theme = dark_theme();
        let diff = "\
--- a/item.py
+++ b/item.py
@@ -1 +1,2 @@
 \"\"\"open string
\"\"\"
+value = 1
";

        let lines = render_unified_diff(diff, &theme, None);
        let inserted = find_rendered_line(&lines, "+value = 1");
        let expected = highlight_sequence("item.py", &["\"\"\"open string", "value = 1"], &theme);
        let advanced = highlight_sequence(
            "item.py",
            &["\"\"\"open string", "\"\"\"", "value = 1"],
            &theme,
        );

        assert_eq!(inserted.spans[1..], expected[1]);
        assert_ne!(inserted.spans[1..], advanced[2]);
    }

    #[test]
    fn aggregate_byte_limit_is_strict_and_boundary_remains_eligible() {
        let theme = dark_theme();
        let source = padded_rust_line(MAX_HIGHLIGHT_LINE_BYTES);
        let exact_diff = format!(
            "--- /dev/null\n+++ b/value.rs\n@@ -0,0 +1,128 @@\n{}",
            (0..(MAX_HIGHLIGHT_BYTES / MAX_HIGHLIGHT_LINE_BYTES))
                .map(|_| format!("+{source}\n"))
                .collect::<String>()
        );
        let over_diff = format!(
            "--- /dev/null\n+++ b/value.rs\n@@ -0,0 +1,129 @@\n{}+x\n",
            (0..(MAX_HIGHLIGHT_BYTES / MAX_HIGHLIGHT_LINE_BYTES))
                .map(|_| format!("+{source}\n"))
                .collect::<String>()
        );

        let exact_parsed = parse_unified_diff(&exact_diff);
        assert_eq!(exact_parsed.aggregate_source_bytes, MAX_HIGHLIGHT_BYTES);
        let exact = render_unified_diff(&exact_diff, &theme, None);
        let exact_source = find_rendered_line(&exact, "+let value");
        assert!(exact_source.spans.len() > 2);

        let over_parsed = parse_unified_diff(&over_diff);
        assert_eq!(over_parsed.aggregate_source_bytes, MAX_HIGHLIGHT_BYTES + 1);
        let over = render_unified_diff(&over_diff, &theme, None);
        let over_source = find_rendered_line(&over, "+let value");
        assert_plain_source(over_source, "    +", &source, theme.diff_add);
    }

    #[test]
    fn aggregate_source_line_limit_is_strict_and_boundary_remains_eligible() {
        let theme = dark_theme();
        let first = "let value = \"syntax\";";
        let exact_diff = format!(
            "--- /dev/null\n+++ b/value.rs\n@@ -0,0 +1,{MAX_HIGHLIGHT_LINES} @@\n+{first}\n{}",
            "+x\n".repeat(MAX_HIGHLIGHT_LINES - 1)
        );
        let over_diff = format!(
            "--- /dev/null\n+++ b/value.rs\n@@ -0,0 +1,{} @@\n+{first}\n{}",
            MAX_HIGHLIGHT_LINES + 1,
            "+x\n".repeat(MAX_HIGHLIGHT_LINES)
        );

        assert_eq!(
            parse_unified_diff(&exact_diff).aggregate_source_lines,
            MAX_HIGHLIGHT_LINES
        );
        let exact = render_unified_diff(&exact_diff, &theme, None);
        assert!(find_rendered_line(&exact, "+let value").spans.len() > 2);

        assert_eq!(
            parse_unified_diff(&over_diff).aggregate_source_lines,
            MAX_HIGHLIGHT_LINES + 1
        );
        let over = render_unified_diff(&over_diff, &theme, None);
        assert_plain_source(
            find_rendered_line(&over, "+let value"),
            "    +",
            first,
            theme.diff_add,
        );
    }

    #[test]
    fn source_line_byte_limit_is_strict_and_boundary_remains_eligible() {
        let theme = dark_theme();
        let exact_source = padded_rust_line(MAX_HIGHLIGHT_LINE_BYTES);
        let over_source = format!("{exact_source}x");
        let exact_diff = format!("--- /dev/null\n+++ b/value.rs\n@@ -0,0 +1 @@\n+{exact_source}\n");
        let over_diff = format!("--- /dev/null\n+++ b/value.rs\n@@ -0,0 +1 @@\n+{over_source}\n");

        let exact = render_unified_diff(&exact_diff, &theme, None);
        assert!(find_rendered_line(&exact, "+let value").spans.len() > 2);

        let over = render_unified_diff(&over_diff, &theme, None);
        assert_plain_source(
            find_rendered_line(&over, "+let value"),
            "    +",
            &over_source,
            theme.diff_add,
        );
    }

    #[test]
    fn aggregate_guardrails_also_disable_refined_overlays() {
        let theme = dark_theme();
        let over_source = padded_rust_line(MAX_HIGHLIGHT_LINE_BYTES + 1);
        let diff = format!("--- /dev/null\n+++ b/value.rs\n@@ -0,0 +1 @@\n+{over_source}\n");
        let mut refined = RefinedDiffStyles::new();
        refined.insert(
            1,
            vec![Span::styled(
                over_source.clone(),
                Style::default().fg(ratatui::style::Color::Magenta),
            )],
        );

        let rendered = render_unified_diff(&diff, &theme, Some(&refined));

        assert_plain_source(
            find_rendered_line(&rendered, "+let value"),
            "    +",
            &over_source,
            theme.diff_add,
        );
    }

    #[test]
    fn unknown_extension_uses_plain_class_colors() {
        let theme = dark_theme();
        let diff = "\
--- a/value.unknown
+++ b/value.unknown
@@ -1,2 +1,2 @@
-old value
+new value
 shared value
";

        let lines = render_unified_diff(diff, &theme, None);

        assert_plain_source(
            find_rendered_line(&lines, "-old value"),
            "    -",
            "old value",
            theme.diff_remove,
        );
        assert_plain_source(
            find_rendered_line(&lines, "+new value"),
            "    +",
            "new value",
            theme.diff_add,
        );
        assert_plain_source(
            find_rendered_line(&lines, "shared value"),
            "     ",
            "shared value",
            theme.muted,
        );
    }

    #[test]
    fn truncation_consumes_eighty_original_lines_and_keeps_exact_marker() {
        let theme = dark_theme();
        let diff = (0..81)
            .map(|index| format!("metadata {index:02}"))
            .collect::<Vec<_>>()
            .join("\n");

        let lines = render_unified_diff(&diff, &theme, None);

        assert_eq!(lines.len(), 81);
        assert_eq!(rendered_text(&lines[0]), "    metadata 00");
        assert_eq!(rendered_text(&lines[79]), "    metadata 79");
        assert_eq!(rendered_text(&lines[80]), "    [... diff truncated ...]");
        assert_eq!(lines[80].spans[0].style.fg, Some(theme.muted));
        assert!(
            !lines
                .iter()
                .any(|line| rendered_text(line).contains("metadata 80"))
        );
    }

    #[test]
    fn malformed_hunk_header_stays_plain_metadata_without_panicking() {
        let theme = dark_theme();
        let diff = "\
--- a/value.rs
+++ b/value.rs
@@ malformed coordinates @@
+let value = 1;
";

        let parsed = parse_unified_diff(diff);
        assert!(parsed.hunks.is_empty());
        assert_eq!(
            parsed.prelude,
            vec![
                "--- a/value.rs",
                "+++ b/value.rs",
                "@@ malformed coordinates @@",
                "+let value = 1;",
            ]
        );
        let lines = render_unified_diff(diff, &theme, None);
        let malformed = find_rendered_line(&lines, "@@ malformed");
        assert_eq!(malformed.spans[0].style.fg, Some(theme.border));
    }

    #[test]
    fn crlf_structural_endings_are_not_kept_in_source_content() {
        let parsed = parse_unified_diff(
            "--- a/value.rs\r\n+++ b/value.rs\r\n@@ -1 +1 @@\r\n-old\r\n+new\r\n",
        );
        let source = parsed.hunks[0].source_lines().collect::<Vec<_>>();

        assert_eq!(source[0].content, "old");
        assert_eq!(source[1].content, "new");
    }

    #[test]
    fn refined_styles_overlay_only_exact_new_side_content() {
        let theme = dark_theme();
        let diff = "\
--- a/value.rs
+++ b/value.rs
@@ -1,2 +1,2 @@
-let old = 1;
+let new = 2;
 shared();
";
        let mut refined = RefinedDiffStyles::new();
        refined.insert(
            1,
            vec![Span::styled(
                "let new = 2;",
                Style::default().fg(ratatui::style::Color::Magenta),
            )],
        );
        refined.insert(
            2,
            vec![Span::styled(
                "wrong text",
                Style::default().fg(ratatui::style::Color::Cyan),
            )],
        );

        let rendered = render_unified_diff(diff, &theme, Some(&refined));
        let deleted = find_rendered_line(&rendered, "-let old");
        let inserted = find_rendered_line(&rendered, "+let new");
        let context = find_rendered_line(&rendered, "shared()");

        assert_eq!(
            inserted.spans[1].style.fg,
            Some(ratatui::style::Color::Magenta)
        );
        assert_ne!(
            deleted.spans[1].style.fg,
            Some(ratatui::style::Color::Magenta)
        );
        assert_ne!(context.spans[1].style.fg, Some(ratatui::style::Color::Cyan));

        let mut delete_only: RefinedDiffStyles = HashMap::new();
        delete_only.insert(
            1,
            vec![Span::styled(
                "let old = 1;",
                Style::default().fg(ratatui::style::Color::Yellow),
            )],
        );
        let delete_rendered = render_unified_diff(diff, &theme, Some(&delete_only));
        assert_ne!(
            find_rendered_line(&delete_rendered, "-let old").spans[1]
                .style
                .fg,
            Some(ratatui::style::Color::Yellow)
        );
    }

    #[test]
    fn full_file_python_scope_warms_refined_field_styles_from_line_one() {
        let theme = dark_theme();
        let file_text = "\
class Item:
    \"\"\"Summary.
    \"\"\"
    field = 1
";
        let diff = "\
--- a/item.py
+++ b/item.py
@@ -3,2 +3,2 @@
     \"\"\"
-    field = 0
+    field = 1
";
        let parsed = parse_unified_diff(diff);

        let refined = compute_file_scoped_styles(
            Path::new("item.py"),
            file_text,
            &parsed.hunks,
            theme.syntax_theme,
        )
        .expect("verified full-file styles");
        let cold = render_parsed_diff(&parsed, &theme, None);
        let warm = render_parsed_diff(&parsed, &theme, Some(&refined));
        let cold_field = find_rendered_line(&cold, "+    field = 1");
        let warm_field = find_rendered_line(&warm, "+    field = 1");
        let direct = highlight_sequence(
            "item.py",
            &[
                "class Item:",
                "    \"\"\"Summary.",
                "    \"\"\"",
                "    field = 1",
            ],
            &theme,
        );

        assert_eq!(refined.len(), 2);
        assert!(refined.contains_key(&3));
        assert!(refined.contains_key(&4));
        assert_ne!(warm_field.spans[1..], cold_field.spans[1..]);
        assert_eq!(refined.get(&4), Some(&direct[3]));
        assert_eq!(warm_field.spans[1..], direct[3]);
    }

    #[test]
    fn file_text_drift_rejects_the_entire_full_file_style_map() {
        let theme = dark_theme();
        let parsed = parse_unified_diff(
            "\
--- a/item.py
+++ b/item.py
@@ -3,2 +3,2 @@
     \"\"\"
-    field = 0
+    field = 1
",
        );
        let drifted_file_text = "\
class Item:
    \"\"\"Summary.
    \"\"\"
    field = 2
";

        assert!(
            compute_file_scoped_styles(
                Path::new("item.py"),
                drifted_file_text,
                &parsed.hunks,
                theme.syntax_theme,
            )
            .is_none()
        );
    }

    #[test]
    fn full_file_refinement_preserves_delete_line_spans() {
        let theme = dark_theme();
        let file_text = "\
class Item:
    \"\"\"Summary.
    \"\"\"
    field = 1
";
        let parsed = parse_unified_diff(
            "\
--- a/item.py
+++ b/item.py
@@ -3,2 +3,2 @@
     \"\"\"
-    field = 0
+    field = 1
",
        );
        let refined = compute_file_scoped_styles(
            Path::new("item.py"),
            file_text,
            &parsed.hunks,
            theme.syntax_theme,
        )
        .expect("verified full-file styles");

        let cold = render_parsed_diff(&parsed, &theme, None);
        let warm = render_parsed_diff(&parsed, &theme, Some(&refined));

        assert_eq!(
            find_rendered_line(&warm, "-    field = 0").spans,
            find_rendered_line(&cold, "-    field = 0").spans
        );
    }

    #[test]
    fn full_file_duplicate_new_lines_reject_conflicts_and_dedupe_identical_text() {
        let theme = dark_theme();
        let conflicting = parse_unified_diff(
            "\
--- a/value.py
+++ b/value.py
@@ -1 +1 @@
 value = 1
@@ -1 +1 @@
 value = 2
",
        );
        let identical = parse_unified_diff(
            "\
--- a/value.py
+++ b/value.py
@@ -1 +1 @@
 value = 1
@@ -1 +1 @@
 value = 1
",
        );

        assert!(
            compute_file_scoped_styles(
                Path::new("value.py"),
                "value = 1\n",
                &conflicting.hunks,
                theme.syntax_theme,
            )
            .is_none()
        );

        let deduped = compute_file_scoped_styles(
            Path::new("value.py"),
            "value = 1\n",
            &identical.hunks,
            theme.syntax_theme,
        )
        .expect("identical duplicate");
        assert_eq!(deduped.len(), 1);
        assert!(deduped.contains_key(&1));
    }

    #[test]
    fn full_file_invalid_new_line_none_is_rejected() {
        let theme = dark_theme();
        let hunks = vec![DiffHunk {
            header: "@@ -1 +1 @@".to_owned(),
            entries: vec![DiffHunkEntry::Source(DiffSourceLine {
                kind: DiffLineKind::Context,
                old_line: Some(1),
                new_line: None,
                content: "value = 1".to_owned(),
            })],
        }];

        assert!(
            compute_file_scoped_styles(
                Path::new("value.py"),
                "value = 1\n",
                &hunks,
                theme.syntax_theme,
            )
            .is_none()
        );
    }

    #[test]
    fn full_file_invalid_new_line_zero_is_rejected() {
        let theme = dark_theme();
        let hunks = vec![DiffHunk {
            header: "@@ -0,0 +0 @@".to_owned(),
            entries: vec![DiffHunkEntry::Source(DiffSourceLine {
                kind: DiffLineKind::Insert,
                old_line: None,
                new_line: Some(0),
                content: "value = 1".to_owned(),
            })],
        }];

        assert!(
            compute_file_scoped_styles(
                Path::new("value.py"),
                "value = 1\n",
                &hunks,
                theme.syntax_theme,
            )
            .is_none()
        );
    }

    #[test]
    fn full_file_invalid_new_line_mixed_with_valid_rejects_entire_map() {
        let theme = dark_theme();
        let hunks = vec![DiffHunk {
            header: "@@ -1,2 +1,2 @@".to_owned(),
            entries: vec![
                DiffHunkEntry::Source(DiffSourceLine {
                    kind: DiffLineKind::Insert,
                    old_line: None,
                    new_line: Some(1),
                    content: "value = 1".to_owned(),
                }),
                DiffHunkEntry::Source(DiffSourceLine {
                    kind: DiffLineKind::Context,
                    old_line: Some(2),
                    new_line: None,
                    content: "ignored = 2".to_owned(),
                }),
            ],
        }];

        assert!(
            compute_file_scoped_styles(
                Path::new("value.py"),
                "value = 1\nignored = 2\n",
                &hunks,
                theme.syntax_theme,
            )
            .is_none()
        );
    }

    #[test]
    fn full_file_missing_expected_line_beyond_eof_is_rejected() {
        let theme = dark_theme();
        let parsed = parse_unified_diff(
            "\
--- a/value.py
+++ b/value.py
@@ -2 +2 @@
 expected = 2
",
        );

        assert!(
            compute_file_scoped_styles(
                Path::new("value.py"),
                "only_line = 1\n",
                &parsed.hunks,
                theme.syntax_theme,
            )
            .is_none()
        );
    }

    #[test]
    fn full_file_delete_only_hunks_produce_an_empty_style_map() {
        let theme = dark_theme();
        let parsed = parse_unified_diff(
            "\
--- a/value.py
+++ /dev/null
@@ -1 +0,0 @@
-removed = 1
",
        );

        let refined = compute_file_scoped_styles(
            Path::new("value.py"),
            "",
            &parsed.hunks,
            theme.syntax_theme,
        )
        .expect("delete-only diff");

        assert!(refined.is_empty());
    }

    #[test]
    fn full_file_guardrails_reject_total_bytes_above_limit() {
        let theme = dark_theme();
        let source_line = "x".repeat(MAX_HIGHLIGHT_LINE_BYTES - 1);
        let parsed = parse_unified_diff(&format!(
            "--- /dev/null\n+++ b/value.rs\n@@ -0,0 +1 @@\n+{source_line}\n"
        ));
        let exact_bytes =
            format!("{source_line}\n").repeat(MAX_HIGHLIGHT_BYTES / MAX_HIGHLIGHT_LINE_BYTES);
        let over_bytes =
            format!("{source_line}\n").repeat(MAX_HIGHLIGHT_BYTES / MAX_HIGHLIGHT_LINE_BYTES + 1);

        assert_eq!(exact_bytes.len(), MAX_HIGHLIGHT_BYTES);
        assert_eq!(exact_bytes.lines().count(), 128);
        assert!(
            compute_file_scoped_styles(
                Path::new("value.rs"),
                &exact_bytes,
                &parsed.hunks,
                theme.syntax_theme,
            )
            .is_some()
        );
        assert!(over_bytes.len() > MAX_HIGHLIGHT_BYTES);
        assert!(over_bytes.lines().count() < MAX_HIGHLIGHT_LINES);
        assert!(
            compute_file_scoped_styles(
                Path::new("value.rs"),
                &over_bytes,
                &parsed.hunks,
                theme.syntax_theme,
            )
            .is_none()
        );
    }

    #[test]
    fn full_file_guardrails_reject_lines_above_byte_limit() {
        let theme = dark_theme();
        let exact_line = "x".repeat(MAX_HIGHLIGHT_LINE_BYTES);
        let exact_parsed = parse_unified_diff(&format!(
            "--- /dev/null\n+++ b/value.rs\n@@ -0,0 +1 @@\n+{exact_line}\n"
        ));
        let overlong_line = "x".repeat(MAX_HIGHLIGHT_LINE_BYTES + 1);
        let overlong_parsed = parse_unified_diff(&format!(
            "--- /dev/null\n+++ b/value.rs\n@@ -0,0 +1 @@\n+{overlong_line}\n"
        ));

        assert!(
            compute_file_scoped_styles(
                Path::new("value.rs"),
                &exact_line,
                &exact_parsed.hunks,
                theme.syntax_theme,
            )
            .is_some()
        );
        assert!(
            compute_file_scoped_styles(
                Path::new("value.rs"),
                &overlong_line,
                &overlong_parsed.hunks,
                theme.syntax_theme,
            )
            .is_none()
        );
    }

    #[test]
    fn full_file_guardrails_reject_too_many_lines() {
        let theme = dark_theme();
        let parsed = parse_unified_diff(
            "\
--- /dev/null
+++ b/value.rs
@@ -0,0 +1 @@
+x
",
        );
        let mut over_lines = "x\n".repeat(MAX_HIGHLIGHT_LINES);
        over_lines.push('x');

        assert_eq!(over_lines.lines().count(), MAX_HIGHLIGHT_LINES + 1);
        assert!(
            compute_file_scoped_styles(
                Path::new("value.rs"),
                &over_lines,
                &parsed.hunks,
                theme.syntax_theme,
            )
            .is_none()
        );
    }

    #[test]
    fn full_file_walk_stops_immediately_after_the_highest_needed_line() {
        let theme = dark_theme();
        let parsed = parse_unified_diff(
            "\
--- a/value.py
+++ b/value.py
@@ -2 +2 @@
 wanted = 2
",
        );
        let pathological_tail = "\"\"\"unterminated content after the requested range";
        let file_text = format!("prefix = 1\nwanted = 2\n{pathological_tail}\n");
        let mut highlighted_lines = 0usize;

        let refined = super::compute_file_scoped_styles_with(
            Path::new("value.py"),
            &file_text,
            &parsed.hunks,
            theme.syntax_theme,
            |highlighter, text| {
                assert_ne!(text, pathological_tail);
                highlighted_lines += 1;
                highlighter.highlight_line(text)
            },
        )
        .expect("bounded full-file pass");

        assert_eq!(highlighted_lines, 2);
        assert_eq!(refined.len(), 1);
        assert!(refined.contains_key(&2));
    }

    #[test]
    fn full_file_styles_preserve_whitespace_and_strip_structural_crlf() {
        let theme = dark_theme();
        let parsed = parse_unified_diff(concat!(
            "--- a/value.py\r\n",
            "+++ b/value.py\r\n",
            "@@ -1,2 +1,2 @@\r\n",
            " value = 1\r\n",
            "     field = 2  \r\n",
        ));
        let refined = compute_file_scoped_styles(
            Path::new("value.py"),
            "value = 1\r\n    field = 2  ",
            &parsed.hunks,
            theme.syntax_theme,
        )
        .expect("CRLF full-file styles");

        assert_eq!(refined.len(), 2);
        assert_eq!(
            refined[&2]
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "    field = 2  "
        );
        assert!(
            refined
                .values()
                .flatten()
                .all(|span| !span.content.contains('\r'))
        );
    }
}
