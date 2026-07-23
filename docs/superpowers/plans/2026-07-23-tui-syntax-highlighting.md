# TUI Syntax Highlighting Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add cached syntax highlighting for fenced Markdown and unified diffs, then progressively refine eligible live edit diffs with verified full-file parser state off the draw thread.

**Architecture:** A focused `syntax_highlight` module owns two-face grammars, bundled themes, guardrails, and syntect line sessions. A `diff_highlight` module parses and renders unified diffs with separate old/new hunk state and computes full-file new-side style maps. A single `edit_highlight_worker` reads capped post-edit files and returns versioned results; `AppState` validates and stores those derived maps, while `TranscriptRenderCache` caches the final highlighted wrapped lines under an explicit syntax-theme revision.

**Tech Stack:** Rust 2024, ratatui 0.29, pulldown-cmark 0.12, syntect 5.3, two-face 0.5, crossbeam-channel, Cargo tests/clippy/fmt.

---

## File Map

- Modify `Cargo.toml`
  - Declare workspace-level `syntect` and `two-face`.
- Modify `Cargo.lock`
  - Record the resolved syntax engine, grammar bundle, and regex backend.
- Modify `crates/orca-tui/Cargo.toml`
  - Consume the two workspace dependencies.
- Modify `crates/orca-tui/src/lib.rs`
  - Register the three new private modules.
- Create `crates/orca-tui/src/syntax_highlight.rs`
  - Own grammar/theme singletons, syntax lookup, guardrails, style conversion,
    complete code-block highlighting, and resumable line highlighting.
- Create `crates/orca-tui/src/diff_highlight.rs`
  - Parse unified diffs, render hunk-local old/new syntax, overlay refined
    new-side styles, and compute verified full-file style maps.
- Create `crates/orca-tui/src/edit_highlight_worker.rs`
  - Own one worker thread, latest-job-wins coalescing, capped file reads, jobs,
    outcomes, and result polling.
- Modify `crates/orca-tui/src/theme.rs`
  - Associate each Orca theme with a two-face syntax theme and stable revision.
- Modify `crates/orca-tui/src/ui.rs`
  - Buffer fenced code blocks, preserve highlighted spans, delegate diff
    rendering, and pass per-message refined styles.
- Modify `crates/orca-tui/src/transcript_view.rs`
  - Add `syntax_theme_revision` to cache matching and expose message index to
    the cache-build closure.
- Modify `crates/orca-tui/src/types.rs`
  - Own workspace/syntax context, worker runtime, pending/applied refinement
    state, stale-result checks, and lifecycle pruning.
- Modify `crates/orca-tui/src/app.rs`
  - Configure the real workspace root and syntax theme, poll worker results,
    and keep the frame scheduler awake only while refinement is pending.
- Modify `crates/orca-tui/src/selection.rs`
  - Verify selection backgrounds preserve several syntax foreground spans.

## Required Working Discipline

- Run each RED command and record that it fails for the named missing behavior.
- Do not add production implementation for a task before its RED command.
- Make only the minimum production change needed for that task's GREEN command.
- Run the task's focused test after refactoring.
- Every implementation commit must end with:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

---

### Task 1: Syntax Engine, Themes, and Hard Guardrails

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/orca-tui/Cargo.toml`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/theme.rs`
- Create: `crates/orca-tui/src/syntax_highlight.rs`

- [ ] **Step 1: Add failing syntax-engine contract tests**

Register the empty test module first in `crates/orca-tui/src/lib.rs`:

```rust
mod syntax_highlight;
```

Then create `crates/orca-tui/src/syntax_highlight.rs` with tests only. Registering
the file before the RED run is essential; otherwise Cargo would not compile the
new tests. The tests define the intended private API before dependencies or
implementation exist:

```rust
#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::{
        MAX_HIGHLIGHT_BYTES, MAX_HIGHLIGHT_LINE_BYTES, MAX_HIGHLIGHT_LINES,
        SyntaxTheme, content_within_limits, highlight_code,
    };

    fn distinct_foregrounds(lines: &[Vec<ratatui::text::Span<'static>>]) -> usize {
        let mut colors = std::collections::HashSet::new();
        for span in lines.iter().flatten() {
            if let Some(color) = span.style.fg {
                colors.insert(color);
            }
        }
        colors.len()
    }

    #[test]
    fn rust_uses_two_face_token_styles() {
        let lines = highlight_code(
            "fn main() { let answer = \"forty two\"; }\n",
            "rust",
            SyntaxTheme::OneHalfDark,
        )
        .expect("Rust syntax");

        assert_eq!(
            lines.iter().flatten().map(|span| span.content.as_ref()).collect::<String>(),
            "fn main() { let answer = \"forty two\"; }"
        );
        assert!(distinct_foregrounds(&lines) >= 2);
        assert!(lines.iter().flatten().all(|span| span.style.bg.is_none()));
    }

    #[test]
    fn fence_metadata_and_aliases_resolve() {
        assert!(highlight_code("let x = 1;\n", "rust,no_run", SyntaxTheme::OneHalfDark).is_some());
        assert!(highlight_code("print('x')\n", "python3", SyntaxTheme::OneHalfDark).is_some());
        assert!(highlight_code("echo hi\n", "shell", SyntaxTheme::OneHalfDark).is_some());
        assert!(highlight_code("x\n", "not-a-real-language", SyntaxTheme::OneHalfDark).is_none());
    }

    #[test]
    fn strict_limits_reject_only_values_above_each_ceiling() {
        let exact_bytes = format!("{}\n", "x".repeat(MAX_HIGHLIGHT_LINE_BYTES - 1))
            .repeat(MAX_HIGHLIGHT_BYTES / MAX_HIGHLIGHT_LINE_BYTES);
        assert_eq!(exact_bytes.len(), MAX_HIGHLIGHT_BYTES);
        assert!(content_within_limits(&exact_bytes));
        let mut too_many_bytes = exact_bytes;
        too_many_bytes.push('x');
        assert!(!content_within_limits(&too_many_bytes));

        let exact_lines = "x\n".repeat(MAX_HIGHLIGHT_LINES);
        assert!(content_within_limits(&exact_lines));
        let mut too_many_lines = exact_lines;
        too_many_lines.push('x');
        assert!(!content_within_limits(&too_many_lines));

        assert!(content_within_limits(&"x".repeat(MAX_HIGHLIGHT_LINE_BYTES)));
        assert!(!content_within_limits(&"x".repeat(MAX_HIGHLIGHT_LINE_BYTES + 1)));
    }

    #[test]
    fn converted_styles_use_foreground_and_optional_bold_only() {
        let lines = highlight_code("pub struct Item;\n", "rust", SyntaxTheme::OneHalfDark)
            .expect("Rust syntax");
        assert!(lines.iter().flatten().all(|span| {
            span.style.fg != Some(Color::Reset)
                && span.style.bg.is_none()
                && !span.style.add_modifier.contains(ratatui::style::Modifier::ITALIC)
                && !span.style.add_modifier.contains(ratatui::style::Modifier::UNDERLINED)
        }));
    }
}
```

- [ ] **Step 2: Run the tests to verify RED**

Run:

```sh
cargo test -p orca-tui syntax_highlight --lib
```

Expected: FAIL because the tested constants, enum, and functions do not exist.

- [ ] **Step 3: Add dependencies**

Add to `[workspace.dependencies]` in `Cargo.toml`:

```toml
syntect = { version = "5.3", default-features = false, features = ["default-syntaxes", "dump-load", "parsing", "regex-fancy"] }
two-face = { version = "0.5", default-features = false, features = ["syntect-fancy"] }
```

Add to `crates/orca-tui/Cargo.toml`:

```toml
syntect = { workspace = true }
two-face = { workspace = true }
```

Run:

```sh
cargo check -p orca-tui
```

Expected: Cargo resolves dependencies and updates `Cargo.lock`; compilation
still fails until the module API is implemented.

- [ ] **Step 4: Implement the minimum syntax engine**

Implement this ownership model in `syntax_highlight.rs`:

```rust
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;

pub(crate) const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
pub(crate) const MAX_HIGHLIGHT_LINES: usize = 10_000;
pub(crate) const MAX_HIGHLIGHT_LINE_BYTES: usize = 4 * 1024;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();

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
        two_face::theme::extra().get(self.embedded())
    }
}

pub(crate) type StyledSourceLine = Vec<Span<'static>>;

pub(crate) fn content_within_limits(content: &str) -> bool {
    if content.len() > MAX_HIGHLIGHT_BYTES {
        return false;
    }
    let mut line_count = 0usize;
    for line in content.lines() {
        line_count += 1;
        if line_count > MAX_HIGHLIGHT_LINES || line.len() > MAX_HIGHLIGHT_LINE_BYTES {
            return false;
        }
    }
    true
}

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

fn first_info_token(info: &str) -> &str {
    info.split([',', ' ', '\t']).next().unwrap_or_default()
}

fn find_syntax(token: &str) -> Option<&'static SyntaxReference> {
    let raw = first_info_token(token);
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

fn to_ratatui_style(style: syntect::highlighting::Style) -> Style {
    let mut output = Style::default().fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));
    if style.font_style.contains(FontStyle::BOLD) {
        output = output.add_modifier(Modifier::BOLD);
    }
    output
}

pub(crate) fn highlight_code(
    code: &str,
    language: &str,
    theme: SyntaxTheme,
) -> Option<Vec<StyledSourceLine>> {
    if code.is_empty() || !content_within_limits(code) {
        return None;
    }
    let syntax = find_syntax(language)?;
    let mut highlighter = HighlightLines::new(syntax, theme.theme());
    let mut output = Vec::new();
    for source_line in LinesWithEndings::from(code) {
        let ranges = highlighter.highlight_line(source_line, syntax_set()).ok()?;
        let mut spans = Vec::new();
        for (style, segment) in ranges {
            let text = segment.trim_end_matches(['\n', '\r']);
            if !text.is_empty() {
                spans.push(Span::styled(text.to_string(), to_ratatui_style(style)));
            }
        }
        if spans.is_empty() {
            spans.push(Span::raw(String::new()));
        }
        output.push(spans);
    }
    Some(output)
}
```

Also add an opaque resumable `LineHighlighter` with:

```rust
pub(crate) struct LineHighlighter {
    inner: HighlightLines<'static>,
}

impl LineHighlighter {
    pub(crate) fn highlight_line(&mut self, text: &str) -> Option<StyledSourceLine>;
}

pub(crate) fn highlighter_for_path(
    path: &std::path::Path,
    theme: SyntaxTheme,
) -> Option<LineHighlighter>;
```

`highlight_line` appends one newline for syntect, strips the structural line
ending from returned spans, and returns `None` on parser failure.

- [ ] **Step 5: Map Orca themes to syntax themes**

Add two fields to `Theme`:

```rust
pub syntax_theme: crate::syntax_highlight::SyntaxTheme,
pub syntax_theme_revision: u64,
```

Set both in every `Theme::named` arm. Use:

```rust
let syntax_theme = match name {
    ThemeName::Dark => SyntaxTheme::OneHalfDark,
    ThemeName::Light => SyntaxTheme::OneHalfLight,
    ThemeName::Solarized => SyntaxTheme::SolarizedDark,
    ThemeName::Catppuccin => SyntaxTheme::CatppuccinMocha,
};
```

Construct the existing color palette and append:

```rust
syntax_theme,
syntax_theme_revision: syntax_theme.revision(),
```

- [ ] **Step 6: Run focused GREEN checks**

Run:

```sh
cargo test -p orca-tui syntax_highlight --lib
cargo test -p orca-tui theme --lib
```

Expected: PASS. Confirm the exact-limit test covers `>` rather than `>=`, and
the no-trailing-newline case counts 10,001 actual lines. The exact-byte fixture
uses 128 lines of 4,095 bytes plus one newline each, so it reaches 512 KiB
without triggering the per-line guard.

- [ ] **Step 7: Commit the syntax engine**

```sh
git add Cargo.toml Cargo.lock crates/orca-tui/Cargo.toml \
  crates/orca-tui/src/lib.rs crates/orca-tui/src/theme.rs \
  crates/orca-tui/src/syntax_highlight.rs
git commit -m "feat(tui): add bounded syntax engine" \
  -m "Use syntect and two-face with theme mapping and Codex-compatible highlight limits." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 2: Fenced Markdown Code Blocks

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`
- Test: `crates/orca-tui/src/ui.rs`

- [ ] **Step 1: Add failing Markdown rendering tests**

Add tests in `ui.rs`:

```rust
#[test]
fn fenced_rust_code_preserves_text_and_uses_token_foregrounds() {
    let theme = Theme::named(orca_core::config::ThemeName::Dark);
    let lines = render_markdown(
        "```rust,no_run\nfn main() { let message = \"hello\"; }\n```\n",
        80,
        &theme,
    );
    let text = lines
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let colors = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter_map(|span| span.style.fg)
        .collect::<std::collections::HashSet<_>>();

    assert!(text.contains("  fn main() { let message = \"hello\"; }"));
    assert!(colors.len() >= 2);
}

#[test]
fn unknown_and_oversized_fences_keep_plain_gray_code_style() {
    let theme = Theme::named(orca_core::config::ThemeName::Dark);
    let unknown = render_markdown("```unknown-lang\nplain\n```\n", 80, &theme);
    assert!(unknown.iter().flat_map(|line| &line.spans).all(|span| {
        span.content.trim().is_empty() || span.style.fg == Some(Color::Gray)
    }));

    let body = "x".repeat(crate::syntax_highlight::MAX_HIGHLIGHT_BYTES + 1);
    let oversized = render_markdown(&format!("```rust\n{body}\n```\n"), 80, &theme);
    assert!(oversized.iter().flat_map(|line| &line.spans).all(|span| {
        span.content.trim().is_empty() || span.style.fg == Some(Color::Gray)
    }));
}

#[test]
fn proposed_plan_keeps_markdown_code_span_styles() {
    let theme = Theme::named(orca_core::config::ThemeName::Dark);
    let mut lines = Vec::new();
    append_proposed_plan_lines(
        &mut lines,
        "```rust\nlet answer = 42;\n```",
        80,
        &theme,
    );
    let colors = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter_map(|span| span.style.fg)
        .collect::<std::collections::HashSet<_>>();
    assert!(colors.len() >= 2);
}
```

- [ ] **Step 2: Run the tests to verify RED**

Run:

```sh
cargo test -p orca-tui fenced_rust_code_preserves_text_and_uses_token_foregrounds --lib
cargo test -p orca-tui proposed_plan_keeps_markdown_code_span_styles --lib
```

Expected: FAIL because `render_markdown` has no `Theme` parameter, code blocks
are rendered as one gray span per line, and proposed plans flatten styled
lines with `line.to_string()`.

- [ ] **Step 3: Buffer complete code blocks and highlight once**

Change the signature:

```rust
fn render_markdown(input: &str, width: usize, theme: &Theme) -> Vec<Line<'static>>
```

Replace `in_code_block: bool` with:

```rust
struct PendingCodeBlock {
    language: Option<String>,
    source: String,
}

let mut code_block: Option<PendingCodeBlock> = None;
```

On `Tag::CodeBlock(kind)`, flush the current prose line and initialize:

```rust
let language = match kind {
    pulldown_cmark::CodeBlockKind::Fenced(info) => Some(info.into_string()),
    pulldown_cmark::CodeBlockKind::Indented => None,
};
code_block = Some(PendingCodeBlock {
    language,
    source: String::new(),
});
```

While a block is active, append every `Event::Text` value to `source` without
rendering it. On `TagEnd::CodeBlock`, use:

```rust
fn append_code_block(
    lines: &mut Vec<Line<'static>>,
    block: PendingCodeBlock,
    theme: &Theme,
) {
    let highlighted = block.language.as_deref().and_then(|language| {
        crate::syntax_highlight::highlight_code(&block.source, language, theme.syntax_theme)
    });
    if let Some(highlighted) = highlighted {
        for source_spans in highlighted {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(source_spans);
            lines.push(Line::from(spans));
        }
        return;
    }
    for source_line in block.source.lines() {
        lines.push(Line::from(Span::styled(
            format!("  {source_line}"),
            Style::default().fg(Color::Gray),
        )));
    }
}
```

Preserve an empty final source line when the fenced content structurally
contains it; use the same logical-line helper as `syntax_highlight` rather than
blindly dropping it with `str::lines`.

- [ ] **Step 4: Preserve spans in all Markdown call sites**

Pass `theme` from assistant and proposed-plan rendering:

```rust
let md_lines = render_markdown(text, width, theme);
```

For proposed plans, replace `line.to_string()` with span-preserving prefixing:

```rust
for mut line in render_markdown(text, width.saturating_sub(2), theme) {
    let mut spans = vec![Span::styled("  ", Style::default().fg(theme.muted))];
    spans.append(&mut line.spans);
    let mut prefixed = Line::from(spans);
    prefixed.alignment = line.alignment;
    lines.push(prefixed);
}
```

- [ ] **Step 5: Run focused GREEN checks**

Run:

```sh
cargo test -p orca-tui fenced_rust_code --lib
cargo test -p orca-tui unknown_and_oversized_fences --lib
cargo test -p orca-tui proposed_plan_keeps_markdown_code_span_styles --lib
cargo test -p orca-tui inline_code --lib
```

Expected: PASS. Existing Markdown table and inline-code tests must remain
green.

- [ ] **Step 6: Commit Markdown highlighting**

```sh
git add crates/orca-tui/src/ui.rs
git commit -m "feat(tui): highlight fenced code blocks" \
  -m "Buffer complete fences so syntect keeps multiline parser state and cache-ready spans." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 3: Unified-Diff Model and Hunk-Local First Paint

**Files:**
- Modify: `crates/orca-tui/src/lib.rs`
- Create: `crates/orca-tui/src/diff_highlight.rs`
- Modify: `crates/orca-tui/src/ui.rs`

- [ ] **Step 1: Add failing parser and render tests**

Register the test module in `crates/orca-tui/src/lib.rs` before the RED run:

```rust
mod diff_highlight;
```

Then create `diff_highlight.rs` with tests defining:

```rust
#[cfg(test)]
mod tests {
    use super::{DiffLineKind, parse_unified_diff, render_unified_diff};
    use crate::theme::Theme;
    use orca_core::config::ThemeName;

    const RUST_DIFF: &str = "\
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,2 +1,2 @@
-fn old() { let value = \"old\"; }
+fn new() { let value = \"new\"; }
 context();
";

    #[test]
    fn parser_tracks_destination_and_old_new_line_numbers() {
        let parsed = parse_unified_diff(RUST_DIFF);
        assert_eq!(parsed.destination_path.as_deref(), Some("src/main.rs"));
        assert_eq!(parsed.hunks.len(), 1);
        let source = parsed.hunks[0].source_lines().collect::<Vec<_>>();
        assert_eq!(source[0].kind, DiffLineKind::Delete);
        assert_eq!(source[0].old_line, Some(1));
        assert_eq!(source[0].new_line, None);
        assert_eq!(source[1].kind, DiffLineKind::Insert);
        assert_eq!(source[1].old_line, None);
        assert_eq!(source[1].new_line, Some(1));
    }

    #[test]
    fn rust_diff_keeps_prefixes_and_adds_syntax_foregrounds() {
        let theme = Theme::named(ThemeName::Dark);
        let lines = render_unified_diff(RUST_DIFF, &theme, None);
        let inserted = lines
            .iter()
            .find(|line| line.to_string().contains("+fn new"))
            .expect("insert line");
        assert_eq!(inserted.spans[0].content.as_ref(), "    +");
        assert_eq!(inserted.spans[0].style.fg, Some(theme.diff_add));
        let token_colors = inserted.spans[1..]
            .iter()
            .filter_map(|span| span.style.fg)
            .collect::<std::collections::HashSet<_>>();
        assert!(token_colors.len() >= 2);
    }

    #[test]
    fn rename_uses_destination_extension() {
        let diff = "\
--- a/src/value.unknown
+++ b/src/value.py
@@ -1 +1 @@
-value = \"old\"
+value = \"new\"
";
        let parsed = parse_unified_diff(diff);
        assert_eq!(parsed.destination_path.as_deref(), Some("src/value.py"));
    }

    #[test]
    fn metadata_lines_remain_in_render_order() {
        let theme = Theme::named(ThemeName::Dark);
        let diff = concat!(
            "--- a/value.rs\n",
            "+++ b/value.rs\n",
            "@@ -1 +1 @@\n",
            "-let value = 1;\n",
            "\\ No newline at end of file\n",
            "+let value = 2;\n",
        );
        let rendered = render_unified_diff(diff, &theme, None)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            vec![
                "    --- a/value.rs",
                "    +++ b/value.rs",
                "    @@ -1 +1 @@",
                "    -let value = 1;",
                "    \\ No newline at end of file",
                "    +let value = 2;",
            ]
        );
    }
}
```

Add these concrete state-isolation tests:

```rust
#[test]
fn old_and_new_hunk_parsers_do_not_leak_multiline_state() {
    let theme = Theme::named(ThemeName::Dark);
    let diff = "\
--- a/item.py
+++ b/item.py
@@ -1,2 +1,2 @@
-\"\"\"old string
-still old\"\"\"
+value = 1
+print(value)
";
    let lines = render_unified_diff(diff, &theme, None);
    let value = lines
        .iter()
        .find(|line| line.to_string().contains("+value = 1"))
        .expect("inserted value line");
    let print = lines
        .iter()
        .find(|line| line.to_string().contains("+print(value)"))
        .expect("inserted print line");
    assert_ne!(value.spans[1..], lines[2].spans[1..]);
    assert!(value.spans[1..].iter().any(|span| span.content.contains("value")));
    assert!(print.spans[1..].iter().any(|span| span.content.contains("print")));
}

#[test]
fn context_advances_both_old_and_new_parsers() {
    let theme = Theme::named(ThemeName::Dark);
    let diff = "\
--- a/item.py
+++ b/item.py
@@ -1,3 +1,3 @@
 \"\"\"shared
-old tail\"\"\"
+new tail\"\"\"
 value = 1
";
    let lines = render_unified_diff(diff, &theme, None);
    let deleted = lines
        .iter()
        .find(|line| line.to_string().contains("-old tail"))
        .expect("delete after shared context");
    let inserted = lines
        .iter()
        .find(|line| line.to_string().contains("+new tail"))
        .expect("insert after shared context");
    assert!(deleted.spans[1..].iter().any(|span| span.content.contains("old tail")));
    assert!(inserted.spans[1..].iter().any(|span| span.content.contains("new tail")));
}
```

- [ ] **Step 2: Run the tests to verify RED**

Run:

```sh
cargo test -p orca-tui diff_highlight --lib
```

Expected: FAIL because the parser/renderer types do not exist.

- [ ] **Step 3: Implement the unified-diff data model**

Implement:

```rust
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
pub(crate) struct DiffHunk {
    pub(crate) header: String,
    pub(crate) entries: Vec<DiffHunkEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DiffHunkEntry {
    Source(DiffSourceLine),
    Metadata(String),
}

impl DiffHunk {
    pub(crate) fn source_lines(&self) -> impl Iterator<Item = &DiffSourceLine> {
        self.entries.iter().filter_map(|entry| match entry {
            DiffHunkEntry::Source(line) => Some(line),
            DiffHunkEntry::Metadata(_) => None,
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ParsedDiff {
    pub(crate) destination_path: Option<String>,
    pub(crate) prelude: Vec<String>,
    pub(crate) hunks: Vec<DiffHunk>,
    pub(crate) aggregate_source_bytes: usize,
    pub(crate) aggregate_source_lines: usize,
}
```

`parse_unified_diff` must:

- strip `a/` and `b/` only from actual `---`/`+++` path headers;
- treat `/dev/null` as absent;
- parse the `-old[,count] +new[,count]` hunk coordinates;
- advance old only for delete, new only for insert, and both for context;
- preserve pre-hunk metadata in `prelude` and in-hunk metadata as
  `DiffHunkEntry::Metadata` at its exact render position without classifying it
  as source;
- count aggregate source bytes and lines for the guardrail check.

- [ ] **Step 4: Implement hunk-local old/new highlighting**

Use:

```rust
pub(crate) type RefinedDiffStyles =
    std::collections::HashMap<usize, crate::syntax_highlight::StyledSourceLine>;

pub(crate) fn render_unified_diff(
    diff: &str,
    theme: &Theme,
    refined: Option<&RefinedDiffStyles>,
) -> Vec<Line<'static>>;
```

Before constructing any parser, reject syntax work when aggregate bytes or
lines exceed the shared limits or any source line exceeds 4 KiB. Rendering
still returns the existing plain diff colors.

For each hunk, iterate `entries`. Render metadata with the muted fallback and
do not advance either parser. For every `DiffHunkEntry::Source`, use:

```rust
let mut old = syntax_path
    .as_deref()
    .and_then(|path| highlighter_for_path(Path::new(path), theme.syntax_theme));
let mut new = syntax_path
    .as_deref()
    .and_then(|path| highlighter_for_path(Path::new(path), theme.syntax_theme));
```

Render each source line as:

- a prefix span `"    +"`, `"    -"`, or `"     "` with the current Orca
  diff/muted color;
- syntax spans for content when available;
- one fallback content span using add/remove/muted color otherwise.

On context, call both highlighters with identical content and render the new
side's spans. On insert, call only new. On delete, call only old. Construct new
highlighters for every hunk.

Use `refined` only for context/insert lines, only when the concatenated refined
span text exactly equals `content`; delete always uses old hunk-local spans.

Preserve the existing 80-visible-line cap and append exactly:

```text
    [... diff truncated ...]
```

when more source diff lines exist.

- [ ] **Step 5: Delegate transcript diff rendering**

Replace `append_diff_lines` in `ui.rs` with:

```rust
fn append_diff_lines(
    lines: &mut Vec<Line<'static>>,
    diff: &str,
    theme: &Theme,
    refined: Option<&crate::diff_highlight::RefinedDiffStyles>,
) {
    lines.extend(crate::diff_highlight::render_unified_diff(
        diff,
        theme,
        refined,
    ));
}
```

Pass `None` from the current message renderer until Task 8 wires derived state.

- [ ] **Step 6: Run focused GREEN checks**

Run:

```sh
cargo test -p orca-tui diff_highlight --lib
cargo test -p orca-tui completed_turn_keeps_tail_marker_visible_after_large_diff --lib
```

Expected: PASS. The existing large-diff viewport tests prove truncation and
tail behavior remain intact.

- [ ] **Step 7: Commit hunk-local diff highlighting**

```sh
git add crates/orca-tui/src/lib.rs crates/orca-tui/src/diff_highlight.rs \
  crates/orca-tui/src/ui.rs
git commit -m "feat(tui): highlight unified diff hunks" \
  -m "Keep independent old and new syntect state while preserving Orca diff prefixes and limits." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 4: Syntax-Theme Revision in TranscriptRenderCache

**Files:**
- Modify: `crates/orca-tui/src/transcript_view.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/types.rs`

- [ ] **Step 1: Add failing cache-key tests**

Extend `prepare_with_counters` to accept `syntax_theme_revision: u64`, then add:

```rust
#[test]
fn syntax_theme_revision_rebuilds_highlighted_wrapped_lines_once() {
    let messages = vec![ChatMessage::Assistant(
        "```rust\nlet answer = 42;\n```".to_string(),
    )];
    let revisions = vec![1];
    let builds = Cell::new(0);
    let parses = Cell::new(0);
    let mut cache = TranscriptRenderCache::default();

    prepare_with_counters_and_syntax_revision(
        &mut cache, &messages, &revisions, 40, 0, 1, &builds, &parses,
    );
    builds.set(0);
    parses.set(0);
    prepare_with_counters_and_syntax_revision(
        &mut cache, &messages, &revisions, 40, 0, 1, &builds, &parses,
    );
    assert_eq!((builds.get(), parses.get()), (0, 0));

    prepare_with_counters_and_syntax_revision(
        &mut cache, &messages, &revisions, 40, 0, 2, &builds, &parses,
    );
    assert_eq!((builds.get(), parses.get()), (1, 1));
}

#[test]
fn spinner_patch_requires_matching_syntax_theme_revision() {
    let messages = vec![ChatMessage::ToolCall {
        id: "running".to_string(),
        name: "read".to_string(),
        target: None,
        status: "running".to_string(),
        output: None,
        diff: None,
        kind: None,
        expanded: false,
    }];
    let revisions = vec![1];
    let builds = Cell::new(0);
    let parses = Cell::new(0);
    let mut cache = TranscriptRenderCache::default();

    prepare_with_counters_and_syntax_revision(
        &mut cache, &messages, &revisions, 40, 0, 1, &builds, &parses,
    );
    builds.set(0);
    prepare_with_counters_and_syntax_revision(
        &mut cache, &messages, &revisions, 40, 2, 2, &builds, &parses,
    );

    assert_eq!(builds.get(), 1);
    assert_eq!(parses.get(), 0);
}
```

- [ ] **Step 2: Run the tests to verify RED**

Run:

```sh
cargo test -p orca-tui syntax_theme_revision_rebuilds --lib
cargo test -p orca-tui spinner_patch_requires_matching_syntax_theme_revision --lib
```

Expected: FAIL because `prepare` and `CachedMessage` do not carry syntax-theme
revision.

- [ ] **Step 3: Add the revision to cache identity**

Add to `CachedMessage`:

```rust
syntax_theme_revision: u64,
```

Add the parameter to `matches` and `patch_spinner`, and require equality in
both.

Add to `TranscriptRenderCache`:

```rust
prepared_syntax_theme_revision: Option<u64>,
```

Change `prepare` to:

```rust
pub fn prepare<F>(
    &mut self,
    messages: &[ChatMessage],
    revisions: &[u64],
    width: usize,
    theme: &Theme,
    syntax_theme_revision: u64,
    tick: u64,
    force_expand: bool,
    mut build_message: F,
) where
    F: FnMut(usize, &ChatMessage, &Theme, usize, u64, bool) -> Vec<Line<'static>>,
```

When the prepared syntax revision changes, dirty all messages exactly like a
width/theme change. Pass `index` into `build_message`, and store the revision
in every `CachedMessage`.

- [ ] **Step 4: Update every cache call site**

In `ui.rs`, pass:

```rust
theme.syntax_theme_revision
```

between `theme` and `tick`. Update welcome-cache and transcript-cache closures
to accept the leading `index`.

Update all `prepare` calls in `transcript_view.rs` and `types.rs` tests with
either:

```rust
theme.syntax_theme_revision
```

or an explicit test revision.

- [ ] **Step 5: Run focused GREEN checks**

Run:

```sh
cargo test -p orca-tui transcript_view --lib
cargo test -p orca-tui scroll_only_second_frame_builds_and_parses_zero_messages --lib
cargo test -p orca-tui tick_patches_running_or_receiving_spinners_without_rebuilding_messages --lib
```

Expected: PASS. The unchanged-revision cases must report zero message builds
and zero Markdown parses.

- [ ] **Step 6: Commit cache invalidation**

```sh
git add crates/orca-tui/src/transcript_view.rs crates/orca-tui/src/ui.rs \
  crates/orca-tui/src/types.rs
git commit -m "perf(tui): key transcript cache by syntax theme" \
  -m "Cache highlighted style runs with wrapped lines and rebuild only when syntax render state changes." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 5: Verified Full-File Style Computation

**Files:**
- Modify: `crates/orca-tui/src/diff_highlight.rs`
- Modify: `crates/orca-tui/src/syntax_highlight.rs`

- [ ] **Step 1: Add the failing Python scope fixture**

Add to `diff_highlight.rs` tests:

```rust
fn python_fixture() -> (String, String, usize) {
    let file = "\
class Item:
    \"\"\"A multiline description.

    More description.
    \"\"\"

    value: int = 42
";
    let diff = concat!(
        "--- a/item.py\n",
        "+++ b/item.py\n",
        "@@ -5,3 +5,3 @@\n",
        "     \"\"\"\n",
        " \n",
        "     value: int = 42\n",
    );
    let field_line = file
        .lines()
        .position(|line| line.contains("value: int"))
        .expect("field line")
        + 1;
    (file.to_string(), diff.to_string(), field_line)
}

#[test]
fn full_file_styles_correct_hunk_local_multiline_scope() {
    let theme = Theme::named(ThemeName::Dark);
    let (file, diff, field_line) = python_fixture();
    let parsed = parse_unified_diff(&diff);
    let cold = render_parsed_diff(&parsed, &theme, None);
    let refined = compute_file_scoped_styles(
        std::path::Path::new("item.py"),
        &file,
        &parsed.hunks,
        theme.syntax_theme,
    )
    .expect("verified style map");
    let warm = render_parsed_diff(&parsed, &theme, Some(&refined));

    assert!(refined.contains_key(&field_line));
    let cold_field = cold.iter().find(|line| line.to_string().contains("value: int")).unwrap();
    let warm_field = warm.iter().find(|line| line.to_string().contains("value: int")).unwrap();
    assert_ne!(cold_field.spans, warm_field.spans);
}

#[test]
fn file_text_drift_rejects_the_entire_style_map() {
    let theme = Theme::named(ThemeName::Dark);
    let (file, diff, _) = python_fixture();
    let parsed = parse_unified_diff(&diff);
    let drifted = file.replace("value: int = 42", "value: str = \"changed\"");

    assert!(compute_file_scoped_styles(
        std::path::Path::new("item.py"),
        &drifted,
        &parsed.hunks,
        theme.syntax_theme,
    )
    .is_none());
}
```

Add this delete-line assertion to
`full_file_styles_correct_hunk_local_multiline_scope` after `cold` and `warm`
are built:

```rust
let delete_diff = "\
--- a/item.py
+++ b/item.py
@@ -7 +7 @@
-    value: str = \"old\"
+    value: int = 42
";
let delete_parsed = parse_unified_diff(delete_diff);
let delete_cold = render_parsed_diff(&delete_parsed, &theme, None);
let delete_refined = compute_file_scoped_styles(
    std::path::Path::new("item.py"),
    &file,
    &delete_parsed.hunks,
    theme.syntax_theme,
)
.expect("matching inserted line");
let delete_warm = render_parsed_diff(&delete_parsed, &theme, Some(&delete_refined));
let cold_delete = delete_cold
    .iter()
    .find(|line| line.to_string().contains("-    value: str"))
    .expect("cold delete");
let warm_delete = delete_warm
    .iter()
    .find(|line| line.to_string().contains("-    value: str"))
    .expect("warm delete");
assert_eq!(cold_delete.spans, warm_delete.spans);
```

- [ ] **Step 2: Run the tests to verify RED**

Run:

```sh
cargo test -p orca-tui full_file_styles_correct_hunk_local_multiline_scope --lib
cargo test -p orca-tui file_text_drift_rejects_the_entire_style_map --lib
```

Expected: FAIL because `compute_file_scoped_styles` and `render_parsed_diff`
are not exposed.

- [ ] **Step 3: Implement one-pass full-file computation**

Add:

```rust
pub(crate) fn compute_file_scoped_styles(
    path: &Path,
    file_text: &str,
    hunks: &[DiffHunk],
    theme: SyntaxTheme,
) -> Option<RefinedDiffStyles>
```

Algorithm:

```rust
if !content_within_limits(file_text) {
    return None;
}
let expected = expected_new_side_lines(hunks)?;
let max_needed = expected.keys().copied().max()?;
let mut highlighter = highlighter_for_path(path, theme)?;
let mut output = RefinedDiffStyles::with_capacity(expected.len());

for (index, file_line) in file_text.lines().enumerate() {
    let line_number = index + 1;
    if line_number > max_needed {
        break;
    }
    let spans = highlighter.highlight_line(file_line)?;
    if let Some(expected_text) = expected.get(&line_number) {
        if file_line != expected_text {
            return None;
        }
        output.insert(line_number, spans);
    }
}

(output.len() == expected.len()).then_some(output)
```

`expected_new_side_lines` includes context and insert only. If the same
new-file line number appears with conflicting text, return `None`. It strips
only structural CR/LF already removed by parsing; it does not trim source
whitespace.

Expose a private `render_parsed_diff` used by both `render_unified_diff` and
tests so first-paint and overlay paths share exactly one renderer.

- [ ] **Step 4: Add direct full-file equivalence coverage**

Add:

```rust
#[test]
fn computed_field_styles_equal_a_direct_full_file_walk() {
    let theme = Theme::named(ThemeName::Dark);
    let (file, diff, field_line) = python_fixture();
    let parsed = parse_unified_diff(&diff);
    let computed = compute_file_scoped_styles(
        std::path::Path::new("item.py"),
        &file,
        &parsed.hunks,
        theme.syntax_theme,
    )
    .expect("computed styles");
    let mut direct = crate::syntax_highlight::highlighter_for_path(
        std::path::Path::new("item.py"),
        theme.syntax_theme,
    )
    .expect("Python syntax");
    let expected = file
        .lines()
        .enumerate()
        .map(|(index, line)| (index + 1, direct.highlight_line(line).unwrap()))
        .find(|(line_number, _)| *line_number == field_line)
        .map(|(_, spans)| spans)
        .expect("direct field styles");

    assert_eq!(computed.get(&field_line), Some(&expected));
}
```

Run:

```sh
cargo test -p orca-tui full_file --lib
```

Expected: PASS, including exact span segmentation and foreground styles.

- [ ] **Step 5: Commit full-file style computation**

```sh
git add crates/orca-tui/src/diff_highlight.rs \
  crates/orca-tui/src/syntax_highlight.rs
git commit -m "feat(tui): compute verified file-scoped diff styles" \
  -m "Walk post-edit source once and retain only matching visible new-side lines." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 6: Single Background Highlight Worker

**Files:**
- Modify: `crates/orca-tui/src/lib.rs`
- Create: `crates/orca-tui/src/edit_highlight_worker.rs`

- [ ] **Step 1: Add failing worker tests**

Register the test module in `crates/orca-tui/src/lib.rs` before the RED run:

```rust
mod edit_highlight_worker;
```

Then create tests defining owned jobs and outcomes:

```rust
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{EditHighlightJob, EditHighlightOutcome, coalesce_jobs, run_job};
    use crate::diff_highlight::parse_unified_diff;
    use crate::syntax_highlight::SyntaxTheme;

    fn job(id: u64, tool_id: &str, path: PathBuf, diff: &str) -> EditHighlightJob {
        EditHighlightJob {
            job_id: id,
            tool_id: tool_id.to_string(),
            message_index: 2,
            message_revision: 7,
            syntax_theme_revision: SyntaxTheme::OneHalfDark.revision(),
            syntax_theme: SyntaxTheme::OneHalfDark,
            absolute_path: path,
            display_path: "src/item.py".to_string(),
            parsed: parse_unified_diff(diff),
        }
    }

    #[test]
    fn coalescing_keeps_latest_job_per_tool_in_fifo_key_order() {
        let first = job(1, "edit-a", PathBuf::from("/a"), "");
        let queued = vec![
            job(2, "edit-a", PathBuf::from("/a2"), ""),
            job(3, "edit-b", PathBuf::from("/b"), ""),
        ];
        let jobs = coalesce_jobs(first, queued);
        assert_eq!(jobs.iter().map(|job| job.job_id).collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn worker_returns_ready_only_for_matching_capped_utf8_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("item.py");
        std::fs::write(&path, "value: int = 42\n").unwrap();
        let diff = "--- a/item.py\n+++ b/item.py\n@@ -1 +1 @@\n-value: int = 1\n+value: int = 42\n";
        let outcome = run_job(&job(1, "edit-a", path, diff));
        assert!(matches!(outcome, EditHighlightOutcome::Ready { .. }));
    }

    #[test]
    fn worker_rejects_unreadable_unsafe_or_drifted_files() {
        let dir = tempfile::tempdir().unwrap();
        let diff = concat!(
            "--- a/item.py\n",
            "+++ b/item.py\n",
            "@@ -1 +1 @@\n",
            "-value = 1\n",
            "+value = 2\n",
        );
        let cases = [
            ("missing.py", None),
            ("binary.py", Some(vec![0xff, 0xfe, 0xfd])),
            (
                "too-large.py",
                Some({
                    let line = format!(
                        "{}\n",
                        "x".repeat(crate::syntax_highlight::MAX_HIGHLIGHT_LINE_BYTES - 1)
                    );
                    let mut content = line
                        .repeat(
                            crate::syntax_highlight::MAX_HIGHLIGHT_BYTES
                                / crate::syntax_highlight::MAX_HIGHLIGHT_LINE_BYTES,
                        )
                        .into_bytes();
                    content.push(b'x');
                    content
                }),
            ),
            (
                "too-many-lines.py",
                Some(
                    format!(
                        "{}x",
                        "x\n".repeat(crate::syntax_highlight::MAX_HIGHLIGHT_LINES)
                    )
                    .into_bytes(),
                ),
            ),
            (
                "long-line.py",
                Some(vec![
                    b'x';
                    crate::syntax_highlight::MAX_HIGHLIGHT_LINE_BYTES + 1
                ]),
            ),
            ("drifted.py", Some(b"value = 3\n".to_vec())),
        ];

        for (name, bytes) in cases {
            let path = dir.path().join(name);
            if let Some(bytes) = bytes {
                std::fs::write(&path, bytes).unwrap();
            }
            assert!(
                matches!(
                    run_job(&job(10, name, path, diff)),
                    EditHighlightOutcome::Failed
                ),
                "unsafe worker fixture unexpectedly highlighted: {name}"
            );
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify RED**

Run:

```sh
cargo test -p orca-tui edit_highlight_worker --lib
```

Expected: FAIL because the worker contracts do not exist.

- [ ] **Step 3: Implement worker contracts and capped reads**

Implement:

```rust
#[derive(Clone, Debug)]
pub(crate) struct EditHighlightJob {
    pub(crate) job_id: u64,
    pub(crate) tool_id: String,
    pub(crate) message_index: usize,
    pub(crate) message_revision: u64,
    pub(crate) syntax_theme_revision: u64,
    pub(crate) syntax_theme: SyntaxTheme,
    pub(crate) absolute_path: PathBuf,
    pub(crate) display_path: String,
    pub(crate) parsed: ParsedDiff,
}

#[derive(Clone, Debug)]
pub(crate) enum EditHighlightOutcome {
    Ready {
        styles: std::sync::Arc<RefinedDiffStyles>,
    },
    Failed,
}

#[derive(Clone, Debug)]
pub(crate) struct EditHighlightResult {
    pub(crate) job: EditHighlightJob,
    pub(crate) outcome: EditHighlightOutcome,
}
```

`read_capped_utf8` must check metadata before read, verify bytes after read,
decode UTF-8, then call the shared full guardrail. `run_job` calls
`compute_file_scoped_styles` with the job's display path and syntax theme.

Use `std::collections::VecDeque` plus a `HashMap<String, usize>` in
`coalesce_jobs` so replacing a tool's job keeps the original key's FIFO
position without adding `indexmap`.

- [ ] **Step 4: Spawn one named worker and add runtime bookkeeping**

Implement:

```rust
pub(crate) struct EditHighlightRuntime {
    job_tx: crossbeam_channel::Sender<EditHighlightJob>,
    result_rx: crossbeam_channel::Receiver<EditHighlightResult>,
    pending: std::collections::HashMap<String, EditHighlightJob>,
    next_job_id: u64,
}
```

The `"orca-edit-highlight"` thread blocks on the first job, drains queued jobs
with `try_iter`, coalesces by tool ID, computes each result, and exits when the
result receiver is gone.

Expose:

```rust
pub(crate) fn new() -> Self;
pub(crate) fn allocate_job_id(&mut self) -> u64;
pub(crate) fn submit(&mut self, job: EditHighlightJob) -> bool;
pub(crate) fn drain_results(&self) -> Vec<EditHighlightResult>;
pub(crate) fn has_pending(&self) -> bool;
pub(crate) fn pending_count(&self) -> usize;
pub(crate) fn pending_matches(&self, job: &EditHighlightJob) -> bool;
pub(crate) fn finish_pending(&mut self, job: &EditHighlightJob) -> bool;
pub(crate) fn clear_pending(&mut self);
#[cfg(test)]
pub(crate) fn pending_job(&self, tool_id: &str) -> Option<EditHighlightJob>;
```

Submitting a newer job replaces the pending identity for the same tool ID.
`finish_pending` removes the entry only when tool ID, job ID, message index,
and message revision all match. `drain_results` owns a `Vec`, avoiding a
receiver borrow while `AppState` mutates pending state.

- [ ] **Step 5: Run focused GREEN checks**

Run:

```sh
cargo test -p orca-tui edit_highlight_worker --lib
```

Expected: PASS with no sleeps in unit tests. Use channel receive timeouts only
for the test that verifies the spawned worker returns a result.

- [ ] **Step 6: Commit the worker**

```sh
git add crates/orca-tui/src/lib.rs \
  crates/orca-tui/src/edit_highlight_worker.rs
git commit -m "feat(tui): add progressive diff highlight worker" \
  -m "Coalesce live edit jobs and compute bounded full-file styles outside the draw thread." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 7: AppState Ownership, Eligibility, and Stale-Result Rejection

**Files:**
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [ ] **Step 1: Add failing state-transition tests**

Add tests in `types.rs`:

```rust
#[test]
fn successful_live_edit_submits_one_versioned_highlight_job() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/item.py"), "value = 2\n").unwrap();
    let mut state = state();
    state.configure_syntax_highlighting(
        dir.path().to_path_buf(),
        crate::syntax_highlight::SyntaxTheme::OneHalfDark,
    );
    state.update(TuiEvent::ToolRequested {
        id: "edit-1".to_string(),
        name: "edit".to_string(),
        target: Some("src/item.py".to_string()),
    });
    state.update(TuiEvent::ToolCompleted {
        id: "edit-1".to_string(),
        name: "edit".to_string(),
        status: "completed".to_string(),
        output: "edited src/item.py".to_string(),
        diff: Some("--- a/src/item.py\n+++ b/src/item.py\n@@ -1 +1 @@\n-value = 1\n+value = 2\n".to_string()),
        kind: None,
    });

    assert!(state.edit_highlight_needs_tick());
    assert_eq!(state.pending_edit_highlight_count(), 1);
}

#[test]
fn replayed_history_messages_never_submit_jobs() {
    let mut state = state();
    state.push_message(ChatMessage::ToolCall {
        id: "historical-edit".to_string(),
        name: "edit".to_string(),
        target: Some("src/item.py".to_string()),
        status: "completed".to_string(),
        output: None,
        diff: Some("--- a/src/item.py\n+++ b/src/item.py\n@@ -1 +1 @@\n-a\n+b\n".to_string()),
        kind: None,
        expanded: false,
    });
    assert_eq!(state.pending_edit_highlight_count(), 0);
}
```

Add deterministic result-application tests by constructing
`EditHighlightResult` directly:

```rust
fn ready_result(job: EditHighlightJob) -> EditHighlightResult {
    let mut styles = crate::diff_highlight::RefinedDiffStyles::new();
    styles.insert(
        1,
        vec![ratatui::text::Span::styled(
            "value = 2",
            ratatui::style::Style::default().fg(ratatui::style::Color::Magenta),
        )],
    );
    EditHighlightResult {
        job,
        outcome: EditHighlightOutcome::Ready {
            styles: std::sync::Arc::new(styles),
        },
    }
}

#[test]
fn exact_edit_highlight_result_touches_only_matching_message() {
    let (mut state, job) = state_with_submitted_edit_job();
    state.push_message(ChatMessage::System("unrelated".to_string()));
    let before = state.message_revisions.clone();

    assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
    assert_ne!(state.message_revisions[job.message_index], before[job.message_index]);
    assert_eq!(
        state.message_revisions[job.message_index + 1],
        before[job.message_index + 1]
    );
    assert!(state.refined_diff_styles(&job.tool_id).is_some());
}

#[test]
fn stale_edit_highlight_identity_is_rejected() {
    let mutations: Vec<Box<dyn Fn(&mut AppState, &mut EditHighlightJob)>> = vec![
        Box::new(|state, job| {
            state.touch_message(job.message_index);
        }),
        Box::new(|_, job| {
            job.job_id += 1;
        }),
        Box::new(|_, job| {
            job.display_path = "src/other.py".to_string();
        }),
        Box::new(|state, job| {
            state.syntax_theme = crate::syntax_highlight::SyntaxTheme::OneHalfLight;
            job.syntax_theme_revision =
                crate::syntax_highlight::SyntaxTheme::OneHalfDark.revision();
        }),
    ];

    for mutate in mutations {
        let (mut state, mut job) = state_with_submitted_edit_job();
        mutate(&mut state, &mut job);
        assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
        assert!(state.refined_diff_styles(&job.tool_id).is_none());
    }
}

#[test]
fn message_lifecycle_prunes_applied_diff_highlights() {
    for action in ["clear", "truncate", "retain"] {
        let (mut state, job) = state_with_submitted_edit_job();
        assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
        match action {
            "clear" => state.clear_messages(),
            "truncate" => state.truncate_messages(job.message_index),
            "retain" => {
                state.retain_messages(|message| {
                    !matches!(
                        message,
                        ChatMessage::ToolCall { id, .. } if id == &job.tool_id
                    )
                });
            }
            _ => unreachable!(),
        }
        assert!(state.refined_diff_styles(&job.tool_id).is_none());
    }
}
```

Implement `state_with_submitted_edit_job` in the test module with the same
temp-directory setup and events as
`successful_live_edit_submits_one_versioned_highlight_job`; return the job
snapshot stored by the runtime's test-visible `pending_job(&tool_id)` accessor.

- [ ] **Step 2: Run the tests to verify RED**

Run:

```sh
cargo test -p orca-tui successful_live_edit_submits_one_versioned_highlight_job --lib
cargo test -p orca-tui stale_edit_highlight --lib
```

Expected: FAIL because `AppState` owns no syntax context, worker, pending jobs,
or refined maps.

- [ ] **Step 3: Add derived state without changing persisted ChatMessage**

Add:

```rust
pub(crate) struct AppliedDiffHighlight {
    pub(crate) display_path: String,
    pub(crate) styles: std::sync::Arc<crate::diff_highlight::RefinedDiffStyles>,
}
```

Add `AppState` fields:

```rust
workspace_root: Option<std::path::PathBuf>,
syntax_theme: crate::syntax_highlight::SyntaxTheme,
edit_highlight_runtime: Option<crate::edit_highlight_worker::EditHighlightRuntime>,
pub(crate) applied_diff_highlights:
    std::collections::HashMap<String, AppliedDiffHighlight>,
```

Initialize with no workspace, `OneHalfDark`, no runtime, and an empty map.

Implement:

```rust
pub(crate) fn configure_syntax_highlighting(
    &mut self,
    workspace_root: PathBuf,
    syntax_theme: SyntaxTheme,
);
pub(crate) fn edit_highlight_needs_tick(&self) -> bool;
pub(crate) fn poll_edit_highlight_results(&mut self) -> bool;
fn apply_edit_highlight_result(
    &mut self,
    result: crate::edit_highlight_worker::EditHighlightResult,
) -> bool;
pub(crate) fn refined_diff_styles(
    &self,
    tool_id: &str,
) -> Option<&RefinedDiffStyles>;
```

- [ ] **Step 4: Resolve only safe live post-edit paths**

Implement `resolve_edit_target`:

1. Require configured workspace root and relative tool target.
2. Canonicalize workspace root.
3. Join target and canonicalize the now-existing post-edit file.
4. Require the canonical file path to start with canonical workspace root.
5. Require `is_file`.

Do not use the shortened display `state.cwd` for filesystem access.

In `app.rs`, retain the real workspace path before calling `shorten_home`:

```rust
let workspace_root = config
    .cwd
    .clone()
    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
let cwd_display = shorten_home(&workspace_root.display().to_string());
...
state.configure_syntax_highlighting(workspace_root, theme.syntax_theme);
```

- [ ] **Step 5: Submit only after a successful live ToolCompleted update**

Refactor the `TuiEvent::ToolCompleted` arm so it obtains the final message index
whether updating an existing tool row or pushing a completion-only row.

After the message mutation:

```rust
if status == "completed" {
    self.submit_edit_highlight_for_message(index);
}
```

Eligibility requires:

- tool message still has matching ID;
- non-empty diff parses to at least one context/insert line;
- message target exists and safely resolves;
- destination path has known syntax;
- current message revision exists.

Build the job with current index/revision/theme revision. Direct
`push_message` during history replay never calls this submit method.

- [ ] **Step 6: Apply results with all stale checks**

For each result:

1. Remove only its exact pending `(tool_id, job_id)`.
2. Find `message_index`; require current message is the same tool ID.
3. Require current message revision equals job revision.
4. Require current syntax theme/revision equals job values.
5. Require current parsed destination path equals job display path.
6. On `Ready`, call `touch_message(index)` and then insert
   `AppliedDiffHighlight`.
7. On `Failed`, leave hunk-only rendering and add no transcript message.

`touch_message`, `replace_message`, and tool-message mutation must remove any
existing applied map for that tool before changing the revision. The successful
result path inserts its map after touching.

`clear_messages`, `replace_messages`, `truncate_messages`, and
`retain_messages` must call one shared pruning helper that retains applied maps
only for tool IDs still present.

- [ ] **Step 7: Run focused GREEN checks**

Run:

```sh
cargo test -p orca-tui successful_live_edit_submits_one_versioned_highlight_job --lib
cargo test -p orca-tui replayed_history_messages_never_submit_jobs --lib
cargo test -p orca-tui stale_edit_highlight --lib
cargo test -p orca-tui retaining_messages_rebases_watermarks_and_cache_entries --lib
```

Expected: PASS. Confirm exact application changes one `message_revisions`
entry and leaves every other revision unchanged.

- [ ] **Step 8: Commit state integration**

```sh
git add crates/orca-tui/src/types.rs crates/orca-tui/src/app.rs
git commit -m "feat(tui): version progressive diff refinements" \
  -m "Submit only live safe edit targets and reject stale worker results before cache invalidation." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 8: Render Refined Styles and Poll Off-Thread Results

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/types.rs`

- [ ] **Step 1: Add failing render/cache integration tests**

Add a `ui.rs` test that builds one tool message with a Python diff and a known
refined style map:

```rust
const REFINED_DIFF: &str = "\
--- a/item.py
+++ b/item.py
@@ -1 +1 @@
-value = 1
+value = 2
";

#[test]
fn tool_message_uses_refined_new_side_styles_but_keeps_delete_hunk_styles() {
    let theme = Theme::named(ThemeName::Dark);
    let tool = ChatMessage::ToolCall {
        id: "edit-1".to_string(),
        name: "edit".to_string(),
        target: Some("item.py".to_string()),
        status: "completed".to_string(),
        output: None,
        diff: Some(REFINED_DIFF.to_string()),
        kind: None,
        expanded: false,
    };
    let parsed = crate::diff_highlight::parse_unified_diff(REFINED_DIFF);
    let mut refined = crate::diff_highlight::RefinedDiffStyles::new();
    refined.insert(
        1,
        vec![Span::styled(
            "value = 2",
            Style::default().fg(Color::Magenta),
        )],
    );
    let cold = crate::diff_highlight::render_parsed_diff(&parsed, &theme, None);
    let lines = build_lines_for_message(&tool, &theme, 80, 0, false, Some(&refined));

    let warm_insert = lines
        .iter()
        .find(|line| line.to_string().contains("+value = 2"))
        .expect("warm insert");
    assert_eq!(warm_insert.spans[1].style.fg, Some(Color::Magenta));

    let cold_delete = cold
        .iter()
        .find(|line| line.to_string().contains("-value = 1"))
        .expect("cold delete");
    let warm_delete = lines
        .iter()
        .find(|line| line.to_string().contains("-value = 1"))
        .expect("warm delete");
    assert_eq!(cold_delete.spans, warm_delete.spans);
}
```

Add this integration test to the existing `types.rs` test module, reusing
`state_with_submitted_edit_job` and `ready_result` from Task 7:

```rust
#[test]
fn refinement_rebuilds_only_matching_message() {
    let (mut state, job) = state_with_submitted_edit_job();
    state.push_message(ChatMessage::System("stable".to_string()));
    let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
    let visited = std::cell::RefCell::new(Vec::new());
    state.transcript_render_cache.prepare(
        &state.messages,
        &state.message_revisions,
        80,
        &theme,
        theme.syntax_theme_revision,
        0,
        false,
        |index, message, theme, width, tick, force_expand| {
            visited.borrow_mut().push(index);
            crate::ui::build_lines_for_messages(
                std::slice::from_ref(message),
                theme,
                width,
                tick,
                force_expand,
            )
        },
    );
    let stable_index = job.message_index + 1;
    let revisions_before = state.message_revisions.clone();
    visited.borrow_mut().clear();

    assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
    state.transcript_render_cache.prepare(
        &state.messages,
        &state.message_revisions,
        80,
        &theme,
        theme.syntax_theme_revision,
        0,
        false,
        |index, message, theme, width, tick, force_expand| {
            visited.borrow_mut().push(index);
            crate::ui::build_lines_for_messages(
                std::slice::from_ref(message),
                theme,
                width,
                tick,
                force_expand,
            )
        },
    );

    assert_eq!(*visited.borrow(), vec![job.message_index]);
    assert_eq!(
        state.message_revisions[stable_index],
        revisions_before[stable_index]
    );
}
```

- [ ] **Step 2: Run the tests to verify RED**

Run:

```sh
cargo test -p orca-tui tool_message_uses_refined_new_side_styles --lib
cargo test -p orca-tui refinement_rebuilds_only_matching_message --lib
```

Expected: FAIL because message rendering cannot receive derived styles.

- [ ] **Step 3: Add an indexed single-message render path**

Introduce:

```rust
fn build_lines_for_message(
    message: &ChatMessage,
    theme: &Theme,
    width: usize,
    tick: u64,
    force_expand: bool,
    refined_diff: Option<&crate::diff_highlight::RefinedDiffStyles>,
) -> Vec<Line<'static>>;
```

Keep `build_lines_for_messages` as the existing test/convenience wrapper; it
loops and passes `None`.

Add `refined_diff` to `append_message_lines`. In the `ToolCall` arm, pass it to
`append_diff_lines`.

- [ ] **Step 4: Look up derived styles inside cache misses only**

In `render_live_messages`, split borrows before `prepare`:

```rust
let messages = &state.messages;
let revisions = &state.message_revisions;
let highlights = &state.applied_diff_highlights;
let cache = &mut state.transcript_render_cache;
cache.prepare(
    messages,
    revisions,
    width,
    theme,
    theme.syntax_theme_revision,
    state.tick,
    false,
    |index, message, theme, width, tick, force_expand| {
        let refined = match message {
            ChatMessage::ToolCall { id, .. } => highlights.get(id).map(|value| value.styles.as_ref()),
            _ => None,
        };
        build_lines_for_message(
            message,
            theme,
            width,
            tick,
            force_expand,
            refined,
        )
    },
);
```

Expose the map to `ui.rs` as `pub(crate)` or provide an immutable accessor that
does not borrow the cache. Do not clone style maps per frame.

- [ ] **Step 5: Poll results in the main loop**

At the beginning of each main-loop iteration, before calculating animation
demand:

```rust
if state.poll_edit_highlight_results() {
    scheduler.mark_dirty();
}
```

Extend animation demand:

```rust
let animation_active = state.status == AppStatus::Running
    || state.copy_notice.is_some()
    || state.drag_edge_scroll.is_some()
    || state.edit_highlight_needs_tick();
```

This guarantees result polling while idle without drawing continuously after
pending jobs reach a terminal result.

- [ ] **Step 6: Run focused GREEN checks**

Run:

```sh
cargo test -p orca-tui tool_message_uses_refined_new_side_styles --lib
cargo test -p orca-tui refinement_rebuilds_only_matching_message --lib
cargo test -p orca-tui scroll_only_second_frame_builds_and_parses_zero_messages --lib
cargo test -p orca-tui tick_patches_running_or_receiving_spinners_without_rebuilding_messages --lib
cargo test -p orca-tui frame_scheduler --lib
```

Expected: PASS. No syntax function may be called from `viewport`,
`materialize_rows`, selection overlay, or clipboard extraction.

- [ ] **Step 7: Commit progressive rendering**

```sh
git add crates/orca-tui/src/ui.rs crates/orca-tui/src/app.rs \
  crates/orca-tui/src/types.rs
git commit -m "feat(tui): apply progressive diff styles in place" \
  -m "Poll verified worker results and rebuild only the affected cached transcript message." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 9: Foreground Preservation, End-to-End Coverage, and Completion Audit

**Files:**
- Modify: `crates/orca-tui/src/selection.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/edit_highlight_worker.rs`
- Verify: `Cargo.toml`
- Verify: `Cargo.lock`
- Verify: `crates/orca-tui/Cargo.toml`
- Verify: `docs/superpowers/specs/2026-07-23-tui-syntax-highlighting-design.md`

- [ ] **Step 1: Add the final failing foreground-preservation test**

Extend the existing selection test:

```rust
#[test]
fn selection_background_preserves_multiple_syntax_foregrounds() {
    let line = Line::from(vec![
        Span::styled("let", Style::default().fg(Color::Magenta)),
        Span::styled(" value = ", Style::default().fg(Color::White)),
        Span::styled("\"hello\"", Style::default().fg(Color::Green)),
    ]);
    let highlighted = apply_selection_to_line(line, 0, None, SEL_BG);
    let foregrounds = highlighted
        .spans
        .iter()
        .map(|span| span.style.fg)
        .collect::<Vec<_>>();

    assert_eq!(
        foregrounds,
        vec![Some(Color::Magenta), Some(Color::White), Some(Color::Green)]
    );
    assert!(highlighted.spans.iter().all(|span| span.style.bg == Some(SEL_BG)));
}
```

- [ ] **Step 2: Run the test to verify RED or existing coverage**

Run:

```sh
cargo test -p orca-tui selection_background_preserves_multiple_syntax_foregrounds --lib
```

Expected: PASS is acceptable only if the pre-existing selection implementation
already satisfies the new syntax-specific contract. If it fails, change only
selection span splitting/background overlay until foregrounds remain intact.

- [ ] **Step 3: Run all focused feature tests**

Run:

```sh
cargo test -p orca-tui syntax_highlight --lib
cargo test -p orca-tui diff_highlight --lib
cargo test -p orca-tui edit_highlight_worker --lib
cargo test -p orca-tui transcript_view --lib
cargo test -p orca-tui selection --lib
```

Expected: all PASS with zero failures.

- [ ] **Step 4: Run the complete changed-crate test suite**

Run:

```sh
cargo test -p orca-tui
```

Expected: PASS with zero failed tests.

- [ ] **Step 5: Format and lint**

Run:

```sh
cargo fmt --all
cargo fmt --all -- --check
cargo clippy -p orca-tui --all-targets -- -D warnings
```

Expected: format check and clippy exit 0 with no warnings. If formatting changes
files, inspect the diff before continuing.

- [ ] **Step 6: Run workspace regression tests**

Run:

```sh
cargo test --workspace
```

Expected: PASS with zero failed tests. Do not fix unrelated failures; record
their exact command and output if the workspace has a pre-existing breakage.

- [ ] **Step 7: Perform the prompt-to-artifact completion audit**

Build and verify this evidence table from the current checkout:

| Requirement | Required evidence |
|---|---|
| `syntect + two-face` | Workspace and `orca-tui` manifests plus `Cargo.lock` entries |
| Fenced code highlighting | Markdown render tests with multiple foregrounds and preserved text |
| Diff syntax highlighting | Parsed Rust/Python diff tests with preserved prefixes |
| `syntax_theme_revision` cache key | Cache struct/match/prepare fields plus revision-change test |
| No per-frame cost | Scroll-only and spinner-only zero-build/zero-parse tests |
| `>512 KiB` fallback | Syntax and worker over-byte tests |
| `>10,000` lines fallback | No-trailing-newline syntax and worker tests |
| single line `>4 KiB` fallback | Syntax and worker long-line tests |
| Hunk-first rendering | Separate old/new parser-state tests |
| Background full-file refinement | Python multiline-scope equivalence test and worker-ready test |
| Disk/diff consistency | Drift rejection test |
| Stale-result safety | message revision, job ID, path, and theme rejection tests |
| Replay safety | history push does not queue worker test |
| Only affected cache invalidated | two-message one-rebuild test |
| Selection compatibility | multiple syntax foreground preservation test |

Inspect the actual source and fresh command output for every row. Treat a
missing test or indirect proxy as incomplete and add the missing direct
coverage before declaring success.

- [ ] **Step 8: Check repository state**

Run:

```sh
git diff --check
git status --short
git diff --stat 575c850d..HEAD
git log --format='%h %s%n%(trailers:key=Co-authored-by,valueonly)' 575c850d..HEAD
```

Expected:

- `git diff --check` exits 0.
- Only intended source, manifest, lockfile, test, and plan files changed.
- Every implementation commit has exactly one
  `TRAE CLI <noreply@bytedance.com>` co-author trailer.

- [ ] **Step 9: Commit final test refinements if Step 1 changed files**

```sh
git add crates/orca-tui/src/selection.rs crates/orca-tui/src/ui.rs \
  crates/orca-tui/src/types.rs crates/orca-tui/src/edit_highlight_worker.rs
git commit -m "test(tui): cover syntax highlight integration" \
  -m "Pin selection foregrounds, progressive stale checks, and cached render behavior." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

Skip this commit only when Steps 1-8 produced no uncommitted file changes.
