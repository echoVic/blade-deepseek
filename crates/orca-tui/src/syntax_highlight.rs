use std::path::Path;
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

use crate::terminal_capabilities::TerminalColorLevel;

pub(crate) const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
pub(crate) const MAX_HIGHLIGHT_LINES: usize = 10_000;
pub(crate) const MAX_HIGHLIGHT_LINE_BYTES: usize = 4 * 1024;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<EmbeddedLazyThemeSet> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum SyntaxTheme {
    OneHalfDark,
    OneHalfLight,
    SolarizedDark,
    CatppuccinMocha,
}

impl SyntaxTheme {
    pub(crate) const fn revision(self) -> u64 {
        match self {
            Self::OneHalfDark => 1,
            Self::OneHalfLight => 2,
            Self::SolarizedDark => 3,
            Self::CatppuccinMocha => 4,
        }
    }

    fn embedded(self) -> EmbeddedThemeName {
        match self {
            Self::OneHalfDark => EmbeddedThemeName::OneHalfDark,
            Self::OneHalfLight => EmbeddedThemeName::OneHalfLight,
            Self::SolarizedDark => EmbeddedThemeName::SolarizedDark,
            Self::CatppuccinMocha => EmbeddedThemeName::CatppuccinMocha,
        }
    }

    fn theme(self) -> &'static Theme {
        theme_set().get(self.embedded())
    }
}

pub(crate) type StyledSourceLine = Vec<Span<'static>>;

pub(crate) fn content_within_limits(content: &str) -> bool {
    if content.len() > MAX_HIGHLIGHT_BYTES {
        return false;
    }

    for (line_index, line) in content.lines().enumerate() {
        if line_index + 1 > MAX_HIGHLIGHT_LINES || line.len() > MAX_HIGHLIGHT_LINE_BYTES {
            return false;
        }
    }

    true
}

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

fn theme_set() -> &'static EmbeddedLazyThemeSet {
    THEME_SET.get_or_init(two_face::theme::extra)
}

fn first_info_token(info: &str) -> &str {
    info.split([',', ' ', '\t']).next().unwrap_or_default()
}

fn find_syntax(token: &str) -> Option<&'static SyntaxReference> {
    let raw = first_info_token(token);
    if raw.is_empty() {
        return None;
    }

    let normalized = raw.to_ascii_lowercase();
    let patched = match normalized.as_str() {
        "csharp" | "c-sharp" => "c#",
        "golang" => "go",
        "python3" => "python",
        "shell" => "bash",
        _ => raw,
    };
    let set = syntax_set();

    set.find_syntax_by_token(patched)
        .or_else(|| set.find_syntax_by_name(patched))
        .or_else(|| {
            set.syntaxes()
                .iter()
                .find(|syntax| syntax.name.eq_ignore_ascii_case(patched))
        })
        .or_else(|| set.find_syntax_by_extension(patched))
}

fn to_ratatui_style(style: syntect::highlighting::Style, color_level: TerminalColorLevel) -> Style {
    let mut output = Style::default().fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));
    if style.font_style.contains(FontStyle::BOLD) {
        output = output.add_modifier(Modifier::BOLD);
    }
    color_level.adapt_style(output)
}

fn structural_line_ending_len(source_line: &str) -> usize {
    if source_line.ends_with("\r\n") {
        2
    } else if source_line.ends_with('\n') {
        1
    } else {
        0
    }
}

fn to_spans(
    ranges: Vec<(syntect::highlighting::Style, &str)>,
    structural_ending_len: usize,
    color_level: TerminalColorLevel,
) -> StyledSourceLine {
    let content_len = ranges
        .iter()
        .map(|(_, segment)| segment.len())
        .sum::<usize>()
        - structural_ending_len;
    let mut spans = Vec::new();
    let mut consumed = 0;
    for (style, segment) in ranges {
        let retained_len = content_len.saturating_sub(consumed).min(segment.len());
        let text = &segment[..retained_len];
        if !text.is_empty() {
            spans.push(Span::styled(
                text.to_owned(),
                to_ratatui_style(style, color_level),
            ));
        }
        consumed += segment.len();
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

pub(crate) fn highlight_code(
    code: &str,
    language: &str,
    theme: SyntaxTheme,
    color_level: TerminalColorLevel,
) -> Option<Vec<StyledSourceLine>> {
    if code.is_empty() || !content_within_limits(code) {
        return None;
    }

    let syntax = find_syntax(language)?;
    let mut highlighter = HighlightLines::new(syntax, theme.theme());
    LinesWithEndings::from(code)
        .map(|source_line| {
            let structural_ending_len = structural_line_ending_len(source_line);
            highlighter
                .highlight_line(source_line, syntax_set())
                .ok()
                .map(|ranges| to_spans(ranges, structural_ending_len, color_level))
        })
        .collect()
}

pub(crate) struct LineHighlighter {
    inner: HighlightLines<'static>,
    color_level: TerminalColorLevel,
}

impl LineHighlighter {
    pub(crate) fn highlight_line(&mut self, text: &str) -> Option<StyledSourceLine> {
        let source_line = format!("{text}\n");
        let ranges = self.inner.highlight_line(&source_line, syntax_set()).ok()?;
        Some(to_spans(ranges, 1, self.color_level))
    }
}

pub(crate) fn highlighter_for_path(
    path: &Path,
    theme: SyntaxTheme,
    color_level: TerminalColorLevel,
) -> Option<LineHighlighter> {
    let syntax = find_syntax_for_path(path)?;

    Some(LineHighlighter {
        inner: HighlightLines::new(syntax, theme.theme()),
        color_level,
    })
}

fn find_syntax_for_path(path: &Path) -> Option<&'static SyntaxReference> {
    path.file_name()
        .and_then(|file_name| file_name.to_str())
        .and_then(find_syntax)
        .or_else(|| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .and_then(find_syntax)
        })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;

    use ratatui::style::{Color, Modifier};
    use ratatui::text::Span;

    use super::{
        MAX_HIGHLIGHT_BYTES, MAX_HIGHLIGHT_LINE_BYTES, MAX_HIGHLIGHT_LINES, SyntaxTheme,
        content_within_limits, find_syntax_for_path, highlight_code, highlighter_for_path,
    };
    use crate::terminal_capabilities::TerminalColorLevel;

    fn distinct_foregrounds(lines: &[Vec<Span<'static>>]) -> usize {
        lines
            .iter()
            .flatten()
            .filter_map(|span| span.style.fg)
            .collect::<HashSet<_>>()
            .len()
    }

    fn color_fits(level: TerminalColorLevel, color: Option<Color>) -> bool {
        match level {
            TerminalColorLevel::TrueColor => true,
            TerminalColorLevel::Ansi256 => !matches!(color, Some(Color::Rgb(..))),
            TerminalColorLevel::Ansi16 => {
                !matches!(color, Some(Color::Rgb(..) | Color::Indexed(_)))
            }
            TerminalColorLevel::Monochrome => color.is_none() || color == Some(Color::Reset),
        }
    }

    #[test]
    fn highlighted_styles_obey_terminal_color_level() {
        for level in [
            TerminalColorLevel::TrueColor,
            TerminalColorLevel::Ansi256,
            TerminalColorLevel::Ansi16,
            TerminalColorLevel::Monochrome,
        ] {
            let lines = highlight_code(
                "pub struct Item;\n",
                "rust",
                SyntaxTheme::OneHalfDark,
                level,
            )
            .unwrap();
            assert!(
                lines
                    .iter()
                    .flatten()
                    .all(|span| color_fits(level, span.style.fg))
            );
        }
    }

    #[test]
    fn rust_preserves_source_and_uses_multiple_foregrounds_without_backgrounds() {
        let lines = highlight_code(
            "fn main() { let answer = \"forty two\"; }\n",
            "rust",
            SyntaxTheme::OneHalfDark,
            TerminalColorLevel::TrueColor,
        )
        .expect("Rust syntax");

        assert_eq!(
            lines
                .iter()
                .flatten()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "fn main() { let answer = \"forty two\"; }"
        );
        assert!(distinct_foregrounds(&lines) >= 2);
        assert!(lines.iter().flatten().all(|span| span.style.bg.is_none()));
    }

    #[test]
    fn fence_metadata_and_aliases_resolve() {
        assert!(
            highlight_code(
                "let value = 1;\n",
                "rust,no_run",
                SyntaxTheme::OneHalfDark,
                TerminalColorLevel::TrueColor,
            )
            .is_some()
        );
        assert!(
            highlight_code(
                "print('value')\n",
                "python3",
                SyntaxTheme::OneHalfDark,
                TerminalColorLevel::TrueColor,
            )
            .is_some()
        );
        assert!(
            highlight_code(
                "echo hi\n",
                "shell",
                SyntaxTheme::OneHalfDark,
                TerminalColorLevel::TrueColor,
            )
            .is_some()
        );
        assert!(
            highlight_code(
                "value\n",
                "not-a-real-language",
                SyntaxTheme::OneHalfDark,
                TerminalColorLevel::TrueColor,
            )
            .is_none()
        );
    }

    #[test]
    fn syntax_theme_revisions_are_stable_and_distinct() {
        assert_eq!(SyntaxTheme::OneHalfDark.revision(), 1);
        assert_eq!(SyntaxTheme::OneHalfLight.revision(), 2);
        assert_eq!(SyntaxTheme::SolarizedDark.revision(), 3);
        assert_eq!(SyntaxTheme::CatppuccinMocha.revision(), 4);
    }

    #[test]
    fn strict_limits_reject_only_values_above_each_ceiling() {
        let exact_bytes = format!("{}\n", "x".repeat(MAX_HIGHLIGHT_LINE_BYTES - 1))
            .repeat(MAX_HIGHLIGHT_BYTES / MAX_HIGHLIGHT_LINE_BYTES);
        assert_eq!(exact_bytes.len(), MAX_HIGHLIGHT_BYTES);
        assert!(content_within_limits(&exact_bytes));
        let mut too_many_bytes = exact_bytes;
        too_many_bytes.push('x');
        assert_eq!(too_many_bytes.len(), MAX_HIGHLIGHT_BYTES + 1);
        assert!(!content_within_limits(&too_many_bytes));

        let exact_lines = "x\n".repeat(MAX_HIGHLIGHT_LINES);
        assert_eq!(exact_lines.lines().count(), MAX_HIGHLIGHT_LINES);
        assert!(content_within_limits(&exact_lines));
        let mut too_many_lines = exact_lines;
        too_many_lines.push('x');
        assert_eq!(too_many_lines.lines().count(), MAX_HIGHLIGHT_LINES + 1);
        assert!(!content_within_limits(&too_many_lines));

        assert!(content_within_limits(&"x".repeat(MAX_HIGHLIGHT_LINE_BYTES)));
        assert!(!content_within_limits(
            &"x".repeat(MAX_HIGHLIGHT_LINE_BYTES + 1)
        ));
    }

    #[test]
    fn highlighting_preserves_logical_blank_lines() {
        let lines = highlight_code(
            "fn first() {}\n\nfn second() {}",
            "rust",
            SyntaxTheme::OneHalfDark,
            TerminalColorLevel::TrueColor,
        )
        .expect("Rust syntax");

        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[1]
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            ""
        );
    }

    #[test]
    fn highlighting_strips_only_structural_line_endings() {
        let crlf = highlight_code(
            "let first = 1;\r\n",
            "rust",
            SyntaxTheme::OneHalfDark,
            TerminalColorLevel::TrueColor,
        )
        .expect("Rust syntax");
        let literal_cr = highlight_code(
            "let second = 2;\r",
            "rust",
            SyntaxTheme::OneHalfDark,
            TerminalColorLevel::TrueColor,
        )
        .expect("Rust syntax");

        assert_eq!(
            crlf.iter()
                .flatten()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "let first = 1;"
        );
        assert_eq!(
            literal_cr
                .iter()
                .flatten()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "let second = 2;\r"
        );
    }

    #[test]
    fn path_highlighter_preserves_multiline_parser_state() {
        let mut continued = highlighter_for_path(
            Path::new("src/main.rs"),
            SyntaxTheme::OneHalfDark,
            TerminalColorLevel::TrueColor,
        )
        .expect("Rust path");
        continued
            .highlight_line("/* comment starts")
            .expect("comment start");
        let continued_second = continued
            .highlight_line("still comment */ let value = 1;")
            .expect("continued second line");

        let mut fresh = highlighter_for_path(
            Path::new("src/main.rs"),
            SyntaxTheme::OneHalfDark,
            TerminalColorLevel::TrueColor,
        )
        .expect("fresh Rust path");
        let fresh_second = fresh
            .highlight_line("still comment */ let value = 1;")
            .expect("fresh second line");

        assert_eq!(
            continued_second
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "still comment */ let value = 1;"
        );
        assert_eq!(
            fresh_second
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "still comment */ let value = 1;"
        );
        assert_ne!(continued_second, fresh_second);
        assert!(
            highlighter_for_path(
                Path::new("src/file.not-a-real-language"),
                SyntaxTheme::OneHalfDark,
                TerminalColorLevel::TrueColor,
            )
            .is_none()
        );
    }

    #[test]
    fn path_lookup_prefers_complete_filename_before_extension() {
        assert_eq!(
            find_syntax_for_path(Path::new("CMakeLists.txt"))
                .expect("CMakeLists syntax")
                .name,
            "CMake"
        );
        assert_eq!(
            find_syntax_for_path(Path::new("src/main.rs"))
                .expect("Rust syntax")
                .name,
            "Rust"
        );
    }

    #[test]
    fn converted_styles_use_foreground_and_optional_bold_only() {
        let lines = highlight_code(
            "pub struct Item;\n",
            "rust",
            SyntaxTheme::OneHalfDark,
            TerminalColorLevel::TrueColor,
        )
        .expect("Rust syntax");

        assert!(lines.iter().flatten().all(|span| {
            matches!(span.style.fg, Some(Color::Rgb(_, _, _)))
                && span.style.bg.is_none()
                && !span.style.add_modifier.contains(Modifier::ITALIC)
                && !span.style.add_modifier.contains(Modifier::UNDERLINED)
                && (span.style.add_modifier.is_empty() || span.style.add_modifier == Modifier::BOLD)
                && span.style.sub_modifier.is_empty()
        }));
    }
}
