# TUI Vim Insert Escape Remap Design

## Goal

Add an optional, configurable two-character Insert-mode escape sequence to the
TUI Vim composer. The initial supported configuration is:

```toml
vim_mode = true
vim_insert_escape = "jj"
```

Typing the configured sequence in Vim Insert mode switches to Normal mode
without inserting either character. The feature is disabled when the setting is
omitted, so every existing Vim and non-Vim input path remains unchanged by
default.

This is the second, independent half of the P2 Vim enhancement roadmap item.
It does not expand the Normal-mode command language and does not create a
general keybindings system.

## Product Decisions

### Default and scope

- `vim_insert_escape` defaults to unset.
- The setting has an effect only when `vim_mode = true`.
- The setting is read at TUI startup. Hot reload is out of scope.
- The sequence is active only in Vim Insert mode.
- Normal, Visual, setup, transcript-search, approval, and session-picker input
  do not interpret the sequence.
- Headless, JSONL, server, workflow-worker, and child-agent behavior remains
  unchanged even though their shared `RunConfig` carries the value.

### Sequence shape

The configured value must contain exactly two Unicode scalar values.

Each character must:

- not be whitespace;
- not be a control character;
- be representable by a normal unmodified `Key::Char` input.

The two characters may be equal (`"jj"`) or distinct (`"jk"`). Invalid values
are rejected by effective-config deserialization with an actionable warning
instead of being silently truncated or accepted.

The matching window is a fixed 500ms. It is intentionally not configurable:
this keeps the configuration surface small while ensuring a single trailing
first character never remains invisible indefinitely. The first sequence
character is held until the second input, an ownership boundary, or the
deadline. The implementation never injects an insert/delete pair into textarea
history.

## Configuration Model

Add a strong type in `orca-core`:

```rust
pub struct VimInsertEscapeSequence {
    first: char,
    second: char,
}
```

The type:

- deserializes from one TOML string;
- validates the exact two-character contract;
- exposes `first()`, `second()`, and `as_str()` or an equivalent display
  method;
- implements `Clone`, `Debug`, `Eq`, and `PartialEq`.

Invalid values use the repository's existing layered-config error contract:
deserialization rejects the effective merged config, the loader prints its
existing actionable `config parse error` warning including the field error,
and returns `FileConfig::default()`. This sub-project does not redesign global
configuration error recovery.

`FileConfig` and `RunConfig` receive:

```rust
pub vim_insert_escape: Option<VimInsertEscapeSequence>
```

`format_config_show` prints:

```text
vim_insert_escape = "<unset>"
```

or the configured two-character value. The value is not sensitive. The output
must escape quotes, backslashes, and TOML control syntax rather than
interpolating raw characters into a quoted line.

All `RunConfig` constructors explicitly propagate the effective file setting
or use `None` in test/runtime fixtures. There is no environment variable,
command-line flag, legacy alias, nested `[vim]` table, or migration.

## Insert Prefix State

`VimState` owns a separate pending Insert-mode prefix:

```rust
struct PendingInsertEscape {
    character: char,
    deadline: Instant,
}
```

This state is independent of:

- `VimCommandParser` Normal-mode count/operator/register prefixes;
- registers;
- dot-repeat state;
- textarea yank state.

`VimState::new` accepts both Vim enablement and the optional configured
sequence. A constructor or small options value may be used to keep call sites
clear.

### First character

For an unmodified Insert-mode `Key::Char` equal to the configured first
character:

1. do not mutate `TextArea`;
2. store the character and `now + 500ms` deadline;
3. report the input as handled so the frame can refresh cursor/block state.

The held character creates no textarea undo entry.

### Exact match

When the next unmodified Insert-mode character equals the configured second
character:

1. clear `pending_insert_escape`;
2. switch to Normal mode;
3. cancel any textarea selection;
4. do not insert either character;
5. leave textarea undo/redo history unchanged.

The remap is not a repeatable Vim change and does not update registers or
dot-repeat state.

### Mismatch and overlap

When the next input does not complete the sequence:

1. insert the held first character exactly once at the current textarea cursor;
2. clear the held prefix;
3. process the current input normally.

If the current input is another copy of the configured first character, it
becomes the next held prefix after the previous one is inserted. This supports
overlap for mappings such as `"jk"`:

```text
j j k  => inserts one "j", then exits Insert mode
```

For `"jj"`, the second `j` always completes the sequence:

```text
j j    => exits Insert mode with no inserted text
```

Each press is evaluated with the current `Instant` when its event is handled.
Before processing the current key, `VimState` flushes an already-expired held
prefix. Events drained in one intake batch retain their order; a second
character completes the mapping only if its handler runs no later than the
stored deadline. The loop-level expiry check and the per-key check share the
same inclusive deadline rule, so batching cannot create two interpretations.

Modified characters (`Ctrl`, `Alt`, or other non-text modifiers), Tab,
Backspace, Delete, Enter, arrows, function keys, and physical Esc cannot
complete the sequence. They first flush the held character, then preserve
their existing behavior. In the integrated TUI, Idle Esc remains the
higher-priority Backtrack shortcut and Running Esc remains Interrupt after the
held character is flushed. At the `VimState` unit boundary, an Esc that reaches
Insert handling still performs the existing Insert-to-Normal transition.

Key release events remain filtered by the existing preflight layer and never
start, complete, or flush the sequence.

### Deadline

The main TUI loop already wakes at most every 16ms through
`FrameScheduler::poll_timeout`, even without animation. At the top of each
iteration it calls:

```rust
vim_state.flush_expired_insert_escape(now, textarea)
```

If the 500ms deadline has elapsed, the method inserts the held character once,
clears the pending state, and returns `true`. The caller resets history
navigation, refreshes slash/mention input state, and marks the scheduler dirty.
No thread, channel, alarm, additional event type, or blocking sleep is added.
The visible insertion may occur up to one frame interval after the deadline.

## Input Ownership Boundaries

A held Insert prefix is user text. It must never be silently dropped when
another layer consumes input.

Introduce one explicit operation:

```rust
pub(crate) fn flush_pending_insert_escape(
    &mut self,
    textarea: &mut TextArea<'_>,
) -> bool
```

It inserts the held character once, clears the pending state, and returns
whether text changed.

Existing `cancel_pending_command` continues to reset only Normal/Visual command
parser state. It must not silently discard Insert text.

Before a higher-priority owner consumes an event, it flushes the held Insert
prefix while it still owns the current textarea cursor:

- global shortcuts and active transcript search;
- shortcut overlay and workflow panel;
- approval-mode cycling;
- slash and mention menus;
- idle/running submit, queue, history, newline, and navigation shortcuts;
- Tab completion or direct textarea Tab input;
- paste;
- mouse press/drag/release and synthetic Enter;
- wheel/scroll input;
- setup, picker, approval, Compacting, and runtime status transitions;
- composer replacement, queue restoration, backtrack restoration, and
  submission rejection.

Ordering is observable:

- `j` then paste produces `j` followed by pasted text;
- `j` then mouse click inserts `j` at the old cursor before the click moves the
  cursor;
- `j` then submit includes `j` in the submitted composer;
- `j` then runtime transition leaves `j` in the composer before the new status
  takes ownership.

If the remap is disabled or Vim is not in Insert mode, flushing is a no-op.

## Paste and IME Behavior

`Event::Paste` is never interpreted as a remap sequence, even when its payload
is `"jj"`. Paste remains an atomic paste and preserves the existing large-paste
placeholder behavior.

The remap observes only complete key events produced by the existing qwertty to
crossterm adapter. It does not inspect terminal escape bytes, IME composition
preedit text, or grapheme internals.

Unicode configured characters are accepted when the terminal emits them as
unmodified `Key::Char` events. Combining sequences are not accepted as one
configured character because the configuration contract counts Unicode scalar
values, not grapheme clusters.

The feature does not change hardware cursor positioning, candidate-window
placement, key release filtering, bracketed paste, or terminal keyboard
protocol setup.

## Undo, Redo, and History

- A successful escape sequence creates no textarea edit.
- A mismatched held prefix is inserted exactly once and participates in normal
  textarea undo history.
- The current input after a mismatch keeps its existing history behavior.
- The implementation never inserts then deletes the first character and never
  calls `undo` internally.
- Integrated Idle/Running Esc flushes the character, then retains existing
  Backtrack/Interrupt priority. An Esc delivered directly to `VimState` flushes
  then exits Insert mode. A later `u` can undo the inserted character normally.
- Successful remap does not update dot-repeat state.

This preserves existing textarea history without private dependency access or
widget reconstruction.

## Reset and Lifecycle Semantics

The held Insert prefix clears only by:

- successful sequence completion;
- deadline expiry, which flushes it into the textarea;
- flushing it into the textarea;
- constructing a fresh `VimState`.

Mode changes that are initiated after a held prefix flush the prefix first.
`reset_insert` and composer replacement call the flush operation before
replacing or resetting the textarea. No reset path may drop the held
character.

Registers and dot-repeat state retain the command-core lifecycle contract.

## Files

### Create

- none.

### Modify

- `crates/orca-core/src/config/mod.rs`
  - define `VimInsertEscapeSequence`;
  - add it to `RunConfig`;
  - show the effective value in `/config show`;
  - validation and formatting tests.
- `crates/orca-core/src/config/file.rs`
  - deserialize and propagate the optional top-level setting;
  - default/valid/invalid TOML tests.
- `src/cli.rs`
  - propagate the setting through every production `RunConfig` constructor.
- `crates/orca-tui/src/vim.rs`
  - own the configured sequence and pending Insert prefix;
  - exact match, mismatch/overlap, flush, history, Unicode, and disabled-mode
    tests.
- `crates/orca-tui/src/composer_input_actions.rs`
  - flush before Tab bypasses `VimState`.
- `crates/orca-tui/src/key_event_actions.rs`
  - receive the textarea and flush before preflight-owned keys.
- `crates/orca-tui/src/idle_key_actions.rs`
  - flush before idle/menu/panel owners consume a key.
- `crates/orca-tui/src/idle_navigation_actions.rs`
  - preserve flush ordering for `ExpandToolOutput`.
- `crates/orca-tui/src/queued_input_actions.rs`
  - flush before running/menu/queue owners consume a key.
- `crates/orca-tui/src/status_key_actions.rs`
  - flush before status-specific owners consume a key.
- `crates/orca-tui/src/runtime_event_actions.rs`
  - flush before status changes or composer replacement.
- `crates/orca-tui/src/app.rs`
  - construct `VimState` with the effective sequence;
  - flush an expired prefix at the top of the loop and refresh input state;
  - flush before paste, mouse, wheel, and synthetic Enter.
- `crates/orca-tui/src/input_event_actions.rs`
  - pass `VimState` and theme where paste must flush before insertion, or keep
    the flush at the app ownership boundary with focused ordering tests.
- all test/runtime files that construct `RunConfig`
  - set `vim_insert_escape: None` unless the test explicitly exercises it.

No manifest, lockfile, shortcut registry, protocol, history format, renderer,
or terminal capability changes are required.

## Test Strategy

### Configuration

- omitted setting produces `None`;
- `"jj"`, `"jk"`, and two Unicode scalar values parse;
- empty, one-character, three-character, whitespace, newline, and control
  values reject the effective config with the existing actionable warning and
  default fallback;
- `/config show` prints configured and unset values;
- every production `RunConfig` constructor propagates the file value.

### Vim Insert behavior

- disabled mapping inserts `jj` normally;
- Vim-disabled mode inserts configured characters normally;
- exact configured pair exits Insert with no text or undo entry;
- a lone first character appears after 500ms with at most one frame of jitter;
- keys processed in one intake batch obey event order and the same inclusive
  deadline rule as the loop expiry check;
- mismatch inserts both characters in order;
- overlap preserves the unmatched prefix;
- Unicode pair matches complete `Key::Char` events;
- modified characters cannot complete the pair;
- physical Esc flushes one prefix then exits;
- Backspace/Delete/Enter/arrows flush first and preserve behavior;
- successful remap does not change registers or dot-repeat state;
- mismatch creates normal, non-duplicated undo history.

### Ownership boundaries

- submit includes a held prefix;
- queue submit and composer restore preserve it;
- Tab, paste, mouse, wheel, search, menus, workflow panel, approval, setup,
  Compacting, and runtime status transitions flush once;
- mouse ordering inserts at the pre-click cursor;
- paste payload `"jj"` remains pasted text;
- release events never affect prefix state.

### Regression

- all existing Vim command-core tests pass;
- Vim-disabled composer input remains unchanged;
- mention completion, queueing, hardware cursor, IME paste, undo/redo, search,
  approval, and workflow notification tests pass.

## Verification

Focused:

```sh
cargo test -p orca-core vim_insert_escape --lib -- --test-threads=1
cargo test -p orca-tui vim_insert_escape --lib -- --test-threads=1
cargo test -p orca-tui pending_insert_escape --lib -- --test-threads=1
cargo test -p orca-tui expired_insert_escape --lib -- --test-threads=1
cargo test -p orca-tui composer_input_actions::tests:: --lib -- --test-threads=1
cargo test -p orca-tui runtime_event_actions::tests:: --lib -- --test-threads=1
```

Package and workspace:

```sh
cargo test -p orca-tui -- --test-threads=1
cargo test -p orca-core -- --test-threads=1
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
cargo test --workspace --all-targets -- --test-threads=1
```

The two unchanged macOS external-tool timeout tests may be skipped only after
proving their source blob matches the pushed baseline, following the
command-core plan.

## Acceptance Criteria

1. The setting is absent by default; invalid values reject effective-config
   deserialization and trigger the existing warning/default fallback.
2. Exact configured Insert-mode pairs exit to Normal without inserting text or
   creating undo history.
3. Mismatches and overlapping prefixes preserve every typed character exactly
   once and in order.
4. Higher-priority key, paste, mouse, scroll, status, and composer-replacement
   owners flush a held prefix before taking ownership.
5. Paste and IME input are never parsed as remap key sequences.
6. Registers, dot-repeat, Normal/Visual commands, search, queue, mention,
   hardware cursor, undo/redo, and Vim-disabled behavior remain unchanged.
7. The matching deadline is the fixed 500ms constant; no configurable timeout,
   hot reload, CLI flag, general keybindings system, insert-session repeat,
   manifest, or protocol change is introduced.
8. The sub-project is independently reviewed, committed with the required TRAE
   trailer, pushed to `feature/tui-syntax-highlighting`, and remote SHA equals
   local `HEAD`.
