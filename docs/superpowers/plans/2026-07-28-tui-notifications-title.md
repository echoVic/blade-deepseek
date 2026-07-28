# TUI Notifications and Terminal Title Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add focus-aware terminal-native notifications and dynamic OSC 0 titles while preserving one steady-state terminal writer.

**Architecture:** A pure `terminal_presentation.rs` state machine converts fixed runtime events and app status into bounded output intents. The synchronous TUI main thread writes encoded OSC 0, OSC 9, tmux passthrough, or BEL through ratatui's existing crossterm backend between frames. Qwertty remains the input/lifecycle owner and adds focus-mode enablement only.

**Tech Stack:** Rust 2024, qwertty 0.1.6, crossterm 0.28, ratatui 0.29, crossbeam-channel.

---

## File Map

- Modify `crates/orca-core/src/config/file.rs`
  - Add default-on `terminal_notifications`.
- Modify `crates/orca-core/src/config/mod.rs`
  - Carry and display the effective runtime value.
- Modify `src/cli.rs`
  - Propagate file config into TUI `RunConfig`; keep non-TUI constructors off.
- Modify TUI test `RunConfig` literals
  - Set explicit values without changing existing desktop notification tests.
- Create `crates/orca-tui/src/terminal_presentation.rs`
  - Model focus, terminal support, notification queue, title animation, pure encoding, and bounded writer behavior.
- Modify `crates/orca-tui/src/lib.rs`
  - Register the presentation module.
- Modify `crates/orca-tui/src/input_runtime.rs`
  - Accept startup options and conditionally enable qwertty focus events.
- Modify `crates/orca-tui/src/input_event_actions.rs`
  - Keep focus events out of normal application input handling.
- Modify `crates/orca-tui/src/runtime_event_actions.rs`
  - Expose a pure notification-trigger classifier that respects allowlisted approval.
- Modify `crates/orca-tui/src/app.rs`
  - Own `TerminalPresentation`, consume focus events, write presentation output between frames, animate titles, invalidate after resume, and reset title before ratatui drop.
- Reuse `crates/orca-tui/src/selection.rs`
  - Reuse `tmux_passthrough`; do not duplicate DCS framing.

## Required Discipline

- Run every RED command before implementation.
- No automated test sends real OSC 0, OSC 9, BEL, or focus-mode bytes.
- Do not write terminal presentation output from runtime/qwertty worker threads.
- Keep `desktop_notifications` behavior byte-for-byte independent.
- Do not add P0 #6 diff behavior.
- Every commit ends with exactly:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

---

### Task 1: Add Independent Default-On Configuration

**Files:**
- Modify: `crates/orca-core/src/config/file.rs`
- Modify: `crates/orca-core/src/config/mod.rs`
- Modify: `src/cli.rs`
- Modify: TUI `RunConfig` literals reported by the compiler

- [ ] **Step 1: Write failing config tests**

Add file-config tests:

```rust
#[test]
fn terminal_notifications_default_on_and_parse_explicit_values() {
    let omitted: FileConfig = toml::from_str("").unwrap();
    let enabled: FileConfig = toml::from_str("terminal_notifications = true").unwrap();
    let disabled: FileConfig = toml::from_str("terminal_notifications = false").unwrap();

    assert!(omitted.terminal_notifications);
    assert!(enabled.terminal_notifications);
    assert!(!disabled.terminal_notifications);
}
```

Extend config-show assertions so both values appear independently:

```rust
assert!(shown.contains("desktop_notifications = true"));
assert!(shown.contains("terminal_notifications = false"));
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-core terminal_notifications --lib
```

Expected: missing field or missing rendered configuration.

- [ ] **Step 3: Implement config propagation**

Add to raw/file config:

```rust
#[serde(default = "default_true")]
pub terminal_notifications: bool,
```

Add to `RunConfig`:

```rust
pub terminal_notifications: bool,
```

Propagate the TUI CLI constructor from `file_config.terminal_notifications`.
Set `false` in headless/server/ACP constructors that never create a TUI.
Set explicit test values in direct literals.

- [ ] **Step 4: Run GREEN**

```sh
cargo test -p orca-core terminal_notifications --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 5: Commit**

```sh
git add crates/orca-core/src/config/file.rs crates/orca-core/src/config/mod.rs \
  src/cli.rs crates/orca-tui/src
git commit -m "feat(core): configure terminal notifications" \
  -m "Enable terminal-native TUI notifications by default without changing system desktop notifications." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 2: Build the Pure Presentation and Encoding Boundary

**Files:**
- Create: `crates/orca-tui/src/terminal_presentation.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [ ] **Step 1: Write failing identity and encoding tests**

Define:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TerminalPresentationProfile {
    pub(crate) osc9_supported: bool,
    pub(crate) tmux_passthrough: bool,
}
```

Test profile classification for Ghostty, iTerm2, Kitty, WezTerm, unknown, and tmux.

Test exact encoding:

```rust
assert_eq!(
    encode_notification("done", profile),
    b"\x1b]9;done\x1b\\".to_vec()
);
assert_eq!(encode_notification("done", unknown), b"\x07".to_vec());
assert_eq!(
    encode_title("Orca", direct),
    b"\x1b]0;Orca\x1b\\".to_vec()
);
```

Assert control/bidi/oversized text is sanitized through qwertty's title sanitizer and tmux doubles every ESC.

- [ ] **Step 2: Write failing title and queue tests**

Define:

```rust
pub(crate) struct TerminalPresentation {
    focused: bool,
    notifications_enabled: bool,
    profile: TerminalPresentationProfile,
    animation_tick: u64,
    last_title: Option<String>,
    pending_notifications: VecDeque<TerminalNotification>,
}
```

Test:

- every `AppStatus` title;
- running/compacting spinner rotation;
- six-tick approval flash;
- 32-item queue bound;
- adjacent duplicate collapse;
- eight-item drain cap;
- unchanged title dedup;
- invalidation re-emits title.

- [ ] **Step 3: Run RED**

```sh
cargo test -p orca-tui terminal_presentation --lib
```

Expected: module and API missing.

- [ ] **Step 4: Implement minimal pure state machine**

Use:

```rust
qwertty::caps::identity_from_env(None, qwertty::caps::std_env_source)
qwertty::commands::osc::sanitize_title
qwertty::commands::osc::set_icon_and_title
crate::selection::tmux_passthrough
```

Known OSC 9 programs are only Ghostty, iTerm2, Kitty, and WezTerm.
tmux passthrough wins independently of the program classification.

The writer API is generic:

```rust
pub(crate) fn write_pending<W: Write>(
    &mut self,
    writer: &mut W,
    status: AppStatus,
) -> io::Result<usize>;
```

Write at most one title plus eight notifications, flush once when bytes were written, and return the first error.

- [ ] **Step 5: Run GREEN and commit**

```sh
cargo test -p orca-tui terminal_presentation --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/terminal_presentation.rs crates/orca-tui/src/lib.rs
git commit -m "feat(tui): encode terminal presentation output" \
  -m "Model bounded focus-aware notifications and deduplicated terminal titles as pure output intents." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 3: Enable and Consume Focus Events

**Files:**
- Modify: `crates/orca-tui/src/input_runtime.rs`
- Modify: `crates/orca-tui/src/input_event_actions.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [ ] **Step 1: Write failing qwertty startup-order tests**

Replace the positional runtime start argument with:

```rust
pub(crate) struct InputRuntimeOptions {
    pub(crate) theme: ThemeName,
    pub(crate) focus_events: bool,
}
```

Extend fake-driver ordering:

```text
probe (Auto only)
alternate
mouse
paste
focus (enabled only)
keyboard
ready
```

Assert disabled focus skips only `focus`, preserving every other transition.

- [ ] **Step 2: Write failing focus-consumption tests**

Test a pure helper:

```rust
fn consume_focus_event(
    event: &Event,
    presentation: &mut TerminalPresentation,
) -> bool;
```

Assert FocusLost/FocusGained mutate only presentation focus and return `true`.
Keys, paste, resize, and mouse return `false`.

- [ ] **Step 3: Run RED**

```sh
cargo test -p orca-tui focus_events --lib
cargo test -p orca-tui consume_focus --lib
```

- [ ] **Step 4: Implement focus lifecycle**

Add driver method:

```rust
async fn enable_focus_events(&mut self) -> io::Result<()>;
```

Call it after bracketed paste and before kitty keyboard only when configured.

In the input event handler, consume focus before paste/resize/mouse/key handling:

```rust
if consume_focus_event(&ev, &mut presentation) {
    return Ok(None);
}
```

- [ ] **Step 5: Run GREEN and commit**

```sh
cargo test -p orca-tui focus_events --lib
cargo test -p orca-tui consume_focus --lib
cargo test -p orca-tui input_runtime --lib
cargo test -p orca-tui input_adapter --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/input_runtime.rs \
  crates/orca-tui/src/input_event_actions.rs crates/orca-tui/src/app.rs
git commit -m "feat(tui): track terminal focus" \
  -m "Enable qwertty focus reporting and consume focus changes before normal TUI input handling." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 4: Trigger Focus-Aware Notifications

**Files:**
- Modify: `crates/orca-tui/src/runtime_event_actions.rs`
- Modify: `crates/orca-tui/src/terminal_presentation.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [ ] **Step 1: Write failing trigger-matrix tests**

Add:

```rust
pub(crate) fn terminal_notification_for_event(
    event: &TuiEvent,
    state: &AppState,
) -> Option<TerminalNotification>;
```

Cover every row in the design matrix, focused suppression, disabled suppression, and allowlisted approval suppression.

Assert notification messages never contain prompt, target, preview, summary, or schema fields.

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui terminal_notification_for_event --lib
```

- [ ] **Step 3: Implement pre-update classification**

Classify before `state.update(event)` so approval allowlist and prior status are available:

```rust
let notification = terminal_notification_for_event(&tui_event, state);
state.update(tui_event);
if let Some(notification) = notification {
    presentation.enqueue(notification);
}
```

Pass `&mut TerminalPresentation` into `handle_runtime_event`.
Do not alter the existing workflow queue or desktop notification paths.

- [ ] **Step 4: Write failing main-writer tests**

Use an in-memory `CrosstermBackend<Vec<u8>>` or generic writer seam.
Assert presentation bytes are emitted before the draw callback and never from a worker thread.
Assert write failure does not mutate `AppState`.

- [ ] **Step 5: Integrate bounded writer**

After input/runtime processing and before `terminal.draw`:

```rust
let writer = terminal.backend_mut().inner_mut();
let _ = presentation.write_pending(writer, state.status);
```

Do not call `terminal.clear()` for OSC/BEL output.

- [ ] **Step 6: Run GREEN and commit**

```sh
cargo test -p orca-tui terminal_notification --lib
cargo test -p orca-tui terminal_presentation --lib
cargo test -p orca-tui runtime_event_actions --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/runtime_event_actions.rs \
  crates/orca-tui/src/terminal_presentation.rs crates/orca-tui/src/app.rs
git commit -m "feat(tui): notify when terminal is unfocused" \
  -m "Emit bounded OSC 9 or BEL alerts for interaction and completion events only while unfocused." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 5: Integrate Animated Titles and Cleanup

**Files:**
- Modify: `crates/orca-tui/src/terminal_presentation.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [ ] **Step 1: Write failing animation/wake tests**

Assert presentation animation is active only for:

```rust
AppStatus::Running | AppStatus::Compacting | AppStatus::WaitingApproval
```

Assert the event-loop timeout wakes at the existing 80 ms cadence for title animation and advances presentation tick once.

- [ ] **Step 2: Write failing resume/exit ordering tests**

Assert:

```text
qwertty Resumed
ratatui clear
presentation invalidate title
dirty repaint
```

Assert orderly exit:

```text
write title Orca
flush writer
drop ratatui
finish qwertty
```

- [ ] **Step 3: Run RED**

```sh
cargo test -p orca-tui terminal_title --lib
cargo test -p orca-tui presentation_resume --lib
cargo test -p orca-tui presentation_exit --lib
```

- [ ] **Step 4: Implement animation and lifecycle**

Include presentation animation in `animation_active`.
When animation is due:

```rust
presentation.advance_tick();
scheduler.did_animate(now);
```

After qwertty resume:

```rust
resume_terminal_render(&mut terminal, &mut scheduler)?;
presentation.invalidate_title();
```

Before `drop(terminal)`:

```rust
let _ = presentation.write_reset_title(
    terminal.backend_mut().inner_mut(),
);
```

- [ ] **Step 5: Run GREEN and commit**

```sh
cargo test -p orca-tui terminal_title --lib
cargo test -p orca-tui presentation_ --lib
cargo test -p orca-tui hardware_cursor --lib
cargo test -p orca-tui ui::tests --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/terminal_presentation.rs crates/orca-tui/src/app.rs
git commit -m "feat(tui): animate terminal status titles" \
  -m "Show running and interaction-required status in the terminal tab and restore Orca on exit." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 6: Final Audit, Review, and Delivery

**Files:**
- Verify every file above.

- [ ] **Step 1: Run focused verification**

```sh
cargo test -p orca-core terminal_notifications --lib
cargo test -p orca-tui terminal_presentation --lib
cargo test -p orca-tui terminal_notification --lib
cargo test -p orca-tui focus_events --lib
cargo test -p orca-tui terminal_title --lib
cargo test -p orca-tui hardware_cursor --lib
cargo test -p orca-tui input_runtime --lib
cargo test -p orca-tui input_adapter --lib
```

- [ ] **Step 2: Run package/workspace gates**

```sh
cargo test -p orca-tui -- --test-threads=1
cargo test --workspace --all-targets -- --test-threads=1
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 3: Prompt-to-artifact audit**

| Requirement | Direct evidence |
|---|---|
| OSC 9 on compatible terminals | exact encoder/profile tests |
| tmux DCS passthrough | exact doubled-ESC tests |
| BEL fallback | unknown-profile encoder tests |
| notify only when unfocused | focus-state trigger matrix |
| existing desktop notifications unchanged | independent config and source diff |
| dynamic running spinner title | title matrix/animation tests |
| flashing `[!]` approval title | six-tick title tests |
| requested-input title | WaitingUserInput title test |
| title restored on exit | completed-frame ordering test |
| no competing terminal writer | source audit and main-writer tests |
| focus mode lifecycle-safe | qwertty fake-driver ordering/leave tests |
| no injection or unbounded queue | sanitizer, cap, queue tests |
| no P0 #6 scope | diff review |

Treat missing evidence as incomplete.

- [ ] **Step 4: Request specification and quality reviews**

Review:

```text
docs/superpowers/specs/2026-07-28-tui-notifications-title-design.md
```

Fix every Critical/Important finding and rerun the full gates.

- [ ] **Step 5: Verify commit trailers and clean scope**

```sh
git status --short
git log --format='%h %s%n%(trailers:key=Co-authored-by,valueonly)' \
  13d154b75bfdd700e6d6650b0bbe50f18b85d66c..HEAD
git diff --check 13d154b75bfdd700e6d6650b0bbe50f18b85d66c..HEAD
git diff --stat 13d154b75bfdd700e6d6650b0bbe50f18b85d66c..HEAD
```

- [ ] **Step 6: Push and verify**

```sh
git push origin feature/tui-syntax-highlighting
local_sha=$(git rev-parse HEAD)
remote_sha=$(git ls-remote --heads origin feature/tui-syntax-highlighting | awk '{print $1}')
test "$local_sha" = "$remote_sha"
```

Keep the branch for P0 #6.
