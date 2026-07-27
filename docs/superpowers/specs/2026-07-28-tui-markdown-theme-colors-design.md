# TUI Markdown Theme Colors Design

## Objective

Route Markdown presentation colors through `Theme` so Dark, Light, Solarized,
and Catppuccin render headings, inline code, prose, quotations, lists, tables,
and plain-code fallbacks with palette-appropriate colors.

The current Markdown renderer bypasses `theme.rs` in several places:

- H1 uses `Color::Cyan`.
- H2 uses `Color::Green`.
- H3 through H6 use `Color::Yellow`.
- Inline code uses `Color::Magenta`.
- Prose and table values use `Color::White`.
- List markers and table separators use `Color::DarkGray`.
- Block quotes and unhighlighted fenced code use `Color::Gray`.

These fixed ANSI colors can have poor contrast or clash with the selected
palette, especially under Light and Solarized themes.

## Scope

This sub-project includes:

- Four Markdown-specific semantic colors on `Theme`:
  - `markdown_h1`
  - `markdown_h2`
  - `markdown_h3`
  - `markdown_inline_code`
- Theme-aware Markdown prose, list markers, block quotes, tables, and
  unhighlighted fenced/indented code.
- Cache invalidation when any Markdown-specific theme color changes.
- Direct renderer tests for all four named themes.
- Cache tests proving one rebuild on a Markdown color change and no rebuild
  once the theme is stable.

It does not include:

- Setup, shortcut, approval, or other non-Markdown hardcoded color cleanup.
- New Markdown syntax or layout behavior.
- Changes to syntect token colors or syntax-theme selection.
- Background colors, borders, padding, or typography changes.
- Terminal capability detection or reduced-color palette selection.
- Runtime theme switching UI.

## Chosen Approach

Add dedicated Markdown semantic fields rather than reusing status colors such
as `success` or `warning`.

This keeps document hierarchy independent from business state. A future theme
author can change a warning color without unintentionally changing every H3,
and can tune inline code independently from approval or plan-mode accents.

Extracting these colors from the syntect theme is rejected because syntect
themes describe source-code scopes, not stable UI semantics. Reusing existing
status fields is rejected because it couples unrelated visual roles.

## Theme Palette

Each named theme declares all four fields explicitly:

| Theme | H1 | H2 | H3–H6 | Inline code |
|---|---|---|---|---|
| Dark | brand blue `#4D6BFE` | purple `#A98BF5` | gold `#D9A441` | teal `#40AAAA` |
| Light | deep blue `#3A56E6` | purple `#8A5CE6` | ochre `#B07A14` | teal `#006666` |
| Solarized | blue `#268BD2` | cyan `#2AA198` | yellow `#B58900` | magenta `#D33682` |
| Catppuccin | mauve `#CBA6F7` | sapphire `#74C7EC` | yellow `#F9E2AF` | pink `#F5C2E7` |

The renderer maps Markdown roles as follows:

| Markdown role | Theme source |
|---|---|
| Plain prose, strong, emphasis, table values | `theme.text` |
| H1 | `theme.markdown_h1` |
| H2 | `theme.markdown_h2` |
| H3–H6 | `theme.markdown_h3` |
| Inline code | `theme.markdown_inline_code` |
| List bullets, blockquote marker/text, table separators | `theme.muted` |
| Table headers | `theme.markdown_h1` plus bold |
| Record-layout table keys | `theme.markdown_h3` |
| Unknown-language or guardrail-rejected code | `theme.muted` |
| Successfully highlighted fenced code | Existing syntect spans, unchanged |

Strong and emphasis continue to inherit the current style and add only their
existing modifiers. Nested strong/emphasis inside a heading therefore retain
the heading color.

## Rendering Changes

`render_markdown` initializes its style stack with:

```rust
Style::default().fg(theme.text)
```

Heading events choose one of the three Markdown heading fields and retain the
existing bold modifier. Inline-code events use
`theme.markdown_inline_code`.

List and blockquote rendering replace fixed gray colors with `theme.muted`.
Blockquote content remains muted until its matching end event.

`append_code_block` keeps successful syntect output unchanged. Its plain-text
fallback uses `theme.muted`, preserving the current source text and two-space
indentation.

The table helpers receive `&Theme`:

```rust
fn render_table(
    rows: &[Vec<String>],
    lines: &mut Vec<Line<'static>>,
    available_width: usize,
    theme: &Theme,
)
```

They use the mapping above without changing table width allocation, wrapping,
record conversion, or emitted text.

## Cache Identity

`TranscriptRenderCache` already stores a value-based `ThemeIdentity`. Add the
four Markdown fields to that identity:

```rust
struct ThemeIdentity {
    // existing fields
    markdown_h1: Color,
    markdown_h2: Color,
    markdown_h3: Color,
    markdown_inline_code: Color,
}
```

No new revision counter is introduced. The existing cache comparison will
dirty all messages exactly once when a Markdown color differs. A subsequent
prepare with the same theme remains a cache hit.

`syntax_theme_revision` remains responsible only for syntect palette changes.
This preserves the current distinction between UI theme identity and syntax
theme identity.

## Error and Edge Handling

- Unknown fenced languages and inputs rejected by syntax guardrails remain
  readable through `theme.muted`.
- Empty Markdown, empty code blocks, narrow tables, and record-form tables keep
  their existing text and row structure.
- Theme changes affect styles only; no Markdown text, wrapping, selection
  extraction, or message revision is changed.
- Every `Theme::named` branch must initialize all new fields, so adding a field
  cannot silently fall back to an ANSI color.

## Testing

Implementation follows strict test-driven development.

### Theme tests

- Every named theme has the exact H1, H2, H3, and inline-code colors from the
  palette table.
- The four Markdown colors are not fixed ANSI `Cyan`, `Green`, `Yellow`, or
  `Magenta`.

### Renderer tests

For each named theme, render one fixture containing:

- H1, H2, and H3.
- Plain prose.
- Strong and emphasis.
- Inline code.
- A list item.
- A block quote.
- A normal table.
- A narrow record-layout table.
- An unknown-language fenced block.

Assert the emitted text remains unchanged and each role uses the documented
theme field. Existing highlighted Rust code tests continue to prove syntect
token colors are preserved.

### Cache tests

- Build one assistant Markdown message.
- Change only one Markdown semantic field in a copied theme.
- Prepare the cache and assert the message rebuilds exactly once.
- Prepare again with the same changed theme and assert zero messages rebuild.

### Regression checks

Run:

```sh
cargo test -p orca-tui markdown_theme --lib
cargo test -p orca-tui inline_code --lib
cargo test -p orca-tui table --lib
cargo test -p orca-tui transcript_view::tests --lib
cargo test -p orca-tui --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

## Delivery

The design, implementation plan, and implementation are committed separately
on `feature/tui-syntax-highlighting`. Every commit ends with:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

After task-level specification and quality reviews plus a final holistic
review, the branch is pushed and the local and remote SHAs are compared.
