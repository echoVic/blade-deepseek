# TUI Terminal Capabilities and Theme Fallback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Detect terminal background and color depth once before TUI startup, select an automatic light/dark theme, and emit only colors supported by the terminal.

**Architecture:** Core config adds `ThemeName::Auto`. A focused `terminal_capabilities.rs` module models background and color depth, performs deterministic color quantization, and resolves a final immutable `Theme`. Syntax and diff styles receive the same color-level identity at generation time. A dedicated qwertty runtime owns terminal input, probing, raw mode, input modes, and cleanup for the full TUI lifetime; crossterm remains ratatui's output backend only.

**Tech Stack:** Rust 2024, ratatui 0.29, crossterm 0.28, qwertty 0.1.6 with Tokio, supports-color 3.0.2, syntect/two-face.

---

## File Map

- Modify `crates/orca-core/src/config/mod.rs`
  - Add `ThemeName::Auto` and make it the default.
  - Render stable kebab-case theme names in config output.
  - Add serialization/default tests.
- Modify `crates/orca-core/src/config/file.rs`
  - Add omitted/auto/explicit theme config tests.
- Add `crates/orca-tui/src/terminal_capabilities.rs`
  - Define background/color-level/profile types.
  - Map supports-color facts.
  - Quantize RGB to ANSI-256/ANSI-16 and adapt styles.
  - Convert qwertty OSC 11 and supports-color facts into a profile.
- Add `crates/orca-tui/src/input_adapter.rs`
  - Convert qwertty's typed input vocabulary to existing crossterm events.
  - Reassemble segmented UTF-8 paste and reject unsupported input safely.
- Add `crates/orca-tui/src/input_runtime.rs`
  - Permanently own `TokioTerminalSession` on a named current-thread runtime.
  - Probe, enable modes, forward bounded input, and restore on stop or panic.
- Add `crates/orca-tui/src/capability_backend.rs`
  - Adapt changed ratatui cells as a final output safety boundary.
  - Delegate the complete ratatui Backend API.
- Modify `crates/orca-tui/src/lib.rs`
  - Register the module.
- Modify `Cargo.toml`, `crates/orca-tui/Cargo.toml`, `Cargo.lock`
  - Add qwertty with Tokio and supports-color.
- Modify `crates/orca-tui/src/theme.rs`
  - Resolve Auto to Dark/Light.
  - Adapt every theme color.
  - Include color-level syntax revision and selection style.
- Modify `crates/orca-tui/src/transcript_view.rs`
  - Include color level in theme identity.
- Modify `crates/orca-tui/src/syntax_highlight.rs`
  - Adapt syntect styles at conversion time.
- Modify `crates/orca-tui/src/diff_highlight.rs`
  - Pass color level to line highlighters.
- Modify `crates/orca-tui/src/edit_highlight_worker.rs`
  - Carry color level through background jobs and identity.
- Modify `crates/orca-tui/src/types.rs`
  - Store syntax color level and reject stale results.
- Modify `crates/orca-tui/src/app.rs`
  - Start the input runtime before constructing ratatui.
  - Consume its bounded event receiver instead of crossterm input.
  - Drop ratatui before stopping and joining qwertty.
- Remove production use of `crates/orca-tui/src/terminal_lifecycle.rs`
  - Qwertty becomes the only raw/alternate/mouse/paste/kitty lifecycle owner.
- Modify `crates/orca-tui/src/selection.rs`
  - Apply a selection Style instead of background-only color.
- Modify `crates/orca-tui/src/ui.rs`
  - Use resolved selection style for transcript, composer, and jump pill.

## Required Working Discipline

- Run every RED command before production changes.
- Keep each task compiling and commit independently.
- Never send a real OSC query from automated tests.
- Preserve explicit theme choices.
- Preserve all text, layout, cursor, selection, diff, and syntax source boundaries.
- Do not add notification/title/focus behavior from P0 #5.
- Every commit ends with exactly:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

---

### Task 1: Add Auto Theme Configuration

**Files:**
- Modify: `crates/orca-core/src/config/mod.rs`
- Modify: `crates/orca-core/src/config/file.rs`

- [ ] **Step 1: Add failing default and serde tests**

In `config/mod.rs` tests:

```rust
#[test]
fn theme_name_defaults_to_auto_and_round_trips_all_values() {
    assert_eq!(ThemeName::default(), ThemeName::Auto);

    for (wire, theme) in [
        ("\"auto\"", ThemeName::Auto),
        ("\"dark\"", ThemeName::Dark),
        ("\"light\"", ThemeName::Light),
        ("\"solarized\"", ThemeName::Solarized),
        ("\"catppuccin\"", ThemeName::Catppuccin),
    ] {
        assert_eq!(serde_json::from_str::<ThemeName>(wire).unwrap(), theme);
        assert_eq!(serde_json::to_string(&theme).unwrap(), wire);
    }
}
```

In `config/file.rs` tests:

```rust
#[test]
fn omitted_and_explicit_auto_theme_parse_as_auto() {
    assert_eq!(toml::from_str::<FileConfig>("").unwrap().theme, ThemeName::Auto);
    assert_eq!(
        toml::from_str::<FileConfig>("theme = \"auto\"").unwrap().theme,
        ThemeName::Auto
    );
}
```

- [ ] **Step 2: Add failing config display test**

Add to `format_config_show_redacts_api_key_and_includes_effective_values` or a focused test:

```rust
let mut config = test_run_config();
config.theme = ThemeName::Auto;
assert!(format_config_show(&config).contains("theme = \"auto\""));
```

- [ ] **Step 3: Run RED**

```sh
cargo test -p orca-core theme_name_defaults_to_auto --lib
cargo test -p orca-core omitted_and_explicit_auto --lib
cargo test -p orca-core format_config_show --lib
```

Expected: compile errors because `ThemeName::Auto` does not exist.

- [ ] **Step 4: Add Auto and stable display names**

Change:

```rust
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeName {
    #[default]
    Auto,
    Dark,
    Light,
    Solarized,
    Catppuccin,
}
```

Add:

```rust
impl ThemeName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Solarized => "solarized",
            Self::Catppuccin => "catppuccin",
        }
    }
}
```

In `format_config_show`, replace debug formatting of the theme with:

```rust
config.theme.as_str()
```

Do not change explicit theme parsing or any non-theme config default.

Because `ThemeName` is shared with `orca-tui`, update the existing exhaustive
matches in `crates/orca-tui/src/theme.rs` in this same task with the temporary
compatibility rule:

```rust
let syntax_theme = match name {
    ThemeName::Auto | ThemeName::Dark => SyntaxTheme::OneHalfDark,
    ThemeName::Light => SyntaxTheme::OneHalfLight,
    ThemeName::Solarized => SyntaxTheme::SolarizedDark,
    ThemeName::Catppuccin => SyntaxTheme::CatppuccinMocha,
};
```

In the palette `match`, change only the existing Dark pattern from
`ThemeName::Dark` to `ThemeName::Auto | ThemeName::Dark`; keep its complete
field initializer and every other branch byte-identical.

for both syntax-theme and palette selection. `Theme::resolve` replaces this
temporary compatibility arm in Task 2. Add:

```rust
#[test]
fn named_auto_uses_the_existing_dark_palette_without_terminal_context() {
    let auto = Theme::named(ThemeName::Auto);
    let dark = Theme::named(ThemeName::Dark);
    assert_eq!(auto.text, dark.text);
    assert_eq!(auto.syntax_theme, dark.syntax_theme);
}
```

- [ ] **Step 5: Run GREEN**

```sh
cargo test -p orca-core theme --lib
cargo test -p orca-core config::file::tests --lib
cargo test -p orca-tui named_auto_uses --lib
cargo check -p orca-core
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 6: Commit**

```sh
git add crates/orca-core/src/config/mod.rs crates/orca-core/src/config/file.rs \
  crates/orca-tui/src/theme.rs
git commit -m "feat(core): add automatic TUI theme selection" \
  -m "Make auto the default while preserving every explicit named theme." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 2: Model Terminal Profiles and Adapt Theme Palettes

**Files:**
- Add: `crates/orca-tui/src/terminal_capabilities.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/theme.rs`
- Modify: `crates/orca-tui/src/transcript_view.rs`

- [ ] **Step 1: Add failing profile resolution tests**

Create the module with tests referring to desired types:

```rust
#[test]
fn auto_uses_detected_background_and_explicit_themes_ignore_it() {
    assert_eq!(
        resolve_base_theme(ThemeName::Auto, TerminalBackground::Light),
        ThemeName::Light
    );
    assert_eq!(
        resolve_base_theme(ThemeName::Auto, TerminalBackground::Dark),
        ThemeName::Dark
    );
    assert_eq!(
        resolve_base_theme(ThemeName::Auto, TerminalBackground::Unknown),
        ThemeName::Dark
    );

    for explicit in [
        ThemeName::Dark,
        ThemeName::Light,
        ThemeName::Solarized,
        ThemeName::Catppuccin,
    ] {
        for background in [
            TerminalBackground::Dark,
            TerminalBackground::Light,
            TerminalBackground::Unknown,
        ] {
            assert_eq!(resolve_base_theme(explicit, background), explicit);
        }
    }
}
```

- [ ] **Step 2: Add failing color-level fact tests**

Define a pure internal fact type:

```rust
#[derive(Clone, Copy, Debug, Default)]
struct ColorSupportFacts {
    has_basic: bool,
    has_256: bool,
    has_16m: bool,
}
```

Test:

```rust
#[test]
fn color_support_facts_map_to_exact_levels() {
    assert_eq!(color_level_from_facts(None), TerminalColorLevel::Monochrome);
    assert_eq!(
        color_level_from_facts(Some(ColorSupportFacts {
            has_basic: true,
            ..Default::default()
        })),
        TerminalColorLevel::Ansi16
    );
    assert_eq!(
        color_level_from_facts(Some(ColorSupportFacts {
            has_basic: true,
            has_256: true,
            ..Default::default()
        })),
        TerminalColorLevel::Ansi256
    );
    assert_eq!(
        color_level_from_facts(Some(ColorSupportFacts {
            has_basic: true,
            has_256: true,
            has_16m: true,
        })),
        TerminalColorLevel::TrueColor
    );
}
```

- [ ] **Step 3: Add failing quantization tests**

Add exact cases:

```rust
#[test]
fn rgb_quantization_uses_stable_xterm_palettes() {
    assert_eq!(
        TerminalColorLevel::Ansi256.adapt_color(Color::Rgb(255, 0, 0)),
        Color::Indexed(196)
    );
    assert_eq!(
        TerminalColorLevel::Ansi256.adapt_color(Color::Rgb(128, 128, 128)),
        Color::Indexed(244)
    );
    assert_eq!(
        TerminalColorLevel::Ansi16.adapt_color(Color::Rgb(255, 0, 0)),
        Color::LightRed
    );
    assert_eq!(
        TerminalColorLevel::Ansi16.adapt_color(Color::Indexed(196)),
        Color::LightRed
    );
}
```

Add:

```rust
#[test]
fn monochrome_style_preserves_modifiers_and_resets_colors() {
    let style = Style::default()
        .fg(Color::Rgb(1, 2, 3))
        .bg(Color::Indexed(42))
        .add_modifier(Modifier::BOLD | Modifier::ITALIC | Modifier::REVERSED);
    let adapted = TerminalColorLevel::Monochrome.adapt_style(style);

    assert_eq!(adapted.fg, Some(Color::Reset));
    assert_eq!(adapted.bg, Some(Color::Reset));
    assert_eq!(adapted.add_modifier, style.add_modifier);
}
```

- [ ] **Step 4: Run RED**

```sh
cargo test -p orca-tui auto_uses_detected_background --lib
cargo test -p orca-tui color_support_facts --lib
cargo test -p orca-tui rgb_quantization --lib
cargo test -p orca-tui monochrome_style --lib
```

Expected: missing module/types/functions.

- [ ] **Step 5: Implement profile and quantization**

Add public(crate) enums/struct from the design. Implement:

```rust
pub(crate) const fn resolve_base_theme(
    requested: ThemeName,
    background: TerminalBackground,
) -> ThemeName
```

with Auto fallback Dark.

Implement xterm index-to-RGB helpers, nearest-index scans with squared `i32`
distance, and:

```rust
impl TerminalColorLevel {
    pub(crate) const fn revision(self) -> u64 {
        match self {
            Self::TrueColor => 0,
            Self::Ansi256 => 0x100,
            Self::Ansi16 => 0x200,
            Self::Monochrome => 0x300,
        }
    }

    pub(crate) fn adapt_color(self, color: Color) -> Color
    pub(crate) fn adapt_style(self, style: Style) -> Style
}
```

Register:

```rust
mod terminal_capabilities;
```

in `lib.rs`.

- [ ] **Step 6: Add failing theme resolution matrix tests**

In `theme.rs`:

```rust
#[test]
fn resolved_themes_choose_base_palette_and_obey_color_level() {
    for (requested, background, expected_text) in [
        (
            ThemeName::Auto,
            TerminalBackground::Light,
            Theme::named(ThemeName::Light).text,
        ),
        (
            ThemeName::Auto,
            TerminalBackground::Dark,
            Theme::named(ThemeName::Dark).text,
        ),
        (
            ThemeName::Solarized,
            TerminalBackground::Light,
            Theme::named(ThemeName::Solarized).text,
        ),
    ] {
        let profile = TerminalProfile {
            background,
            color_level: TerminalColorLevel::TrueColor,
        };
        assert_eq!(Theme::resolve(requested, profile).text, expected_text);
    }

    for level in [
        TerminalColorLevel::Ansi256,
        TerminalColorLevel::Ansi16,
        TerminalColorLevel::Monochrome,
    ] {
        for name in [
            ThemeName::Dark,
            ThemeName::Light,
            ThemeName::Solarized,
            ThemeName::Catppuccin,
        ] {
            assert_theme_colors_fit_level(Theme::resolve(
                name,
                TerminalProfile {
                    background: TerminalBackground::Unknown,
                    color_level: level,
                },
            ));
        }
    }
}
```

Create a test helper that inspects every public theme color.

- [ ] **Step 7: Run theme test RED**

```sh
cargo test -p orca-tui resolved_themes_choose --lib
```

Expected: `Theme::resolve` and color-level fields missing.

- [ ] **Step 8: Implement resolved Theme**

Refactor the existing constructor:

```rust
fn base(name: ThemeName) -> Self
```

where Auto delegates to Dark and all current RGB values remain byte-identical.

Keep:

```rust
pub fn named(name: ThemeName) -> Self {
    Self::resolve(
        name,
        TerminalProfile {
            background: TerminalBackground::Unknown,
            color_level: TerminalColorLevel::TrueColor,
        },
    )
}
```

Add:

```rust
pub(crate) fn resolve(name: ThemeName, profile: TerminalProfile) -> Self
```

Choose the base theme, adapt every color field, set `color_level`, and compute:

```rust
syntax_theme_revision =
    syntax_theme.revision() + profile.color_level.revision();
```

Add:

```rust
pub(crate) fn selection_style(self) -> Style
```

per design.

- [ ] **Step 9: Extend transcript cache identity**

Add `color_level: TerminalColorLevel` to `ThemeIdentity` and map it from Theme.
Do not alter cache algorithms.

Add a focused test that two otherwise colliding adapted themes with different
levels rebuild once and then remain stable.

- [ ] **Step 10: Run Task 2 GREEN**

```sh
cargo test -p orca-tui terminal_capabilities --lib
cargo test -p orca-tui theme::tests --lib
cargo test -p orca-tui transcript_view::tests --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 11: Commit**

```sh
git add crates/orca-tui/src/terminal_capabilities.rs \
  crates/orca-tui/src/lib.rs \
  crates/orca-tui/src/theme.rs \
  crates/orca-tui/src/transcript_view.rs
git commit -m "feat(tui): resolve terminal-safe theme palettes" \
  -m "Model independent background and color-depth capabilities and quantize every named palette once." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 3: Adapt Syntax and Diff Highlighting

**Files:**
- Modify: `crates/orca-tui/src/syntax_highlight.rs`
- Modify: `crates/orca-tui/src/diff_highlight.rs`
- Modify: `crates/orca-tui/src/edit_highlight_worker.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/ui.rs`

- [ ] **Step 1: Add failing fenced-code color-level tests**

In `syntax_highlight.rs`:

```rust
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
        assert!(lines.iter().flatten().all(|span| color_fits(level, span.style.fg)));
    }
}
```

The test helper accepts `Rgb` only for TrueColor, `Indexed`/named for ANSI-256,
named only for ANSI-16, and Reset/None for Monochrome.

- [ ] **Step 2: Add failing parsed-diff and worker identity tests**

In `diff_highlight.rs`, render a Rust diff using a theme resolved at each level
and assert every syntax span fits.

In `edit_highlight_worker.rs`, extend the job fixture test expectation:

```rust
assert_eq!(actual.syntax_color_level, expected.syntax_color_level);
```

Add a coalescing/pending identity case where jobs differ only by level and must
not match.

- [ ] **Step 3: Run RED**

```sh
cargo test -p orca-tui highlighted_styles_obey --lib
cargo test -p orca-tui parsed_diff_styles_obey --lib
cargo test -p orca-tui syntax_color_level --lib
```

Expected: signature and field compilation failures.

- [ ] **Step 4: Thread level through syntax conversion**

Change signatures:

```rust
pub(crate) fn highlight_code(
    code: &str,
    language: &str,
    theme: SyntaxTheme,
    color_level: TerminalColorLevel,
) -> Option<Vec<StyledSourceLine>>

pub(crate) fn highlighter_for_path(
    path: &Path,
    theme: SyntaxTheme,
    color_level: TerminalColorLevel,
) -> Option<LineHighlighter>
```

Store the level in `LineHighlighter`. Convert syntect styles with:

```rust
color_level.adapt_style(output)
```

Update every test call explicitly with `TrueColor` unless it tests degradation.

- [ ] **Step 5: Thread level through foreground rendering**

Update:

- Markdown `append_code_block`;
- parsed diff `highlighter_for_path` calls;
- diff test helpers;
- `compute_parsed_diff_file_scoped_styles` and its internal helper.

Use `theme.color_level` at UI boundaries.

- [ ] **Step 6: Thread level through AppState and background jobs**

Add to `AppState`:

```rust
pub(crate) syntax_color_level: TerminalColorLevel,
```

Default to TrueColor.

Add:

```rust
fn syntax_style_revision(
    syntax_theme: SyntaxTheme,
    color_level: TerminalColorLevel,
) -> u64 {
    syntax_theme.revision() + color_level.revision()
}
```

Use this single helper for:

- `Theme::syntax_theme_revision`;
- `EditHighlightJob::syntax_theme_revision` submission;
- stale-result validation;
- tests that mutate theme/level.

Change:

```rust
configure_syntax_highlighting(
    workspace_root,
    syntax_theme,
    syntax_color_level,
)
```

Add to `EditHighlightJob`:

```rust
pub(crate) syntax_color_level: TerminalColorLevel,
```

Include it in `same_job_identity`, fixtures, job submission, worker
`compute_parsed_diff_file_scoped_styles`, and stale-result rejection.

Add a test that changes only `syntax_color_level`, keeps `syntax_theme`
unchanged, and proves the prior result is rejected by both level and revision.

Use the resolved theme values in app configuration helpers.

- [ ] **Step 7: Run Task 3 GREEN**

```sh
cargo test -p orca-tui syntax_highlight --lib
cargo test -p orca-tui diff_highlight --lib
cargo test -p orca-tui edit_highlight --lib
cargo test -p orca-tui types::tests --lib
cargo test -p orca-tui app::tests --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 8: Commit**

```sh
git add crates/orca-tui/src/syntax_highlight.rs \
  crates/orca-tui/src/diff_highlight.rs \
  crates/orca-tui/src/edit_highlight_worker.rs \
  crates/orca-tui/src/types.rs \
  crates/orca-tui/src/app.rs \
  crates/orca-tui/src/ui.rs
git commit -m "feat(tui): downgrade syntax colors by capability" \
  -m "Generate fenced-code and diff refinement styles in the terminal's supported color space." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 4: Make Selection Visible Without Color

**Files:**
- Modify: `crates/orca-tui/src/selection.rs`
- Modify: `crates/orca-tui/src/ui.rs`

- [ ] **Step 1: Add failing selection-style tests**

Change selection tests to pass a `Style` rather than a color. Add:

```rust
#[test]
fn monochrome_selection_reverses_while_preserving_source_modifiers() {
    let source = Style::default().add_modifier(Modifier::BOLD);
    let selected = Style::default().add_modifier(Modifier::REVERSED);
    let line = Line::from(Span::styled("abc", source));

    let rendered = apply_selection_to_line(line, 0, None, selected);
    assert!(rendered.spans[0].style.add_modifier.contains(Modifier::BOLD));
    assert!(
        rendered.spans[0]
            .style
            .add_modifier
            .contains(Modifier::REVERSED)
    );
}
```

Add completed buffer tests for transcript selection, composer selection, and
jump pill using a monochrome resolved theme.

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui monochrome_selection --lib
cargo test -p orca-tui monochrome_composer_selection --lib
cargo test -p orca-tui monochrome_jump_pill --lib
```

Expected: signature mismatch and fixed `LightBlue` behavior.

- [ ] **Step 3: Apply selection Style**

Change:

```rust
pub fn apply_selection_to_line(
    line: Line<'static>,
    col_start: usize,
    col_end: Option<usize>,
    selection_style: Style,
) -> Line<'static>
```

In `flush_run`, combine styles with:

```rust
style.patch(selection_style)
```

Update callers/tests.

- [ ] **Step 4: Route UI selection through Theme**

Use `theme.selection_style()` for:

- transcript selection overlay;
- jump-to-bottom pill.

For the pill:

```rust
theme
    .selection_style()
    .fg(theme.text)
```

For the custom composer renderer, preserve the existing pure helper used by
layout tests and add a styled entry point:

```rust
fn textarea_visual_layout(textarea: &TextArea, width: usize) -> TextareaVisualLayout {
    textarea_visual_layout_with_selection(
        textarea,
        width,
        Style::default().bg(Color::LightBlue),
    )
}

fn textarea_visual_layout_with_selection(
    textarea: &TextArea,
    width: usize,
    selection_style: Style,
) -> TextareaVisualLayout
```

Pass `selection_style` into `render_textarea_visual_line` instead of creating
the fixed LightBlue style there. Production composer/setup layout creation
uses `theme.selection_style()`. Existing pure layout tests may continue using
`textarea_visual_layout` and preserve their old default unless they explicitly
exercise monochrome.

Change the production composer helper:

```rust
fn composer_visual_layout(
    area_width: u16,
    textarea: &TextArea,
    theme: &Theme,
) -> TextareaVisualLayout
```

and pass `theme` from top-level `render`. In `render_textarea_surface`, the
non-precomputed setup path calls `textarea_visual_layout_with_selection`
directly with `theme.selection_style()`. Click mapping may keep the default
layout helper because it consumes row/range metadata, not rendered styles.

Do not change geometry or selection indexing.

- [ ] **Step 5: Run Task 4 GREEN**

```sh
cargo test -p orca-tui selection --lib
cargo test -p orca-tui composer --lib
cargo test -p orca-tui jump_pill --lib
cargo test -p orca-tui hardware_cursor --lib
cargo test -p orca-tui vim_modes --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 6: Commit**

```sh
git add crates/orca-tui/src/selection.rs crates/orca-tui/src/ui.rs
git commit -m "fix(tui): preserve selection in monochrome terminals" \
  -m "Use reversible style overlays when background colors are unavailable." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 5: Add the Capability-Safe Backend

**Files:**
- Add: `crates/orca-tui/src/capability_backend.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [ ] **Step 1: Add a recording backend test double**

Inside the new module's test block, add a `RecordingBackend` that implements
every ratatui 0.29 `Backend` method, records drawn `(x, y, Cell)` values and
delegation calls, and provides deterministic size/window/cursor responses.

Its `draw` must own cloned cells so assertions can inspect them after the
borrowed iterator is consumed.

- [ ] **Step 2: Add failing draw adaptation tests**

Add:

```rust
#[test]
fn capability_backend_adapts_changed_cells_and_preserves_metadata() {
    let source = Cell::default()
        .set_symbol("界")
        .set_fg(Color::Rgb(255, 0, 0))
        .set_bg(Color::Indexed(42))
        .add_modifier(Modifier::BOLD);
    source.set_skip(true);

    for level in [
        TerminalColorLevel::Ansi256,
        TerminalColorLevel::Ansi16,
        TerminalColorLevel::Monochrome,
    ] {
        let recorder = RecordingBackend::default();
        let mut backend = CapabilityBackend::new(recorder, level);
        backend.draw(std::iter::once((3, 4, &source))).unwrap();

        let drawn = &backend.inner().drawn[0];
        assert_eq!((drawn.0, drawn.1), (3, 4));
        assert_eq!(drawn.2.symbol(), "界");
        assert_eq!(drawn.2.modifier, Modifier::BOLD);
        assert!(drawn.2.skip);
        assert!(cell_colors_fit(level, &drawn.2));
    }
}
```

Build the source cell with separate mutable calls if the fluent API returns a
mutable reference.

Add a TrueColor test asserting the exact original cell is forwarded.

- [ ] **Step 3: Add failing delegation tests**

Call and assert delegation for:

- `append_lines`;
- `hide_cursor` / `show_cursor`;
- `get_cursor_position` / `set_cursor_position`;
- `clear` / `clear_region`;
- `size` / `window_size`;
- `flush`;
- `scroll_region_up` / `scroll_region_down`.

- [ ] **Step 4: Run RED**

```sh
cargo test -p orca-tui capability_backend_adapts --lib
cargo test -p orca-tui capability_backend_delegates --lib
```

Expected: module/type missing.

- [ ] **Step 5: Implement CapabilityBackend**

Add:

```rust
pub(crate) struct CapabilityBackend<B> {
    inner: B,
    color_level: TerminalColorLevel,
}
```

Provide:

```rust
pub(crate) const fn new(inner: B, color_level: TerminalColorLevel) -> Self
pub(crate) const fn inner(&self) -> &B
pub(crate) fn inner_mut(&mut self) -> &mut B
```

Implement the complete ratatui `Backend` contract. For degraded draw:

```rust
let adapted = content
    .map(|(x, y, cell)| {
        let mut cell = cell.clone();
        cell.fg = color_level.adapt_color(cell.fg);
        cell.bg = color_level.adapt_color(cell.bg);
        (x, y, cell)
    })
    .collect::<Vec<_>>();
self.inner
    .draw(adapted.iter().map(|(x, y, cell)| (*x, *y, cell)))
```

For TrueColor, delegate `content` directly.

Register the module in `lib.rs`.

- [ ] **Step 6: Run Task 5 GREEN**

```sh
cargo test -p orca-tui capability_backend --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 7: Commit**

```sh
git add crates/orca-tui/src/capability_backend.rs \
  crates/orca-tui/src/lib.rs
git commit -m "feat(tui): enforce terminal-safe output colors" \
  -m "Adapt changed cells at the backend boundary while preserving terminal operations and frame semantics." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 6: Give Qwertty Permanent Terminal Input Ownership

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/orca-tui/Cargo.toml`
- Create: `crates/orca-tui/src/input_adapter.rs`
- Create: `crates/orca-tui/src/input_runtime.rs`
- Modify: `crates/orca-tui/src/terminal_capabilities.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `docs/superpowers/specs/2026-07-28-tui-terminal-capabilities-design.md`
- Modify: `docs/superpowers/plans/2026-07-28-tui-terminal-capabilities.md`

- [ ] **Step 1: Remove the rejected one-shot detector**

Delete the `terminal-colorsaurus` detector, timeout/quarantine logic, SSH
special cases, and the `detect_profile_then_enable_raw` startup helper before
writing new tests. Restore `app.rs` to the Task 5 input/lifecycle behavior while
retaining the approved `CapabilityBackend`, resolved theme, syntax color-level,
and clear-scrollback changes.

This deletion is required because the replacement must be developed test-first,
not adapted from production code that already implements a different behavior.

- [ ] **Step 2: Add the exact dependencies**

Replace `terminal-colorsaurus` with:

```toml
qwertty = { version = "=0.1.6", features = ["tokio"] }
supports-color = "3.0.2"
```

to workspace dependencies and reference them with `workspace = true` from
`orca-tui`. Also add workspace `tokio` to `orca-tui`.

Run `cargo update -p qwertty --precise 0.1.6` after editing manifests.

- [ ] **Step 3: Write failing qwertty-to-crossterm adapter tests**

Create `input_adapter.rs` with tests driving real qwertty values produced by
`SemanticDecoder` or public constructors. The intended API is:

```rust
#[derive(Default)]
pub(crate) struct InputAdapter {
    paste: Option<Vec<u8>>,
}

impl InputAdapter {
    pub(crate) fn adapt(
        &mut self,
        event: qwertty::Event,
    ) -> Option<crossterm::event::Event>;
}
```

Cover these exact mappings:

```text
Key::Char(c)                  -> KeyCode::Char(c)
Up/Down/Left/Right            -> matching KeyCode
Enter/Tab/Backspace/Escape    -> matching KeyCode
Home/End/PageUp/PageDown      -> matching KeyCode
Insert/Delete                 -> matching KeyCode
Function(1..=35)              -> KeyCode::F(n)
Shift+Tab                     -> KeyCode::BackTab
Control(0)                    -> Ctrl+Char(' ')
Control(1..=26)               -> Ctrl+Char('a'..='z')
Control(28..=31)              -> Ctrl+Char('4'..='7')
```

Map `SHIFT`, `CTRL`, `ALT`, `SUPER`, `HYPER`, and `META`; map Caps/Num to
`KeyEventState` without inventing keyboard modifiers. Map qwertty Press,
Repeat, and Release to crossterm's matching `KeyEventKind`.

Decode mouse fixtures through `SemanticDecoder` and assert:

- `(1, 1)` becomes `(0, 0)`;
- press/release use the matching standard button;
- `Moved + standard button` becomes `Drag(button)`;
- `Moved + None` becomes `Moved`;
- four scroll directions map directly;
- zero coordinates and `Other` buttons return `None`.

Decode focus, resize, segmented bracketed paste, malformed UTF-8 paste, and a
complete unknown OSC token. Assert one final UTF-8 `Event::Paste`, invalid paste
resets adapter state without emission, and `Event::Syntax` returns `None`.

- [ ] **Step 4: Run adapter RED**

```sh
cargo test -p orca-tui input_adapter --lib
```

Expected: `input_adapter` module/API is missing.

- [ ] **Step 5: Implement the minimal input adapter**

Implement exhaustive `#[non_exhaustive]` matches with a final `_ => None`.
Use `checked_sub(1)` for both mouse coordinates. Do not use qwertty associated
text as a second business event; preserve Orca's existing one-key-event
semantics.

Paste handling rules:

```rust
if segment.is_first() {
    self.paste = Some(Vec::new());
}
let bytes = self.paste.as_mut()?;
bytes.extend_from_slice(segment.data());
if segment.is_final() {
    let bytes = self.paste.take()?;
    return String::from_utf8(bytes).ok().map(Event::Paste);
}
None
```

- [ ] **Step 6: Run adapter GREEN and commit**

```sh
cargo test -p orca-tui input_adapter --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add Cargo.toml Cargo.lock crates/orca-tui/Cargo.toml \
  crates/orca-tui/src/input_adapter.rs crates/orca-tui/src/lib.rs
git commit -m "feat(tui): adapt qwertty terminal input" \
  -m "Preserve existing crossterm event semantics while safely dropping unsupported terminal syntax." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

- [ ] **Step 7: Write failing profile/startup-order tests**

In `terminal_capabilities.rs`, add pure helpers:

```rust
pub(crate) fn terminal_background_from_rgb(
    requested: ThemeName,
    background: Option<qwertty::Rgb>,
) -> TerminalBackground;

pub(crate) fn system_color_level() -> TerminalColorLevel;
```

The real driver extracts `Capabilities::background_color.value_copied()` and
passes the resulting `Option<qwertty::Rgb>` into the pure helper.
Assert explicit themes ignore background; Auto maps lightness `> 0.5` to Light,
the boundary and darker values to Dark, and no result to Unknown. Keep existing
supports-color fact tests.

In `input_runtime.rs`, introduce a private async `TerminalDriver` trait
implemented by a fake. Test this ordered contract:

```text
open
probe (Auto only)
enter alternate screen
enable ButtonEvent mouse
enable bracketed paste
push the three existing kitty flags
ready(profile)
read until stop
leave
```

Explicit themes skip `probe` but still perform every mode transition.

- [ ] **Step 8: Run runtime RED**

```sh
cargo test -p orca-tui input_runtime --lib
cargo test -p orca-tui terminal_capabilities --lib
```

Expected: runtime and qwertty profile APIs are missing.

- [ ] **Step 9: Implement the supervised input runtime**

Create:

```rust
pub(crate) struct InputRuntime {
    profile: TerminalProfile,
    events: crossbeam_channel::Receiver<crossterm::event::Event>,
    stop_tx: Option<tokio::sync::watch::Sender<bool>>,
    join: Option<std::thread::JoinHandle<io::Result<()>>>,
}
```

`InputRuntime::start(ThemeName)` creates:

- a bounded event channel;
- a bounded one-shot startup channel;
- a Tokio watch shutdown channel;
- an `orca-tui-input` OS thread;
- a current-thread Tokio runtime on that thread;
- one persistent `TokioTerminalSession::open()`.

Inside the async owner:

```rust
let capabilities = if requested == ThemeName::Auto {
    Some(session.probe_capabilities(Duration::from_millis(250)).await?)
} else {
    None
};
session.enter_alternate_screen().await?;
session.enable_mouse(MouseMode::ButtonEvent).await?;
session.enable_bracketed_paste().await?;
session.push_kitty_keyboard(
    KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KittyKeyboardFlags::REPORT_EVENT_TYPES)
        .union(KittyKeyboardFlags::REPORT_ALTERNATE_KEYS),
).await?;
```

Send Ready only after all mode operations succeed. Install a cloned
`RestoreHandle` into a process-wide panic-hook registry before Ready. Install
the hook once with `OnceLock`; it restores the currently registered handle and
then calls the hook that was active at installation time. Normal shutdown
clears only its own registered handle. Serialize TUI ownership so concurrent
runs cannot overwrite the active restore slot, and ensure repeated sequential
runs do not accumulate hooks.

The read loop selects between shutdown and `next_event()`. On Unix it also
selects `ResizeStream::next_resize()` and converts the returned `TerminalSize`
to `Event::Resize`; on Windows resize remains in `next_event()`. Do not enable
in-band resize. On both platforms, also select qwertty `SignalStream`:
`Suspend`/`Continue` call `suspend()`/`resume(false)`, while
`Terminate`/`Interrupt` leave through the normal cleanup path. Unknown future
signals are ignored.

Route suspend through a bounded main-thread control channel. The frame loop
acknowledges before qwertty writes suspend cleanup and waits for a Resumed
control before drawing again. On Resumed, clear ratatui's retained terminal
buffer before marking the frame dirty, so the alternate-screen clear is fully
repainted. Make control sends and acknowledgement waits
shutdown-aware so a full or abandoned mailbox cannot block `leave()`.

Forward adapted events with a shutdown-aware bounded send. Never block forever
on a full mailbox: select/retry against shutdown. On every exit path consume
the session with `leave().await`; preserve the first operational error while
still attempting leave.

`finish()` and `Drop` share one idempotent stop/join implementation. Tests use a
fake driver and tiny mailbox to prove blocked `next_event`, full mailbox,
startup failure, normal stop, and Drop all reach leave and join.

Represent global terminal ownership with a once-released RAII lease rather than
an unconditional atomic clear. Scope the panic restore registry to the TUI
owner thread and qwertty input thread so caught worker panics cannot dismantle
the active terminal.

- [ ] **Step 10: Run runtime GREEN and commit**

```sh
cargo test -p orca-tui input_runtime --lib
cargo test -p orca-tui terminal_capabilities --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/input_runtime.rs \
  crates/orca-tui/src/terminal_capabilities.rs crates/orca-tui/src/lib.rs
git commit -m "feat(tui): own terminal input with qwertty" \
  -m "Keep probing, decoding, input modes, and restoration under one persistent terminal session." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

- [ ] **Step 11: Write failing application integration tests**

Extract a pure batch receiver:

```rust
fn receive_input_batch(
    receiver: &crossbeam_channel::Receiver<Event>,
    timeout: Duration,
    limit: usize,
) -> Result<Vec<Event>, crossbeam_channel::RecvTimeoutError>;
```

Test that it waits for the first event, drains only immediately available
events, caps at 64, and reports timeout/disconnect without polling crossterm.

Add source/ownership tests or injected lifecycle tests proving:

- `run_tui_inner` starts `InputRuntime` before constructing `Terminal`;
- app production code contains no `crossterm::event::poll/read/EventStream`;
- app production code contains no crossterm raw/alternate/mouse/paste/kitty
  mode setup;
- the ratatui terminal drops before `InputRuntime::finish`;
- non-TUI modes do not construct the runtime.

- [ ] **Step 12: Run integration RED**

```sh
cargo test -p orca-tui receive_input_batch --lib
cargo test -p orca-tui terminal_input_ownership --lib
```

Expected: helper and qwertty ownership integration are missing.

- [ ] **Step 13: Integrate qwertty into `app.rs`**

At startup:

```rust
let pending_input_runtime = InputRuntime::start(config.theme)?;
let theme = Theme::resolve(config.theme, pending_input_runtime.profile());
let input_rx = pending_input_runtime.events().clone();
```

Then construct:

```rust
CapabilityBackend::new(CrosstermBackend::new(io::stdout()), theme.color_level)
```

Remove `TerminalCleanup`, `terminal::enable_raw_mode`, alternate-screen entry,
mouse capture, bracketed-paste setup, kitty push, and crossterm `poll/read`.
Replace the poll block with `receive_input_batch(&input_rx,
scheduler.poll_timeout(...), 64)`, treating timeout as an empty batch and
disconnect as an `UnexpectedEof` runtime error.

On exit:

```rust
drop(terminal);
terminal_input.finish()?;
```

Move `pending_input_runtime` into a `terminal_input` binding declared after the
agent runtime, so Rust's reverse declaration-order drop guarantees every early
return drops ratatui, restores/joins qwertty, and only then joins the agent
runtime. Only after explicit finish shut down mention search and supervised
agent runtimes. Keep
`clear_terminal_scrollback` routed through `CapabilityBackend::inner_mut()`;
qwertty performs no steady-state writes, so this output-only operation remains
safe.

- [ ] **Step 14: Run Task 6 GREEN**

```sh
cargo test -p orca-tui input_adapter --lib
cargo test -p orca-tui input_runtime --lib
cargo test -p orca-tui terminal_capabilities --lib
cargo test -p orca-tui capability_backend --lib
cargo test -p orca-tui app::tests --lib
cargo test -p orca-tui clear_terminal --lib
cargo test -p orca-tui setup --lib
cargo test -p orca-core theme --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 15: Commit Task 6 application integration and docs**

```sh
git add Cargo.toml Cargo.lock crates/orca-tui/Cargo.toml \
  crates/orca-tui/src/input_adapter.rs \
  crates/orca-tui/src/input_runtime.rs \
  crates/orca-tui/src/terminal_capabilities.rs \
  crates/orca-tui/src/app.rs \
  crates/orca-tui/src/lib.rs \
  docs/superpowers/specs/2026-07-28-tui-terminal-capabilities-design.md \
  docs/superpowers/plans/2026-07-28-tui-terminal-capabilities.md
git commit -m "feat(tui): integrate terminal capability runtime" \
  -m "Route application input through the persistent qwertty owner and restore it before supervised runtime shutdown." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 7: Final Verification and Delivery

**Files:**
- Verify all files listed above.

- [ ] **Step 1: Run focused capability verification**

```sh
cargo test -p orca-core theme --lib
cargo test -p orca-tui terminal_capabilities --lib
cargo test -p orca-tui theme::tests --lib
cargo test -p orca-tui syntax_highlight --lib
cargo test -p orca-tui diff_highlight --lib
cargo test -p orca-tui selection --lib
cargo test -p orca-tui edit_highlight --lib
cargo test -p orca-tui hardware_cursor --lib
```

- [ ] **Step 2: Run package and workspace gates**

```sh
cargo test -p orca-tui -- --test-threads=1
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
cargo test --workspace --all-targets -- --test-threads=1
```

- [ ] **Step 3: Perform prompt-to-artifact audit**

| Requirement | Direct evidence |
|---|---|
| COLORTERM/TTY color-depth detection | supports-color mapping and injected detector tests |
| OSC 11 background query | qwertty probe profile mapping and startup-order tests |
| Auto chooses light/dark | pure resolution matrix |
| Explicit themes are preserved | detector call-count and resolution tests |
| Truecolor remains RGB | identity tests |
| ANSI-256 is safe | all-theme field audit plus syntax/diff tests |
| ANSI-16 is safe | all-theme field audit plus syntax/diff tests |
| Monochrome preserves semantics | selection/cursor/diff/heading tests |
| Capabilities degrade independently | profile matrix |
| No per-frame detection | startup source inspection |
| No input race | one-session probe/typeahead/Syntax-drop/runtime ownership tests |
| Fenced and parsed diff styles degrade | completed style tests |
| Background refinement cannot go stale | job identity/stale-result tests |
| Cache identity includes color level | cache rebuild test |
| Existing explicit configs remain valid | core serde/file tests |
| Detection ownership is safe | persistent session, bounded shutdown, leave/join, and panic restore tests |

Treat any missing evidence as incomplete.

- [ ] **Step 4: Review commits and scope**

```sh
git status --short
git log --format='%h %s%n%(trailers:key=Co-authored-by,valueonly)' 97dce189..HEAD
git diff --check 97dce189..HEAD
git diff --stat 97dce189..HEAD
```

Expected: clean worktree, one trailer per commit, and no P0 #5 notification or
title changes.

- [ ] **Step 5: Request final holistic review**

Review the full range against:

```text
docs/superpowers/specs/2026-07-28-tui-terminal-capabilities-design.md
```

- [ ] **Step 6: Push and verify**

```sh
git push origin feature/tui-syntax-highlighting
local_sha=$(git rev-parse HEAD)
remote_sha=$(git ls-remote --heads origin feature/tui-syntax-highlighting | awk '{print $1}')
test "$local_sha" = "$remote_sha"
```

Keep the branch for P0 #5.
