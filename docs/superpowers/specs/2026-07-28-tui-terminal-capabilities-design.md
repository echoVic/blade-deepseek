# TUI Terminal Capabilities and Theme Fallback Design

## Objective

Detect the terminal's background and color depth once at TUI startup, select an
appropriate light or dark theme when the user chooses `auto`, and ensure Orca
never emits colors beyond the terminal's declared capability.

The four existing palettes use `Color::Rgb` throughout. Crossterm does not
downgrade unsupported truecolor sequences, so an RGB palette can render as
wrong colors or control-sequence artifacts on ANSI-256 and ANSI-16 terminals.
Orca also defaults unconditionally to the Dark theme and never asks the
terminal whether its background is light.

## Scope

This sub-project includes:

- A new `auto` theme selection and making it the configuration default.
- OSC 11-based background detection for `auto`.
- Environment and TTY-based color-depth detection.
- Independent background and color-depth capability results.
- Truecolor, ANSI-256, ANSI-16, and monochrome output profiles.
- Palette adaptation for every `Theme` color.
- Syntax-highlight adaptation for fenced code, parsed diffs, and background
  full-file diff refinement.
- A backend safety adapter for fixed colors emitted outside the theme system or
  by third-party widgets.
- Monochrome-safe selection and jump-to-bottom styling.
- Cache and syntax-style identity updates for color-depth changes.
- Failure-safe startup behavior and deterministic pure tests.

It does not include:

- Runtime re-probing after startup.
- User-visible diagnostics or `/doctor`; that belongs to the later diagnostics
  sub-project.
- OSC 9 notifications, focus tracking, or terminal titles.
- Diff backgrounds, gutters, hunk markers, or word-level diff rendering.
- Terminal font, Unicode glyph, hyperlink, image, or synchronized-output
  capability detection.
- Theme selection in onboarding.
- Changes to the semantic RGB values designed in previous sub-projects.

## Chosen Architecture

Resolve one immutable `TerminalProfile` before Orca enters its own raw-mode
event loop:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TerminalProfile {
    pub(crate) background: TerminalBackground,
    pub(crate) color_level: TerminalColorLevel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalBackground {
    Dark,
    Light,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalColorLevel {
    TrueColor,
    Ansi256,
    Ansi16,
    Monochrome,
}
```

The profile is passed to theme resolution:

```rust
let profile = detect_terminal_profile(config.theme);
let theme = Theme::resolve(config.theme, profile);
```

All later rendering consumes the resolved `Theme` and its color level. No
environment lookup or terminal query runs per frame.

This is preferred over adapting colors at every call site because it gives the
render cache a stable value identity and avoids repeated checks. It is
preferred over environment-only background detection because the objective
explicitly requires terminal background discovery. It is preferred over a
custom `/dev/tty` OSC parser because terminal query ownership, timeout,
Windows, macOS polling, DA1 feature detection, and terminal quirks are already
handled by a focused maintained library.

A `CapabilityBackend` wraps the crossterm backend as a final safety boundary.
Most styles are already adapted in `Theme` or syntax generation. The wrapper
still validates changed cells before crossterm emits them, catching fixed
colors in setup screens and colors produced by third-party widgets.

## Dependencies

Add workspace dependencies:

```toml
terminal-colorsaurus = "1.0.3"
supports-color = "3.0.2"
```

`orca-tui` consumes both directly.

`terminal-colorsaurus` owns the OSC 11 exchange, raw-mode guard, `/dev/tty`
or Windows terminal handle, DA1 feature detection, response consumption, and
timeout. This avoids passing OSC responses through crossterm 0.28, whose public
event parser does not expose OSC response events.

`supports-color` owns stdout TTY and environment heuristics, including
`COLORTERM`, `TERM`, `TERM_PROGRAM`, `NO_COLOR`, `FORCE_COLOR`, and Windows
terminal support.

## Theme Selection

Extend `ThemeName`:

```rust
pub enum ThemeName {
    #[default]
    Auto,
    Dark,
    Light,
    Solarized,
    Catppuccin,
}
```

Existing configuration values remain valid. A missing `theme` now deserializes
to `Auto`.

Only `Auto` consults the detected background:

| Requested theme | Detected background | Base palette |
|---|---|---|
| Auto | Light | Light |
| Auto | Dark | Dark |
| Auto | Unknown or query failure | Dark |
| Dark | Any | Dark |
| Light | Any | Light |
| Solarized | Any | Solarized |
| Catppuccin | Any | Catppuccin |

Explicit user choices are never overridden by OSC results.

`Theme::named(ThemeName)` remains available for existing tests and focused
callers. `ThemeName::Auto` resolves to the truecolor Dark palette in that
context because no terminal profile is supplied. Production startup uses
`Theme::resolve`.

## Startup Detection

### Ordering

Detection happens at the start of `run_tui_inner`, before:

- `crossterm::terminal::enable_raw_mode`;
- alternate-screen entry;
- mouse, paste, focus, or keyboard-enhancement setup;
- construction of the crossterm event reader;
- the initial frame.

This gives `terminal-colorsaurus` exclusive ownership of the terminal query and
ensures OSC/DA1 responses cannot race with Orca input events.

### Background

When the configured theme is `Auto`, call:

```rust
terminal_colorsaurus::background_color(QueryOptions {
    timeout: Duration::from_millis(250),
})
```

Map a successful color using `Color::perceived_lightness()`:

- `<= 0.5` is Dark;
- `> 0.5` is Light.

Any error maps to `TerminalBackground::Unknown`.

When the configured theme is explicit, do not call the library; record
`Unknown`. Background capability and color depth are independent, so explicit
themes still receive color-depth adaptation.

The 250 ms timeout bounds startup latency on SSH or unusual terminals.
Known-unsupported terminals generally return before the timeout through the
library's DA1 feature detection.

### Color Depth

Call `supports_color::on(Stream::Stdout)` once:

| Detection result | Color level |
|---|---|
| `has_16m` | TrueColor |
| otherwise `has_256` | Ansi256 |
| otherwise `has_basic` | Ansi16 |
| `None` | Monochrome |

The output stream must be a terminal for normal TUI startup. Tests exercise
the pure mapping from detected facts rather than relying on the test process's
TTY.

Detection failures never stop TUI startup.

## Color Adaptation

`TerminalColorLevel` owns:

```rust
fn adapt_color(self, color: Color) -> Color
fn adapt_style(self, style: Style) -> Style
fn revision(self) -> u64
```

### Truecolor

Return colors unchanged.

### ANSI-256

Convert `Color::Rgb` to the nearest xterm fixed color among indices 16 through
255:

- indices 16–231: the 6×6×6 color cube using component levels
  `0, 95, 135, 175, 215, 255`;
- indices 232–255: grayscale levels `8 + 10n`.

Use squared Euclidean distance in RGB space and lower index as the stable
tie-breaker.

Indices 0–15 are excluded because their RGB values are terminal-configurable.
Existing `Color::Indexed` and named ANSI colors remain unchanged at this level.

### ANSI-16

Convert RGB and indexed colors to the nearest canonical ANSI-16 color, then
emit ratatui's named variants:

```text
Black, Red, Green, Yellow, Blue, Magenta, Cyan, Gray,
DarkGray, LightRed, LightGreen, LightYellow,
LightBlue, LightMagenta, LightCyan, White
```

Use the canonical xterm 16-color RGB table for distance calculation and the
table order as the stable tie-breaker.

Existing named ANSI colors remain unchanged.

### Monochrome

Map every non-`Reset` foreground and background to `Color::Reset`. Preserve
all style modifiers, including `BOLD`, `ITALIC`, and `REVERSED`.

Semantic meaning still has non-color channels:

- cursor: reversed software cell plus hardware cursor;
- selection: reversed style;
- status: icons and text labels;
- diff: `+`, `-`, and context prefixes;
- headings: bold;
- reasoning: italic.

## Output Safety Backend

Wrap the production crossterm backend:

```rust
pub(crate) struct CapabilityBackend<B> {
    inner: B,
    color_level: TerminalColorLevel,
}
```

It implements ratatui's `Backend` by delegating every method. `draw` has two
paths:

- TrueColor delegates the incoming iterator directly, with no cell cloning or
  conversion.
- ANSI-256, ANSI-16, and Monochrome clone only the changed cells supplied by
  ratatui's diff renderer, adapt each cell style, then delegate the adapted
  iterator.

This is not the primary palette conversion path. It is a correctness safety
net that guarantees crossterm never receives an unsupported `Rgb`, indexed, or
named ANSI color even if a fixed style remains outside `Theme`.

The wrapper exposes `inner_mut()` for terminal lifecycle commands that must
reach `CrosstermBackend<Stdout>`, such as clearing native scrollback.

Backend adaptation preserves symbols, modifiers, skip flags, and coordinates.
It does not alter the ratatui frame buffer retained for selection, layout, or
cache tests.

## Resolved Theme

`Theme::resolve` first chooses the base named truecolor palette, then adapts
every color field through the profile:

- border, text, muted, user;
- success, warning, error, approval, plan mode;
- Markdown H1/H2/H3 and inline code;
- diff add/remove;
- selection background.

Add:

```rust
pub(crate) color_level: TerminalColorLevel,
pub(crate) syntax_theme_revision: u64,
```

The syntax revision preserves current truecolor values and adds color-level
identity for degraded output:

```text
truecolor: existing SyntaxTheme revision
ansi256:   existing revision + 0x100
ansi16:    existing revision + 0x200
mono:      existing revision + 0x300
```

`ThemeIdentity` includes `color_level` explicitly even though most adapted
fields also differ. This prevents accidental cache collisions when two base
colors quantize to the same reduced color.

## Syntax Highlighting

Syntect continues to parse and produce its original RGB styles. Adaptation is
applied at the style-output boundary, not by modifying embedded themes.

Add `TerminalColorLevel` to the conversion functions:

```rust
fn to_ratatui_style(
    style: syntect::highlighting::Style,
    color_level: TerminalColorLevel,
) -> Style
```

Thread the color level through:

- `highlight_code`;
- `highlighter_for_path`;
- `LineHighlighter`.

Fenced Markdown and foreground diff parsing therefore produce capability-safe
spans directly.

The background full-file diff worker also receives a color level in
`EditHighlightJob`. Its identity includes:

- syntax theme;
- syntax-theme/color-level revision;
- color level.

This ensures a style map can never be applied under a different output
capability.

## Refined Diff Styles

`AppState` stores both:

```rust
syntax_theme: SyntaxTheme,
syntax_color_level: TerminalColorLevel,
```

`configure_syntax_highlighting` receives both from the resolved `Theme`.

Foreground parsed-diff highlighting and background full-file refinement pass
the same level into `highlighter_for_path`. Existing refined-style validation
continues comparing text and job identity. No late per-frame RGB conversion is
needed.

## Selection and Monochrome

The current selection API accepts only a background color. Replace it with a
selection `Style` so monochrome can use `REVERSED`:

```rust
pub(crate) fn selection_style(&self) -> Style {
    match self.color_level {
        TerminalColorLevel::Monochrome => {
            Style::default().add_modifier(Modifier::REVERSED)
        }
        _ => Style::default().bg(self.selection_bg),
    }
}
```

`apply_selection_to_line` patches the source span style with the supplied
selection style. Foreground colors and syntax modifiers remain intact in color
modes; monochrome selection remains visible through reversal.

The jump-to-bottom pill uses the same selection style plus the theme text
foreground. Other layout and hit-testing behavior is unchanged.

The composer selection currently uses a fixed `LightBlue` background. Route it
through `theme.selection_style()` so monochrome and reduced-color modes follow
the same contract.

## Configuration and Compatibility

- `theme = "auto"` is accepted.
- Missing theme defaults to Auto.
- Existing explicit values deserialize unchanged.
- Config display prints `auto` when defaulted.
- CLI and runtime `RunConfig` continue carrying `ThemeName`; no terminal
  profile enters core runtime logic.
- Non-TUI modes do not execute terminal probing.

## Error Handling

- Background query error or timeout: `Unknown`, then Dark for Auto.
- Color support returns `None`: Monochrome.
- No stdout TTY: the TUI's existing startup path determines whether it can run;
  detection itself never panics.
- Unsupported `screen`, `TERM=dumb`, redirected streams, Windows versions
  without query support, and malformed terminal responses are library errors
  and fall back silently.
- No diagnostic escape sequence or error text is written into the TUI buffer.
- Terminal query state is restored by `terminal-colorsaurus` before Orca
  enables its own raw mode.

## Testing

Implementation follows strict test-driven development.

### Configuration

- `ThemeName::default()` is Auto.
- `auto`, Dark, Light, Solarized, and Catppuccin deserialize and serialize.
- Existing explicit theme config tests remain unchanged.

### Pure Profile Resolution

- Auto + Light chooses Light.
- Auto + Dark chooses Dark.
- Auto + Unknown chooses Dark.
- Explicit themes ignore every detected background.
- Each supports-color fact combination maps to one color level.
- Background and color-level changes are independent.

### Color Mapping

- Known RGB values map to stable expected ANSI-256 indices.
- Known RGB and indexed values map to stable ANSI-16 names.
- Truecolor is identity.
- Monochrome removes foreground/background but preserves modifiers.
- All fields of all four base themes satisfy the selected output level.
- The backend safety adapter removes unsupported colors from fixed-color and
  third-party cells while preserving content and modifiers.

### Syntax and Diff

- Fenced code emits no `Rgb` under ANSI-256, ANSI-16, or Monochrome.
- Parsed diff syntax spans obey the same profile.
- Background refined diff jobs include the level in identity and output.
- Existing source-boundary and guardrail tests remain green.

### Selection

- Color modes preserve syntax foregrounds and apply adapted backgrounds.
- Monochrome selection and composer selection use `REVERSED`.
- Hardware cursor and Vim mode cursor tests remain green.

### Startup

Use an injected detector trait/function in tests:

- explicit themes never invoke background detection;
- Auto invokes it once;
- query success, timeout, unsupported, and error map correctly;
- detection occurs before raw-mode setup in the startup orchestration helper.

No automated test sends a real OSC query to the developer's terminal.

### Backend

- TrueColor forwards draw content unchanged.
- ANSI-256 forwards no RGB cells.
- ANSI-16 forwards only named ANSI or Reset colors.
- Monochrome forwards only Reset/None colors.
- Symbols, coordinates, modifiers, and skip flags are preserved.
- Clear, cursor, append-lines, and scrolling-region methods delegate exactly.

### Regression Commands

```sh
cargo test -p orca-core theme --lib
cargo test -p orca-tui terminal_capabilities --lib
cargo test -p orca-tui theme::tests --lib
cargo test -p orca-tui syntax_highlight --lib
cargo test -p orca-tui diff_highlight --lib
cargo test -p orca-tui selection --lib
cargo test -p orca-tui edit_highlight --lib
cargo test -p orca-tui -- --test-threads=1
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

## Delivery

The design, plan, and implementation are separate commits on
`feature/tui-syntax-highlighting`. Each commit ends with:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

Every implementation task receives specification and quality review. Final
delivery includes a prompt-to-artifact audit, fresh package and workspace
tests, push, and local/remote SHA comparison.
