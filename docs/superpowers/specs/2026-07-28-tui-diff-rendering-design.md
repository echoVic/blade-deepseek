# TUI Diff Rendering Upgrade Design

## Goal

Upgrade Orca's unified-diff presentation from prefix-only coloring to a compact,
readable review surface with:

- capability-aware added/deleted line backgrounds;
- a right-aligned old/new line-number gutter;
- quiet `⋮` hunk separators instead of raw `@@` coordinates;
- conservative word-level emphasis for adjacent replacements;
- exact preservation of the current parser, syntax highlighting, progressive
  full-file refinement, malformed-input fallback, and 80-row bound.

This is P0 item 6 only. It does not include streaming checkpoints, transcript
search, queue UI, status-bar metadata, Vim expansion, keybinding configuration,
diagnostics, or onboarding.

## Current State

`crates/orca-tui/src/diff_highlight.rs` already owns the relevant pipeline:

1. `parse_unified_diff` validates unified-diff structure and records exact
   `old_line` / `new_line` values.
2. `render_parsed_diff` applies first-paint syntax highlighting.
3. `RefinedDiffStyles` can replace new-side syntax spans with verified
   full-file parser state from the background worker.
4. malformed or ambiguous input falls back to the retained raw diff.
5. `MAX_RENDERED_DIFF_LINES` bounds the view at 80 rows.

The parser and refinement pipeline are already heavily regression-tested. The
upgrade therefore belongs in the presentation layer rather than a replacement
parser or a new width-dependent widget.

## Chosen Approach

Keep `ParsedDiff`, the highlight worker, transcript caching, and the UI call
site unchanged. Add:

- diff background colors to `Theme`;
- a pure gutter formatter;
- a pure adjacent replacement-cluster detector;
- a bounded inline-emphasis helper using `similar`;
- background composition in the existing rendered-line path.

This approach has the smallest behavioral surface and keeps diff work inside
the already cached message rendering path. No parsing or highlighting work is
moved into ratatui's per-frame draw stage.

## Alternatives Rejected

### Rebuild the View from a New `similar::TextDiff`

This would make word-level matching central, but it would bypass Orca's strict
unified-diff validation and its exact file/hunk metadata. It also risks
inventing pairings for malformed or multi-file input.

### Introduce a Width-Aware Diff Widget

A dedicated widget could repeat gutters on wrapped visual rows, but it would
require terminal width to enter the transcript cache contract and would expand
this low-cost visual upgrade into a layout rewrite. Existing transcript
wrapping remains the owner of narrow-terminal behavior.

## Theme Model

`Theme` gains four colors:

```rust
pub diff_add_bg: Color,
pub diff_remove_bg: Color,
pub diff_add_emphasis_bg: Color,
pub diff_remove_emphasis_bg: Color,
```

Foreground fields `diff_add` and `diff_remove` remain unchanged.

### Truecolor Palettes

| Theme | Added row | Deleted row | Added emphasis | Deleted emphasis |
|---|---|---|---|---|
| Dark | `#213A2B` | `#4A221D` | `#315C40` | `#71352A` |
| Light | `#DCFCE7` | `#FEE2E2` | `#86EFAC` | `#FCA5A5` |
| Solarized | `#163C3A` | `#4C2A2A` | `#245B52` | `#713C35` |
| Catppuccin | `#294436` | `#4A303A` | `#3D654D` | `#704555` |

`Theme::resolve` passes all four through
`TerminalColorLevel::adapt_color`. This produces deterministic xterm-256 and
ANSI-16 fallbacks through the existing color quantizer.

For `Monochrome`, adapted backgrounds become `Color::Reset`. The renderer must
not set a background in that mode. Added/deleted identity remains visible in
the gutter, and changed inline tokens use `Modifier::BOLD`.

Theme tests include the new fields in the palette equality and capability-fit
checks so a future RGB-only addition cannot bypass terminal degradation.

## Gutter

Structured source lines render with a shared old/new number width:

```text
 12    - deleted source
     12 + added source
 13  13   context source
```

The exact formatter is:

```text
{old:>width} {new:>width} {marker} 
```

where absent numbers render as `width` spaces and `marker` is `-`, `+`, or one
space. The number width is the maximum decimal width of any old/new source-line
number in the parsed diff, with a minimum of one.

The gutter is one styled span. Its foreground is:

- `theme.diff_remove` for deletions;
- `theme.diff_add` for insertions;
- `theme.muted` for context.

Added and deleted gutter cells receive the same row background as their source
content. Context gutters have no background.

The renderer does not depend on terminal width. Existing transcript wrapping
may wrap long source content, but the source text and first-row gutter remain
stable and cacheable.

## Hunk Separators

Raw coordinate headers such as:

```text
@@ -12,4 +12,5 @@ fn parse()
```

render as:

```text
 ⋮   ⋮   fn parse()
```

Both vertical ellipses occupy the old/new number columns. The coordinate
section is removed. Text after the closing `@@` is preserved after trimming
outer whitespace. When there is no suffix, the separator contains only the
gutter ellipses.

The separator uses `theme.border` for `⋮` and `theme.muted` for an optional
scope suffix. Hunk separators continue to consume one row from the existing
80-row budget.

Metadata inside a hunk remains metadata and is not assigned fabricated line
numbers. File headers and prelude lines retain their current conservative
presentation.

## Row Background Composition

`rendered_source_line` continues to select source foreground spans in this
order:

1. verified `RefinedDiffStyles` on the new side;
2. first-paint syntax spans;
3. the existing diff foreground fallback.

The new row style is then merged onto every gutter and content span:

- insertion: `diff_add_bg`;
- deletion: `diff_remove_bg`;
- context: no background.

The merge changes only `Style::bg`; it preserves syntax foreground,
modifiers, and underline color. This ensures full-file refinement remains
visible over the diff surface.

## Word-Level Emphasis

### Pairing Policy

Inline refinement is deliberately conservative.

Within one hunk, a replacement cluster is eligible only when:

1. one or more consecutive deletion entries are immediately followed by;
2. one or more consecutive insertion entries;
3. no context or metadata entry separates the two blocks.

Clusters do not cross hunk boundaries. The implementation does not search a
whole hunk for a "best" match.

For an eligible cluster, old and new block text is joined with `\n` and passed
to `similar::TextDiff::from_lines`. Inline changes are read from
`iter_inline_changes` in source order. The returned old/new line indexes map
directly back to the cluster entries.

`similar`'s default minimum ratio remains in force. If it declines to refine a
replacement, the renderer keeps only the whole-row background.

### Emphasis Style

For changed tokens:

- insertion spans use `diff_add_emphasis_bg`;
- deletion spans use `diff_remove_emphasis_bg`;
- equal tokens retain the ordinary row background.

In monochrome, changed tokens use `Modifier::BOLD` and equal tokens keep their
existing modifiers. Text must remain byte-for-byte equal to the parsed source
line after span concatenation.

### Bounds

Inline work runs only for source entries already admitted by the 80-row render
budget. A cluster is skipped when either side exceeds any existing syntax
guardrail:

- aggregate input above 512 KiB;
- more than 10,000 source lines;
- a source line above 4 KiB.

The renderer uses the `similar` API with an explicit short deadline rather
than its default 500 ms deadline. The deadline is 5 ms per replacement
cluster. A timeout or unusable result falls back to whole-row styling.

This keeps pathological diffs bounded while preserving the existing parser's
larger structural validation.

## Raw and Malformed Fallback

Malformed, structurally ambiguous, and raw fragments remain fail-closed:

- exact original text is retained;
- no fabricated line numbers are displayed;
- no word-level pairing runs;
- no syntax/refined spans are applied;
- existing add/remove/header foreground classification remains.

Raw fallback does not gain row backgrounds because it cannot prove whether a
line is source data or diff metadata. This intentionally favors truthful
structure over visual consistency.

## Truncation

The total display budget remains 80 parsed rows plus the existing truncation
marker when content was omitted.

Gutter computation does not add rows. Word-level emphasis does not add or
remove rows. A replacement cluster cut by the 80-row boundary is not inline
refined unless both its deletion and insertion blocks are fully admitted.
Partially visible clusters receive only whole-row backgrounds.

The truncation marker stays:

```text
    [... diff truncated ...]
```

and uses `theme.muted`.

## Dependencies and Files

### Modify `Cargo.toml`

Keep the existing workspace `similar = "3.1.1"` declaration.

### Modify `crates/orca-tui/Cargo.toml`

Add:

```toml
similar = { workspace = true, features = ["inline"] }
```

as a direct dependency because `similar` does not enable its inline-change API
through default features.

### Modify `crates/orca-tui/src/theme.rs`

Add and capability-adapt the four background colors and extend theme tests.

### Modify `crates/orca-tui/src/diff_highlight.rs`

Add gutter formatting, hunk separator formatting, replacement-cluster
classification, bounded inline refinement, and style composition.

No change is required in `ui.rs`, `types.rs`, the edit-highlight worker,
transcript cache, parser output, or runtime event handling.

## Test Matrix

### Gutter and Separators

- old/new numbers are right aligned for one-, two-, and three-digit values;
- absent old/new numbers occupy spaces rather than zero;
- context lines show both numbers;
- deletion and insertion lines show only their real side;
- hunk coordinates disappear;
- the optional scope suffix remains;
- multiple hunks reuse one stable gutter width.

### Backgrounds

- Dark truecolor uses exact `#213A2B` and `#4A221D`;
- all content spans, including syntax and refined spans, inherit row
  backgrounds without losing foregrounds;
- xterm-256 output contains no RGB colors;
- ANSI-16 output contains no RGB or indexed colors;
- monochrome output contains no foreground/background color and preserves
  add/delete identity in text.

### Inline Changes

- a neighboring delete/insert replacement highlights only changed words;
- multiple adjacent lines map to their original old/new line numbers;
- syntax/refined foreground survives emphasis background composition;
- nonadjacent delete/insert entries do not pair;
- metadata/context breaks a cluster;
- low-similarity replacements keep only row backgrounds;
- Unicode and combining text is preserved exactly;
- a cluster cut by the 80-row boundary is not partially refined;
- over-limit lines skip inline work.

### Existing Contracts

- 80-row truncation remains exact;
- malformed raw fallback remains exact and unsegmented;
- multi-file ambiguity remains syntax-ineligible;
- progressive full-file refinement remains new-side only;
- guardrails remain 512 KiB, 10,000 lines, and 4 KiB per line;
- all existing parser and UI tests remain green.

## Delivery Gates

Before delivery:

1. run focused gutter, separator, background, inline-change, fallback, and
   truncation tests;
2. run every `diff_highlight` test;
3. run `ui::tests`;
4. run the complete `orca-tui` package serially;
5. run the workspace all-targets suite serially;
6. run `cargo check -p orca-tui`, formatting, and `git diff --check`;
7. request independent specification and quality reviews;
8. verify every commit has exactly one required co-author trailer;
9. push `feature/tui-syntax-highlighting` and compare local/remote SHAs.
