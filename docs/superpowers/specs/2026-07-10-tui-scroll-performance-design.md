# TUI Scroll Performance Design

## Goal

Keep Orca's current fullscreen, in-application transcript scrolling behavior while making
wheel, trackpad, and keyboard scrolling responsive for both short and long sessions. The
implementation must remain responsive while model output is streaming and must be verified
with the real DeepSeek API in a real terminal.

## Constraints

- Do not restore the removed inline viewport/native scrollback architecture. Users must still
  be able to reach all retained history through the application's own scroll controls.
- Preserve the rendered transcript, Markdown styling, tool expansion behavior, auto-follow
  behavior, composer, approval overlays, and workflow/agent panels.
- Avoid an unbounded render cache. Cache entries correspond one-to-one with retained messages
  and are removed when the transcript is truncated or replaced.
- A terminal width or theme change may invalidate layout data, but scrolling alone must not
  reparse Markdown or rebuild off-screen messages.
- Long transcripts must not saturate at 65,535 visual lines.

## Root Causes

The event loop currently reads one terminal event and takes an early `continue` from mouse and
keyboard handling before reaching `Terminal::draw`. A continuous trackpad or key-repeat stream
can therefore starve both drawing and runtime-event projection. Independently, every draw
rebuilds `Vec<Line>` for the entire transcript, reparses every assistant Markdown message,
hashes all message contents for the height cache, and passes the complete transcript to
`Paragraph::scroll`, whose wrapper walks from the beginning to the requested offset.

The existing `flushed_count` model describes a removed native-scrollback implementation and is
not a suitable fix for the current fullscreen UI. It can remain as transcript compatibility
state, but performance must not depend on advancing it.

## Architecture

### Frame scheduling

Add a small frame scheduler with an explicit dirty bit and frame deadline. Terminal events are
read in bounded batches, adjacent wheel events are coalesced into a single signed line delta,
runtime events are drained every loop iteration, and neither input source can bypass the draw
decision. Dirty frames render at a maximum cadence of approximately 60 FPS. Running animations
use a slower independent tick and mark the frame dirty only when their visible state advances.
When no state or animation changes, the terminal is not redrawn.

The scheduler is pure state-management code so tests can prove that a continuous event stream
still reaches frame deadlines, multiple dirty notifications collapse into one draw, and idle
iterations do not draw.

### Message render cache

Add a transcript render cache keyed by stable message identity/revision, terminal width, and
theme identity. Each cached entry owns the styled logical `Line` values produced for one
message plus its wrapped visual height. Cached entries for unchanged messages are reused without
rehashing their full text. Mutations invalidate only the affected entry; append-only streaming
normally invalidates only the final assistant/reasoning/tool message. Transcript replacement or
truncation reconciles the cache by dropping entries beyond the retained message list.

Message revisions live alongside `AppState` rather than being inferred by hashing message text.
Every state mutation that changes a message increments that message's revision. Direct test and
startup population is handled conservatively by comparing message discriminant and lightweight
structural metadata, rebuilding entries when no tracked revision is available.

### Viewport virtualization

The cache maintains each message's wrapped visual height and a cumulative-height index using
`usize`. Given the resolved scroll offset and viewport height, it locates the first visible
message, renders only intersecting messages plus one-message overscan, and applies a small local
scroll offset to the first rendered message. The final `Paragraph` therefore contains a bounded
window rather than the entire transcript.

Auto-follow still resolves to `total_visual_lines - viewport_height`; manual scrolling keeps the
current offset clamped to the new total. Public scroll fields and operations move from `u16` to
`usize`, converting to `u16` only at ratatui's final local `Paragraph::scroll` boundary.

## Correctness And Performance Tests

- A scheduler test feeds more terminal-event batches than a frame interval and proves a draw is
  due without requiring the input queue to become empty.
- A wheel-coalescing test proves signed deltas and non-wheel ordering are preserved.
- Cache tests instrument message rendering and prove a second scroll-only frame performs zero
  message rebuilds and zero Markdown parses.
- A streaming test proves appending to the last assistant message rebuilds only that message.
- A width-change test proves layout entries are recomputed and scrolling remains clamped.
- A virtualization test uses thousands of messages and proves the rendered message window is
  bounded by the visible region rather than transcript length.
- A long-transcript test proves offsets above 65,535 lines remain reachable.
- Existing Orca TUI tests, the full workspace test suite, formatting, and compilation remain
  green.

## Real Environment Verification

Build the debug binary and launch the actual TUI in a PTY using the existing authenticated Orca
configuration or `DEEPSEEK_API_KEY` without printing the secret. Submit a prompt that produces a
long Markdown response and at least one streaming turn. During and after streaming, exercise
trackpad/wheel-equivalent mouse events plus PageUp/PageDown and verify:

- input produces visible movement while events are still arriving;
- model deltas and the activity indicator continue to update during scrolling;
- scrolling reaches old history and returns to auto-follow at the bottom;
- no rendering corruption, panic, API error, or terminal cleanup failure occurs;
- recorded frame timing and cache counters show bounded visible-window work on scroll-only frames.

The real API check is required in addition to automated tests; a headless `exec` response alone
does not verify the TUI scrolling path.
