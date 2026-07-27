# TUI Terminal Capabilities and Theme Fallback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Detect terminal background and color depth once before TUI startup, select an automatic light/dark theme, and emit only colors supported by the terminal.

**Architecture:** Core config adds `ThemeName::Auto`. A focused `terminal_capabilities.rs` module models background and color depth, performs deterministic color quantization, and resolves a final immutable `Theme`. Syntax and diff styles receive the same color-level identity at generation time; startup detection is injected and runs before Orca enables raw mode.

**Tech Stack:** Rust 2024, ratatui 0.29, crossterm 0.28, terminal-colorsaurus 1.0.3, supports-color 3.0.2, syntect/two-face.

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
  - Provide an injected startup detector boundary.
- Add `crates/orca-tui/src/capability_backend.rs`
  - Adapt changed ratatui cells as a final output safety boundary.
  - Delegate the complete ratatui Backend API.
- Modify `crates/orca-tui/src/lib.rs`
  - Register the module.
- Modify `Cargo.toml`, `crates/orca-tui/Cargo.toml`, `Cargo.lock`
  - Add terminal-colorsaurus and supports-color.
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
  - Detect profile before raw mode.
  - Resolve theme and configure syntax state with color level.
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

### Task 6: Detect Capabilities Before TUI Raw Mode

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/orca-tui/Cargo.toml`
- Modify: `crates/orca-tui/src/terminal_capabilities.rs`
- Modify: `crates/orca-tui/src/capability_backend.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [ ] **Step 1: Add dependencies**

Add:

```toml
terminal-colorsaurus = "1.0.3"
supports-color = "3.0.2"
```

to workspace dependencies and reference them with `workspace = true` from
`orca-tui`.

- [ ] **Step 2: Add failing detector orchestration tests**

Define an injectable pure boundary:

```rust
trait TerminalDetector {
    fn background(&mut self) -> TerminalBackground;
    fn color_level(&mut self) -> TerminalColorLevel;
}

fn detect_terminal_profile_with(
    requested: ThemeName,
    detector: &mut impl TerminalDetector,
) -> TerminalProfile
```

Test:

```rust
#[test]
fn explicit_themes_skip_background_but_still_detect_color_depth() {
    let mut detector = RecordingDetector::new(
        TerminalBackground::Light,
        TerminalColorLevel::Ansi256,
    );
    let profile = detect_terminal_profile_with(ThemeName::Solarized, &mut detector);

    assert_eq!(profile.background, TerminalBackground::Unknown);
    assert_eq!(profile.color_level, TerminalColorLevel::Ansi256);
    assert_eq!(detector.background_calls, 0);
    assert_eq!(detector.color_calls, 1);
}

#[test]
fn auto_detects_background_and_color_depth_once() {
    let mut detector = RecordingDetector::new(
        TerminalBackground::Light,
        TerminalColorLevel::TrueColor,
    );
    let profile = detect_terminal_profile_with(ThemeName::Auto, &mut detector);

    assert_eq!(profile.background, TerminalBackground::Light);
    assert_eq!(detector.background_calls, 1);
    assert_eq!(detector.color_calls, 1);
}
```

Add a startup-order helper test:

```rust
fn detect_profile_then_enable_raw(
    requested: ThemeName,
    detector: &mut impl TerminalDetector,
    enable_raw_mode: impl FnOnce() -> io::Result<()>,
) -> io::Result<TerminalProfile>
```

Record calls and assert `background`, `color`, then `raw`.

- [ ] **Step 3: Run RED**

```sh
cargo test -p orca-tui explicit_themes_skip_background --lib
cargo test -p orca-tui auto_detects_background --lib
cargo test -p orca-tui terminal_detection_precedes_raw_mode --lib
```

Expected: detector API missing.

- [ ] **Step 4: Implement production detector**

Add:

```rust
struct SystemTerminalDetector;
```

Background:

```rust
terminal_colorsaurus::background_color(QueryOptions {
    timeout: Duration::from_millis(250),
})
.map(|color| {
    if color.perceived_lightness() <= 0.5 {
        TerminalBackground::Dark
    } else {
        TerminalBackground::Light
    }
})
.unwrap_or(TerminalBackground::Unknown)
```

Color level:

```rust
supports_color::on(supports_color::Stream::Stdout)
```

map through the pure facts function. Add no output/logging.

- [ ] **Step 5: Integrate before raw mode**

At the first line of `run_tui_inner`:

```rust
let profile = detect_profile_then_enable_raw(
    config.theme,
    &mut SystemTerminalDetector,
    terminal::enable_raw_mode,
)?;
let mut pending_terminal_cleanup = TerminalCleanup::raw_mode_enabled();
let theme = Theme::resolve(config.theme, profile);
```

The helper computes the profile first, then invokes the fallible raw-mode
closure, then returns the profile. Raw-mode errors propagate. Construct cleanup
immediately after the helper returns. Remove the later
`Theme::named(config.theme)`.

After terminal setup creates stdout, wrap the production backend:

```rust
let backend = CapabilityBackend::new(
    CrosstermBackend::new(stdout),
    theme.color_level,
);
```

Change:

```rust
type InlineTerminal =
    Terminal<CapabilityBackend<CrosstermBackend<std::io::Stdout>>>;
```

In `clear_terminal_scrollback`, reach crossterm through:

```rust
let stdout = terminal.backend_mut().inner_mut();
```

Do not change cleanup, alternate-screen, cursor, or clear semantics.

Ensure:

- no OSC query in non-TUI CLI paths;
- setup textarea and main textarea use the resolved theme;
- syntax state receives theme syntax theme and color level;
- the detector is dropped before event-loop reads.

- [ ] **Step 6: Test background failure mapping**

The injected detector returns Unknown for timeout/unsupported/error test cases.
Assert Auto resolves to Dark and explicit Light remains Light.

Do not call the real library in tests.

- [ ] **Step 7: Run Task 6 GREEN**

```sh
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

- [ ] **Step 8: Commit**

```sh
git add Cargo.toml Cargo.lock crates/orca-tui/Cargo.toml \
  crates/orca-tui/src/terminal_capabilities.rs \
  crates/orca-tui/src/capability_backend.rs \
  crates/orca-tui/src/app.rs \
  crates/orca-tui/src/lib.rs
git commit -m "feat(tui): detect terminal capabilities at startup" \
  -m "Resolve automatic background and color depth before Orca owns raw-mode input." \
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
| OSC 11 background query | production SystemTerminalDetector source inspection |
| Auto chooses light/dark | pure resolution matrix |
| Explicit themes are preserved | detector call-count and resolution tests |
| Truecolor remains RGB | identity tests |
| ANSI-256 is safe | all-theme field audit plus syntax/diff tests |
| ANSI-16 is safe | all-theme field audit plus syntax/diff tests |
| Monochrome preserves semantics | selection/cursor/diff/heading tests |
| Capabilities degrade independently | profile matrix |
| No per-frame detection | startup source inspection |
| No input race | detection-before-raw-mode test/source inspection |
| Fenced and parsed diff styles degrade | completed style tests |
| Background refinement cannot go stale | job identity/stale-result tests |
| Cache identity includes color level | cache rebuild test |
| Existing explicit configs remain valid | core serde/file tests |
| Failure does not block startup | injected Unknown/error tests |

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
