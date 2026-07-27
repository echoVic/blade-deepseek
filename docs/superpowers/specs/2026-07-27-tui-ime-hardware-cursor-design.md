# TUI IME Hardware Cursor Design

## Objective

Expose the real terminal cursor at the exact visible insertion cell of Orca's
editable text area.

The current TUI paints a reversed software cursor through `tui-textarea`, but
never calls `Frame::set_cursor_position`. Ratatui therefore hides the hardware
cursor after each frame. Chinese and Japanese IME candidate windows, terminal
accessibility tools, and screen readers cannot discover the insertion point.

This change keeps the existing reversed software cursor and places the real
terminal cursor on the same cell.

## Scope

This sub-project includes:

- Hardware cursor positioning for the main composer.
- Hardware cursor positioning for the API-key setup input.
- Exact handling of Orca's custom soft wrapping and composer scrolling.
- Display-width-aware columns for CJK, emoji, and zero-width code points.
- Cursor hiding on non-editable pages and modal overlays.
- Pure coordinate tests and `TestBackend` integration tests.

It does not include:

- Cursor shape or blink-style configuration.
- A custom draw loop that de-duplicates cursor escape sequences.
- Terminal capability probing.
- IME protocol handling beyond exposing the native cursor.
- Changes to Vim command semantics.
- Changes to the existing reversed software cursor.

## Branch and Delivery

The implementation is added to `feature/tui-syntax-highlighting`, after the
syntax-highlighting work already pushed on that branch.

The IME change receives its own design, implementation plan, and commits. The
updated feature branch is pushed after its completion audit.

## Reference Behavior

Ratatui 0.29 defines the required lifecycle:

- Calling `Frame::set_cursor_position` makes the native cursor visible after
  the frame is flushed and moves it to the requested cell.
- Omitting the call hides the cursor for that frame.

Codex follows the same ownership model: a renderable computes an optional
cursor position and the outer frame calls `set_cursor_position`.

Orca cannot use `TextArea::cursor()` plus an area offset directly. The main
composer does not render the upstream `tui-textarea` widget. It owns custom
word/hard wrapping, visible-window selection, and selection styling in
`ui.rs`. The native cursor must be derived from that exact layout.

## Cursor Visibility

### Main composer

Show the native cursor when the main composer is visible and no full-screen
modal owns the foreground:

- `Idle`
- `Running`
- `Compacting`
- `WaitingUserInput`
- other conversation states where `composer_visible` is true

Hide it when:

- `WaitingApproval`
- `SessionPicker`
- non-input setup pages
- the shortcuts overlay is open

Slash and mention popups do not hide the cursor because they render above the
composer and the composer remains the active input surface.

Vim Insert, Normal, and Visual modes all keep the native cursor at the
software-cursor cell. This preserves the current mode-colored reversed block
while giving IMEs and accessibility tools a real position.

### Setup

Only setup step 1, the API-key input step, shows the native cursor.

Setup welcome and completion pages omit it.

## Shared Textarea Layout

### Layout result

Replace the current tuple returned by `composer_visual_lines` with an internal
layout object:

```rust
struct TextareaVisualLayout {
    lines: Vec<Line<'static>>,
    cursor_visual_row: usize,
    cursor_display_col: usize,
    alignment: Alignment,
}
```

The layout is the single source of truth for:

- rendered visual lines,
- the software-cursor row,
- the native-cursor row,
- the native-cursor display column.

No second wrapping pass may independently approximate the cursor.

### Display column

`tui-textarea` cursor columns are character indices. Terminal cursor columns
are display cells.

For the wrap range containing the logical cursor, calculate the width of the
characters from `range.start` to `cursor_col` with Unicode display-width
semantics. The resulting value is `cursor_display_col`.

When `TextArea::mask_char` is set, wrapping, rendered text, and display-column
calculation use a masked display line containing one mask character per
original source character. Cursor and selection indices remain indexes into
the original logical line.

Consequences:

- ASCII advances one cell.
- CJK characters advance two cells.
- emoji use their terminal display width.
- combining and zero-width code points do not advance the hardware cursor.
- the cursor points to the leading cell of the current character.

### Exact-width line ending

When the cursor is at the logical end of a line and the preceding visual row
fills the inner width exactly, the insertion point is the first cell of the
next terminal row.

The layout must add a synthetic empty cursor row:

- `cursor_visual_row` becomes the new row.
- `cursor_display_col` is zero.
- the software cursor is rendered on that row.
- composer height and scrolling include that row.

Clamping the cursor to the last cell is not allowed because it would report the
wrong insertion point to the IME.

### Empty textarea

An empty textarea produces one visual row with:

- software cursor at column zero,
- native cursor at column zero,
- placeholder text after the cursor.

## Visible Window and Screen Coordinates

Create a pure helper that converts a visual layout and inner rectangle into a
visible cursor:

```rust
fn visible_textarea_cursor(
    layout: &TextareaVisualLayout,
    inner: Rect,
) -> Option<Position>
```

It uses the same scroll policy as rendering:

```text
start = 0                                      if all rows fit
start = cursor_visual_row + 1 - inner.height   if cursor is below viewport
start = 0                                      otherwise
```

The screen coordinate is:

```text
x = inner.x + cursor_display_col
y = inner.y + cursor_visual_row - start
```

Return `None` if:

- `inner` is empty,
- the cursor row is outside the visible slice,
- the display column is outside `inner.width`,
- coordinate conversion would overflow `u16`.

Rendering and coordinate calculation consume the same `start`.

## Rendering API

Refactor the custom input renderer into one shared function:

```rust
fn render_textarea_surface(
    frame: &mut Frame,
    area: Rect,
    textarea: &TextArea,
    copy_notice: Option<CopyNotice>,
    theme: &Theme,
    show_hardware_cursor: bool,
)
```

Responsibilities:

1. Render the optional block and obtain `inner`.
2. Render the copy notice when supplied.
3. Build one `TextareaVisualLayout`.
4. Compute the visible row slice.
5. Render the paragraph.
6. If enabled and visible, call:

```rust
frame.set_cursor_position(position);
```

The existing `render_input` becomes a small main-composer adapter that supplies
the current copy notice and visibility flag.

The top-level `render` decides the visibility flag before rendering the input.
It must include modal state such as `show_shortcuts`; setting the cursor and
then drawing a modal is not sufficient because the frame would still expose
the cursor.

Setup step 1 calls the same renderer without a copy notice. This avoids
depending on `tui-textarea`'s private viewport state and keeps the hardware and
software cursors aligned.

## Masked Setup Input

The shared custom line renderer must honor `TextArea::mask_char`.

For a masked textarea:

- every visible source character renders as the mask character,
- wrapping uses the masked display text,
- display-column calculation uses the rendered mask character width,
- cursor logical movement still follows the original character index,
- the underlying API key is never exposed in the buffer or tests.

The API-key mask is `*`, so each character occupies one cell.

Main composer behavior is unchanged because it has no mask.

## Alignment

The main composer currently uses left alignment. Hardware-cursor positioning is
defined for left alignment in this sub-project.

If a future caller supplies centered or right-aligned text, the helper returns
`None` rather than reporting an incorrect coordinate. Supporting non-left
alignment requires adding the paragraph's alignment offset to the cursor and
belongs in a later change.

## Error and Edge Handling

- Zero-width inner areas render no native cursor.
- Cursor coordinates are checked before conversion to `u16`.
- A cursor beyond the current logical line is clamped by `tui-textarea`; the
  layout trusts `TextArea::cursor()`.
- If no wrap range contains the cursor, native cursor positioning is omitted
  rather than guessed.
- The frame API owns visibility. No direct `Show`, `Hide`, or `MoveTo` escape
  sequences are emitted.
- Terminal cursor blink may restart on animated frames because ratatui emits
  cursor commands on every draw. Cursor command de-duplication is explicitly
  deferred to the terminal-capability project.

## Testing

Implementation follows test-driven development.

### Pure layout tests

- Empty textarea: row 0, column 0.
- ASCII insertion point.
- CJK text before the cursor produces two-cell increments.
- Emoji and combining-mark behavior uses display width.
- Multiple logical lines map to different visual rows.
- Word wrapping maps the cursor to the correct wrapped row.
- Hard wrapping of a long token maps correctly.
- Cursor exactly at a wrap boundary belongs to the next row.
- Cursor after a row that exactly fills the width creates a synthetic cursor
  row.
- A composer taller than its viewport scrolls the cursor into the last visible
  row.
- Non-zero rectangle origins and border insets are included exactly.
- Zero-sized and unsupported-alignment surfaces return no native cursor.
- Masked text uses mask width and never renders the secret.

### Frame integration tests

Use `ratatui::backend::TestBackend` and inspect its cursor state:

- Idle composer draw sets the backend cursor to the software-cursor cell.
- Running, compacting, and waiting-user-input draws set it.
- Waiting-approval draw does not expose the composer cursor.
- Session picker draw omits it.
- Shortcuts overlay omits it.
- Setup step 1 sets the cursor inside the API-key input.
- Setup steps 0 and 2 omit it.
- CJK and wrapped composer integration positions match pure layout results.

Tests must verify `Frame::set_cursor_position` through the completed terminal
draw, not only the pure coordinate helper.

`TestBackend::assert_cursor_position` verifies visible cursor placement. It
does not expose its visibility bit, so hidden-state tests use a minimal
recording backend that delegates buffer operations and records calls to
`show_cursor`, `hide_cursor`, and `set_cursor_position`. A completed modal draw
must record `hide_cursor` and no cursor-position update for that frame.

## Validation

Run focused checks:

```sh
cargo test -p orca-tui hardware_cursor --lib
cargo test -p orca-tui composer_cursor --lib
cargo test -p orca-tui setup_cursor --lib
```

Then:

```sh
cargo test -p orca-tui
cargo fmt --all -- --check
git diff --check
```

Run the workspace suite and classify any known baseline timing races using the
existing base/feature evidence rather than changing unrelated process code.

## Completion Criteria

The sub-project is complete only when:

1. A real terminal cursor is placed at the exact visible software-cursor cell
   for the main composer.
2. CJK, emoji, zero-width characters, wrapping, scrolling, and borders are
   reflected in the coordinate.
3. The API-key setup input exposes a correct masked cursor position without
   revealing the key.
4. The cursor is omitted on non-editable or modal pages.
5. Existing reversed software-cursor rendering remains.
6. Pure and completed-frame tests directly cover visibility and coordinates.
7. Existing TUI tests, formatting, and diff checks pass.
