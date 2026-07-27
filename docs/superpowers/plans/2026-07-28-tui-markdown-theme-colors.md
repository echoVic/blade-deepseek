# TUI Markdown Theme Colors Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every Markdown presentation color come from the selected Orca theme while preserving text, layout, syntax highlighting, and transcript-cache performance.

**Architecture:** `Theme` gains four document-specific semantic colors. `ui.rs` maps Markdown roles to those fields plus the existing `text` and `muted` colors, while `TranscriptRenderCache::ThemeIdentity` includes the new fields so style-only theme changes rebuild cached rows exactly once.

**Tech Stack:** Rust 2024, ratatui 0.29, pulldown-cmark 0.12, existing syntect/two-face syntax engine.

---

## File Map

- Modify `crates/orca-tui/src/theme.rs`
  - Add four Markdown semantic colors.
  - Define exact values for Dark, Light, Solarized, and Catppuccin.
  - Add palette contract tests.
- Modify `crates/orca-tui/src/ui.rs`
  - Replace Markdown-only fixed ANSI colors with `Theme` fields.
  - Pass `Theme` through table render helpers.
  - Add direct role-to-color tests for every named theme.
- Modify `crates/orca-tui/src/transcript_view.rs`
  - Include Markdown colors in the value-based cache identity.
  - Add cache invalidation and steady-state hit tests.
- Verify `crates/orca-tui/src/syntax_highlight.rs`
  - No production change; highlighted code continues using syntect spans.

## Required Working Discipline

- Run every RED command before its production change.
- Confirm failures are color-contract failures, not fixture or parsing errors.
- Do not change Markdown text, row count, wrapping, table layout, syntax-token
  colors, or selection behavior.
- Do not clean up hardcoded colors outside the Markdown rendering chain.
- Every commit ends with exactly:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

---

### Task 1: Define the Markdown Theme Contract

**Files:**
- Modify: `crates/orca-tui/src/theme.rs`

- [ ] **Step 1: Add failing named-palette tests**

Extend the `theme.rs` test module:

```rust
use ratatui::style::Color;

#[test]
fn named_themes_define_markdown_semantic_colors() {
    let cases = [
        (
            ThemeName::Dark,
            [
                Color::Rgb(77, 107, 254),
                Color::Rgb(169, 139, 245),
                Color::Rgb(217, 164, 65),
                Color::Rgb(64, 170, 170),
            ],
        ),
        (
            ThemeName::Light,
            [
                Color::Rgb(58, 86, 230),
                Color::Rgb(138, 92, 230),
                Color::Rgb(176, 122, 20),
                Color::Rgb(0, 102, 102),
            ],
        ),
        (
            ThemeName::Solarized,
            [
                Color::Rgb(38, 139, 210),
                Color::Rgb(42, 161, 152),
                Color::Rgb(181, 137, 0),
                Color::Rgb(211, 54, 130),
            ],
        ),
        (
            ThemeName::Catppuccin,
            [
                Color::Rgb(203, 166, 247),
                Color::Rgb(116, 199, 236),
                Color::Rgb(249, 226, 175),
                Color::Rgb(245, 194, 231),
            ],
        ),
    ];

    for (name, expected) in cases {
        let theme = Theme::named(name);
        assert_eq!(
            [
                theme.markdown_h1,
                theme.markdown_h2,
                theme.markdown_h3,
                theme.markdown_inline_code,
            ],
            expected,
            "{name:?}"
        );
    }
}

#[test]
fn markdown_semantic_colors_do_not_use_fixed_ansi_accents() {
    let forbidden = [Color::Cyan, Color::Green, Color::Yellow, Color::Magenta];

    for name in [
        ThemeName::Dark,
        ThemeName::Light,
        ThemeName::Solarized,
        ThemeName::Catppuccin,
    ] {
        let theme = Theme::named(name);
        for color in [
            theme.markdown_h1,
            theme.markdown_h2,
            theme.markdown_h3,
            theme.markdown_inline_code,
        ] {
            assert!(!forbidden.contains(&color), "{name:?}: {color:?}");
        }
    }
}
```

- [ ] **Step 2: Run the theme tests to verify RED**

Run:

```sh
cargo test -p orca-tui markdown_semantic_colors --lib
cargo test -p orca-tui named_themes_define_markdown --lib
```

Expected: compilation fails because the four `Theme` fields do not exist.

- [ ] **Step 3: Add the Markdown fields and exact palettes**

Add to `Theme` after `plan_mode`:

```rust
pub markdown_h1: Color,
pub markdown_h2: Color,
pub markdown_h3: Color,
pub markdown_inline_code: Color,
```

Initialize each `Theme::named` branch with the exact values in the failing
test. Keep `syntax_theme` and `syntax_theme_revision` unchanged.

- [ ] **Step 4: Run Task 1 GREEN checks**

Run:

```sh
cargo test -p orca-tui theme::tests --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

Expected: PASS without warnings.

- [ ] **Step 5: Commit the theme contract**

```sh
git add crates/orca-tui/src/theme.rs
git commit -m "feat(tui): add Markdown theme colors" \
  -m "Define document-specific heading and inline-code accents for every named palette." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 2: Route Markdown Rendering Through Theme Semantics

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`

- [ ] **Step 1: Replace the old inline-code expectation with a failing theme test**

Rename `inline_code_keeps_magenta_style` and change its assertion:

```rust
#[test]
fn inline_code_uses_the_selected_markdown_theme_color() {
    for name in [
        ThemeName::Dark,
        ThemeName::Light,
        ThemeName::Solarized,
        ThemeName::Catppuccin,
    ] {
        let theme = Theme::named(name);
        let lines = render_markdown("Use `cargo test` now.", 80, &theme);
        let inline = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == "`cargo test`")
            .expect("inline code span");

        assert_eq!(
            inline.style.fg,
            Some(theme.markdown_inline_code),
            "{name:?}"
        );
    }
}
```

- [ ] **Step 2: Add failing heading and prose role tests**

Add:

```rust
#[test]
fn markdown_roles_use_selected_theme_semantics() {
    let fixture = "# One\n## Two\n### Three\n\nPlain **bold** *italic*.\n\n- item\n\n> quote";

    for name in [
        ThemeName::Dark,
        ThemeName::Light,
        ThemeName::Solarized,
        ThemeName::Catppuccin,
    ] {
        let theme = Theme::named(name);
        let lines = render_markdown(fixture, 80, &theme);

        let span = |text: &str| {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content == text)
                .expect(text)
        };
        assert_eq!(span("One").style.fg, Some(theme.markdown_h1), "{name:?}");
        assert_eq!(span("Two").style.fg, Some(theme.markdown_h2), "{name:?}");
        assert_eq!(
            span("Three").style.fg,
            Some(theme.markdown_h3),
            "{name:?}"
        );
        assert_eq!(span("Plain ").style.fg, Some(theme.text), "{name:?}");
        assert_eq!(span("bold").style.fg, Some(theme.text), "{name:?}");
        assert_eq!(span("italic").style.fg, Some(theme.text), "{name:?}");
        assert_eq!(span("• ").style.fg, Some(theme.muted), "{name:?}");
        assert_eq!(span("│ ").style.fg, Some(theme.muted), "{name:?}");
        assert_eq!(span("quote").style.fg, Some(theme.muted), "{name:?}");
    }
}
```

- [ ] **Step 3: Add failing table and plain-code tests**

Add:

```rust
#[test]
fn markdown_tables_and_plain_code_use_theme_semantics() {
    for name in [
        ThemeName::Dark,
        ThemeName::Light,
        ThemeName::Solarized,
        ThemeName::Catppuccin,
    ] {
        let theme = Theme::named(name);
        let grid = render_markdown("| Name | Value |\n|---|---|\n| A | B |", 80, &theme);
        let header = grid
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains("Name"))
            .expect("table header");
        let value = grid
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.trim() == "A")
            .expect("table value");
        let separator = grid
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains('━'))
            .expect("table separator");
        assert_eq!(header.style.fg, Some(theme.markdown_h1), "{name:?}");
        assert_eq!(value.style.fg, Some(theme.text), "{name:?}");
        assert_eq!(separator.style.fg, Some(theme.muted), "{name:?}");

        let records = render_markdown(
            "| First column | Second column | Third column |\n|---|---|---|\n| one | two | three |",
            18,
            &theme,
        );
        let key = records
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains("First column:"))
            .expect("record key");
        assert_eq!(key.style.fg, Some(theme.markdown_h3), "{name:?}");

        let plain = render_markdown("```not-a-real-language\ncall();\n```", 80, &theme);
        let source = plain
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains("call();"))
            .expect("plain code");
        assert_eq!(source.style.fg, Some(theme.muted), "{name:?}");
    }
}
```

- [ ] **Step 4: Run renderer tests to verify RED**

Run:

```sh
cargo test -p orca-tui inline_code_uses_the_selected --lib
cargo test -p orca-tui markdown_roles_use_selected --lib
cargo test -p orca-tui markdown_tables_and_plain_code --lib
```

Expected: assertions fail because current spans use fixed ANSI colors.

- [ ] **Step 5: Migrate prose, headings, inline code, lists, and quotes**

In `render_markdown`:

```rust
let mut style_stack = vec![Style::default().fg(theme.text)];
```

Map headings:

```rust
let color = match level {
    HeadingLevel::H1 => theme.markdown_h1,
    HeadingLevel::H2 => theme.markdown_h2,
    _ => theme.markdown_h3,
};
```

Use `theme.markdown_inline_code` for `Event::Code`. Use `theme.muted` for list
markers, quote markers, and quote content. Keep strong and emphasis modifier
logic unchanged.

In `append_code_block`, change only the fallback:

```rust
let style = Style::default().fg(theme.muted);
```

- [ ] **Step 6: Thread the theme through table helpers**

Change:

```rust
fn render_table(
    rows: &[Vec<String>],
    lines: &mut Vec<Line<'static>>,
    available_width: usize,
    theme: &Theme,
)
```

Pass `theme` from `render_markdown`, then to:

```rust
fn render_table_grid(
    rows: &[Vec<String>],
    col_widths: &[usize],
    col_gap: usize,
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
)

fn render_table_as_records(
    rows: &[Vec<String>],
    lines: &mut Vec<Line<'static>>,
    available_width: usize,
    theme: &Theme,
)
```

Inside both helpers:

```rust
let header_style = Style::default()
    .fg(theme.markdown_h1)
    .add_modifier(Modifier::BOLD);
let cell_style = Style::default().fg(theme.text);
let separator_style = Style::default().fg(theme.muted);
```

For record tables additionally use:

```rust
let key_style = Style::default().fg(theme.markdown_h3);
let value_style = Style::default().fg(theme.text);
```

Do not change table text, width allocation, or row insertion.

- [ ] **Step 7: Update old fixed-color regression assertions**

Change `unknown_and_oversized_fences_keep_plain_gray_code_style` and
`gray_code_fallback_preserves_source_boundaries` to assert `theme.muted`
instead of `Color::Gray`. Rename the first test to:

```rust
fn unknown_and_oversized_fences_use_muted_theme_fallback()
```

Preserve all source-boundary assertions.

- [ ] **Step 8: Run Task 2 GREEN checks**

Run:

```sh
cargo test -p orca-tui markdown_ --lib
cargo test -p orca-tui inline_code --lib
cargo test -p orca-tui table --lib
cargo test -p orca-tui fenced_ --lib
cargo test -p orca-tui gray_code_fallback --lib
cargo test -p orca-tui ui::tests --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

Expected: PASS. Existing highlighted Rust tests must still report multiple
token foregrounds.

- [ ] **Step 9: Commit the renderer migration**

```sh
git add crates/orca-tui/src/ui.rs
git commit -m "refactor(tui): theme Markdown rendering" \
  -m "Replace fixed ANSI document colors with named palette semantics without changing Markdown layout." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 3: Invalidate Transcript Rows on Markdown Palette Changes

**Files:**
- Modify: `crates/orca-tui/src/transcript_view.rs`

- [ ] **Step 1: Add a theme-aware cache test helper**

Refactor the test helper to accept an explicit theme:

```rust
fn prepare_with_theme_and_counters(
    cache: &mut TranscriptRenderCache,
    messages: &[ChatMessage],
    revisions: &[u64],
    width: usize,
    theme: &Theme,
    syntax_theme_revision: u64,
    tick: u64,
    counters: RenderCounters<'_>,
) {
    cache.prepare(
        messages,
        revisions,
        TranscriptRenderContext::new(theme, width, tick, false)
            .with_syntax_theme_revision(syntax_theme_revision),
        |_, message, theme, width, tick, force_expand| {
            counters
                .message_builds
                .set(counters.message_builds.get() + 1);
            if matches!(message, ChatMessage::Assistant(_)) {
                counters
                    .markdown_parses
                    .set(counters.markdown_parses.get() + 1);
            }
            build_lines_for_messages(
                std::slice::from_ref(message),
                theme,
                width,
                tick,
                force_expand,
            )
        },
    );
}
```

Keep `prepare_with_counters` as a small wrapper that creates the Dark theme so
existing tests remain concise.

- [ ] **Step 2: Add the failing cache identity test**

Add:

```rust
#[test]
fn markdown_theme_color_change_rebuilds_wrapped_lines_once() {
    let messages = vec![ChatMessage::Assistant(
        "# Heading\n\nUse `cargo test`.".to_string(),
    )];
    let revisions = vec![1];
    let builds = Cell::new(0);
    let parses = Cell::new(0);
    let mut cache = TranscriptRenderCache::default();
    let theme = theme();

    prepare_with_theme_and_counters(
        &mut cache,
        &messages,
        &revisions,
        40,
        &theme,
        theme.syntax_theme_revision,
        0,
        RenderCounters::new(&builds, &parses),
    );
    builds.set(0);
    parses.set(0);

    let mut changed = theme;
    changed.markdown_inline_code = Color::Rgb(1, 2, 3);
    prepare_with_theme_and_counters(
        &mut cache,
        &messages,
        &revisions,
        40,
        &changed,
        changed.syntax_theme_revision,
        0,
        RenderCounters::new(&builds, &parses),
    );
    assert_eq!(builds.get(), 1);
    assert_eq!(parses.get(), 1);

    builds.set(0);
    parses.set(0);
    prepare_with_theme_and_counters(
        &mut cache,
        &messages,
        &revisions,
        40,
        &changed,
        changed.syntax_theme_revision,
        0,
        RenderCounters::new(&builds, &parses),
    );
    assert_eq!(builds.get(), 0);
    assert_eq!(parses.get(), 0);
    assert_eq!(cache.last_prepare_visited(), 0);
}
```

- [ ] **Step 3: Run the cache test to verify RED**

Run:

```sh
cargo test -p orca-tui markdown_theme_color_change --lib
```

Expected: the changed theme remains a cache hit because `ThemeIdentity` does
not yet include the Markdown field.

- [ ] **Step 4: Extend `ThemeIdentity`**

Add:

```rust
markdown_h1: Color,
markdown_h2: Color,
markdown_h3: Color,
markdown_inline_code: Color,
```

Populate all four fields in `impl From<&Theme> for ThemeIdentity`.

Do not change `syntax_theme_revision`, cache matching, spinner patching, or
dirty-index logic.

- [ ] **Step 5: Run Task 3 GREEN checks**

Run:

```sh
cargo test -p orca-tui markdown_theme_color_change --lib
cargo test -p orca-tui syntax_theme_revision --lib
cargo test -p orca-tui spinner_patch_requires_matching --lib
cargo test -p orca-tui transcript_view::tests --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

Expected: PASS. The new test reports one rebuild after the color change and
zero visits for the stable follow-up prepare.

- [ ] **Step 6: Commit the cache identity**

```sh
git add crates/orca-tui/src/transcript_view.rs
git commit -m "perf(tui): key Markdown cache by theme colors" \
  -m "Rebuild wrapped transcript rows exactly once when document palette semantics change." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 4: Final Verification and Delivery

**Files:**
- Verify: `crates/orca-tui/src/theme.rs`
- Verify: `crates/orca-tui/src/ui.rs`
- Verify: `crates/orca-tui/src/transcript_view.rs`
- Verify: `crates/orca-tui/src/syntax_highlight.rs`

- [ ] **Step 1: Run focused feature verification**

Run:

```sh
cargo test -p orca-tui markdown_theme --lib
cargo test -p orca-tui inline_code --lib
cargo test -p orca-tui table --lib
cargo test -p orca-tui fenced_ --lib
cargo test -p orca-tui transcript_view::tests --lib
```

Expected: PASS with nonzero selected tests.

- [ ] **Step 2: Run full package verification**

Run:

```sh
cargo test -p orca-tui -- --test-threads=1
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

Expected: PASS.

- [ ] **Step 3: Audit prompt-to-artifact coverage**

Verify from current source and fresh command output:

| Requirement | Direct evidence |
|---|---|
| H1/H2/H3 no fixed ANSI colors | Per-theme renderer assertions |
| Inline code no fixed Magenta | Per-theme inline-code assertion |
| Light/Solarized have explicit document palettes | Exact `Theme::named` tests |
| Prose uses selected theme text | Prose/strong/emphasis assertions |
| Quotes/lists use selected muted color | Marker and quote-content assertions |
| Grid and record tables are themed | Header/value/separator/key assertions |
| Plain code fallback is themed | Unknown and oversized fence assertions |
| Syntax-highlighted code is unchanged | Existing Rust token foreground tests |
| Markdown color changes invalidate cache | One-rebuild cache test |
| Stable theme does not rebuild | Zero-visit follow-up cache assertion |
| Non-Markdown hardcoded colors untouched | Range diff inspection |

Treat any missing direct evidence as incomplete.

- [ ] **Step 4: Review commit metadata and range**

Run:

```sh
git status --short
git log --format='%h %s%n%(trailers:key=Co-authored-by,valueonly)' 9d6604f..HEAD
git diff --check 9d6604f..HEAD
git diff --stat 9d6604f..HEAD
```

Expected: clean worktree, one trailer per commit, and changes limited to the
design, plan, `theme.rs`, `ui.rs`, and `transcript_view.rs`.

- [ ] **Step 5: Request final holistic review**

Review the complete range against:

```text
docs/superpowers/specs/2026-07-28-tui-markdown-theme-colors-design.md
```

The review must confirm semantic coverage, cache correctness, no syntax-theme
regression, and no unrelated color cleanup.

- [ ] **Step 6: Push and verify the remote branch**

After approval:

```sh
git push origin feature/tui-syntax-highlighting
local_sha=$(git rev-parse HEAD)
remote_sha=$(git ls-remote --heads origin feature/tui-syntax-highlighting | awk '{print $1}')
test "$local_sha" = "$remote_sha"
```

Keep the branch and working directory for P0 #4.
