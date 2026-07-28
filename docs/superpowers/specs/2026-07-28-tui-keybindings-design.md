# TUI Custom Keybindings Design

## Goal

Add user-configurable TUI keybindings with:

- context-specific actions;
- sequential key chords;
- live reload while the TUI is running;
- shortcut help generated from the active bindings.

The feature is additive. With no keybindings file, every existing key keeps its
current meaning and routing priority.

This sub-project covers stable application actions already represented by
`GlobalShortcut`, `IdleShortcut`, `RunningShortcut`, and `ApprovalShortcut`.
It does not make slash-menu navigation, mention-menu navigation, setup,
session-picker input, workflow-panel navigation, approval option numbers, or
Vim editing commands configurable.

## Considered Approaches

### Replace static arrays at startup

Parse a configuration file into the existing binding arrays and keep
`resolve_shortcut` stateless.

This is the smallest change, but it cannot represent a chord that is pending
between events and cannot hot-reload without restarting the TUI. It does not
meet the requested behavior.

### Put mutable chord logic in each key handler

Pass a mutable resolver through global, idle, running, queued, compacting, and
approval handlers. Each handler would own part of chord progression and
reload behavior.

This preserves local dispatch, but duplicates timing and mismatch rules. It
also makes menu priority, runtime context changes, and reload cancellation
hard to reason about.

### Dynamic keymap plus one chord runtime

Keep stable semantic action enums, replace static lookup with an immutable
`Keymap`, and give the main event pipeline one `KeymapRuntime` that owns the
active map, pending chord, reload polling, and dynamic help.

This is the selected design. Existing handlers still execute semantic actions;
only binding resolution becomes dynamic. Central pending-chord ownership gives
one timeout and cancellation contract without rewriting command dispatch.

## Configuration File

### Location and lifecycle

The optional file is:

```text
$ORCA_HOME/keybindings.json
```

When `ORCA_HOME` is unset, the path is:

```text
~/.orca/keybindings.json
```

There is no project-local keybindings file. This avoids allowing a repository
to change the user's terminal controls through trust or checkout changes.

The file is requested:

1. once before the first interactive frame;
2. at most once every 500ms while the TUI runs.

The main loop never opens or reads the file. Every poll sends a coalesced load
request to one off-loop reload worker. The worker reads at most 64 KiB plus one
sentinel byte and returns an observation over a bounded latest-value channel.
The main loop applies completed observations between event-loop iterations.
Comparing observations detects in-place writes, atomic renames, deletion, and
recreation without platform-specific file notifications. A sentinel byte means
the candidate is oversized and is never passed to JSON parsing.

The path must resolve to a regular file. Directories and special files are
rejected. Symbolic links are rejected rather than followed, keeping polling
bounded and preventing an unexpected target switch between observations. On
Unix, opening uses `O_NOFOLLOW` in addition to a `symlink_metadata` check so a
replacement between inspection and open is also rejected.

The worker is a loader, not a watcher: it performs I/O only after an explicit
poll request. The UI never waits for it. Shutdown closes its channels and joins
only when the worker has already reported completion; an OS-blocked loader is
detached so terminal restoration and process shutdown do not wait on
filesystem I/O.

Initial loading is explicitly eventual. The request is queued before the first
frame, but the first frame and any input received before the worker responds
use built-ins. The first completed observation is applied atomically like a hot
reload. This avoids placing an unbounded filesystem wait on startup.

An invalid or unreadable initial observation is reported as one in-TUI system
notice because it may arrive after alternate-screen setup. It is not printed to
stderr while the TUI owns the terminal. If TUI startup fails before ownership
is established, the existing startup error path remains unchanged.

Missing at startup means the built-in keymap. Deleting the file while running
restores the built-in keymap. The poll does not run in headless, JSONL,
server, workflow-worker, or child-agent execution.

### Schema

The file has an explicit version and an action-to-sequences map:

```json
{
  "version": 1,
  "bindings": {
    "global.open-transcript-search": ["ctrl+f", "ctrl+x ctrl+f"],
    "global.toggle-shortcuts": ["f1", "ctrl+k"],
    "idle.submit": ["enter"],
    "running.interrupt": ["esc", "ctrl+g"],
    "approval.confirm": ["enter"]
  }
}
```

`version` must equal `1`. Unknown top-level fields, unknown action IDs, unknown
key names, unknown modifiers, empty sequence strings, and sequences longer
than four strokes reject the complete candidate file.

Action IDs are stable lowercase identifiers:

```text
global.cancel
global.open-transcript-search
global.toggle-shortcuts
global.scroll-bottom
global.scroll-top
global.clear-screen

idle.submit
idle.newline
idle.edit-latest-queued
idle.history-previous
idle.history-next
idle.scroll-up
idle.scroll-down
idle.page-up
idle.page-down
idle.half-page-up
idle.half-page-down
idle.backtrack
idle.expand-tool-output

running.background-current-turn
running.interrupt
running.submit-queued
running.newline
running.edit-latest-queued
running.scroll-up
running.scroll-down
running.page-up
running.page-down
running.half-page-up
running.half-page-down

approval.select-allow
approval.select-deny
approval.toggle-selection
approval.confirm
```

An omitted action keeps all of its built-in bindings. A present action replaces
its built-in bindings. An empty array disables the action, except
`global.cancel`.

The effective `global.cancel` action must retain at least one single-stroke
binding. A candidate file that removes all single-stroke cancel bindings is
invalid. This ensures a malformed preference cannot leave the terminal without
an immediate keyboard exit path.

`global.cancel` accepts single-stroke sequences only. Every effective cancel
stroke is reserved: it cannot occur at any position in another sequence. This
keeps cancel ahead of every pending chord and preserves the current immediate
interrupt or double-press exit path.

Every stroke in a configurable Global sequence must be either:

- `f1` through `f24`; or
- a character carrying Ctrl, Alt, Super, Hyper, or Meta.

Shift alone does not satisfy this rule. Built-in Global sequences are exempt
because their non-character keys are known compatibility bindings. This
prevents user Global bindings from shadowing fixed modal controls such as Esc,
Enter, arrows, Tab, BackTab, and `shift+tab`, while still supporting practical
global chords such as `ctrl+x ctrl+f`.

Approval option keys remain fixed and are not action IDs:

```text
1 2 3 4 y a A n d
```

They retain their current option-dependent meanings and are reserved from every
configurable Approval sequence. They are also reserved at every position of
every configurable Global sequence, because Global bindings participate in
Approval context. In particular, `a` continues to mean “always allow this
tool,” not generic approve. Approval selection, toggle, and confirm remain
configurable around those fixed direct keys.

### Key syntax

One sequence string contains one to four whitespace-separated strokes:

```text
ctrl+x ctrl+f
g g
alt+shift+enter
```

A stroke contains zero or more modifiers followed by one key. Parsing is
ASCII case-insensitive. The canonical modifier order is:

```text
ctrl+alt+shift+super+hyper+meta
```

Supported named keys are:

```text
backspace enter left right up down home end pageup pagedown tab backtab
delete insert esc space f1 ... f24
```

A single Unicode scalar value represents a character key. The scalar cannot
be whitespace or a control character; `space` represents the space key.
Duplicate modifiers and a stroke with more than one non-modifier component are
invalid.

The parser uses the existing key normalization contract:

- raw C0 control characters match their `ctrl+<character>` form;
- uppercase ASCII input normalizes to `shift+<lowercase>`;
- key release events never resolve bindings;
- press and repeat events resolve bindings.

The formatter emits canonical strings and is the only source used by dynamic
shortcut help.

## Merge and Validation

Loading builds a complete immutable `Keymap` from configurable built-ins plus
replacements. Fixed controls, including approval direct keys, remain outside
the `Keymap` and are validated as reserved strokes. Validation happens after
merging.

Within each effective context, a sequence can map to only one action. Global
bindings participate in every context and keep priority over contextual
bindings. Therefore these are conflicts:

- two global actions with the same sequence;
- two actions in one context with the same sequence;
- a global action and an Idle, Running, or Approval action with the same
  sequence.

The same sequence may map to different actions in distinct non-global contexts.

One effective sequence cannot be the prefix of another effective sequence in
the same context. Prefix ambiguity would otherwise force a complete
single-stroke action to wait for the chord timeout. The candidate file is
rejected instead.

For Global, Idle, and Running chords, every non-final stroke must be
non-textual. A non-textual stroke is either:

- a named non-character key; or
- a character with Ctrl, Alt, Super, Hyper, or Meta.

`shift+<character>` alone is textual and cannot be a chord prefix. Approval
chords may use bare-character prefixes because approval context has no text
composer. These rules prevent a timed-out or mismatched chord from swallowing
ordinary composer text.

Validation errors include the action ID and sequence or conflict involved.

## Runtime Resolution

### Components

Add a focused `keybindings` module containing:

```rust
struct Keymap { /* immutable effective bindings and help rows */ }
struct KeymapRuntime { /* active map, pending chord, reload state */ }
struct PendingChord { /* context, matched strokes, candidates, deadline */ }
struct InputOwnerFingerprint { /* status, modal owner, Vim mode */ }
struct ShortcutInvocation { /* action plus single-key or chord origin */ }

enum ShortcutResolution {
    NoMatch,
    Pending,
    Action(ShortcutInvocation),
}
```

The semantic shortcut enums remain the dispatch interface. The new module does
not call `UserAction`, mutate `AppState`, edit the textarea, or know how an
action is executed.

`KeymapRuntime` receives an `Instant` from the caller. It does not sleep,
create a timer thread, or read the clock internally in deterministic unit
tests.

### Routing priority

Existing ownership order is preserved:

1. global cancel;
2. a previously started chord is advanced before a new layer can consume its
   next key;
3. transcript search;
4. other global shortcuts;
5. shortcut overlay, selection dismissal, approval-mode cycling, and existing
   status-specific preflight;
6. slash menu, mention menu, and workflow panel;
7. contextual Idle, Running, Compacting, or Approval shortcuts;
8. composer and Vim input.

Resolving global cancel first clears any pending chord before executing cancel.
The same event is not offered to chord advancement afterward. If an Idle first
cancel press only arms the existing double-press exit notice, the next
non-cancel key starts from normal routing with no stale chord.

A contextual chord can start only at step 7, after the active menu or panel has
declined the key. Once started, its continuation has step-1 priority while the
recorded input owner remains unchanged. This is the same ownership rule used by
the existing Vim Insert escape sequence: a lower layer cannot steal the
continuation of an explicitly initiated sequence, but a real ownership change
fences stale pending state.

Global chords start in global preflight because global shortcuts already have
that priority.

Completing a chord returns a `ShortcutInvocation` carrying its semantic
`ShortcutAction` and `Chord` origin. Single-stroke resolution carries the
original event and `Key` origin.

Context handlers are split into resolution and action-only execution. A
pre-resolved chord calls the action executor directly; it is never represented
as the final raw key and is never fed to `TextArea`.

For existing single-key bindings, raw-key-sensitive compatibility remains:

- Idle Up/Down move within a multiline composer before navigating history;
- Idle ScrollUp/ScrollDown can preserve textarea cursor behavior where it
  currently applies;
- `idle.expand-tool-output` falls back to normal composer input when its guard
  declines the action.

For chord invocations, semantic action behavior is explicit:

- `idle.history-previous` and `idle.history-next` navigate history regardless
  of the final chord stroke;
- `idle.scroll-up` and `idle.scroll-down` scroll the transcript;
- guarded `idle.expand-tool-output` is a no-op when the composer is non-empty
  or no expandable output exists;
- no chord stroke is inserted, replayed, or added to textarea undo history.

All other actions execute the same semantic operations for single keys and
chords. Vim cancellation, queue behavior, Compacting allowlisting, and approval
behavior remain in their current action modules.

### Timeout and mismatch

The chord timeout is a fixed one second from the most recent accepted stroke.
It is intentionally not configurable in version 1.

If the next key matches one or more candidates:

- a complete candidate emits its action;
- otherwise the pending candidate set narrows and the deadline resets.

If the next key does not match:

1. clear the pending chord;
2. process the current key from the start of normal routing exactly once.

The earlier prefix is not replayed. Validation guarantees it was not ordinary
text and was not also a complete shortcut.

When `now` is later than the pending deadline, the pending chord is cleared
before the current event is resolved. No action fires on timeout. The main loop
also caps its next blocking wait by the pending deadline. Under an idle event
loop, stale state is cleared at the first loop checkpoint after the deadline;
already-running synchronous event handling may delay that checkpoint.

Key release events neither start, advance, mismatch, nor clear a chord.
Paste, mouse input, focus changes, terminal resize, and synthetic Enter clear
the pending chord before preserving their existing behavior.

### Context and state fences

A pending chord records an `InputOwnerFingerprint` computed by the app before
each routed event. The fingerprint contains:

- the effective shortcut context;
- the active modal owner, if any;
- the current panel;
- the current Vim mode.

The runtime synchronizes this fingerprint before advancing or starting a chord.
A difference clears the pending state before the current event is processed.
The chord is therefore cleared when:

- `AppStatus` changes to a different shortcut context;
- a modal owner opens or closes, including transcript search, shortcut help,
  slash menu, mention menu, workflow panel, setup, session picker, or approval
  dialog;
- Vim mode changes;
- terminal input suspends;
- the active keymap changes;
- the TUI exits.

Clearing a chord never mutates the composer or its undo history.

## Hot Reload

`KeymapRuntime::poll_reload(now)` returns one of:

```rust
enum ReloadOutcome {
    Unchanged,
    Applied,
    RestoredDefaults,
    Rejected(String),
}
```

On a valid changed file:

1. parse and fully validate a candidate `Keymap`;
2. atomically swap the active immutable map;
3. clear any pending chord;
4. mark the frame dirty so open shortcut help refreshes.

On deletion, restore built-ins through the same atomic swap path.

On an invalid initial observation, keep built-ins and append one system notice.
Startup is not delayed waiting for a blocked worker: the first completed
observation applies like any hot reload. On an invalid later reload, keep the
last-known-good map and append one system notice describing the rejected
reload. The same observed invalid bytes are not reparsed or reported on every
poll. A later byte change is tried again.

Read errors other than not-found follow the invalid-reload rule. Files over
64 KiB are rejected before JSON parsing.

The reload path changes only keybindings. It does not rebuild `RunConfig`,
restart qwertty, reset the textarea, replace runtime configuration, or reload
the theme.

## Dynamic Shortcut Help

The shortcut overlay reads bindings from the active `Keymap`; it does not use
static key strings for configurable actions.

Static help descriptors retain the current human grouping and labels. A
descriptor contains one scope, one label, one or more semantic actions, and its
legacy default key string.
For example, `jump to top or bottom` references both global scroll actions,
while `show or hide shortcuts` references one action with two default
sequences.

For each active scope and descriptor:

- collect the current sequences for every referenced action;
- when those referenced actions exactly equal their built-in sequences, display
  the descriptor's legacy key string byte-for-byte;
- otherwise join the current canonical sequences with ` / `;
- render canonical chord strokes separated by a space;
- omit the row only when every referenced action is disabled;
- keep the existing human-readable action labels.

Fixed, non-configurable controls remain explicit help rows:

- `shift+tab` approval-mode cycling;
- current approval option numbers;
- the currently displayed approval direct keys `y/A/a/n`;
- modal-local close or navigation controls where they are already shown.

Fixed suffixes such as approval option keys may be included by their help
descriptor without becoming configurable actions.

The overlay receives the same immutable map snapshot used by resolution for
that event-loop generation, so displayed bindings and active bindings cannot
disagree during a frame. With built-ins, row grouping, order, labels, and key
strings remain identical to the current overlay.

The existing fixed `d` deny alias remains functional but intentionally remains
omitted from the default help row, matching the current overlay. This project
does not broaden fixed-control help content.

The welcome screen and status bar also receive the active map snapshot.
Configurable key names are formatted through the same keymap helpers:

- the welcome send/newline tip keeps its current literal text while both
  referenced actions equal built-ins; otherwise it shows their active canonical
  sequences, or is omitted when both actions are disabled;
- the welcome shortcut-help tip and status-bar shortcut hint show the active
  `global.toggle-shortcuts` sequences; each keeps its current F1/Ctrl+K wording
  while that action equals built-ins, or uses key-independent “shortcuts”
  wording when the action is disabled.

No user-visible surface may hard-code a configurable key.

## Compatibility

With no file, generated defaults must be byte-for-byte equivalent at the
normalized binding level for every configurable action. Fixed approval direct
keys are verified separately at the handler boundary. Together they preserve
all existing key behavior.

The implementation must preserve:

- global-before-context priority;
- menu and panel ownership before contextual shortcuts;
- transcript-search routing;
- queued message editing and submission;
- Compacting's allowlist of Running actions;
- approval option number handling;
- C0 and shifted-character normalization;
- hardware cursor behavior;
- composer undo and redo;
- Vim-disabled behavior;
- Vim command parsing and Insert escape behavior;
- input batch limits and runtime event fairness;
- terminal suspension and cleanup.

No direct crossterm terminal commands, new terminal owner, continuously
watching filesystem thread, or `notify` dependency is added. One bounded,
request-driven reload worker is permitted and never owns terminal state.

## Testing

### Parser and merge

- missing file produces exact defaults;
- each supported named key and modifier parses and formats canonically;
- Unicode scalar bindings parse;
- C0 and shifted input normalization still matches;
- omitted action inherits defaults;
- present action replaces defaults;
- empty replacement disables an action;
- disabling all single-stroke cancel bindings fails;
- multi-stroke cancel and use of a cancel stroke in another chord fail;
- configurable Global named/modal keys and shift-only characters fail, while
  modified characters and function keys succeed;
- unknown fields, version, actions, keys, and modifiers fail;
- fixed Approval direct keys conflict with configurable Approval sequences and
  with every stroke position in configurable Global sequences;
- duplicate and prefix conflicts fail with context;
- a sequence reused in separate non-global contexts succeeds;
- text chord prefixes fail in Global, Idle, and Running;
- bare-character Approval chords succeed;
- four-stroke chords succeed and five-stroke chords fail;
- symbolic links, special files, and oversized files fail before JSON parsing.

### Chord state machine

- exact two-, three-, and four-stroke chords emit once;
- cancel clears any pending chord and interrupts immediately; the following key
  is routed normally;
- mismatch clears the prefix and reroutes the current key once;
- timeout emits nothing and allows the next key normally;
- an accepted intermediate stroke resets the deadline;
- repeat events follow press behavior and release events are ignored;
- non-key events clear pending state;
- context, modal, Vim, suspension, and keymap-generation changes clear state;
- a menu that appears asynchronously changes the input owner and clears a
  pending contextual chord before routing the current key;
- global chords and contextual chords keep existing priority.

### Hot reload

- delayed initial missing, valid, invalid, unreadable, and oversized
  observations;
- first frame and first key use built-ins while the initial observation is
  pending, then switch atomically when it arrives;
- in-place write, atomic rename, delete, and recreate;
- a blocking loader never stalls input, drawing, terminal restoration, or
  shutdown;
- valid reload changes resolution and open help together;
- invalid reload keeps last-known-good;
- unchanged invalid bytes report only once;
- a later valid edit recovers;
- reload clears a pending chord;
- polling is capped at once per 500ms.

### Integration and regression

- one custom action in each context executes through its existing handler;
- Compacting still rejects disallowed Running actions after custom resolution;
- slash, mention, workflow, search, setup, picker, and approval-number routing
  retain priority;
- every chord-bindable Idle action is tested with non-empty and multiline
  composers; final chord strokes never alter text or undo history;
- fixed Approval keys `1/2/3/4/y/a/A/n/d` retain behavior; existing help
  remains `1/2/3/4/y/A/a/n`;
- Vim command and configured Insert escape tests remain unchanged;
- shortcut overlay reflects replacements, disabled actions, and chords;
- welcome and status hints reflect replacements and disabled actions;
- every legacy help descriptor retains its exact current string while its
  referenced bindings are default, even when unrelated bindings are customized;
- no-file snapshots retain current help and behavior;
- batched input and frame scheduling remain bounded.

Focused tests run first, followed by:

```text
cargo test -p orca-core
cargo test -p orca-tui
cargo test --workspace --all-targets
cargo check --workspace
cargo fmt --all -- --check
git diff --check
```

Any pre-existing flaky workspace test must be isolated with an exact rerun and
source-diff evidence before it is classified as unrelated.

## Out of Scope

- project-local or repository-provided keybindings;
- changing bindings from inside the TUI;
- recording keypresses interactively;
- conditional bindings based on terminal, model, workspace, or Vim sub-mode;
- configurable chord timeout;
- simultaneous key combinations;
- mouse gestures;
- macro recording or arbitrary command strings;
- slash-command, mention-menu, setup, picker, workflow-panel, or Vim command
  remapping;
- reloading general `config.toml`.
