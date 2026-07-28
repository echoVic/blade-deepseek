# TUI Notifications and Terminal Title Design

## Objective

Add terminal-native notifications and an informative terminal title without
reintroducing competing terminal writers or changing the existing system
desktop-notification behavior.

The TUI should:

- enable terminal focus reporting;
- notify only when the terminal is unfocused;
- use OSC 9 on known compatible terminals and through tmux passthrough;
- fall back to BEL when OSC 9 support is not known;
- show running progress and interaction-required state in the terminal title;
- restore the title to `Orca` on orderly exit.

## Scope

This sub-project includes:

- `terminal_notifications = true` as a new file/runtime configuration value;
- qwertty-owned focus-mode enablement and cleanup;
- focus tracking from qwertty events adapted to crossterm events;
- OSC 9 notification encoding with bounded sanitized text;
- tmux DCS passthrough for OSC 9 and OSC 0;
- BEL fallback for terminals not known to support OSC 9;
- notification triggering for approval, requested input, task completion or
  failure, and workflow completion or failure;
- OSC 0 title updates for idle, running, compacting, approval, and user-input
  states;
- deduplicated output and orderly title reset;
- deterministic tests that never write real terminal escape sequences.

It does not include:

- operating-system desktop notification changes;
- notification history, action buttons, images, sounds, or urgency levels;
- terminal capability diagnostics or `/doctor`;
- notification/title behavior in headless, ACP, server, or JSONL modes;
- P0 #6 diff rendering changes.

## Configuration

Add the following independent value:

```toml
terminal_notifications = true
```

It defaults to `true`. It controls terminal-native OSC 9/BEL notifications and
focus-mode enablement only.

The existing `desktop_notifications` setting remains unchanged:

- it continues to control `osascript`/`notify-send`;
- it does not enable or disable terminal-native notifications;
- enabling both settings may intentionally deliver through both channels.

Terminal titles are always enabled while the TUI runs. They contain only fixed
Orca status text and never user, model, path, tool-output, or workflow-summary
content.

## Terminal Output Ownership

The TUI main thread remains the only steady-state terminal output owner:

- ratatui writes frame output through
  `CapabilityBackend<CrosstermBackend<Stdout>>`;
- terminal presentation writes OSC 0, OSC 9, or BEL through the same backend
  writer, between completed frame operations;
- qwertty writes only startup mode setup, suspend/resume lifecycle commands,
  and final cleanup under the barriers established by the terminal-capability
  sub-project.

Notification and title output must not be sent from the qwertty input thread,
runtime workers, or desktop-notification threads. This avoids interleaving
escape bytes with a ratatui frame.

`TerminalPresentation` is a main-thread state machine. It owns:

```rust
pub(crate) struct TerminalPresentation {
    focused: bool,
    notifications_enabled: bool,
    osc9_supported: bool,
    tmux_passthrough: bool,
    animation_tick: u64,
    last_title: Option<String>,
    pending_notifications: VecDeque<TerminalNotification>,
}
```

It computes output intents separately from encoding and writing.

## Focus Ownership

When `terminal_notifications` is enabled, qwertty startup calls:

```rust
session.enable_focus_events().await?;
```

The same qwertty mode ledger disables focus reporting on suspend, leave,
panic restoration, and normal drop.

The adapter continues producing:

```rust
Event::FocusGained
Event::FocusLost
```

The application consumes these events before ordinary key/mouse handling:

- initial focus is `true`;
- `FocusLost` sets `focused = false`;
- `FocusGained` sets `focused = true`;
- focus events do not dirty application state or reach textarea/key handlers;
- losing focus does not retroactively notify for an already-visible state.

Notifications are evaluated when their triggering runtime event arrives. If
the terminal is focused at that instant, the notification is suppressed.

## Notification Trigger Matrix

Only terminal-native notifications use this matrix:

| Runtime event | Message |
|---|---|
| `ApprovalNeeded` | `Approval required` |
| `PermissionApprovalNeeded` | `Permission approval required` |
| `UserInputRequested` | `Input required` |
| `McpElicitationRequested` | `MCP input required` |
| `SessionCompleted { status: "success" }` | `Task completed` |
| `SessionCompleted { status }` for any other status | `Task {status}` |
| `WorkflowNotification { status: "completed" }` | `Workflow completed` |
| `WorkflowNotification { status }` for any other status | `Workflow {status}` |

Automatic allowlist approval is not notified because
`handle_runtime_event` resolves it without entering `WaitingApproval`.

Each runtime event can enqueue at most one notification. Focus changes and
title changes never enqueue notifications.

The pending queue keeps at most 32 items and collapses adjacent identical
messages. A full queue drops the oldest item before accepting the newest.
Each event-loop iteration writes at most eight notifications, so a runtime
event burst cannot monopolize the terminal writer.

The message vocabulary is fixed and status fragments are sanitized and
bounded before encoding. User prompts, tool arguments, provider text, and
workflow summaries never enter OSC 9.

## Notification Encoding

Known-compatible terminals are:

- Ghostty;
- iTerm2;
- Kitty;
- WezTerm.

Support is inferred once from qwertty's environment-backed terminal identity.
When tmux is present, Orca emits OSC 9 inside the existing tmux DCS passthrough
envelope so the outer terminal can receive it.

OSC 9 encoding is:

```text
ESC ] 9 ; <sanitized-message> ESC \
```

The message uses qwertty's title sanitizer and its 240-character bound. This
removes C0/C1 controls, bidi/invisible injection characters, and raw
terminators.

For a known-compatible terminal:

- emit OSC 9;
- do not emit BEL.

For tmux:

- emit the OSC 9 sequence wrapped by `tmux_passthrough`;
- do not emit BEL.

For every other or unknown terminal:

- emit one BEL byte;
- do not emit an unknown OSC command.

The pure writer returns its first `io::Error` so tests can assert propagation.
Production calls it best-effort and discards that result. A failure does not
change application status, add transcript messages, or retry in a loop.

## Terminal Title

Titles use qwertty's sanitized OSC 0 encoder:

```rust
qwertty::commands::osc::set_icon_and_title(title)
```

When tmux is present, the encoded OSC 0 sequence is wrapped in the same DCS
passthrough helper.

Title values are:

| App status | Title |
|---|---|
| Setup, SessionPicker, Idle | `Orca` |
| Running | `{braille-frame} Orca` |
| Compacting | `{braille-frame} Orca · compacting` |
| WaitingApproval, visible phase | `[!] Orca` |
| WaitingApproval, hidden phase | `Orca` |
| WaitingUserInput | `[?] Orca` |

The braille frames reuse the existing sequence:

```text
⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏
```

Running and compacting titles advance at the existing animation cadence.
Approval alternates every six animation ticks, approximately 480 ms under the
current 80 ms interval.

`TerminalPresentation` emits a title only when the encoded logical title
differs from `last_title`. Repeated idle frames and unchanged status events
produce no output.

After qwertty resume clears/re-enters the alternate screen, `last_title` is
invalidated so the title is reasserted alongside the full ratatui repaint.

On orderly TUI exit:

1. write title `Orca` through the live ratatui backend;
2. flush the writer;
3. drop ratatui;
4. finish qwertty terminal cleanup.

Panic and fatal-signal paths rely on qwertty's emergency terminal restoration;
they do not guarantee title restoration.

## Event-Loop Integration

The loop order is:

1. receive input/control;
2. consume focus events into `TerminalPresentation`;
3. process input and runtime events;
4. derive notification intents from the original runtime events;
5. update presentation animation;
6. write deduplicated title and pending notification bytes;
7. draw a ratatui frame when scheduled.

Presentation output occurs before `terminal.draw` and only on the main thread.
It does not mutate ratatui's retained cell buffer because OSC 0, OSC 9, and BEL
do not move the cursor or alter screen cells.

The existing 64-input and 256-runtime event batch limits remain unchanged.
Notification and title work is bounded per iteration:

- at most one title write;
- at most eight notification writes;
- at most 32 pending notifications;
- no subprocess, network, or blocking terminal query.

## Error Handling

- Focus-mode setup failure aborts TUI startup and uses normal qwertty leave.
- Focus channel/input disconnect follows the terminal-capability teardown
  barrier.
- The pure output writer returns its first OSC/BEL/title `io::Error`;
  production ignores the result and does not terminate the session.
- Unknown terminal identity uses BEL, never a guessed OSC 9.
- Unknown future focus or terminal event variants are ignored.
- Title and notification text is sanitized before framing.
- No output is emitted after ratatui is dropped.

## Testing

Implementation follows strict test-driven development.

### Configuration

- omitted `terminal_notifications` parses as `true`;
- explicit `true` and `false` round-trip into `RunConfig`;
- config display prints the effective value;
- `desktop_notifications` remains independent.

### Encoding

- OSC 9 emits exact ST-terminated bytes;
- control, bidi, and oversized text is sanitized and bounded;
- known terminals choose OSC 9 without BEL;
- unknown terminals choose BEL without OSC 9;
- tmux wraps OSC 9 and OSC 0 with doubled ESC bytes;
- title encoding delegates to qwertty's sanitized OSC 0 command.

### Focus and Notifications

- initial focus suppresses notifications;
- `FocusLost` followed by each trigger emits exactly one notification;
- `FocusGained` suppresses later triggers;
- allowlisted approval emits no notification;
- focus events never become key events or dirty textarea state;
- disabling `terminal_notifications` skips focus-mode setup and all alerts.

### Titles

- every `AppStatus` maps to the specified title;
- running and compacting spinner frames rotate;
- approval alternates at six ticks;
- unchanged titles are deduplicated;
- resume invalidation re-emits an unchanged logical title;
- orderly exit writes `Orca` before ratatui drop.

### Regression

- IME hardware cursor tests remain green;
- input adapter, legacy Alt, paste, mouse, signal, suspend, and teardown tests
  remain green;
- no real OSC 9, OSC 0, BEL, or focus-mode command is sent by automated tests;
- full `orca-tui` and workspace all-target tests pass.

## Delivery

The design, plan, and implementation are separate commits on
`feature/tui-syntax-highlighting`. Each commit ends with:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

Final delivery includes specification review, quality review, complete package
and workspace tests, push, and local/remote SHA comparison.
