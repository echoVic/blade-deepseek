# Binding-Only Mentions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every unbound `@...` token literal text, expand only structured Mention bindings, recover the TUI from pre-turn rejection, and keep a double-Ctrl+C force-exit path.

**Architecture:** Remove legacy raw-text file expansion from the shared runtime and CLI, leaving `MentionBinding` as the only `@` attachment boundary across TUI and app-server. Add a dedicated `SubmissionRejected` TUI terminal event for failures before provider execution, and make global cancellation remember the first Ctrl+C even while running so a second press can exit.

**Tech Stack:** Rust 2024, crossbeam channels, crossterm/ratatui TUI, JSONL app-server protocol, Cargo integration tests.

---

### Task 1: Make Runtime Mention Expansion Binding-Only

**Files:**
- Modify: `crates/orca-runtime/src/mentions.rs:632-707`
- Modify: `crates/orca-runtime/src/mentions.rs:875-908`
- Modify: `crates/orca-runtime/src/mentions.rs:1084-1312`
- Test: `crates/orca-runtime/src/mentions.rs:1331-1880`

- [ ] **Step 1: Write failing literal-text tests**

Add tests that call the shared `expand_mentions` API with empty bindings:

```rust
#[test]
fn unbound_at_tokens_remain_literal_even_when_they_look_like_paths() {
    let cwd = tempfile::tempdir().unwrap();
    fs::write(cwd.path().join("README.md"), "do not inject").unwrap();
    let roots = vec![cwd.path().to_path_buf()];
    let registry = orca_mcp::McpRegistry::default();

    for input in ["@oai/sky还能逆向吗", "read @README.md", "email foo@example.com"] {
        let expanded = expand_mentions(
            input,
            &MentionBindings::new(input),
            cwd.path(),
            &roots,
            &registry,
        )
        .unwrap();

        assert_eq!(expanded, input);
    }
}
```

- [ ] **Step 2: Run the focused test and confirm RED**

Run:

```bash
cargo test -p orca-runtime unbound_at_tokens_remain_literal_even_when_they_look_like_paths -- --nocapture
```

Expected: FAIL because `@oai/sky还能逆向吗` is resolved as a path or `@README.md` injects a `<file>` block.

- [ ] **Step 3: Remove raw-text expansion from `expand_mentions`**

Keep only validated binding expansion:

```rust
pub fn expand_mentions(
    input: &str,
    bindings: &MentionBindings,
    cwd: &Path,
    workspace_roots: &[PathBuf],
    mcp_registry: &orca_mcp::McpRegistry,
) -> Result<String, String> {
    let valid_bindings = bindings
        .bindings()
        .iter()
        .filter(|binding| {
            binding.end <= input.len()
                && input.is_char_boundary(binding.start)
                && input.is_char_boundary(binding.end)
                && input[binding.start..binding.end] == binding.visible
        });
    let mut blocks = Vec::new();
    let mut seen_targets = std::collections::HashSet::new();
    for binding in valid_bindings {
        if seen_targets.insert(binding.target.stable_id()) {
            blocks.push(expand_bound_target(
                &binding.target,
                cwd,
                workspace_roots,
                mcp_registry,
            )?);
        }
    }
    append_mention_blocks(input, blocks)
}
```

Delete `expand_file_mentions`, `file_mention_blocks`, `find_mentions`, `extract_mention_tokens`, `MentionOccurrence`, `extract_mention_occurrences`, `is_plain_at_word`, `resolve_mention_path`, `LegacyMentionTarget`, `LineRange`, `select_lines`, and `line_spans` after production callers are removed in Task 2.

Simplify `file_block` because selected bindings do not carry legacy line ranges:

```rust
fn file_block(
    display_path: &str,
    resolved: &Path,
    mention: &str,
    root: Option<&Path>,
) -> Result<String, String> {
    let content = orca_tools::file_admission::read_text_file_with_limit(
        resolved,
        MAX_MENTION_SOURCE_BYTES,
        || false,
    )
    .map_err(|error| match error {
        orca_tools::file_admission::FileAdmissionError::InvalidUtf8 => {
            format!("@{mention} appears to be a binary file")
        }
        error => format!("failed to read @{mention}: {error}"),
    })?;
    if content.as_bytes().contains(&0) {
        return Err(format!("@{mention} appears to be a binary file"));
    }
    let (content, truncated) = truncate_content(&content);
    let marker = if truncated {
        "\n[... truncated ...]"
    } else {
        ""
    };
    let root_attr = root
        .map(|root| format!(r#" root="{}""#, escape_attr(&root.display().to_string())))
        .unwrap_or_default();
    Ok(format!(
        r#"<file path="{}"{}>
{}{}</file>"#,
        escape_attr(display_path),
        root_attr,
        content,
        marker
    ))
}
```

- [ ] **Step 4: Rewrite legacy tests around selected bindings**

Remove tests whose only contract is automatic raw `@file` expansion. Preserve admission coverage by calling the simplified `file_block` directly, and retain all atomic binding, same-name, multi-root, Skill, Plugin, and MCP Resource tests.

- [ ] **Step 5: Run runtime Mention tests and confirm GREEN**

Run:

```bash
cargo test -p orca-runtime mentions::tests -- --nocapture
```

Expected: all Mention unit tests PASS.

- [ ] **Step 6: Commit runtime semantics**

```bash
git add crates/orca-runtime/src/mentions.rs
git commit -m "fix(runtime): require bindings for at mentions"
```

### Task 2: Remove CLI Legacy File Expansion

**Files:**
- Modify: `src/cli.rs:742-771`
- Modify: `tests/history_contract.rs:43-75`

- [ ] **Step 1: Rewrite the history contract to expect literal text**

Replace the legacy expansion test with:

```rust
#[test]
fn exec_preserves_unbound_at_tokens_as_literal_history() {
    let home = TempDir::new().expect("temp home");
    let project = TempDir::new().expect("temp project");
    std::fs::write(project.path().join("notes.txt"), "alpha\nbeta\ngamma\n")
        .expect("write file that must not be injected");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .arg("exec")
        .arg("--provider")
        .arg("mock")
        .arg("--cwd")
        .arg(project.path())
        .arg("summarize")
        .arg("@notes.txt#L2")
        .output()
        .expect("run orca");
    assert_eq!(output.status.code(), Some(0));

    let show = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "show", "latest"])
        .output()
        .expect("show history");
    let stdout = String::from_utf8_lossy(&show.stdout);
    assert!(stdout.contains("summarize @notes.txt#L2"));
    assert!(!stdout.contains("<file"));
    assert!(!stdout.contains("beta</file>"));
}
```

- [ ] **Step 2: Run the contract and confirm RED**

Run:

```bash
cargo test --test history_contract exec_preserves_unbound_at_tokens_as_literal_history -- --nocapture
```

Expected: FAIL because CLI preprocessing injects the file block.

- [ ] **Step 3: Delete CLI mention preprocessing**

Remove the `cwd_for_mentions` setup and this branch:

```rust
let prompt = match crate::mentions::expand_file_mentions(&prompt, &cwd_for_mentions) {
    Ok(prompt) => prompt,
    Err(error) => {
        eprintln!("ERROR: {error}");
        return 1;
    }
};
```

Pass the assembled prompt directly into the existing exec request.

- [ ] **Step 4: Run history and exec contracts**

Run:

```bash
cargo test --test history_contract exec_preserves_unbound_at_tokens_as_literal_history -- --nocapture
cargo test --test exec_jsonl -- --nocapture
```

Expected: both commands PASS.

- [ ] **Step 5: Commit CLI behavior**

```bash
git add src/cli.rs tests/history_contract.rs
git commit -m "fix(cli): keep unbound at tokens literal"
```

### Task 3: Add a Submission Rejection Terminal Event

**Files:**
- Modify: `crates/orca-tui/src/types.rs:129-249`
- Modify: `crates/orca-tui/src/types.rs:1738-1831`
- Modify: `crates/orca-tui/src/runtime_event_actions.rs:16-69`
- Modify: `crates/orca-tui/src/submitted_turn.rs:39-110`
- Modify: `crates/orca-tui/src/app.rs:3762-3820`
- Test: `crates/orca-tui/src/app.rs` existing test module
- Test: `crates/orca-tui/src/types.rs` existing test module

- [ ] **Step 1: Write failing AppState rejection tests**

Add a state test that starts from an optimistic running submission:

```rust
#[test]
fn submission_rejection_removes_optimistic_user_and_returns_idle() {
    let mut state = state();
    state.push_message(ChatMessage::Assistant("before".to_string()));
    state.push_message(ChatMessage::User("review @gone.txt".to_string()));
    state.enter_running();

    state.update(TuiEvent::SubmissionRejected {
        prompt: "review @gone.txt".to_string(),
        message: "bound file is no longer available".to_string(),
    });

    assert_eq!(state.status, AppStatus::Idle);
    assert!(matches!(
        state.messages.as_slice(),
        [ChatMessage::Assistant(before), ChatMessage::Error(error)]
            if before == "before" && error == "bound file is no longer available"
    ));
    assert!(state.mention_bindings.is_empty());
}
```

Add an app/runtime-event test asserting the textarea becomes `review @gone.txt` after handling the event.

- [ ] **Step 2: Run focused TUI tests and confirm RED**

Run:

```bash
cargo test -p orca-tui submission_rejection -- --nocapture
```

Expected: compile failure because `SubmissionRejected` does not exist.

- [ ] **Step 3: Define the event and recovery projection**

Add:

```rust
SubmissionRejected {
    prompt: String,
    message: String,
},
```

In `AppState::update`, handle it separately from generic `Error`:

```rust
TuiEvent::SubmissionRejected { message, .. } => {
    self.remove_after_last_user();
    self.mention_bindings.clear();
    self.clear_receiving_tool_progress();
    self.push_message(ChatMessage::Error(message));
    self.set_status(AppStatus::Idle);
}
```

In `handle_runtime_event`, capture the restore text before consuming the event:

```rust
let restored_prompt = match &tui_event {
    TuiEvent::Backtracked { prompt }
    | TuiEvent::SubmissionRejected { prompt, .. } => Some(prompt.clone()),
    _ => None,
};
```

After `state.update`, rebuild the textarea from `restored_prompt` with `make_textarea_with_text`.

- [ ] **Step 4: Emit rejection for user submission preparation failures**

Add a `SubmittedTurn` method that returns a restorable prompt only for user turns:

```rust
pub(crate) fn rejection_prompt(&self) -> Option<&str> {
    match &self.kind {
        SubmittedTurnKind::User { prompt, .. } => Some(prompt),
        SubmittedTurnKind::WorkflowNotification(_) => None,
    }
}
```

Before initializing or expanding, save `submitted_turn.rejection_prompt().map(str::to_string)`. On user-turn initialization or expansion failure, send `SubmissionRejected { prompt, message }` and return. Preserve the existing workflow-notification error handling without restoring its internal prompt into the composer.

- [ ] **Step 5: Run focused TUI tests and confirm GREEN**

Run:

```bash
cargo test -p orca-tui submission_rejection -- --nocapture
cargo test -p orca-tui idle_submit_carries_atomic_mention_bindings -- --nocapture
```

Expected: all focused tests PASS.

- [ ] **Step 6: Commit rejection lifecycle**

```bash
git add crates/orca-tui/src/types.rs crates/orca-tui/src/runtime_event_actions.rs crates/orca-tui/src/submitted_turn.rs crates/orca-tui/src/app.rs
git commit -m "fix(tui): recover rejected mention submissions"
```

### Task 4: Make Double Ctrl+C Exit a Stale Running State

**Files:**
- Modify: `crates/orca-tui/src/global_actions.rs:15-58`
- Test: `crates/orca-tui/src/global_actions.rs:63-123`

- [ ] **Step 1: Write the failing running-state double-cancel test**

```rust
#[test]
fn second_ctrl_c_exits_even_when_status_remains_running() {
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "model".to_string(),
        "/tmp".to_string(),
    );
    state.enter_running();
    let cancel = CancelToken::new();

    let first = handle_global_shortcut(
        GlobalShortcut::Cancel,
        &mut state,
        &action_tx,
        &cancel,
        || Ok(()),
    )
    .unwrap();
    let second = handle_global_shortcut(
        GlobalShortcut::Cancel,
        &mut state,
        &action_tx,
        &cancel,
        || Ok(()),
    )
    .unwrap();

    assert!(matches!(first, GlobalShortcutFlow::Continue));
    assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
    assert!(matches!(second, GlobalShortcutFlow::Exit(130)));
    assert!(matches!(action_rx.try_recv(), Ok(UserAction::Cancel)));
}
```

- [ ] **Step 2: Run the test and confirm RED**

Run:

```bash
cargo test -p orca-tui second_ctrl_c_exits_even_when_status_remains_running -- --nocapture
```

Expected: FAIL because running cancellation returns before setting `last_ctrl_c`.

- [ ] **Step 3: Check force-exit before the running interrupt branch**

Refactor `GlobalShortcut::Cancel` so it always computes `now`, exits on a recent prior Ctrl+C, records the first press, and then either interrupts a running turn or shows the idle exit hint:

```rust
let now = Instant::now();
if state
    .last_ctrl_c
    .is_some_and(|pressed| now.duration_since(pressed) < Duration::from_secs(2))
{
    let _ = action_tx.send(UserAction::Cancel);
    return Ok(GlobalShortcutFlow::Exit(130));
}
state.last_ctrl_c = Some(now);
if matches!(state.status, AppStatus::Running | AppStatus::Compacting) {
    cancel_token.cancel();
    let _ = action_tx.send(UserAction::Interrupt);
}
state.push_message(ChatMessage::System("Press Ctrl+C again to quit.".into()));
state.scroll_to_bottom();
```

- [ ] **Step 4: Run global action tests**

Run:

```bash
cargo test -p orca-tui global_actions::tests -- --nocapture
```

Expected: all global shortcut tests PASS.

- [ ] **Step 5: Commit the exit fail-safe**

```bash
git add crates/orca-tui/src/global_actions.rs
git commit -m "fix(tui): allow force exit after interrupt"
```

### Task 5: Lock App-Server Plain Text and Update Contracts

**Files:**
- Modify: `tests/session_server_contract.rs:6638-6740`
- Modify: `docs/architecture/adr/0002-unified-atomic-mention-system.md:132-190`
- Modify: `docs/architecture/mention-search-glossary.md:73-77`
- Modify: `docs/harness-contract.md:95-120`
- Modify: `README.md` Mention usage sections

- [ ] **Step 1: Add a failing app-server literal-text regression**

Start a thread rooted at a temp directory containing `same.txt`, submit plain text `inspect @same.txt`, then submit `mock_history_echo`. Assert the echoed history contains the literal prompt and does not contain the file body:

```rust
assert!(echoed.contains("inspect @same.txt"));
assert!(!echoed.contains("must-not-be-injected"));
assert!(!echoed.contains("<file"));
```

- [ ] **Step 2: Run the focused server contract and confirm RED**

Run:

```bash
PATH=/Users/bytedance/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin:$PATH \
  cargo test --test session_server_contract server_mode_plain_at_file_remains_literal -- --nocapture
```

Expected: FAIL because shared expansion currently injects the file.

- [ ] **Step 3: Run again after Tasks 1-2 and confirm GREEN**

Use the same command. Expected: PASS without additional server branching, proving the shared expansion boundary is correct.

- [ ] **Step 4: Update source-of-truth documentation**

Make these exact contract changes:

- ADR-0002 compatibility: unbound `@...` is ordinary text; `$skill` compatibility remains.
- ADR invariant 8: only structured bindings expand `@` Mentions.
- Verification contract: add raw-text literal regressions and remove legacy file expansion.
- Glossary: a Mention Token owns search behavior only; it becomes a Mention after candidate selection creates a binding.
- Harness contract: plain text never infers a target; structured `mention` input is required.
- README: remove raw `@file` attachment claims and state that candidate selection performs attachment.

- [ ] **Step 5: Run docs and server checks**

Run:

```bash
git diff --check
PATH=/Users/bytedance/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin:$PATH \
  cargo test --test session_server_contract server_mode_atomic_file_mention_uses_the_bound_workspace_root -- --nocapture
PATH=/Users/bytedance/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin:$PATH \
  cargo test --test session_server_contract server_mode_plain_at_file_remains_literal -- --nocapture
```

Expected: diff check and both structured/literal contracts PASS.

- [ ] **Step 6: Commit protocol and documentation contracts**

```bash
git add tests/session_server_contract.rs README.md docs/architecture/adr/0002-unified-atomic-mention-system.md docs/architecture/mention-search-glossary.md docs/harness-contract.md
git commit -m "docs(mentions): require structured at bindings"
```

### Task 6: Verify the Complete Change

**Files:**
- Verify all modified source, test, and documentation files.

- [ ] **Step 1: Format and inspect whitespace**

Run:

```bash
cargo fmt --check
git diff --check
```

Expected: both commands exit 0.

- [ ] **Step 2: Run focused crates and contracts**

Run:

```bash
PATH=/Users/bytedance/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin:$PATH \
  cargo test -p orca-runtime -p orca-tui -- --test-threads=1
PATH=/Users/bytedance/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin:$PATH \
  cargo test --test history_contract -- --test-threads=1
PATH=/Users/bytedance/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin:$PATH \
  cargo test --test session_server_contract server_mode_atomic_file_mention_uses_the_bound_workspace_root -- --nocapture
PATH=/Users/bytedance/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin:$PATH \
  cargo test --test session_server_contract server_mode_plain_at_file_remains_literal -- --nocapture
```

Expected: all commands exit 0.

- [ ] **Step 3: Run workspace check**

Run:

```bash
PATH=/Users/bytedance/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin:$PATH cargo check --workspace
```

Expected: exit 0.

- [ ] **Step 4: Review final scope**

Run:

```bash
git status --short
git diff --stat a7975833
```

Expected: only binding-only Mention implementation, tests, and contract docs are part of this work; pre-existing TUI mouse-selection changes remain preserved and are not silently reverted.
