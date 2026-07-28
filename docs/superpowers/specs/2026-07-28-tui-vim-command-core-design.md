# TUI Vim Command Core Design

## Goal

Strengthen the existing optional Vim composer mode with a composable normal-mode
command core:

- numeric counts for supported motions and changes;
- `dd`, `yy`, `gg`, and `G`;
- unnamed and lowercase named registers;
- `.` repeat for completed atomic normal-mode changes;
- deterministic prefix cancellation across modes and composer replacement.

This is the first half of the P2 Vim enhancement roadmap item. The configurable
insert-mode `jj -> Esc` remap is intentionally a separate follow-up because it
changes the configuration and input-timing surface rather than the normal-mode
editing language.

## Current Behavior

`crates/orca-tui/src/vim.rs` currently owns:

- Insert, Normal, and Visual modes;
- single-key motions `h/j/k/l/w/e/b/^/$`;
- mode entry with `i/a/A/o/O/v`;
- visual `y/d/c`;
- normal `x/D/C/p/u/Ctrl+R`;
- transcript-search intents `/`, `n`, and `N`.

The implementation is a single `match` over one `Input`. It has no state for:

- a pending count;
- a pending operator or `g` prefix;
- a selected register;
- a replayable last change.

Composer input first passes through global/status shortcuts and mention/menu
routing. Only unclaimed composer keys reach `VimState::handle`. This routing
must remain unchanged: Vim parsing never steals approvals, transcript-search
keys, queue submission, mention acceptance, or global cancellation.

`tui-textarea` 0.7 exposes the APIs needed for this feature:

- `cursor`, `lines`, `selection_range`;
- `delete_str`, `delete_line_by_end`, `cut`, `copy`;
- `yank_text`, `set_yank_text`, `paste`;
- `insert_str`, `move_cursor`, `undo`, and `redo`.

The command core can therefore preserve the existing textarea history instead
of reconstructing the widget from copied line vectors.

## Scope

### Counts

Counts are accepted in Normal mode before supported commands:

```text
3h  2j  4w  5x  2dd  2yy  3p  3G  4gg
```

Rules:

- digits `1` through `9` start a count;
- `0` appends only after a count has started;
- bare `0` is a line-head motion;
- counts saturate at `9999`;
- omitted count means `1`;
- counts apply to `h/j/k/l/w/e/b`, `x`, `dd`, `yy`, `p`, `gg`, and `G`;
- count prefixes before `D` or `C` are consumed, but those existing commands
  execute once; counted delete-to-end and insert-session replay are out of
  scope;
- counts do not apply to transcript search `/`, `n`, or `N`;
- `u` and `Ctrl+R` retain their existing one-step behavior.

Motion counts stop naturally at textarea boundaries and still count as handled
input even when no cursor movement occurs.

### `dd`

`dd` deletes whole logical composer lines.

- `2dd` deletes the current line and the following line.
- The count clamps to the remaining line count.
- Deleting the only line leaves one empty line.
- Deleting through the final line removes the preceding separator rather than
  leaving an extra empty line.
- The cursor lands at column zero of the next surviving line, or at the end of
  the previous line when deletion reaches the buffer end.
- One `dd` command produces one textarea edit-history entry.
- The yanked register value contains the deleted lines joined by `\n` and is
  marked linewise independently of whether the deleted range reached EOF.

The implementation performs one selection plus one `cut`:

1. move to the current line head;
2. start selection;
3. when a following line survives, move to its head so the selected range
   includes each deleted newline;
4. when deletion reaches EOF, select from the preceding line end through the
   buffer end, while separately preserving the requested deleted-line text for
   the register;
5. call `cut` exactly once.

It does not replace `TextArea`, so `u` restores the deletion as one change.

### `yy`

`yy` copies whole logical composer lines without modifying text.

- `2yy` copies the current line and the following line.
- The count clamps to the remaining line count.
- The register value contains copied lines joined by `\n`.
- The value is linewise even at EOF.
- The cursor and textarea undo history are unchanged.
- `yy` updates registers but never becomes the dot-repeat target.

`yy` is a dedicated atomic line command. This task still does not implement the
general `y{motion}` operator language.

### `gg` and `G`

- `gg` moves to the first line, column zero.
- `4gg` moves to one-based line 4, column zero.
- bare `G` moves to the final line, preserving the dependency's bottom-motion
  column behavior.
- `4G` moves to one-based line 4, column zero.
- Out-of-range counts clamp to the final line.

The implementation uses `CursorMove::Top`, `CursorMove::Bottom`, and bounded
`Down` operations. It does not cast a `usize` row into the dependency's
`CursorMove::Jump(u16, u16)`, so large composers do not wrap row indices.

### Registers

Support:

- the unnamed register;
- lowercase named registers `a` through `z`;
- explicit unnamed selection with `""`.

Examples:

```text
"add
"ap
"ayy
""p
```

Uppercase append registers, numbered delete registers, the small-delete
register, black-hole register, expression register, system clipboard
registers, and macros are out of scope.

Register selection is one-shot:

1. `"` enters register-prefix state;
2. `a-z` or `"` selects the next register;
3. the selection applies to one complete yank/delete/paste command;
4. after execution, cancellation, invalid input, or mode change, selection
   returns to unnamed.

Writes always update the unnamed register. When a named register was selected,
the same value also replaces that named register.

Reads use the selected named register when present, otherwise unnamed. Reading
an empty register is a handled no-op.

Each register stores:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VimRegisterSelector {
    Unnamed,
    Named(u8),
}

enum VimRegisterKind {
    Characterwise,
    Linewise,
}

struct VimRegisterValue {
    text: String,
    kind: VimRegisterKind,
}

struct VimRegisterBank {
    unnamed: Option<VimRegisterValue>,
    named: [Option<VimRegisterValue>; 26],
}
```

`Named(0)` through `Named(25)` correspond to `a` through `z`. Parser
construction is the only place that converts a character into an index; all
bank access validates the index and returns `None` when it is out of range.

Normal `x`, `D`, and visual selections are characterwise. `dd` is linewise.
`yy` is also linewise.

`VimState` mirrors the chosen value into `TextArea::set_yank_text` immediately
before a paste. Existing `TextArea::paste` remains the characterwise insertion
primitive.

Counted characterwise paste concatenates the register text `count` times,
sets that combined value as the textarea yank, and calls `paste` once.

Linewise paste normalizes the stored register text by removing one trailing
newline if present, repeats the normalized lines `count` times separated by
`\n`, prefixes the complete payload with one `\n`, and calls
`TextArea::paste` once. Before paste, the cursor moves to the current line end.
After paste, the cursor remains at the end of the final inserted line, matching
`TextArea::paste`; this task does not add a second cursor-repositioning pass.
The single paste preserves one undo entry.

### Dot Repeat

`.` repeats the last completed atomic Normal-mode change.

Supported replay values:

```rust
enum RepeatableChange {
    DeleteChars {
        count: usize,
        register: VimRegisterSelector,
    },
    DeleteToEnd {
        register: VimRegisterSelector,
    },
    DeleteLines {
        count: usize,
        register: VimRegisterSelector,
    },
    Paste {
        count: usize,
        register: VimRegisterSelector,
    },
}
```

Rules:

- only successful text mutations replace `last_change`;
- cursor motions, yanks, mode switches, search, undo, and redo do not replace
  it;
- a failed change such as `x` at end-of-line leaves the prior repeat intact;
- `.` with no previous change is a handled no-op;
- a count before `.` multiplies the stored change count, saturating the
  effective count at `9999`; `DeleteToEnd` executes once per dot count;
- replay does not recursively replace `last_change`;
- replay uses the stored register selector, but reads that register's current
  value for paste;
- replayed delete commands write registers exactly as the original command.

Insert-session changes are not replayed in this sub-project. In particular,
plain `i...Esc`, `a...Esc`, `o...Esc`, `O...Esc`, and `C...Esc` do not become
the dot target. This avoids pretending to support Vim insert replay while
dropping backspaces, cursor moves, paste, or multiline edits.

## Parser Architecture

Create `crates/orca-tui/src/vim_command.rs`.

The parser is pure and owns no textarea:

```rust
pub(crate) struct VimCommandParser {
    count: Option<usize>,
    selected_register: Option<VimRegisterSelector>,
    pending: VimPendingPrefix,
}

pub(crate) enum VimCommandResolution {
    Pending,
    Execute(VimCommand),
    Consumed,
    Unhandled,
}

pub(crate) enum VimCommand {
    Motion {
        motion: VimMotion,
        count: usize,
    },
    DeleteChars {
        count: usize,
        register: VimRegisterSelector,
    },
    DeleteToEnd {
        register: VimRegisterSelector,
    },
    ChangeToEnd {
        register: VimRegisterSelector,
    },
    DeleteLines {
        count: usize,
        register: VimRegisterSelector,
    },
    YankLines {
        count: usize,
        register: VimRegisterSelector,
    },
    Paste {
        count: usize,
        register: VimRegisterSelector,
    },
    GotoLine {
        one_based: Option<usize>,
    },
    Repeat {
        count: usize,
    },
}
```

`VimPendingPrefix` has only:

- none;
- waiting for a register name;
- waiting for the second `d`;
- waiting for the second `y`;
- waiting for the second `g`.

`selected_register: None` means no explicit register prefix and resolves to the
unnamed register for register-aware commands. `Some(Unnamed)` represents the
explicit `""` prefix. This distinction lets motions remain valid when no
register was explicitly selected while still rejecting `"ah`.

The full command parser runs in Normal mode. Visual mode reuses only its
register-prefix methods; counts and `d/y/g` prefixes are unavailable there.

Count and register prefixes may appear in either order before a command:

```text
2"add
"a2dd
```

Digits after an operator prefix (`d2d` or `y2y`) are invalid continuations;
operator and motion counts are not multiplied in this sub-project.

The parser accepts only plain character inputs without Ctrl or Alt. Modified
keys clear pending prefixes and return `Unhandled` so existing `Ctrl+R` and
other textarea behavior remains available.

Invalid continuation behavior is fail-closed:

- `d` followed by anything except `d` clears the pending operator and consumes
  the continuation;
- `y` followed by anything except `y` clears the pending operator and consumes
  the continuation;
- `g` followed by anything except `g` clears the prefix and consumes it;
- `"` followed by an unsupported register clears the prefix and consumes it;
- a selected register followed by a command that does not read or write a
  register is invalid, consumed, and reset;
- Esc clears prefixes before mode cancellation;
- no invalid sequence inserts text in Normal mode.

This avoids recursively reprocessing a failed continuation as a new command,
which could unexpectedly mutate the composer.

`Pending` means the parser is waiting for another key. `Consumed` means a
sequence ended without an executable command and the current key must not fall
through. `Unhandled` alone permits the existing single-key match to run.

## Execution Architecture

`VimState` owns:

```rust
parser: VimCommandParser,
registers: VimRegisterBank,
last_change: Option<RepeatableChange>,
```

`vim_command.rs` is responsible only for parsing. `vim.rs` executes typed
commands through focused helpers:

- `execute_motion`;
- `delete_chars`;
- `delete_lines`;
- `yank_lines`;
- `paste_register`;
- `execute_repeat`.

Execution returns:

```rust
struct VimCommandOutcome {
    redraw: bool,
    text_changed: bool,
}
```

`VimState::handle` keeps its existing public `bool` result for callers:

- `true` when text, cursor, selection, or mode state changed as currently
  expected;
- prefix-only input is handled but returns `false` because composer content did
  not change;
- consumed invalid continuations also return `false`;
- executor logic uses `text_changed`, not the public bool, when deciding whether
  to record `last_change`;
- caller menu/history refresh retains the current broader behavior for cursor
  and mode changes.

To distinguish handled prefix input from an unhandled key, the command parser
is invoked inside `VimState`; Normal-mode unknown keys remain consumed by the
existing Vim path and never fall through to text insertion.

## Existing Commands

The command core preserves existing behavior:

- `i/a/A/o/O/v`;
- visual `y/d/c`;
- `D/C`;
- `u/Ctrl+R`;
- `/`, `n`, `N` transcript search routing;
- cursor styling and mode titles.

Normal `p` keeps the existing insertion-at-current-cursor behavior for
characterwise text. This task does not change it to Vim's after-cursor
characterwise placement, because that would be an unrelated compatibility
change.

Visual `y/d/c`:

- permit `"` plus `a-z` or `"` while Visual mode is active;
- consume a one-shot register selection;
- use `TextArea::copy` or `cut`;
- copy `textarea.yank_text()` into the register bank;
- remain characterwise;
- do not become dot-repeat targets.

Normal `D` and `C` consume a selected register and write their deleted
characterwise text to unnamed plus that named register. `D` becomes a
`DeleteToEnd` dot target only when it mutates text. `C` enters Insert mode and
does not become a dot target.

Count prefixes before legacy non-count commands (`^`, `$`, `i`, `a`, `A`, `o`,
`O`, `v`, `D`, `C`, `u`) are cleared and those commands execute once. A selected
register is valid only for `x`, `dd`, `yy`, `D`, `C`, `p`, and visual `y/d/c`.

Counted `x` starts one selection, moves forward up to the requested count with
`CursorMove::Forward`, and calls `cut` once. It preserves current cross-newline
behavior while producing one undo entry and one characterwise register write.

## Reset and Lifecycle Semantics

`VimState::reset_insert` and Esc clear:

- pending count;
- pending `d`, `y`, or `g`;
- incomplete register prefix;
- selected one-shot register.

They preserve:

- named and unnamed register contents;
- `last_change`;
- Vim enabled state.

`VimState` also exposes:

```rust
pub(crate) fn cancel_pending_command(&mut self);
```

This clears only parser count/prefix/register selection. It does not change
mode, textarea selection, register contents, or `last_change`.

Submitting, queueing, or restoring a composer already calls `reset_insert`, so
incomplete command prefixes cannot cross composer replacement.

Disabling Vim is not dynamic in the current TUI; `VimState::new(false)` starts
with empty parser/register/repeat state.

## Input Routing

No shortcut definitions or precedence rules change. The existing status, idle,
and running routing functions gain only pending-parser cancellation calls.

Precedence remains:

1. setup/session picker/approval;
2. transcript search intents `/`, `n`, `N`;
3. idle/running shortcuts such as Enter, Esc, `Alt+Up`, and scrolling;
4. mention and slash-menu handling;
5. composer Vim command parsing.

Counts and prefixes therefore operate only when the key already belongs to the
composer.

Every earlier route that consumes a key calls `cancel_pending_command()` first:

- transcript-search `/`, `n`, and `N`;
- idle and running shortcuts, including Enter, Esc, scrolling, and `Alt+Up`;
- accepted or dismissed mention/slash-menu input;
- workflow/panel key handling.

Keys that are not consumed continue to the composer without cancellation, so a
valid multi-key command can complete.

The app loop also cancels pending commands before handling:

- bracketed paste that mutates the composer;
- mouse interactions reported as handled;
- synthetic Enter generated by a mouse click;
- batched wheel scrolling.

Focus and resize events do not cancel pending commands because they neither
express an editing command nor mutate composer content.

`handle_key_event_preflight` therefore receives `&mut VimState`. It cancels
pending state only on branches that return `Continue` or `Exit` after consuming
the key:

- global shortcuts;
- active transcript-search input;
- shortcut-overlay Esc;
- mouse-selection Esc;
- approval-mode BackTab;
- workflows-panel Esc.

Release events keep their existing behavior and do not cancel pending state.

## Error and Bound Handling

- count accumulation uses checked multiplication/addition and saturates at
  `9999`;
- line counts clamp to available lines;
- bare `0` moves backward exactly the current character column, so it reaches
  the same line's column zero without `CursorMove::Head` crossing to the
  previous line;
- counted `x` selects forward up to the end of the buffer and cuts once;
- characterwise and linewise paste payload construction use checked size
  arithmetic and return a handled no-op if repeating the register would exceed
  1 MiB;
- repeat counts saturate and cannot recurse;
- empty registers are handled no-ops;
- invalid prefixes reset without mutation;
- all edits use public textarea APIs and preserve dependency invariants;
- no command panics on an empty one-line composer;
- no loop executes more than `9999` iterations for one key sequence.

## Files

### Create

- `crates/orca-tui/src/vim_command.rs`
  - pure prefix/count/register parser;
  - typed command and motion values;
  - parser unit tests.

### Modify

- `crates/orca-tui/src/lib.rs`
  - register `vim_command`.
- `crates/orca-tui/src/vim.rs`
  - own register bank and repeat state;
  - execute typed commands;
  - integrate existing Normal/Visual behavior;
  - lifecycle reset and integration tests.
- `crates/orca-tui/src/status_key_actions.rs`
  - cancel pending commands at search and shortcut boundaries;
  - integration tests proving routing/queue/search contracts remain unchanged.
- `crates/orca-tui/src/idle_key_actions.rs`
  - cancel pending commands when menus, panels, or idle shortcuts consume input.
- `crates/orca-tui/src/queued_input_actions.rs`
  - cancel pending commands when running menus or shortcuts consume input.
- `crates/orca-tui/src/key_event_actions.rs`
  - pass Vim state through preflight and cancel at global/search/panel
    consumption boundaries.
- `crates/orca-tui/src/app.rs`
  - pass the existing `VimState` reference into key preflight;
  - cancel pending commands at paste, mouse, synthetic-enter, and wheel-scroll
    boundaries.

No configuration, manifest, lockfile, shortcut registry, app-state, runtime,
history, or renderer changes are required.

## Test Strategy

### Parser tests

- digits accumulate and saturate;
- bare zero is a motion while zero after a count appends;
- `2dd`, `4gg`, `3G`, `"add`, `"ap`, `""p`, and `3.` parse exactly;
- `2yy` and `"ayy` parse exactly;
- pending `d/y/g/"` consume invalid continuations and reset;
- Esc/reset clears every pending prefix;
- every upper-layer consumed-key boundary clears pending prefixes without
  clearing registers or repeat;
- paste and mouse/scroll input clear pending prefixes without clearing
  registers or repeat;
- modified keys return unhandled after clearing prefixes;
- register and count can be combined before the command.

### Motion and line-operation tests

- count motions stop at boundaries;
- `dd` deletes first, middle, final, only, and multiple lines;
- `yy` copies first, middle, final, only, and multiple lines without mutation;
- `u` restores one `dd`;
- `gg`, counted `gg`, `G`, and counted `G` land on exact rows;
- counts clamp without overflow or panic;
- empty composer operations are safe.

### Register tests

- delete writes unnamed;
- named delete writes both named and unnamed;
- visual yank/delete honors named selection;
- `yy` writes a linewise unnamed or named register;
- named paste reads the selected register;
- selected register resets after one command;
- linewise paste inserts below and preserves line boundaries;
- counted paste inserts one atomic history edit;
- invalid/empty register is a no-op.

### Dot-repeat tests

- `x` then `.` repeats deletion;
- `2dd` then `.` deletes two more lines;
- named delete repeat updates the same named register;
- named paste repeat reads the current named value;
- count before dot repeats the stored change;
- failed mutation does not replace the prior repeat;
- movement/yank/undo/search do not replace repeat;
- dot with no prior change is safe.

### Routing regressions

- `/`, `n`, and `N` retain transcript-search precedence;
- Enter still submits/queues before Vim parsing;
- Esc still interrupts Running before Vim mode handling;
- mention selection still wins over composer commands;
- queue submit/reset clears prefixes but preserves registers/repeat;
- Vim-disabled input remains byte-for-byte unchanged.

## Verification

Focused:

```sh
cargo test -p orca-tui vim_command --lib -- --test-threads=1
cargo test -p orca-tui vim::tests:: --lib -- --test-threads=1
cargo test -p orca-tui status_key_actions::tests::vim_ --lib -- --test-threads=1
```

Package and workspace:

```sh
cargo test -p orca-tui -- --test-threads=1
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
cargo test --workspace --all-targets -- --test-threads=1
```

## Acceptance Criteria

1. Counts work for the declared motions and changes with a hard `9999` bound.
2. `dd`, `yy`, `gg`, counted `gg`, `G`, and counted `G` match the specified
   line semantics.
3. Unnamed and lowercase named registers work for declared delete/yank/paste
   commands.
4. Linewise `dd` registers paste below the current line with preserved line
   boundaries.
5. `.` repeats the declared successful atomic Normal-mode changes and never
   records failed or non-mutating commands.
6. All pending prefixes clear on Esc, mode change, submit, queue, and composer
   restoration.
7. Registers and last repeat persist across composer reset within the same TUI
   process.
8. Existing search, shortcut, mention, queue, cursor, undo/redo, and
   Vim-disabled contracts remain unchanged.
9. No configuration, `jj` remap, operator+motion, insert-session repeat,
   uppercase register append, macro, or clipboard-register behavior is added.
10. The sub-project is independently reviewed, committed with the required
    TRAE trailer, pushed to `feature/tui-syntax-highlighting`, and remote SHA
    equals local `HEAD`.
