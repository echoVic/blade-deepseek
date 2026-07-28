# TUI Queued Message Preview Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let users compose visible FIFO follow-up messages during a running turn, preview them in at most three rows, and restore the newest queued message with `Alt+Up`.

**Architecture:** A new `QueuedUserMessage` value owns visible text, expanded submission text, mention bindings, and large-paste payloads. `AppState` is the only mutable owner of the pre-dispatch FIFO and an admission-fence item; focused action helpers edit, queue, restore, and promote messages. Runtime boundaries promote exactly one item with nonblocking channel admission, while the UI renders only an O(1) head/tail snapshot outside the transcript cache.

**Tech Stack:** Rust, ratatui 0.29, tui-textarea, crossterm, crossbeam-channel, existing Orca mention/paste/shortcut/streaming infrastructure.

---

## Scope and Baseline

Implementation baseline:

```text
bad16c2 docs(tui): finalize queued follow-up contracts
```

Design authority:

```text
docs/superpowers/specs/2026-07-28-tui-queued-message-preview-design.md
```

Do not implement:

- cwd or git-branch footer content;
- Vim counts, `dd`, `gg`, `G`, registers, dot-repeat, or `jj`;
- configurable keybindings or terminal-specific `Alt+Up` alternatives;
- `/doctor`, FPS HUD, or onboarding changes;
- active-turn steer semantics;
- queue persistence across process restart or session switch.

Every commit in this plan must end exactly once with:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

## File Map

### Create

- `crates/orca-tui/src/queued_input.rs`
  - immutable queued message/composer restore values;
  - visible/submission binding reconciliation;
  - preview normalization and O(1) snapshot.
- `crates/orca-tui/src/queued_input_actions.rs`
  - enqueue from composer;
  - restore newest queue item;
  - promote one FIFO item with nonblocking channel admission;
  - Running composer key routing.

### Modify

- `crates/orca-tui/src/lib.rs`
  - register both modules.
- `crates/orca-tui/src/display_text.rs`
  - make display-width truncation extended-grapheme safe.
- `crates/orca-tui/src/types.rs`
  - own FIFO, admission fence, autosend flag, reducer methods, lifecycle reset.
- `crates/orca-tui/src/shortcuts.rs`
  - Running submit/newline and Idle/Running `Alt+Up`.
- `crates/orca-tui/src/status_key_actions.rs`
  - route Running input to the focused handler.
- `crates/orca-tui/src/input_event_actions.rs`
  - allow Running paste.
- `crates/orca-tui/src/mention_search_manager.rs`
  - enable Running mention lookup.
- `crates/orca-tui/src/composer_input_actions.rs`
  - keep slash menu idle-only.
- `crates/orca-tui/src/idle_key_actions.rs`
  - restore latest queued item in Idle.
- `crates/orca-tui/src/idle_navigation_actions.rs`
  - cover the new Idle shortcut variant.
- `crates/orca-tui/src/idle_submit_actions.rs`
  - resume autosend on a new user foreground turn.
- `crates/orca-tui/src/running_actions.rs`
  - pause autosend on interrupt and promote after backgrounding.
- `crates/orca-tui/src/global_actions.rs`
  - pause autosend on Running `Ctrl+C`.
- `crates/orca-tui/src/runtime_event_actions.rs`
  - terminal-boundary promotion, workflow ordering, exact rejection restore.
- `crates/orca-tui/src/ui.rs`
  - bounded preview rows and layout.
- `crates/orca-tui/src/app.rs`
  - completed event-loop and hosted-controller integration tests.

---

### Task 1: Add Grapheme-Safe Display Truncation

**Files:**
- Modify: `crates/orca-tui/src/display_text.rs`

- [ ] **Step 1: Write failing grapheme-preservation tests**

Add:

```rust
#[test]
fn truncation_never_splits_extended_graphemes() {
    for grapheme in ["e\u{301}", "👍🏽", "👨‍👩‍👧‍👦", "1️⃣"] {
        let grapheme_width = unicode_width::UnicodeWidthStr::width(grapheme);
        assert_eq!(
            truncate_to_display_width(
                &format!("{grapheme}x"),
                grapheme_width + 1,
            ),
            format!("{grapheme}x"),
            "{grapheme:?}"
        );
        assert_eq!(
            truncate_to_display_width(
                &format!("{grapheme}x"),
                grapheme_width,
            ),
            "…",
            "{grapheme:?}"
        );
    }
}
```

Add a direct expected-value table:

```rust
#[test]
fn truncation_keeps_combining_emoji_and_keycap_clusters_atomic() {
    assert_eq!(truncate_to_display_width("e\u{301}x", 1), "…");
    assert_eq!(truncate_to_display_width("e\u{301}xy", 2), "e\u{301}…");
    assert_eq!(truncate_to_display_width("👍🏽x", 2), "…");
    assert_eq!(truncate_to_display_width("👍🏽xy", 3), "👍🏽…");
    assert_eq!(truncate_to_display_width("1️⃣x", 2), "…");
    assert_eq!(truncate_to_display_width("1️⃣xy", 3), "1️⃣…");
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui truncation_never_splits_extended --lib
cargo test -p orca-tui truncation_keeps_combining --lib
```

Expected: at least one combining/emoji assertion fails because the current
implementation iterates scalar characters.

- [ ] **Step 3: Implement grapheme-safe truncation**

Import:

```rust
use unicode_segmentation::UnicodeSegmentation;
```

Replace the scalar loop with:

```rust
for grapheme in text.graphemes(true) {
    let grapheme_width = UnicodeWidthStr::width(grapheme);
    if width + grapheme_width > content_width {
        break;
    }
    truncated.push_str(grapheme);
    width += grapheme_width;
}
```

Keep the existing max-width, ellipsis, ASCII, and CJK behavior unchanged.

- [ ] **Step 4: Run GREEN and regressions**

```sh
cargo test -p orca-tui truncation_ --lib
cargo test -p orca-tui display_text --lib
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 5: Commit**

```sh
git add crates/orca-tui/src/display_text.rs
git commit -m "fix(tui): truncate display text by grapheme" \
  -m "Keep combining sequences, emoji modifiers, ZWJ families, and keycaps intact when bounded UI labels are shortened." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 2: Create the Atomic Queued Input Value

**Files:**
- Create: `crates/orca-tui/src/queued_input.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [ ] **Step 1: Register the module and write failing construction tests**

Add to `lib.rs`:

```rust
mod queued_input;
```

Create `queued_input.rs` with test imports and these tests first:

```rust
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use orca_runtime::mentions::{
        MentionBinding, MentionBindings, MentionFileKind, MentionTarget,
    };

    use super::*;

    fn binding(text: &str, visible: &str) -> MentionBindings {
        let start = text.find(visible).expect("visible mention");
        MentionBindings::from_bindings(
            text,
            vec![MentionBinding {
                start,
                end: start + visible.len(),
                visible: visible.to_string(),
                target: MentionTarget::File {
                    root: PathBuf::from("/workspace"),
                    path: visible.trim_start_matches('@').to_string(),
                    kind: MentionFileKind::File,
                },
            }],
        )
    }

    #[test]
    fn queued_message_rejects_blank_input_and_preserves_atomic_composer_state() {
        assert!(
            QueuedUserMessage::from_composer(
                " \n ".to_string(),
                Vec::new(),
                MentionBindings::default(),
            )
            .is_none()
        );

        let visible = "review @item.rs [Pasted Content 1001 chars]";
        let pasted = "body\n".repeat(201);
        let message = QueuedUserMessage::from_composer(
            visible.to_string(),
            vec![(
                "[Pasted Content 1001 chars]".to_string(),
                pasted.clone(),
            )],
            binding(visible, "@item.rs"),
        )
        .expect("queued message");

        assert_eq!(message.visible_text(), visible);
        assert_eq!(
            message.submission_text(),
            format!("review @item.rs {}", pasted.trim())
        );
        assert_eq!(message.composer_bindings().bindings().len(), 1);
        assert_eq!(message.submission_bindings().bindings().len(), 1);

        let restored = message.into_composer_state();
        assert_eq!(restored.visible_text, visible);
        assert_eq!(restored.pending_pastes.len(), 1);
        assert_eq!(restored.mention_bindings.bindings().len(), 1);
    }
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui queued_message_rejects_blank --lib
```

Expected: `queued_input` types do not exist.

- [ ] **Step 3: Implement values and construction**

Use:

```rust
use orca_runtime::mentions::MentionBindings;

use crate::composer_textarea::expand_pending_pastes;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueuedUserMessage {
    visible_text: String,
    submission_text: String,
    composer_bindings: MentionBindings,
    submission_bindings: MentionBindings,
    pending_pastes: Vec<(String, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueuedComposerState {
    pub(crate) visible_text: String,
    pub(crate) mention_bindings: MentionBindings,
    pub(crate) pending_pastes: Vec<(String, String)>,
}
```

Implement `from_composer` exactly:

1. reconcile a clone of bindings against the untrimmed visible text;
2. trim visible text and reconcile composer bindings against that trim;
3. return `None` if empty;
4. expand pending pastes from the untrimmed visible text;
5. trim expanded text;
6. clone the original bindings and reconcile through expanded then trimmed text;
7. retain only placeholder payloads referenced by the trimmed visible text;
8. store both binding forms.

Expose:

```rust
pub(crate) fn visible_text(&self) -> &str;
pub(crate) fn submission_text(&self) -> &str;
pub(crate) fn composer_bindings(&self) -> &MentionBindings;
pub(crate) fn submission_bindings(&self) -> &MentionBindings;
pub(crate) fn pending_pastes(&self) -> &[(String, String)];
pub(crate) fn into_composer_state(self) -> QueuedComposerState;
```

- [ ] **Step 4: Add preview normalization tests**

```rust
#[test]
fn queued_preview_collapses_whitespace_and_never_expands_large_paste() {
    let visible = "alpha\n  beta [Pasted Content 1001 chars]";
    let message = QueuedUserMessage::from_composer(
        visible.to_string(),
        vec![(
            "[Pasted Content 1001 chars]".to_string(),
            "secret payload\n".repeat(100),
        )],
        MentionBindings::default(),
    )
    .unwrap();

    assert_eq!(
        message.preview_text(),
        "alpha beta [Pasted Content 1001 chars]"
    );
    assert!(!message.preview_text().contains("secret payload"));
}
```

Implement:

```rust
pub(crate) fn preview_text(&self) -> String {
    self.visible_text.split_whitespace().collect::<Vec<_>>().join(" ")
}
```

- [ ] **Step 5: Run GREEN**

```sh
cargo test -p orca-tui queued_message --lib
cargo test -p orca-tui queued_preview --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 6: Commit**

```sh
git add crates/orca-tui/src/lib.rs crates/orca-tui/src/queued_input.rs
git commit -m "feat(tui): model queued follow-up input" \
  -m "Keep visible text, expanded submission content, mention bindings, and large-paste payloads in one restorable value." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 3: Add FIFO, LIFO Restore, Capacity, and Admission Fence

**Files:**
- Modify: `crates/orca-tui/src/queued_input.rs`
- Modify: `crates/orca-tui/src/types.rs`

- [ ] **Step 1: Add failing queue reducer tests in `types.rs`**

Add a test helper:

```rust
fn queued(text: &str) -> crate::queued_input::QueuedUserMessage {
    crate::queued_input::QueuedUserMessage::from_composer(
        text.to_string(),
        Vec::new(),
        orca_runtime::mentions::MentionBindings::default(),
    )
    .unwrap()
}
```

Add:

```rust
#[test]
fn queued_follow_ups_promote_fifo_restore_lifo_and_fence_admission() {
    let mut state = state();
    state.enqueue_user_message(queued("first")).unwrap();
    state.enqueue_user_message(queued("second")).unwrap();
    state.enqueue_user_message(queued("third")).unwrap();
    state.set_status(AppStatus::Idle);

    let action = state.begin_next_queued_message().expect("first action");
    assert!(matches!(
        action,
        UserAction::SubmitWithMentions { prompt, .. } if prompt == "first"
    ));
    assert_eq!(state.queued_user_messages.len(), 2);
    assert!(state.queued_submission_in_flight.is_some());
    assert!(state.begin_next_queued_message().is_none());
    assert!(matches!(
        state.messages.last(),
        Some(ChatMessage::User(text)) if text == "first"
    ));

    state.update(TuiEvent::TurnStarted {
        turn: 1,
        task: None,
    });
    assert!(state.queued_submission_in_flight.is_none());
    assert_eq!(
        state.pop_latest_queued_message().unwrap().visible_text(),
        "third"
    );
    assert_eq!(
        state.queued_user_messages.front().unwrap().visible_text(),
        "second"
    );
}
```

- [ ] **Step 2: Add failing exact-capacity test**

```rust
#[test]
fn queued_follow_up_capacity_matches_user_action_mailbox() {
    let mut state = state();
    for index in 0..crate::channels::USER_ACTION_CAPACITY {
        state
            .enqueue_user_message(queued(&format!("queued {index}")))
            .unwrap();
    }
    let rejected = state
        .enqueue_user_message(queued("overflow"))
        .expect_err("65th item rejected");
    assert_eq!(rejected.visible_text(), "overflow");
    assert_eq!(
        state.queued_user_messages.len(),
        crate::channels::USER_ACTION_CAPACITY
    );
}
```

- [ ] **Step 3: Run RED**

```sh
cargo test -p orca-tui queued_follow_ups_promote --lib
cargo test -p orca-tui queued_follow_up_capacity --lib
```

Expected: queue fields and reducer methods are missing.

- [ ] **Step 4: Add AppState fields and defaults**

Import `QueuedUserMessage` and add:

```rust
pub(crate) queued_user_messages: VecDeque<QueuedUserMessage>,
pub(crate) queued_submission_in_flight: Option<QueuedUserMessage>,
pub(crate) queued_follow_up_autosend: bool,
pub(crate) queued_input_error: Option<String>,
```

Initialize with:

```rust
queued_user_messages: VecDeque::new(),
queued_submission_in_flight: None,
queued_follow_up_autosend: true,
queued_input_error: None,
```

- [ ] **Step 5: Implement reducer methods**

Add:

```rust
pub(crate) fn enqueue_user_message(
    &mut self,
    message: QueuedUserMessage,
) -> Result<(), QueuedUserMessage> {
    if self.queued_user_messages.len() >= crate::channels::USER_ACTION_CAPACITY {
        return Err(message);
    }
    self.queued_user_messages.push_back(message);
    Ok(())
}

pub(crate) fn pop_latest_queued_message(&mut self) -> Option<QueuedUserMessage> {
    self.queued_user_messages.pop_back()
}
```

Implement `begin_next_queued_message` with the eight design gates. It clones the
item into `queued_submission_in_flight` and returns:

```rust
UserAction::SubmitWithMentions {
    prompt: message.submission_text().to_string(),
    bindings: message.submission_bindings().clone(),
}
```

Add:

```rust
pub(crate) fn commit_queued_submission_admission(&mut self);
pub(crate) fn rollback_queued_submission(&mut self) -> Option<QueuedUserMessage>;
pub(crate) fn take_rejected_queued_composer_state(
    &mut self,
) -> Option<QueuedComposerState>;
pub(crate) fn suspend_queued_follow_up_autosend(&mut self);
pub(crate) fn resume_queued_follow_up_autosend(&mut self);
pub(crate) fn queued_follow_up_pending_or_in_flight(&self) -> bool;
```

Rollback removes the optimistic final user turn via `remove_after_last_user`,
pushes the item back to the FIFO front, and returns to Idle.
`commit_queued_submission_admission` records the in-flight visible prompt in
input history only after the action channel accepts the submission.

- [ ] **Step 6: Integrate lifecycle resets**

`TurnStarted` clears `queued_submission_in_flight`.

`clear_messages` and `replace_messages` clear:

```rust
self.queued_user_messages.clear();
self.queued_submission_in_flight = None;
self.queued_follow_up_autosend = true;
self.queued_input_error = None;
```

Do not clear queue state on ordinary `SessionCompleted`, truncation, retain, or
transcript search reset.

- [ ] **Step 7: Run GREEN**

```sh
cargo test -p orca-tui queued_follow_up --lib
cargo test -p orca-tui clear_resets_search --lib
cargo test -p orca-tui replacing_messages --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 8: Commit**

```sh
git add crates/orca-tui/src/queued_input.rs crates/orca-tui/src/types.rs
git commit -m "feat(tui): own queued follow-up state" \
  -m "Add bounded FIFO promotion, LIFO restoration, exact admission fencing, and conversation-boundary reset semantics." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 4: Add Shortcuts and Pure Composer Queue Actions

**Files:**
- Create: `crates/orca-tui/src/queued_input_actions.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/shortcuts.rs`
- Modify: `crates/orca-tui/src/composer_input_actions.rs`

- [ ] **Step 1: Write failing shortcut resolver tests**

Add shortcut variants:

```rust
IdleShortcut::EditLatestQueued,
RunningShortcut::SubmitQueued,
RunningShortcut::Newline,
RunningShortcut::EditLatestQueued,
```

Write tests before bindings:

```rust
#[test]
fn queued_message_shortcuts_are_context_specific() {
    assert_eq!(
        resolve_shortcut(
            ShortcutContext::Idle,
            key(KeyCode::Up, KeyModifiers::ALT)
        ),
        Some(ShortcutAction::Idle(IdleShortcut::EditLatestQueued))
    );
    assert_eq!(
        resolve_shortcut(
            ShortcutContext::Running,
            key(KeyCode::Up, KeyModifiers::ALT)
        ),
        Some(ShortcutAction::Running(RunningShortcut::EditLatestQueued))
    );
    assert_eq!(
        resolve_shortcut(
            ShortcutContext::Running,
            key(KeyCode::Enter, KeyModifiers::NONE)
        ),
        Some(ShortcutAction::Running(RunningShortcut::SubmitQueued))
    );
    for modifiers in [
        KeyModifiers::SHIFT,
        KeyModifiers::ALT,
    ] {
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Running,
                key(KeyCode::Enter, modifiers)
            ),
            Some(ShortcutAction::Running(RunningShortcut::Newline))
        );
    }
}
```

Add `Ctrl+J` to the Running newline assertions.

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui queued_message_shortcuts --lib
```

Expected: variants/bindings are missing.

- [ ] **Step 3: Register bindings and hints**

Add Idle `Alt+Up`, Running Enter/newline/`Alt+Up` entries. Add backed hints:

```text
Running · enter · queue follow-up
Running · alt+enter / shift+enter · insert newline
Running · alt+up · edit latest queued message
Composer · alt+up · edit latest queued message
```

Update exhaustive matches in `idle_navigation_actions.rs` and
`running_actions.rs` temporarily with no-op arms for variants owned by the new
focused handler.

- [ ] **Step 4: Create queue action helpers and failing tests**

Register:

```rust
mod queued_input_actions;
```

Write tests:

```rust
#[test]
fn enqueue_from_composer_clears_only_after_acceptance() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut state = state(tx);
    state.enter_running();
    let theme = theme();
    let mut vim = VimState::new(false);
    let mut textarea = make_textarea_with_text("follow up", &vim, &theme);

    assert!(enqueue_composer_follow_up(
        &mut state,
        &mut textarea,
        &mut vim,
        &theme,
    ));
    assert_eq!(state.queued_user_messages.len(), 1);
    assert_eq!(textarea_text(&textarea), "");
    assert!(rx.try_recv().is_err());
    assert_eq!(state.status, AppStatus::Running);
}

#[test]
fn full_queue_keeps_composer_and_emits_no_action() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut state = state(tx);
    state.enter_running();
    for index in 0..crate::channels::USER_ACTION_CAPACITY {
        state.enqueue_user_message(queued(&format!("{index}"))).unwrap();
    }
    let theme = theme();
    let mut vim = VimState::new(false);
    let mut textarea = make_textarea_with_text("keep me", &vim, &theme);

    assert!(!enqueue_composer_follow_up(
        &mut state,
        &mut textarea,
        &mut vim,
        &theme,
    ));
    assert_eq!(textarea_text(&textarea), "keep me");
    assert!(rx.try_recv().is_err());
    assert!(state.queued_input_error.is_some());
    assert!(
        !state
            .messages
            .iter()
            .any(|message| matches!(message, ChatMessage::Error(_)))
    );
}
```

- [ ] **Step 5: Implement enqueue and restore**

`enqueue_composer_follow_up`:

1. read visible textarea text;
2. move `pending_pastes` and `mention_bindings` only after queue construction;
3. call `QueuedUserMessage::from_composer`;
4. on capacity rejection, leave textarea/bindings/pastes byte-identical and set
   one bounded `queued_input_error`;
5. on success, clear menus/bindings/pastes, reset history navigation, and reset
   composer with the existing submit convention.

Implement:

```rust
pub(crate) fn restore_latest_queued_message(
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
) -> bool;
```

Guard on Conversation panel, Idle/Running, search closed, shortcuts closed,
slash menu absent, and mention projection absent.

On success:

```rust
vim_state.reset_insert(textarea, theme);
*textarea = make_textarea_with_text(
    &composer.visible_text,
    vim_state,
    theme,
);
state.mention_bindings = composer.mention_bindings;
state.pending_pastes = composer.pending_pastes;
state.reset_history_navigation();
```

Reapply `vim_state.configure_block` after textarea replacement so title/cursor
match the reset mode.

- [ ] **Step 6: Keep slash menu idle-only**

Change `refresh_input_menus`:

```rust
if state.status == AppStatus::Idle {
    update_slash_menu(textarea, state, config);
} else {
    state.slash_menu = None;
}
```

Add:

```rust
#[test]
fn running_slash_text_never_opens_local_command_menu() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut state = AppState::new(
        tx,
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    state.enter_running();
    let config = crate::test_support::test_run_config();
    let theme = Theme::named(orca_core::config::ThemeName::Dark);
    let mut vim = VimState::new(false);
    let mut textarea = make_textarea_with_text("/compact", &vim, &theme);
    let event = Event::Key(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
    ));
    let Event::Key(key) = event else { unreachable!() };

    apply_composer_key_input(
        &Event::Key(key),
        &key,
        &mut state,
        &config,
        &mut textarea,
        &mut vim,
        &theme,
    );

    assert_eq!(textarea_text(&textarea), "/compactx");
    assert!(state.slash_menu.is_none());
}
```

- [ ] **Step 7: Run GREEN**

```sh
cargo test -p orca-tui queued_message_shortcuts --lib
cargo test -p orca-tui enqueue_from_composer --lib
cargo test -p orca-tui full_queue_keeps_composer --lib
cargo test -p orca-tui running_slash_text --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 8: Commit**

```sh
git add crates/orca-tui/src/lib.rs \
  crates/orca-tui/src/queued_input_actions.rs \
  crates/orca-tui/src/shortcuts.rs \
  crates/orca-tui/src/composer_input_actions.rs \
  crates/orca-tui/src/idle_navigation_actions.rs \
  crates/orca-tui/src/running_actions.rs
git commit -m "feat(tui): add queued follow-up actions" \
  -m "Register Running submit/newline and Alt+Up shortcuts with atomic composer queue and restore helpers." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 5: Route Running Editing, Mentions, Paste, and Vim

**Files:**
- Modify: `crates/orca-tui/src/queued_input_actions.rs`
- Modify: `crates/orca-tui/src/status_key_actions.rs`
- Modify: `crates/orca-tui/src/idle_key_actions.rs`
- Modify: `crates/orca-tui/src/input_event_actions.rs`
- Modify: `crates/orca-tui/src/mention_search_manager.rs`

- [ ] **Step 1: Write failing Running input tests**

In `status_key_actions.rs` tests, add this helper beside the existing
`config()` helper:

```rust
#[allow(clippy::too_many_arguments)]
fn press_status_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    state: &mut AppState,
    config: &mut RunConfig,
    shared: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    operation: &TestOperationInterrupt,
    textarea: &mut TextArea,
    vim: &mut VimState,
    theme: &Theme,
) {
    let key = KeyEvent::new(code, modifiers);
    let preloaded = Arc::new(Mutex::new(None));
    handle_status_key(
        &Event::Key(key),
        &key,
        state,
        config,
        shared,
        action_tx,
        operation,
        &preloaded,
        textarea,
        vim,
        theme,
        None,
        || Ok(()),
    )
    .unwrap();
}
```

Add:

```rust
#[test]
fn running_composer_edits_newlines_queues_and_keeps_scroll_shortcuts() {
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    state.enter_running();
    state.total_lines = 20;
    state.visible_height = 5;
    state.scroll_offset = 10;
    state.auto_scroll = false;
    let mut config = config();
    let shared = Arc::new(Mutex::new(config.clone()));
    let operation = TestOperationInterrupt::default();
    let theme = Theme::named(ThemeName::Dark);
    let mut vim = VimState::new(false);
    let mut textarea = TextArea::default();

    press_status_key(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &mut textarea,
        &mut vim,
        &theme,
    );
    assert_eq!(textarea.lines(), &["x".to_string()]);

    press_status_key(
        KeyCode::Enter,
        KeyModifiers::SHIFT,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &mut textarea,
        &mut vim,
        &theme,
    );
    assert_eq!(textarea.lines(), &["x".to_string(), String::new()]);

    assert!(textarea.insert_str("/compact"));
    press_status_key(
        KeyCode::Enter,
        KeyModifiers::NONE,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &mut textarea,
        &mut vim,
        &theme,
    );
    assert_eq!(state.queued_user_messages.len(), 1);
    assert_eq!(
        state.queued_user_messages[0].visible_text(),
        "x\n/compact"
    );
    assert!(textarea.is_empty());
    assert_eq!(state.status, AppStatus::Running);
    assert!(action_rx.try_recv().is_err());

    press_status_key(
        KeyCode::Up,
        KeyModifiers::NONE,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &mut textarea,
        &mut vim,
        &theme,
    );
    assert_eq!(state.scroll_offset, 9);
    assert!(action_rx.try_recv().is_err());
}
```

- [ ] **Step 2: Add failing mention and Vim tests**

```rust
#[test]
fn running_mention_enter_selects_before_queueing() {
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/workspace".to_string(),
    );
    state.enter_running();
    state.mention.candidates = vec![MentionCandidate::from_file_match(
        &orca_file_search::SearchMatch {
            root: PathBuf::from("/workspace"),
            path: "item.rs".to_string(),
            kind: orca_file_search::MatchKind::File,
            score: 42,
            indices: vec![0],
        },
    )];
    state.mention.phase = Some(orca_file_search::SearchPhase::Complete);
    let mut config = config();
    let shared = Arc::new(Mutex::new(config.clone()));
    let operation = TestOperationInterrupt::default();
    let theme = Theme::named(ThemeName::Dark);
    let mut vim = VimState::new(false);
    let mut textarea = make_textarea_with_text("@ite", &vim, &theme);

    press_status_key(
        KeyCode::Enter,
        KeyModifiers::NONE,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &mut textarea,
        &mut vim,
        &theme,
    );
    assert_eq!(textarea_text(&textarea), "@item.rs");
    assert_eq!(state.mention_bindings.bindings().len(), 1);
    assert!(state.queued_user_messages.is_empty());

    press_status_key(
        KeyCode::Enter,
        KeyModifiers::NONE,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &mut textarea,
        &mut vim,
        &theme,
    );
    assert_eq!(state.queued_user_messages.len(), 1);
    assert_eq!(
        state.queued_user_messages[0]
            .submission_bindings()
            .bindings()
            .len(),
        1
    );
    assert!(action_rx.try_recv().is_err());
}

#[test]
fn running_vim_edits_and_queued_submit_uses_existing_reset_mode() {
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    state.enter_running();
    let mut config = config();
    config.vim_mode = true;
    let shared = Arc::new(Mutex::new(config.clone()));
    let operation = TestOperationInterrupt::default();
    let theme = Theme::named(ThemeName::Dark);
    let mut vim = VimState::new(true);
    vim.mode = crate::vim::VimMode::Insert;
    let mut textarea = TextArea::default();

    press_status_key(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &mut textarea,
        &mut vim,
        &theme,
    );
    assert_eq!(textarea_text(&textarea), "x");

    press_status_key(
        KeyCode::Enter,
        KeyModifiers::NONE,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &mut textarea,
        &mut vim,
        &theme,
    );
    assert_eq!(state.queued_user_messages.len(), 1);
    assert_eq!(vim.mode, crate::vim::VimMode::Normal);
    assert!(textarea.is_empty());
    assert!(action_rx.try_recv().is_err());
}
```

The mention test installs one real `MentionCandidate`, presses Enter once, and
asserts insertion with queue length zero; a second Enter queues.

The Vim test covers Insert character editing and queue submission, then asserts
the same post-submit Vim mode as idle submit.

- [ ] **Step 3: Run RED**

```sh
cargo test -p orca-tui running_composer_edits --lib
cargo test -p orca-tui running_mention_enter --lib
cargo test -p orca-tui running_vim_edits --lib
```

Expected: Running status does not route ordinary composer input.

- [ ] **Step 4: Implement `handle_running_key`**

Add to `queued_input_actions.rs`:

```rust
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_running_key(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &RunConfig,
    action_tx: &mpsc::Sender<UserAction>,
    operation: &impl TuiOperationInterrupt,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
) -> bool;
```

Order:

1. if panel is not Conversation, delegate only existing running controls;
2. mention popup selection/dismissal;
3. match Running shortcut:
   - edit latest;
   - submit queued;
   - newline;
   - existing background/interrupt/scroll;
4. otherwise call `apply_composer_key_input`.

Do not call slash menu actions.

- [ ] **Step 5: Route status handling**

Replace the Running-only shortcut block in `status_key_actions.rs` with one
`handle_running_key` call. Keep:

- Setup;
- SessionPicker;
- WaitingApproval;
- transcript Vim intent;
- Idle/WaitingUserInput;
- Compacting

in their current relative order.

- [ ] **Step 6: Route Idle `Alt+Up`**

In `idle_key_actions.rs`, after slash/mention/workflow ownership and before
ordinary navigation, handle `IdleShortcut::EditLatestQueued` via the restore
helper only when `state.status == AppStatus::Idle`.

WaitingUserInput must leave the queue untouched.

- [ ] **Step 7: Allow Running paste**

Change paste status:

```rust
AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput => {
    if insert_composer_paste(textarea, &mut state.pending_pastes, pasted) {
        state.reset_history_navigation();
        refresh_input_menus(textarea, state, config);
    }
}
```

`refresh_input_menus` keeps slash menu closed in Running.

Add:

```rust
#[test]
fn running_large_paste_queues_placeholder_and_restores_payload() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut state = AppState::new(
        tx,
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    state.enter_running();
    let config = test_run_config();
    let theme = Theme::named(ThemeName::Dark);
    let mut vim = VimState::new(false);
    let mut textarea = TextArea::default();
    let pasted = "secret payload\n".repeat(100);

    assert!(handle_paste_event(
        &Event::Paste(pasted.clone()),
        &mut state,
        &config,
        &mut textarea,
    ));
    let placeholder = textarea_text(&textarea);
    assert!(placeholder.starts_with("[Pasted Content "));

    assert!(crate::queued_input_actions::enqueue_composer_follow_up(
        &mut state,
        &mut textarea,
        &mut vim,
        &theme,
    ));
    assert!(crate::queued_input_actions::restore_latest_queued_message(
        &mut state,
        &mut textarea,
        &mut vim,
        &theme,
    ));
    assert_eq!(textarea_text(&textarea), placeholder);
    assert_eq!(state.pending_pastes.len(), 1);
    assert_eq!(state.pending_pastes[0].1, pasted);
}
```

- [ ] **Step 8: Enable Running mention lookup**

Change `MentionSearchManager::is_enabled` to require Conversation panel and:

```rust
matches!(
    state.status,
    AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
)
```

Keep `slash_menu.is_none()`.

Add:

```rust
#[test]
fn mention_search_enablement_covers_only_editable_conversation_states() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut state = AppState::new(
        tx,
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    for (status, enabled) in [
        (AppStatus::Idle, true),
        (AppStatus::Running, true),
        (AppStatus::WaitingUserInput, true),
        (AppStatus::Compacting, false),
        (AppStatus::WaitingApproval, false),
        (AppStatus::Setup, false),
        (AppStatus::SessionPicker, false),
    ] {
        state.status = status;
        state.panel_mode = PanelMode::Conversation;
        state.slash_menu = None;
        assert_eq!(MentionSearchManager::is_enabled(&state), enabled, "{status:?}");
    }
    state.status = AppStatus::Running;
    for panel in [PanelMode::Workflows, PanelMode::Agents] {
        state.panel_mode = panel;
        assert!(!MentionSearchManager::is_enabled(&state), "{panel:?}");
    }
}
```

- [ ] **Step 9: Run GREEN and focused regressions**

```sh
cargo test -p orca-tui running_composer --lib
cargo test -p orca-tui running_mention --lib
cargo test -p orca-tui running_vim --lib
cargo test -p orca-tui running_large_paste --lib
cargo test -p orca-tui mention_search_manager --lib
cargo test -p orca-tui status_key_actions --lib
cargo test -p orca-tui search_ --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 10: Commit**

```sh
git add crates/orca-tui/src/queued_input_actions.rs \
  crates/orca-tui/src/status_key_actions.rs \
  crates/orca-tui/src/idle_key_actions.rs \
  crates/orca-tui/src/input_event_actions.rs \
  crates/orca-tui/src/mention_search_manager.rs
git commit -m "feat(tui): queue input during running turns" \
  -m "Reuse the composer, paste, mention, and Vim paths while keeping active search and modal input ownership unchanged." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 6: Promote One Follow-Up at Runtime Boundaries

**Files:**
- Modify: `crates/orca-tui/src/queued_input_actions.rs`
- Modify: `crates/orca-tui/src/runtime_event_actions.rs`
- Modify: `crates/orca-tui/src/idle_submit_actions.rs`
- Modify: `crates/orca-tui/src/running_actions.rs`
- Modify: `crates/orca-tui/src/global_actions.rs`

- [ ] **Step 1: Define dispatch outcome and failing admission tests**

Add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QueuedDispatch {
    Started,
    None,
    Blocked,
    Failed,
}
```

Write:

```rust
#[test]
fn queued_dispatch_sends_one_fifo_item_nonblocking() {
    let (action_tx, action_rx) = mpsc::bounded(1);
    let mut state = state(action_tx.clone());
    state.enqueue_user_message(queued("first")).unwrap();
    state.enqueue_user_message(queued("second")).unwrap();

    assert_eq!(
        dispatch_next_queued_user_message(&mut state, &action_tx),
        QueuedDispatch::Started
    );
    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitWithMentions { prompt, .. }) if prompt == "first"
    ));
    assert_eq!(state.queued_user_messages.len(), 1);
    assert_eq!(state.queued_user_messages[0].visible_text(), "second");
    assert!(state.queued_submission_in_flight.is_some());
    assert_eq!(state.input_history.last().map(String::as_str), Some("first"));
}

#[test]
fn full_and_disconnected_action_channels_restore_queue_front() {
    for disconnected in [false, true] {
        let (action_tx, action_rx) = mpsc::bounded(1);
        if disconnected {
            drop(action_rx);
        } else {
            action_tx
                .send(UserAction::Remember("occupy".to_string()))
                .unwrap();
        }
        let mut state = state(action_tx.clone());
        state.enqueue_user_message(queued("first")).unwrap();

        assert_eq!(
            dispatch_next_queued_user_message(&mut state, &action_tx),
            QueuedDispatch::Failed,
            "disconnected={disconnected}"
        );
        assert_eq!(state.status, AppStatus::Idle);
        assert_eq!(state.queued_user_messages.len(), 1);
        assert_eq!(state.queued_user_messages[0].visible_text(), "first");
        assert!(state.queued_submission_in_flight.is_none());
        assert!(
            !state
                .messages
                .iter()
                .any(|message| matches!(message, ChatMessage::User(text) if text == "first"))
        );
        assert!(!state.input_history.iter().any(|entry| entry == "first"));
        assert!(state.queued_input_error.is_some());
    }
}
```

The full-channel test uses `crossbeam_channel::bounded(1)`, pre-fills it, and
asserts:

- dispatch returns `Failed`;
- status returns Idle;
- optimistic user row is absent;
- original item is again at queue front;
- no item is lost.

The disconnected test drops the receiver and asserts the same.

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui queued_dispatch_sends_one --lib
cargo test -p orca-tui full_and_disconnected_action --lib
```

- [ ] **Step 3: Implement nonblocking dispatch**

```rust
pub(crate) fn dispatch_next_queued_user_message(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> QueuedDispatch;
```

Rules:

- return `Blocked` when admission fence is occupied;
- return `None` when not Idle, autosend false, or queue empty;
- call `begin_next_queued_message`;
- use `try_send`, never `send`;
- call `commit_queued_submission_admission` only after `try_send` succeeds;
- on `Full` or `Disconnected`, rollback and set one bounded
  `queued_input_error`;
- return `Started` only after accepted send.

- [ ] **Step 4: Add failing terminal-boundary ordering tests**

In `runtime_event_actions.rs`:

```rust
fn test_presentation() -> TerminalPresentation {
    TerminalPresentation::new(
        false,
        crate::terminal_presentation::TerminalPresentationProfile {
            osc9_supported: false,
            tmux_passthrough: false,
        },
    )
}

fn queue_text(state: &mut AppState, text: &str) {
    state
        .enqueue_user_message(
            crate::queued_input::QueuedUserMessage::from_composer(
                text.to_string(),
                Vec::new(),
                orca_runtime::mentions::MentionBindings::default(),
            )
            .unwrap(),
        )
        .unwrap();
}

#[test]
fn terminal_boundary_promotes_one_user_follow_up_before_workflow_notification() {
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    queue_text(&mut state, "first");
    queue_text(&mut state, "second");
    state.pending_workflow_notifications.push_back(
        crate::types::PendingWorkflowNotification {
            id: "workflow-1".to_string(),
            prompt: "internal workflow".to_string(),
        },
    );
    state.enter_running();
    let pending = bridge::PendingWorkflowNotifications::new();
    let theme = Theme::named(ThemeName::Dark);
    let mut textarea = TextArea::default();
    let mut vim = VimState::new(false);
    let mut presentation = test_presentation();

    for (turn, expected) in [(1, "first"), (2, "second")] {
        handle_runtime_event(
            TuiEvent::SessionCompleted {
                status: "success".to_string(),
            },
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim,
            &theme,
            &mut presentation,
        );
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWithMentions { prompt, .. }) if prompt == expected
        ));
        assert_eq!(state.pending_workflow_notifications.len(), 1);
        state.update(TuiEvent::TurnStarted { turn, task: None });
    }

    handle_runtime_event(
        TuiEvent::SessionCompleted {
            status: "success".to_string(),
        },
        &mut state,
        &action_tx,
        &pending,
        &mut textarea,
        &mut vim,
        &theme,
        &mut presentation,
    );
    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitWorkflowNotification(notification))
            if notification.id == "workflow-1"
    ));
}

#[test]
fn every_terminal_status_promotes_one_follow_up() {
    for status in ["success", "failed", "verification_failed", "cancelled"] {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        queue_text(&mut state, status);
        state.enter_running();
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = TextArea::default();
        let mut vim = VimState::new(false);
        let mut presentation = test_presentation();

        handle_runtime_event(
            TuiEvent::SessionCompleted {
                status: status.to_string(),
            },
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim,
            &theme,
            &mut presentation,
        );
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWithMentions { prompt, .. }) if prompt == status
        ));
    }
}

#[test]
fn occupied_admission_fence_blocks_late_background_terminal() {
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    queue_text(&mut state, "first");
    queue_text(&mut state, "second");
    state.set_status(AppStatus::Idle);
    assert_eq!(
        crate::queued_input_actions::dispatch_next_queued_user_message(
            &mut state,
            &action_tx,
        ),
        crate::queued_input_actions::QueuedDispatch::Started
    );
    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitWithMentions { prompt, .. }) if prompt == "first"
    ));
    state.set_status(AppStatus::Idle);

    let pending = bridge::PendingWorkflowNotifications::new();
    let theme = Theme::named(ThemeName::Dark);
    let mut textarea = TextArea::default();
    let mut vim = VimState::new(false);
    let mut presentation = test_presentation();
    handle_runtime_event(
        TuiEvent::SessionCompleted {
            status: "backgrounded".to_string(),
        },
        &mut state,
        &action_tx,
        &pending,
        &mut textarea,
        &mut vim,
        &theme,
        &mut presentation,
    );

    assert!(action_rx.try_recv().is_err());
    assert_eq!(state.queued_user_messages.len(), 1);
    assert_eq!(state.queued_user_messages[0].visible_text(), "second");
}
```

- [ ] **Step 5: Integrate `handle_runtime_event`**

After `state.update(tui_event)` and workflow-notification drain:

1. call queue dispatch for terminal boundaries;
2. call `submit_pending_workflow_notification` only when queue dispatch returns
   `None`;
3. do not submit workflow notification for `Started`, `Blocked`, or `Failed`.

Keep terminal notification and auto-scroll behavior unchanged.

- [ ] **Step 6: Add exact rejection restore tests**

```rust
#[test]
fn rejected_promoted_follow_up_restores_visible_paste_and_mentions() {
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/workspace".to_string(),
    );
    let visible = "review @item.rs [Pasted Content 1001 chars]";
    let pasted = "payload\n".repeat(150);
    state
        .enqueue_user_message(queued_message_with_binding_and_paste(
            visible,
            pasted.clone(),
        ))
        .unwrap();
    state.set_status(AppStatus::Idle);
    assert_eq!(
        crate::queued_input_actions::dispatch_next_queued_user_message(
            &mut state,
            &action_tx,
        ),
        crate::queued_input_actions::QueuedDispatch::Started
    );
    let prompt = match action_rx.try_recv().unwrap() {
        UserAction::SubmitWithMentions { prompt, .. } => prompt,
        other => panic!("unexpected action: {other:?}"),
    };
    let pending = bridge::PendingWorkflowNotifications::new();
    let theme = Theme::named(ThemeName::Dark);
    let mut textarea = TextArea::default();
    let mut vim = VimState::new(false);
    let mut presentation = test_presentation();

    handle_runtime_event(
        TuiEvent::SubmissionRejected {
            prompt,
            message: "rejected".to_string(),
        },
        &mut state,
        &action_tx,
        &pending,
        &mut textarea,
        &mut vim,
        &theme,
        &mut presentation,
    );

    assert_eq!(textarea_text(&textarea), visible);
    assert_eq!(state.pending_pastes[0].1, pasted);
    assert_eq!(state.mention_bindings.bindings().len(), 1);
    assert!(state.queued_submission_in_flight.is_none());
}

#[test]
fn ordinary_submission_rejection_keeps_existing_prompt_restore() {
    let (action_tx, _action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    state.push_message(ChatMessage::User("ordinary".to_string()));
    state.enter_running();
    let pending = bridge::PendingWorkflowNotifications::new();
    let theme = Theme::named(ThemeName::Dark);
    let mut textarea = TextArea::default();
    let mut vim = VimState::new(false);
    let mut presentation = test_presentation();

    handle_runtime_event(
        TuiEvent::SubmissionRejected {
            prompt: "ordinary".to_string(),
            message: "rejected".to_string(),
        },
        &mut state,
        &action_tx,
        &pending,
        &mut textarea,
        &mut vim,
        &theme,
        &mut presentation,
    );
    assert_eq!(textarea_text(&textarea), "ordinary");
    assert!(state.pending_pastes.is_empty());
}
```

Define `queued_message_with_binding_and_paste` in the same test module using the
real `MentionBinding` construction from Task 2.

Before `state.update`, detect whether the event is a `SubmissionRejected` and
the admission fence is occupied. After update:

- queued rejection consumes `take_rejected_queued_composer_state`;
- ordinary rejection uses the event prompt exactly as the existing test does.

Restore `pending_pastes`, `mention_bindings`, textarea visible text, and existing
Vim reset convention.

- [ ] **Step 7: Suspend and resume autosend**

`running_actions.rs`:

- Interrupt suspends autosend before control dispatch.
- Background sends `BackgroundCurrentTurn`, sets Idle, resumes autosend, then
  calls queue dispatch.

`global_actions.rs`:

- Running/Compacting/Waiting interaction `Ctrl+C` suspends autosend before
  interrupt.

`idle_submit_actions.rs`:

- a normal new user foreground turn resumes autosend before sending;
- WaitingUserInput response does not change autosend.

Add:

```rust
#[test]
fn queued_autosend_interrupt_matrix() {
    let (action_tx, action_rx) = mpsc::unbounded();
    let operation = TestOperationInterrupt::default();

    let mut running = state(action_tx.clone());
    running.enter_running();
    running.resume_queued_follow_up_autosend();
    handle_running_shortcut(
        RunningShortcut::Interrupt,
        &mut running,
        &action_tx,
        &operation,
    );
    assert!(!running.queued_follow_up_autosend);
    assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));

    let mut global = state(action_tx.clone());
    global.enter_running();
    global.resume_queued_follow_up_autosend();
    handle_global_shortcut(
        GlobalShortcut::Cancel,
        &mut global,
        &action_tx,
        &operation,
        || Ok(()),
    )
    .unwrap();
    assert!(!global.queued_follow_up_autosend);
    assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
    assert_eq!(operation.call_count(), 2);
}

#[test]
fn idle_submit_resumes_queued_autosend() {
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = state(action_tx.clone());
    state.suspend_queued_follow_up_autosend();
    let mut config = crate::test_support::test_run_config();
    let shared = Arc::new(Mutex::new(config.clone()));
    let theme = Theme::named(ThemeName::Dark);
    let mut vim = VimState::new(false);
    let mut textarea = make_textarea_with_text("new foreground", &vim, &theme);

    assert!(handle_idle_submit(
        &mut textarea,
        &mut vim,
        &theme,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
    ));
    assert!(state.queued_follow_up_autosend);
    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitWithMentions { prompt, .. })
            if prompt == "new foreground"
    ));
}

#[test]
fn background_control_precedes_one_queued_submit() {
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = state(action_tx.clone());
    state.enter_running();
    state.enqueue_user_message(queued("follow up")).unwrap();
    let operation = TestOperationInterrupt::default();

    handle_running_shortcut(
        RunningShortcut::BackgroundCurrentTurn,
        &mut state,
        &action_tx,
        &operation,
    );

    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::BackgroundCurrentTurn)
    ));
    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitWithMentions { prompt, .. })
            if prompt == "follow up"
    ));
    assert!(state.queued_submission_in_flight.is_some());
}
```

Use each target module's existing `state`, config, and interrupt helpers. Add
the shared `queued` helper from Task 3 where it is not already in scope.

- [ ] **Step 8: Run GREEN**

```sh
cargo test -p orca-tui queued_dispatch --lib
cargo test -p orca-tui terminal_boundary_promotes --lib
cargo test -p orca-tui every_terminal_status --lib
cargo test -p orca-tui occupied_admission_fence --lib
cargo test -p orca-tui rejected_promoted_follow_up --lib
cargo test -p orca-tui ordinary_submission_rejection --lib
cargo test -p orca-tui queued_autosend --lib
cargo test -p orca-tui background --lib
cargo test -p orca-tui workflow_notification --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 9: Commit**

```sh
git add crates/orca-tui/src/queued_input_actions.rs \
  crates/orca-tui/src/runtime_event_actions.rs \
  crates/orca-tui/src/idle_submit_actions.rs \
  crates/orca-tui/src/running_actions.rs \
  crates/orca-tui/src/global_actions.rs
git commit -m "feat(tui): dispatch queued follow-ups safely" \
  -m "Promote one FIFO message per terminal boundary, preserve user-before-workflow order, and restore exact input on admission failures." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 7: Render the Bounded Three-Row Preview

**Files:**
- Modify: `crates/orca-tui/src/queued_input.rs`
- Modify: `crates/orca-tui/src/ui.rs`

- [ ] **Step 1: Add an O(1) preview snapshot**

In `queued_input.rs`:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueuedPreviewSnapshot {
    pub(crate) len: usize,
    pub(crate) first: String,
    pub(crate) second: Option<String>,
    pub(crate) latest: Option<String>,
}
```

Implement from `VecDeque` using only:

```rust
queue.len()
queue.front()
queue.get(1)
queue.back()
```

Under `#[cfg(test)]`, use a local `Cell<usize>` counter passed to a private
builder so tests can assert at most two message previews are materialized for a
64-item queue.

- [ ] **Step 2: Write failing snapshot tests**

```rust
#[test]
fn queued_preview_snapshot_reads_at_most_head_and_tail() {
    let queue = (0..64)
        .map(|index| {
            QueuedUserMessage::from_composer(
                format!("item {index}"),
                Vec::new(),
                MentionBindings::default(),
            )
            .unwrap()
        })
        .collect::<VecDeque<_>>();
    let reads = Cell::new(0);
    let snapshot = QueuedPreviewSnapshot::from_queue_with_probe(
        &queue,
        || reads.set(reads.get() + 1),
    );
    assert_eq!(snapshot.len, 64);
    assert_eq!(snapshot.first, "item 0");
    assert_eq!(snapshot.latest.as_deref(), Some("item 63"));
    assert!(reads.get() <= 2);
}
```

For length two, snapshot may read first and second without separately reading
tail.

- [ ] **Step 3: Add failing row-contract tests in `ui.rs`**

Create pure:

```rust
fn queued_preview_lines(
    state: &AppState,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>>;
```

Tests:

```rust
#[test]
fn queued_preview_uses_two_three_and_exactly_three_rows() {
    let theme = Theme::named(ThemeName::Dark);
    for (count, expected_rows) in [(1, 2), (2, 3), (3, 3), (10, 3), (64, 3)] {
        let mut state = test_state();
        for index in 0..count {
            state
                .enqueue_user_message(queued(&format!("item {index}")))
                .unwrap();
        }
        let lines = queued_preview_lines(&state, 80, &theme);
        assert_eq!(lines.len(), expected_rows, "count={count}");
        assert!(lines[0].to_string().contains(&format!("Queued {count}")));
        assert!(lines[1].to_string().contains("item 0"));
        if count > 2 {
            assert!(lines[2].to_string().contains(&format!("item {}", count - 1)));
        }
    }
}

#[test]
fn queued_preview_keeps_unicode_clusters_and_paste_placeholders() {
    let theme = Theme::named(ThemeName::Dark);
    let mut state = test_state();
    let visible =
        "e\u{301} 👍🏽 👨‍👩‍👧‍👦 1️⃣ 中文 [Pasted Content 1001 chars]";
    state
        .enqueue_user_message(
            QueuedUserMessage::from_composer(
                visible.to_string(),
                vec![(
                    "[Pasted Content 1001 chars]".to_string(),
                    "secret payload".repeat(100),
                )],
                MentionBindings::default(),
            )
            .unwrap(),
        )
        .unwrap();
    let rendered = queued_preview_lines(&state, 80, &theme)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    for cluster in ["e\u{301}", "👍🏽", "👨‍👩‍👧‍👦", "1️⃣", "中文"] {
        assert!(rendered.contains(cluster), "{cluster:?}: {rendered:?}");
    }
    assert!(rendered.contains("[Pasted Content 1001 chars]"));
    assert!(!rendered.contains("secret payload"));
}

#[test]
fn queued_preview_is_hidden_outside_conversation_idle_or_running() {
    let theme = Theme::named(ThemeName::Dark);
    let mut state = test_state();
    state.enqueue_user_message(queued("queued")).unwrap();
    for (status, panel, visible) in [
        (AppStatus::Idle, PanelMode::Conversation, true),
        (AppStatus::Running, PanelMode::Conversation, true),
        (AppStatus::WaitingUserInput, PanelMode::Conversation, false),
        (AppStatus::WaitingApproval, PanelMode::Conversation, false),
        (AppStatus::Compacting, PanelMode::Conversation, false),
        (AppStatus::Idle, PanelMode::Workflows, false),
        (AppStatus::Idle, PanelMode::Agents, false),
    ] {
        state.status = status;
        state.panel_mode = panel;
        assert_eq!(
            !queued_preview_lines(&state, 80, &theme).is_empty(),
            visible,
            "{status:?} {panel:?}"
        );
    }
}
```

- [ ] **Step 4: Run RED**

```sh
cargo test -p orca-tui queued_preview_snapshot --lib
cargo test -p orca-tui queued_preview_uses_two --lib
cargo test -p orca-tui queued_preview_keeps_unicode --lib
```

- [ ] **Step 5: Implement preview lines**

Header:

```text
 Queued N · Alt+Up edit latest
```

When `state.queued_input_error` is present, use:

```text
 Queue error · <bounded reason>
```

in `theme.error`, then retain the same item rows.

Rows:

- one item: header + ` ↳ <first>`;
- two items: header + first + second;
- more: header + first +
  ` … K more · latest: <latest>`.

Use `truncate_to_display_width` with the exact available row width. Use
`theme.muted`; item rows add `Modifier::ITALIC`.

No queue loop is allowed in production rendering.

- [ ] **Step 6: Integrate layout**

Compute:

```rust
let queue_preview_lines = queued_preview_lines(state, frame.area().width, theme);
let queue_preview_height = queue_preview_lines.len().min(3) as u16;
```

Add one `Constraint::Length(queue_preview_height)` between activity and search.

New chunk map:

```text
0 goal
1 transcript/panel
2 plan
3 activity
4 queue preview
5 search
6 composer
7 status
```

Update every render and popup reference to the new indices. Render preview with
`Paragraph::new(queue_preview_lines)`.

- [ ] **Step 7: Add completed compact-frame tests**

```rust
#[test]
fn queued_preview_never_overlaps_search_composer_status_or_cursor() {
    let mut state = test_state();
    state.enter_running();
    state.enqueue_user_message(queued("queued follow up")).unwrap();
    state.open_transcript_search();
    let theme = Theme::named(ThemeName::Dark);
    let textarea = TextArea::from(["draft"]);
    let (backend, events) = RecordingBackend::new(40, 10);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| render(frame, &mut state, &textarea, &theme))
        .unwrap();

    let rendered = format!("{:?}", terminal.backend().inner.buffer());
    assert!(rendered.contains("Queued 1"));
    let search = state.search_area.unwrap();
    let input = state.input_area.unwrap();
    assert!(search.bottom() <= input.y);
    assert!(take_cursor_events(&events)
        .iter()
        .any(|event| matches!(event, CursorEvent::Move(_))));
}

#[test]
fn queued_preview_keeps_slash_and_mention_popup_geometry_above_composer() {
    for popup in ["slash", "mention"] {
        let mut state = test_state();
        state.enter_running();
        state.enqueue_user_message(queued("queued")).unwrap();
        if popup == "slash" {
            state.slash_menu = Some(SlashMenu {
                items: vec![SlashMenuItem {
                    command: "/test".to_string(),
                    description: "test".to_string(),
                }],
                selected: 0,
                sub_menu: None,
            });
        } else {
            state.mention.phase = Some(orca_file_search::SearchPhase::Complete);
            state.mention.candidates = vec![mention_candidate("item.rs")];
        }
        let theme = Theme::named(ThemeName::Dark);
        let textarea = TextArea::from(["draft"]);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10))
                .unwrap();
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .unwrap();
        let input = state.input_area.unwrap();
        assert!(input.bottom() <= terminal.backend().size().unwrap().height);
    }
}

#[test]
fn compact_frame_can_reduce_transcript_to_zero_without_chrome_overlap() {
    let theme = Theme::named(ThemeName::Dark);
    for width in [0, 1, 2, 8] {
        for height in 1..=8 {
            let mut state = test_state();
            state.enter_running();
            state.enqueue_user_message(queued("queued")).unwrap();
            let textarea = TextArea::from(["draft"]);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                    .unwrap();
            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .unwrap();
            if let Some(input) = state.input_area {
                assert!(input.bottom() <= height);
            }
        }
    }
}
```

Add local `queued`, `mention_candidate`, and `RecordingBackend::inner` test
helpers using the existing queue/mention/backend patterns in this module.
These tests assert:

- queue text is present;
- composer software/hardware cursor coordinate still matches;
- popup border/content never overwrites cursor cell;
- search row, input rect, and status are disjoint;
- no panic at widths 0, 1, 2, 8 and heights 1..8.

- [ ] **Step 8: Run GREEN**

```sh
cargo test -p orca-tui queued_preview --lib
cargo test -p orca-tui compact_frame_can_reduce --lib
cargo test -p orca-tui popup_geometry --lib
cargo test -p orca-tui hardware_cursor --lib
cargo test -p orca-tui search_frame --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 9: Commit**

```sh
git add crates/orca-tui/src/queued_input.rs crates/orca-tui/src/ui.rs
git commit -m "feat(tui): render queued follow-up preview" \
  -m "Show an O(1), Unicode-safe, three-row head/latest summary without entering transcript cache or overlapping fixed chrome." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 8: Prove End-to-End Queue Behavior

**Files:**
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/runtime_event_actions.rs`
- Modify: `crates/orca-tui/src/status_key_actions.rs`

- [ ] **Step 1: Add completed event-loop frame test**

```rust
#[test]
fn running_type_queue_preview_restore_edit_and_dispatch_frames_are_consistent() {
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    state.enter_running();
    let mut config = test_config(HistoryMode::Record);
    let shared = Arc::new(Mutex::new(config.clone()));
    let operation = crate::test_support::TestOperationInterrupt::default();
    let preloaded = Arc::new(Mutex::new(None));
    let theme = Theme::named(ThemeName::Dark);
    let mut vim = VimState::new(false);
    let mut textarea = TextArea::default();

    for code in [KeyCode::Char('f'), KeyCode::Char('o'), KeyCode::Char('o')] {
        let key = KeyEvent::new(code, KeyModifiers::NONE);
        handle_status_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &preloaded,
            &mut textarea,
            &mut vim,
            &theme,
            None,
            || Ok(()),
        )
        .unwrap();
    }
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    handle_status_key(
        &Event::Key(enter),
        &enter,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &preloaded,
        &mut textarea,
        &mut vim,
        &theme,
        None,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(state.queued_user_messages.len(), 1);
    assert!(action_rx.try_recv().is_err());

    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10))
            .unwrap();
    terminal
        .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
        .unwrap();
    assert!(format!("{:?}", terminal.backend().buffer()).contains("Queued 1"));

    let restore = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
    handle_status_key(
        &Event::Key(restore),
        &restore,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &preloaded,
        &mut textarea,
        &mut vim,
        &theme,
        None,
        || Ok(()),
    )
    .unwrap();
    assert!(state.queued_user_messages.is_empty());
    assert_eq!(textarea_text(&textarea), "foo");

    assert!(textarea.insert_char('!'));
    handle_status_key(
        &Event::Key(enter),
        &enter,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &preloaded,
        &mut textarea,
        &mut vim,
        &theme,
        None,
        || Ok(()),
    )
    .unwrap();
    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });
    assert_eq!(
        crate::queued_input_actions::dispatch_next_queued_user_message(
            &mut state,
            &action_tx,
        ),
        crate::queued_input_actions::QueuedDispatch::Started
    );
    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitWithMentions { prompt, .. }) if prompt == "foo!"
    ));
    assert!(matches!(
        state.messages.last(),
        Some(ChatMessage::User(text)) if text == "foo!"
    ));
}
```

- [ ] **Step 2: Add multi-message FIFO/LIFO frame test**

```rust
#[test]
fn three_queued_messages_preview_head_latest_restore_latest_and_dispatch_fifo() {
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    state.enter_running();
    for text in ["first", "second", "third"] {
        state.enqueue_user_message(queued(text)).unwrap();
    }
    let theme = Theme::named(ThemeName::Dark);
    let lines = queued_preview_lines(&state, 80, &theme);
    assert_eq!(lines.len(), 3);
    assert!(lines[1].to_string().contains("first"));
    assert!(lines[2].to_string().contains("third"));

    let mut vim = VimState::new(false);
    let mut textarea = TextArea::default();
    assert!(crate::queued_input_actions::restore_latest_queued_message(
        &mut state,
        &mut textarea,
        &mut vim,
        &theme,
    ));
    assert_eq!(textarea_text(&textarea), "third");
    assert_eq!(state.queued_user_messages.len(), 2);

    state.set_status(AppStatus::Idle);
    assert_eq!(
        crate::queued_input_actions::dispatch_next_queued_user_message(
            &mut state,
            &action_tx,
        ),
        crate::queued_input_actions::QueuedDispatch::Started
    );
    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitWithMentions { prompt, .. }) if prompt == "first"
    ));
    state.update(TuiEvent::TurnStarted {
        turn: 1,
        task: None,
    });
    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });
    assert_eq!(
        crate::queued_input_actions::dispatch_next_queued_user_message(
            &mut state,
            &action_tx,
        ),
        crate::queued_input_actions::QueuedDispatch::Started
    );
    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitWithMentions { prompt, .. }) if prompt == "second"
    ));
}
```

- [ ] **Step 3: Add modal/search/interrupt ownership matrix**

```rust
#[test]
fn queued_restore_and_submit_respect_search_modal_and_interrupt_priority() {
    let (action_tx, _action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx,
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    state.enqueue_user_message(queued("queued")).unwrap();
    let theme = Theme::named(ThemeName::Dark);
    let mut vim = VimState::new(false);
    let mut textarea = TextArea::default();

    state.enter_running();
    state.open_transcript_search();
    let restore = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(
        crate::key_event_actions::handle_transcript_search_key(
            restore,
            &mut state,
        ),
        crate::key_event_actions::SearchKeyFlow::Handled
    );
    assert_eq!(state.queued_user_messages.len(), 1);
    assert!(textarea.is_empty());

    state.close_transcript_search();
    for status in [AppStatus::WaitingApproval, AppStatus::WaitingUserInput] {
        state.status = status;
        assert!(
            !crate::queued_input_actions::restore_latest_queued_message(
                &mut state,
                &mut textarea,
                &mut vim,
                &theme,
            )
        );
    }
    assert_eq!(state.queued_user_messages.len(), 1);
}
```

- [ ] **Step 4: Add hosted controller proof**

Use the existing mock provider and controller harness:

```rust
#[test]
fn hosted_tui_runs_queued_follow_ups_one_at_a_time_in_fifo_order() {
    with_orca_home(|_| {
        let mut harness =
            HostedTuiHarness::start(test_config(HistoryMode::Record), None);
        harness.send(UserAction::Submit(
            "mock_stream_delay_ms 100".to_string(),
        ));
        harness.recv_until(|event| matches!(event, TuiEvent::TurnStarted { .. }));
        harness.recv_until(|event| {
            matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        });

        for expected_count in [2, 3] {
            harness.send(UserAction::Submit("mock_history_echo".to_string()));
            harness.recv_until(|event| matches!(event, TuiEvent::TurnStarted { .. }));
            let delta = harness.recv_until(|event| {
                matches!(event, TuiEvent::MessageDelta(text) if text.contains("Mock history users:"))
            });
            let TuiEvent::MessageDelta(text) = delta else { unreachable!() };
            assert!(
                text.contains(&format!("Mock history users: {expected_count}")),
                "{text}"
            );
            harness.recv_until(|event| {
                matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
            });
        }
        harness.shutdown();
    });
}
```

This hosted test proves serialized user-turn order at the controller boundary.
The completed frame tests above prove the AppState queue-to-action transition;
do not substitute dispatcher unit counts for either layer.

- [ ] **Step 5: Run focused gates**

```sh
cargo test -p orca-tui queued_ --lib
cargo test -p orca-tui running_ --lib
cargo test -p orca-tui search_ --lib
cargo test -p orca-tui mention_ --lib
cargo test -p orca-tui paste --lib
cargo test -p orca-tui workflow_notification --lib
cargo test -p orca-tui action_dispatcher --lib
cargo test -p orca-tui ui::tests --lib
cargo test -p orca-tui app::tests --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 6: Commit**

```sh
git add crates/orca-tui/src/app.rs \
  crates/orca-tui/src/runtime_event_actions.rs \
  crates/orca-tui/src/status_key_actions.rs
git commit -m "test(tui): verify queued follow-up integration" \
  -m "Cover completed keyboard frames, exact restore, FIFO dispatch, modal priority, interrupt suppression, and hosted turn ordering." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 9: Final Review, Audit, Push, and Remote Verification

**Files:**
- Verify every file in this plan.

- [ ] **Step 1: Prompt-to-artifact audit**

| Requirement | Direct evidence |
|---|---|
| Running composer accepts text | completed status-key frame |
| Enter queues without action | action receiver + queue test |
| FIFO next-turn order | reducer + hosted controller test |
| one item per terminal boundary | status matrix test |
| LIFO `Alt+Up` | restore test |
| exact paste restoration | large-paste test |
| exact mention restoration | mention binding test |
| 3 physical preview rows | pure row + frame tests |
| head/latest O(1) preview | 64-item read probe |
| queued text absent from transcript before dispatch | frame + state assertions |
| history recorded at dispatch | reducer/history assertions |
| user queue before workflow notification | boundary ordering test |
| interrupt suppresses autosend | Esc/Ctrl+G/Ctrl+C matrix |
| background starts exactly one next turn | action ordering + fence test |
| late background terminal cannot double-promote | admission-fence test |
| full/disconnected channel recovery | bounded-channel tests |
| all terminal statuses drain | status table |
| search owns open input | search/Alt+Up test |
| approval/MCP inputs stay separate | modal matrix |
| slash-looking Running input stays literal | Running slash test |
| Running mention and paste work | real mention/paste tests |
| Vim convention preserved | Running Vim test |
| fixed chrome/cursor/popup preserved | compact backend tests |
| no transcript cache use | changed-file/symbol audit |
| no later P2 leakage | changed-file/symbol audit |

Treat any missing row as incomplete.

- [ ] **Step 2: Request independent reviews**

Specification review against:

```text
docs/superpowers/specs/2026-07-28-tui-queued-message-preview-design.md
```

Quality review focuses on:

- queue ownership and duplicate sources of truth;
- mention binding reconciliation after placeholder expansion;
- full/disconnected channel rollback;
- in-flight fence clearing and stale terminal events;
- interrupt/background races;
- workflow notification starvation/order;
- Running input and search/modal priority;
- preview O(1) evidence;
- Unicode grapheme truncation;
- hardware cursor and compact layout;
- accidental later-P2 leakage.

Fix every Critical or Important finding with RED/GREEN evidence.

- [ ] **Step 3: Run package and workspace gates**

```sh
cargo test -p orca-tui -- --test-threads=1
cargo test --workspace --all-targets -- --test-threads=1
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

If an unchanged process-cleanup timing test flakes:

1. prove its blob equals baseline `5726f5c8872cba9710fb4a6fb399cf707a5fb10f`;
2. rerun the exact serialized test five times;
3. skip only proven flaky tests in a workspace rerun;
4. do not edit unrelated process code.

- [ ] **Step 4: Audit commits, trailers, and scope**

```sh
git status --short
git log --format='%H%n%s%n%(trailers:key=Co-authored-by,valueonly)%n---' \
  5726f5c8872cba9710fb4a6fb399cf707a5fb10f..HEAD
git diff --check 5726f5c8872cba9710fb4a6fb399cf707a5fb10f..HEAD
git diff --name-status 5726f5c8872cba9710fb4a6fb399cf707a5fb10f..HEAD
git diff --stat 5726f5c8872cba9710fb4a6fb399cf707a5fb10f..HEAD
```

Programmatically prove every commit contains exactly one final required trailer.

Scope audit rejects additions containing:

```text
git branch footer
keybindings.json
Vim counts/registers/dot repeat
/doctor
FPS
provider/model/theme onboarding
```

- [ ] **Step 5: Push and verify remote SHA**

```sh
git push origin feature/tui-syntax-highlighting
local_sha=$(git rev-parse HEAD)
remote_sha=$(
  git ls-remote --heads origin feature/tui-syntax-highlighting |
    awk '{print $1}'
)
test -n "$remote_sha"
test "$local_sha" = "$remote_sha"
git status --short --branch
```

Keep the branch and worktree for the remaining P2 roadmap.
