# TUI Transcript Search Design

**Date:** 2026-07-28

**Status:** Approved

## Goal

Add interactive full-transcript search to the fullscreen TUI:

- `Ctrl+F` opens search in ordinary TUI use;
- Vim normal-mode `/` opens the same search surface;
- matches are highlighted in the existing transcript viewport;
- users can jump forward and backward through matches;
- searching reuses `TranscriptRenderCache` and does not re-render or rescan on
  steady frames and scroll-only frames.

The feature searches the text users actually see after Markdown, tool, plan,
and status rendering. It does not introduce a second transcript cache.

## Scope

This sub-project includes:

- one-line interactive search input;
- smart-case literal substring matching;
- visible match highlighting;
- active-match navigation with wraparound;
- `Ctrl+F`, `Enter`, `Shift+Enter`, `Ctrl+G`, and
  `Ctrl+Shift+G` interaction;
- Vim normal-mode `/`, `n`, and `N` interaction;
- cache-generation-aware rescanning;
- Unicode display-column correctness;
- streaming, resize, selection, and auto-follow integration;
- shortcut help and completed-frame tests.

This sub-project excludes:

- regular expressions;
- fuzzy transcript search;
- persistent query history;
- search-and-replace;
- configurable keybindings;
- Vim counts, registers, `dd`, `gg`, `G`, or dot-repeat;
- searching hidden source text that is not rendered;
- searching historical sessions outside the current loaded transcript;
- a separate search index or background worker.

Those exclusions keep this project independently reviewable and avoid mixing
later P2 work into transcript search.

## Search Semantics

Search uses literal substring matching over one rendered logical line at a
time.

### Smart Case

- If the query contains no uppercase Unicode character, matching is
  case-insensitive.
- If the query contains at least one uppercase Unicode character, matching is
  case-sensitive.
- Matching is Unicode-aware and never slices invalid UTF-8.

Examples:

| Query | Text | Match |
|---|---|---|
| `error` | `ERROR` | yes |
| `Error` | `ERROR` | no |
| `中文` | `中文结果` | yes |

### Match Boundaries

- A match may span adjacent styled spans in one rendered logical line.
- A match may span visual rows created by soft wrapping because those rows
  belong to one logical line.
- A match does not cross a hard line break.
- A match does not cross a message boundary.
- A match does not include whitespace dropped by the wrapper unless that
  whitespace exists in the rendered logical line.
- Empty queries produce no matches.

Search is performed over rendered text, not raw Markdown source. For example:

- rendered code content is searchable;
- rendered tool output is searchable;
- transient running/receiving spinner glyphs are excluded from search while
  the stable tool label and status text remain searchable;
- Markdown control delimiters that are not displayed are not searchable;
- hidden newline-gated assistant suffixes are not searchable until they enter
  the transcript projection;
- held Markdown tables are not searchable until the table is released.

## User Interaction

### Opening Search

`Ctrl+F` opens transcript search when the conversation view is available in:

- `Idle`;
- `Running`;
- `WaitingUserInput`.

Vim normal-mode `/` opens the same search state.

Opening search:

- preserves the current transcript scroll position;
- initializes an empty editable query;
- hides the main composer hardware cursor;
- gives the search field the terminal hardware cursor;
- clears no existing transcript selection;
- suspends composer input handling until search closes.

If search is already open, `Ctrl+F` keeps it open and focuses its query.

### Editing

While search is open:

- printable characters insert into the query;
- bracketed paste inserts into the query rather than the composer, with line
  breaks normalized to single spaces;
- Backspace deletes the previous character;
- `Ctrl+U` clears the query;
- query changes recompute matches only when the transcript cache generation or
  query identity changed;
- the active match is preserved by coordinate when it still exists;
- otherwise the first match at or below the current viewport becomes active;
- if no match is at or below the viewport, the first match becomes active.

Composer slash and mention popups do not open while search owns input.

### Navigation

- `Enter` moves to the next match.
- `Ctrl+G` moves to the next match.
- `Shift+Enter` moves to the previous match.
- `Ctrl+Shift+G` moves to the previous match.
- Navigation wraps at both ends.
- `Esc` closes the search field and preserves the current transcript scroll
  position.

When search is closed and a non-empty search session still exists:

- Vim normal-mode `n` moves to the next match;
- Vim normal-mode `N` moves to the previous match.

Opening a new search replaces the prior query. Closing search does not erase
the query or matches, so `n` and `N` remain useful.

## Jump Behavior

Activating a match:

- disables auto-follow;
- sets `scroll_offset` so the full active match is visible when possible;
- keeps the current scroll position unchanged if the active match is already
  fully visible;
- aligns an off-screen match to the nearest viewport edge rather than always
  centering it;
- never scrolls past the transcript bounds;
- does not mutate message revisions or transcript cache entries.

If streaming or resize changes the active match coordinates:

- the search state recomputes from the cache generation;
- it preserves the active match by stable rendered-line identity and match
  byte range when possible;
- if that match disappeared, it chooses the nearest following match, wrapping
  only when no later match exists;
- it does not automatically scroll to a newly arriving match.

## Search Field

The search field occupies one fixed row immediately above the main composer.
It is visible while search input is open in conversation mode.

The row contains:

```text
 Find: <query>                                      3/17
```

Behavior:

- `current/total` is one-based when a match is active;
- no results show `0/0`;
- an empty query shows `0/0`;
- the query truncates horizontally while preserving the cursor;
- the result count remains visible in narrow terminals when space permits;
- at extremely narrow widths, query content yields before the prefix and
  result count;
- the search field never overlaps the composer, activity line, plan panel, or
  status line.

The field uses the shared textarea surface and hardware-cursor positioning
machinery already used by the composer and setup input. It does not issue
direct terminal cursor commands.

## Match Model

Add a focused module:

```text
crates/orca-tui/src/transcript_search.rs
```

Core types:

```rust
pub(crate) struct TranscriptSearchState {
    open: bool,
    query: String,
    matches: Vec<TranscriptSearchMatch>,
    active: Option<usize>,
    cache_generation: u64,
    query_identity: SearchQueryIdentity,
}

pub(crate) struct TranscriptSearchMatch {
    start: SelectionPos,
    end: SelectionPos,
    line_identity: TranscriptLineIdentity,
    byte_range: Range<usize>,
}
```

Field visibility may remain private or `pub(crate)` according to call-site
needs. The following contracts are fixed:

- matches are stored as absolute transcript visual coordinates;
- each match preserves enough logical-line identity to survive unrelated
  appends and rewrap recomputation;
- no message text is copied into the search state;
- match ranges are ordered in transcript reading order;
- match ranges never overlap unless the literal matcher deliberately supports
  overlapping matches.

Literal matching uses non-overlapping occurrences. For query `aa` in `aaaa`,
the matches are `[0..2, 2..4]`.

## Cache Integration

`TranscriptRenderCache` remains the authority for rendered transcript text and
visual coordinates.

### Content Generation

Add a monotonic cache content generation:

```rust
content_generation: u64
```

The generation changes when searchable rendered content or its coordinates
may change:

- an entry is built or rebuilt;
- an entry is invalidated and later prepared;
- cache width changes;
- theme identity changes;
- syntax-theme revision changes;
- force-expand identity changes;
- messages are truncated;
- messages are retained/reindexed;
- the cache is cleared.

The generation does not change for:

- scroll-only viewport materialization;
- steady `prepare` calls that visit zero entries;
- spinner glyph patching when searchable non-spinner text is unchanged;
- search highlight changes;
- selection highlight changes.

Wrapping generation on integer overflow is allowed, but zero is not treated as
a permanent sentinel.

### Read-Only Search API

Expose this read-only API:

```rust
pub(crate) fn search(
    &self,
    first_retained_message: usize,
    query: &SearchQuery,
) -> Vec<TranscriptSearchMatch>;
```

The API:

- scans cached logical-line text;
- skips entries before `first_retained_message`, because those rows are not
  navigable in the live viewport;
- maps byte ranges to absolute visual rows and display columns;
- handles spans and soft-wrap boundaries;
- does not materialize the entire viewport;
- does not modify dirty indices;
- does not rebuild cache entries;
- returns no match for missing/unprepared entries;
- is deterministic for the same cache generation and query.

The cache may expose focused helpers for:

- content generation;
- absolute total height;
- match visibility;
- scroll offset required to reveal a range.

It must not learn about keyboard shortcuts or search UI state.

## Unicode and Display Coordinates

Matching operates on UTF-8 string boundaries. Highlight coordinates operate
on terminal display columns.

Required behavior:

- CJK characters occupy their rendered width;
- combining sequences highlight intact source characters without invalid
  slicing;
- emoji and extended grapheme sequences preserve the existing rendered text;
- zero-width controls do not create invalid columns;
- soft-wrapped matches highlight every visual row segment that belongs to the
  logical match;
- an oversized wide character that the wrapper omits cannot become a visible
  match;
- smart-case comparison does not change stored source text.

The implementation may use Unicode lowercase normalization for comparison, but
must retain a mapping from folded bytes or characters back to original byte
ranges. A simpler character-window matcher is preferred over allocating a
second folded copy of the full transcript.

## Highlight Rendering

Search highlighting is a render-time overlay, like mouse selection. It does
not enter `TranscriptRenderCache`.

Visible materialized rows receive overlays in this order:

1. base cached styles;
2. non-active search match style;
3. active search match style;
4. mouse selection style.

Mouse selection wins so copy selection remains visually unambiguous.

Theme additions:

```rust
search_match: Style
search_match_active: Style
```

Themes must provide:

- truecolor styles;
- ANSI-256-safe styles;
- ANSI-16/monochrome-safe styles;
- visible contrast in Dark, Light, Solarized, and Catppuccin.

Monochrome search uses modifiers such as underline and reverse rather than
depending on color.

Only matches intersecting the materialized viewport are split into spans.
Off-screen matches cost no per-frame span work.

## Input Routing

Search input is handled before:

- global transcript selection dismissal;
- workflow panel keys;
- slash menu keys;
- mention menu keys;
- composer history;
- composer Vim input.

Global emergency actions remain available:

- `Ctrl+C` keeps its existing cancellation/exit behavior;
- terminal focus, resize, suspend, and shutdown paths remain unchanged.

`Esc` closes search before it:

- dismisses mouse selection;
- cancels a running turn;
- backtracks an idle turn;
- closes the workflow panel.

This requires a focused search-input handler rather than embedding search
logic in `vim.rs`.

Shortcut registration adds semantic actions for:

- open search;
- next match;
- previous match.

The future `keybindings.json` project may replace their bindings without
changing search behavior.

## Vim Integration

`vim.rs` remains responsible only for recognizing normal-mode `/`, `n`, and
`N` as search intents. It does not own the query, match list, or scrolling.

Because `VimState::handle` currently operates only on the composer textarea,
the implementation must route these keys before composer mutation and
translate them into transcript-search actions.

Rules:

- `/` opens search only in Vim normal mode;
- `/` inserts normally in Vim insert mode;
- `n` and `N` navigate only when a prior non-empty transcript search exists;
- otherwise they preserve existing Vim behavior;
- visual-mode behavior is unchanged.

## Streaming and Mutation Behavior

The search state tracks cache generation, not message revisions directly.

When transcript content changes:

- a closed search retains its query;
- an open or closed search recomputes lazily before rendering or navigation;
- a user viewing an existing active match is not auto-jumped to new tail
  matches;
- newly released streaming checkpoints become searchable;
- hidden partial lines and held tables remain absent;
- clear/replace/backtrack that removes matches clamps or clears the active
  match safely;
- history replay uses the same rendered-cache search path.

Clearing the screen resets the search query and match state because no
transcript remains.

## Selection and Copy

Search does not replace `AppState.selection`.

- Search highlights and selection can coexist.
- Mouse drag behavior is unchanged.
- `extract_text` continues to use only the selection.
- Jumping between matches does not stage clipboard content.
- Closing search preserves selection and scroll position.
- Clearing messages invalidates both selection and search matches.

## Auto-Follow

Opening search alone does not disable auto-follow.

The first explicit match jump disables auto-follow. This distinction avoids
changing scroll behavior when a user opens search only to inspect the current
viewport.

While search is open and auto-follow remains enabled:

- streaming may continue to pin the viewport to the tail;
- search matches recompute lazily;
- the active match is not automatically changed.

Once a jump disables auto-follow, existing jump-to-bottom behavior remains the
only way to re-arm it.

## Performance

The feature must preserve existing transcript rendering bounds.

Required evidence:

- 10,000 cached messages can be searched without rebuilding any entry;
- one query change scans searchable logical text once;
- a steady frame with unchanged query and generation performs zero search
  scans;
- a scroll-only frame performs zero search scans;
- navigating existing matches performs zero transcript scans;
- appending one message invalidates only the query-generation result, not
  cache entries;
- only visible matches are converted into styled spans;
- search state stores coordinates and ranges, not copies of transcript
  messages.

No hard byte cap is imposed on the current loaded transcript because users
must be able to search long sessions. The implementation must:

- avoid quadratic concatenation;
- scan one cached logical line at a time;
- reuse result vectors where practical;
- avoid lowercasing the entire transcript into one allocation;
- use saturating arithmetic for visual rows and scroll offsets.

## Error Handling

Literal search has no query parse errors.

Safe fallback behavior:

- missing or unprepared cache entries are skipped;
- invalid internal byte boundaries fail closed for that candidate;
- an empty or all-cleared transcript produces `0/0`;
- a vanished active match selects the nearest following match or clears
  active state;
- terminals that cannot distinguish search colors use modifier-only styles;
- extremely narrow layouts preserve fixed chrome and may hide query content
  before overlapping other UI.

## Files

### Create `crates/orca-tui/src/transcript_search.rs`

Owns:

- smart-case literal query identity;
- search state;
- match ordering;
- active-match preservation;
- next/previous navigation;
- focused pure tests.

### Modify `crates/orca-tui/src/lib.rs`

Registers the focused module.

### Modify `crates/orca-tui/src/transcript_view.rs`

Adds:

- content generation;
- read-only logical-line search;
- visual-coordinate mapping;
- jump-offset helpers;
- cache/performance tests.

The existing prepare, wrapping, cumulative-height, viewport, and extraction
algorithms remain generic.

### Modify `crates/orca-tui/src/types.rs`

Adds search state to `AppState` and resets/reconciles it across transcript
mutation paths.

### Modify `crates/orca-tui/src/shortcuts.rs`

Registers default semantic bindings and shortcut hints.

### Modify `crates/orca-tui/src/global_actions.rs`

Dispatches the semantic open-search shortcut through `AppState`.

### Modify `crates/orca-tui/src/key_event_actions.rs`

Gives open search and active search input the required routing priority.

### Modify `crates/orca-tui/src/input_event_actions.rs`

Routes bracketed paste to the open query and preserves composer paste behavior
when search is closed.

### Modify `crates/orca-tui/src/status_key_actions.rs`

Routes Vim normal-mode `/`, `n`, and `N` before Idle, Running, and
WaitingUserInput status-specific handling.

### Modify `crates/orca-tui/src/idle_key_actions.rs`

No production change is planned. The file is included in focused regression
tests to prove search routing does not alter ordinary composer behavior.

### Modify `crates/orca-tui/src/ui.rs`

Adds:

- fixed search row layout;
- shared textarea surface rendering;
- visible match overlay;
- active/non-active styles;
- completed-frame tests.

### Modify `crates/orca-tui/src/selection.rs`

Generalizes the existing display-column style overlay so search and mouse
selection share the proven span-splitting behavior.

### Modify `crates/orca-tui/src/theme.rs`

Adds capability-safe search styles.

### Modify `crates/orca-tui/src/app.rs`

Routes bracketed paste to the open search query before composer paste handling
and adds event-loop integration tests.

### Modify `crates/orca-tui/src/vim.rs`

Only if a focused intent enum is required to distinguish search commands from
composer edits. Search state and scrolling do not move into this file.

## Test Matrix

### Query

- empty query;
- smart-case lowercase query;
- smart-case uppercase query;
- ASCII and Unicode text;
- non-overlapping repeated matches;
- no invalid UTF-8 slicing.

### Cache Mapping

- one logical line;
- styled-span boundary;
- hard line boundary;
- message boundary;
- soft-wrap boundary;
- CJK display columns;
- combining text;
- emoji and zero-width controls;
- coordinates above `u16::MAX`;
- missing cache entries.

### State

- open/close without scroll mutation;
- next/previous wraparound;
- first match at or below viewport;
- preserving active match after unrelated append;
- nearest following match after removal;
- clear/replace/truncate/retain/backtrack;
- no auto-follow change until explicit jump;
- jump disables auto-follow;
- no scan on steady or scroll-only frames.

### Input

- `Ctrl+F`;
- `Enter` and `Ctrl+G`;
- `Shift+Enter` and `Ctrl+Shift+G`;
- `Esc` priority over selection/backtrack/cancel;
- `Ctrl+U`;
- bracketed paste updates only the search query;
- Vim normal `/`, `n`, and `N`;
- Vim insert `/` remains text;
- slash/mention menus do not consume search input;
- `Ctrl+C` remains global.

### Rendering

- search row at Idle, Running, and WaitingUserInput;
- composer and status remain visible;
- hardware cursor is on the search query cell;
- active and inactive match styles;
- monochrome modifier fallback;
- mouse selection wins over search;
- narrow terminal geometry;
- completed `TestBackend` jump frames.

### Performance

- 10,000-message initial scan;
- unchanged query/generation zero scan;
- scroll-only zero scan;
- navigation zero scan;
- one appended message causes one result recomputation without cache rebuild;
- off-screen matches produce no overlay span work.

## Delivery Gates

Before delivery:

```sh
cargo test -p orca-tui transcript_search --lib
cargo test -p orca-tui search_match --lib
cargo test -p orca-tui search_shortcut --lib
cargo test -p orca-tui search_frame --lib
cargo test -p orca-tui transcript_view --lib
cargo test -p orca-tui ui::tests --lib
cargo test -p orca-tui app::tests --lib
cargo test -p orca-tui -- --test-threads=1
cargo test --workspace --all-targets -- --test-threads=1
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

If an unchanged external process-cleanup timing test flakes:

1. prove its source blob is unchanged from this sub-project baseline;
2. run the exact test serialized five times;
3. rerun the workspace with only proven flaky tests skipped;
4. do not change unrelated process code.

Request independent specification and quality reviews. Fix every Critical or
Important finding before delivery.

Every commit must end exactly once with:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

After all gates pass:

```sh
git push origin feature/tui-syntax-highlighting
local_sha=$(git rev-parse HEAD)
remote_sha=$(git ls-remote --heads origin feature/tui-syntax-highlighting | awk '{print $1}')
test -n "$remote_sha"
test "$local_sha" = "$remote_sha"
```

Keep the branch and worktree for the remaining P2 roadmap items.
