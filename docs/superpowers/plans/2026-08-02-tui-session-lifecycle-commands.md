# TUI Session Lifecycle Commands Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `/fork`, `/rename`, `/status`, `/copy`, and contextual saved-session actions while keeping `/resume` as Orca's only saved-session command.

**Architecture:** Slash commands remain presentation intents. Current-session mutations cross `UserAction` into the hosted controller and use runtime host/surface APIs; the TUI receives identity and completion events. Saved-session management stays inside an explicit resume-picker state machine whose mutations capture stable session IDs.

**Tech Stack:** Rust 2024, ratatui 0.29, crossterm, crossbeam channels, Orca runtime surfaces, JSONL session store.

---

## File Map

- Modify `crates/orca-tui/src/commands/mod.rs`
  - Parse and advertise the four commands.
- Modify `crates/orca-tui/src/slash_command_actions.rs`
  - Dispatch command intents, format status, select copy targets, and make `/resume` unambiguous.
- Modify `crates/orca-tui/src/idle_submit_actions.rs`
  - Apply composer-prefill outcomes for argument-requiring commands.
- Modify `crates/orca-tui/src/slash_menu_actions.rs`
  - Route `/rename` to its argument phase.
- Modify `crates/orca-tui/src/types.rs`
  - Add lifecycle actions/events, current identity, picker phases, and pure state helpers.
- Modify `crates/orca-tui/src/app.rs`
  - Execute fork/rename/picker actions through the hosted runtime and publish identity changes.
- Modify `crates/orca-tui/src/surface_actions.rs`
  - Expose typed title mutation with revision preconditions.
- Modify `crates/orca-tui/src/session_picker_actions.rs`
  - Implement the picker phase state machine and emit typed actions instead of changing history mode locally.
- Modify `crates/orca-tui/src/ui.rs`
  - Render picker actions, rename input, confirmations, and inline errors.
- Modify `crates/orca-tui/src/input_event_actions.rs`
  - Keep mouse hit-testing correct after picker layout changes.
- Modify `crates/orca-tui/src/action_dispatcher.rs`
  - Classify lifecycle command overflow as an operation rejection.
- Modify `crates/orca-tui/src/surface_boundary_tests.rs`
  - Extend the architectural allowlist for typed lifecycle ownership.
- Modify `tests/history_contract.rs`
  - Cover durable rename/archive/delete/fork behavior through public boundaries.
- Modify `README.md` and `README.zh-CN.md`
  - Document the final interactive and CLI session commands.

Written design:

```text
docs/superpowers/specs/2026-08-02-tui-session-lifecycle-commands-design.md
```

---

### Task 1: Parse Commands And Make Resume Unambiguous

**Files:**
- Modify: `crates/orca-tui/src/commands/mod.rs`
- Modify: `crates/orca-tui/src/slash_command_actions.rs`
- Modify: `crates/orca-tui/src/idle_submit_actions.rs`
- Modify: `crates/orca-tui/src/slash_menu_actions.rs`

- [ ] **Step 1: Write failing parser and dispatch tests**

Add focused tests asserting:

```rust
assert_eq!(parse("/fork"), Some(SlashCommand::Fork(None)));
assert_eq!(parse("/fork auth experiment"), Some(SlashCommand::Fork(Some("auth experiment".into()))));
assert_eq!(parse("/rename release triage"), Some(SlashCommand::Rename(Some("release triage".into()))));
assert_eq!(parse("/status"), Some(SlashCommand::Status));
assert_eq!(parse("/copy 2"), Some(SlashCommand::Copy(Some("2".into()))));
assert!(!all_commands().iter().any(|(name, _)| *name == "/history"));
```

Add a dispatch test with `recoverable_operation_id` populated and assert `/resume` enters `AppStatus::SessionPicker` without emitting `UserAction::ResumeOperation`.

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui commands::tests::parses_session_lifecycle_commands --lib
cargo test -p orca-tui resume_always_opens_saved_session_picker --lib
```

Expected: missing enum variants and current `/resume` dispatches operation recovery.

- [ ] **Step 3: Add command variants and parser rules**

Use these public shapes:

```rust
Fork(Option<String>),
Rename(Option<String>),
Status,
Copy(Option<String>),
```

Collect all remaining tokens for fork/rename names so spaces are preserved after normalized joining. Keep `/clear` parsed as `New` but absent from `all_commands()`.

- [ ] **Step 4: Add a composer-prefill slash outcome**

Change the outcome to:

```rust
pub(crate) enum SlashOutcome {
    Continue,
    Prefill(String),
}
```

Return `Prefill("/rename ".to_string())` for `/rename` without a title. Apply the value in both direct-submit and slash-menu paths with `make_textarea_with_text`.

- [ ] **Step 5: Make `/resume` only open the picker**

Remove the recoverable-operation branch from `SlashCommand::Resume`. Always list saved sessions and enter the picker. Keep `ResumeOperation` as an internal `UserAction` triggered by the recovery UI, not by `/resume`.

- [ ] **Step 6: Run GREEN**

```sh
cargo test -p orca-tui commands::tests --lib
cargo test -p orca-tui slash_command_actions --lib
cargo test -p orca-tui slash_menu_actions --lib
```

Expected: all focused command tests pass.

### Task 2: Add Session Identity, Status, And Copy

**Files:**
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/slash_command_actions.rs`

- [ ] **Step 1: Write failing identity, status, and copy tests**

Cover these APIs:

```rust
state.update(TuiEvent::SessionIdentityUpdated {
    session_id: "session-1".into(),
    title: "Auth investigation".into(),
});
assert_eq!(state.current_session_id.as_deref(), Some("session-1"));

assert_eq!(state.nth_final_assistant_response(1), Some("latest"));
assert_eq!(state.nth_final_assistant_response(2), Some("older"));
assert_eq!(state.nth_final_assistant_response(0), None);
```

Add slash tests proving `/copy` stages the latest finalized response, ignores `AssistantChunk`, rejects invalid indices, and `/status` includes identity, model, mode, context, tokens, cost, cwd, and active-work summaries.

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui session_identity_updates_current_projection --lib
cargo test -p orca-tui copy_slash_command_stages_nth_final_response --lib
cargo test -p orca-tui status_slash_command_reports_session_snapshot --lib
```

Expected: identity fields, helper, and command behavior are missing.

- [ ] **Step 3: Add identity projection**

Add:

```rust
TuiEvent::SessionIdentityUpdated { session_id: String, title: String }
```

and `AppState.current_session_id/current_session_title`. Publish it from `announce_runtime_ready` by reading the runtime surface snapshot. Update fields on the event and clear them before an unpersisted startup only.

- [ ] **Step 4: Implement finalized-response selection**

Add a pure reverse iterator over `ChatMessage::Assistant` only. Validate `/copy [N]`, call `stage_clipboard_copy(text, Instant::now())`, and emit a transcript error for invalid or missing responses.

- [ ] **Step 5: Implement pure status formatting**

Create `format_status(state, config) -> String`. Build a stable multiline block from state/config without filesystem access. Use `-` for unknown identity/context values and include active task/workflow counts from `workflow_panel.tasks`.

- [ ] **Step 6: Run GREEN**

```sh
cargo test -p orca-tui session_identity_updates_current_projection --lib
cargo test -p orca-tui copy_slash_command --lib
cargo test -p orca-tui status_slash_command --lib
```

Expected: all identity/status/copy tests pass.

### Task 3: Fork And Rename The Current Session

**Files:**
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/surface_actions.rs`
- Modify: `crates/orca-tui/src/action_dispatcher.rs`

- [ ] **Step 1: Write failing slash-action tests**

Assert idle commands emit:

```rust
UserAction::ForkCurrentSession { title: Some("experiment".into()) }
UserAction::RenameCurrentSession { title: "release triage".into() }
```

Assert `/fork` is rejected before emission when presentation state is non-idle, while rename remains allowed.

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui fork_slash_command_dispatches_typed_action --lib
cargo test -p orca-tui rename_slash_command_dispatches_typed_action --lib
```

Expected: lifecycle actions do not exist.

- [ ] **Step 3: Add typed lifecycle actions and events**

Add:

```rust
UserAction::ForkCurrentSession { title: Option<String> },
UserAction::RenameCurrentSession { title: String },
TuiEvent::SessionRenamed { session_id: String, title: String },
```

Classify fork/rename queue overflow as `OperationRejected`.

- [ ] **Step 4: Implement title mutation through the surface**

`TuiSurfaceActions::rename_current_session` reads the current snapshot, constructs `SessionMetadataPatch::SetTitle`, and submits it with `SessionMetadataPrecondition::Exact { revision }`. The hosted controller then persists the same title using `RuntimeSurfaceHostHandle::rename_saved_session`. Only after both succeed does it emit `SessionRenamed`.

- [ ] **Step 5: Implement fork as an atomic session swap**

Extract the existing active-work predicate from `start_new_hosted_session`. `start_forked_hosted_session` must:

```rust
let source_id = current.session_id().ok_or("current conversation is not resumable")?;
next_config.history_mode = HistoryMode::Fork(source_id.to_string());
let started = host.start_thread_with_request(request)?;
current.shutdown()?;
*thread = Some(started);
```

If shutdown fails, shut down `started` and retain `current`. On success update shared config, clear preloaded state, announce runtime ready, emit copied history, and emit the new identity.

- [ ] **Step 6: Write hosted integration tests**

Use `with_orca_home` and `HostedTuiHarness` to prove:

- the fork receives a different ID;
- copied user/assistant history is visible;
- the source transcript remains loadable and unchanged;
- active work rejects the fork; and
- rename changes both `history::load_session(id).meta.title` and `AppState` projection.

- [ ] **Step 7: Run GREEN**

```sh
cargo test -p orca-tui fork_slash_command --lib
cargo test -p orca-tui hosted_tui_fork --lib -- --test-threads=1
cargo test -p orca-tui hosted_tui_rename --lib -- --test-threads=1
```

Expected: current-session lifecycle tests pass.

### Task 4: Build Resume Picker Context Actions

**Files:**
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/session_picker_actions.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/input_event_actions.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [ ] **Step 1: Write failing phase-transition tests**

Instantiate a picker with two sessions and verify:

```rust
Tab => SessionPickerPhase::Actions { captured_session_id, selected: 0 }
Esc => Browsing
Rename => Renaming { captured_session_id, value: "", cursor: 0 }
Archive => ConfirmArchive { captured_session_id, captured_title }
Delete => ConfirmDelete { captured_session_id, captured_title }
```

Change `session_picker_selected` after confirmation opens and assert the emitted action still contains the captured ID.

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui session_picker_actions_capture_selected_session_id --lib
cargo test -p orca-tui session_picker_delete_confirmation_uses_captured_id --lib
```

Expected: picker phase state and actions are missing.

- [ ] **Step 3: Add picker state and typed actions**

Implement `SessionPickerPhase` from the design and add:

```rust
UserAction::ResumeSavedSession { session_id: String },
UserAction::ForkSavedSession { session_id: String },
UserAction::RenameSavedSession { session_id: String, title: String },
UserAction::ArchiveSavedSession { session_id: String },
UserAction::DeleteSavedSession { session_id: String },
```

Replace picker-local `HistoryMode` mutation with these actions.

- [ ] **Step 4: Implement hosted saved-session actions**

Resume and fork use the same start-new-then-shutdown-old swap helper with `HistoryMode::Resume/Fork`. Rename/archive/delete call the runtime host store facade. Emit success/error events carrying the captured ID, then refresh the picker list without clearing its query.

- [ ] **Step 5: Render every phase**

Keep the browsing list layout stable. Render actions below the selected session, a one-line rename editor, and archive/delete confirmations with Cancel selected first. Show `session_picker_error` above the footer and update `session_picker_hit_index` for the added rows.

- [ ] **Step 6: Add completed-frame and mouse tests**

Assert 80x24 and narrow 44x18 buffers contain non-overlapping hints, bounded titles, action labels, confirmation warnings, and errors. Preserve click-select-then-resume behavior only in Browsing; clicks in other phases must not synthesize resume.

- [ ] **Step 7: Run GREEN**

```sh
cargo test -p orca-tui session_picker --lib -- --test-threads=1
cargo test -p orca-tui resume_picker --lib -- --test-threads=1
```

Expected: picker state, rendering, keyboard, and mouse tests pass.

### Task 5: Recovery Interaction, Documentation, And Verification

**Files:**
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/status_key_actions.rs`
- Modify: `crates/orca-tui/src/surface_boundary_tests.rs`
- Modify: `tests/history_contract.rs`
- Modify: `README.md`
- Modify: `README.zh-CN.md`

- [ ] **Step 1: Write a failing recovery-interaction test**

After `RecoveryAvailable`, assert the UI displays explicit resume/cancel choices and that the resume choice emits `UserAction::ResumeOperation` without typing `/resume`.

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui recoverable_operation_uses_explicit_interaction --lib
```

Expected: recovery is notice-only and `/resume` owns the action.

- [ ] **Step 3: Add the explicit recovery interaction**

Represent recovery as a small selected action state. Route Left/Right or Up/Down and Enter while Idle before composer handling. Resume emits `ResumeOperation`; cancel emits `CancelOperation`; Esc dismisses the choice without mutating the operation.

- [ ] **Step 4: Update contracts and user documentation**

Document:

```text
/new                 start a fresh saved conversation
/resume              choose a saved conversation
/fork [name]         branch the current conversation
/rename [name]       rename the current conversation
/status              inspect session/runtime state
/copy [N]            copy an assistant response
orca --resume [ID]   enter an existing saved conversation
orca --fork ID       fork a saved conversation at startup
```

State explicitly that `/history` is retired and `Ctrl+L` only clears the display.

- [ ] **Step 5: Run focused and broad verification**

```sh
cargo fmt --all -- --check
cargo test -p orca-tui --lib -- --test-threads=1
cargo test --test history_contract -- --test-threads=1
cargo test --test cli_architecture_contract -- --test-threads=1
cargo check --workspace
git diff --check
```

Expected: every command exits 0; the TUI suite reports zero failed tests.

- [ ] **Step 6: Audit scope and repository state**

```sh
git status --short --branch
git diff --stat
git diff -- docs/superpowers/specs/2026-08-02-tui-session-lifecycle-commands-design.md docs/superpowers/plans/2026-08-02-tui-session-lifecycle-commands.md crates/orca-tui tests/history_contract.rs README.md README.zh-CN.md
```

Expected: only lifecycle-command implementation, tests, and documentation are changed; no generated artifacts or unrelated metadata churn.
