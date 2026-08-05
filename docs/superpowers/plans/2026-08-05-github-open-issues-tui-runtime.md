# TUI And Runtime Open Issues Implementation Plan

**Goal:** Fix GitHub issues #19, #20, and #21 without changing valid prompt submission behavior.

**Architecture:** Make Goal actor state transitions unconditional in optimized builds. Route slash commands through command handling in both idle and running composer states, and reject malformed or unknown slash-prefixed input before it reaches the model.

**Tech Stack:** Rust, Tokio runtime actor, Ratatui TUI, Cargo tests in debug and release profiles.

### Task 1: Preserve Goal state transitions in release builds

**Files:**
- Modify: `crates/orca-runtime/src/runtime_host.rs`

1. Reproduce the existing Goal test hang under `cargo test --release` with an external timeout.
2. Move `begin_blocking()` outside `debug_assert!` so it always executes.
3. Run the same release test and confirm it terminates successfully.

### Task 2: Reject malformed slash commands

**Files:**
- Modify: `crates/orca-tui/src/idle_submit_actions.rs`

1. Add a failing test for `/workflow audit` proving no model submission action is emitted.
2. Add a bounded error message with the valid `/workflow:<name>` syntax.
3. Add coverage for a generic unknown slash command.

### Task 3: Handle slash commands while a turn is running

**Files:**
- Modify: `crates/orca-tui/src/queued_input_actions.rs`
- Modify: `crates/orca-tui/src/status_key_actions.rs`

1. Add a failing test that `/workflows` opens the local workflow panel instead of becoming a queued model message.
2. Reuse the slash command dispatcher for running-state submissions.
3. Reject unknown running-state slash commands consistently.

### Task 4: Verify and commit

1. Run focused TUI tests in debug mode.
2. Run the Goal regression test in release mode.
3. Run crate formatting, type checks, and `git diff --check`.
4. Commit with `Fixes #19`, `Fixes #20`, and `Fixes #21` trailers.
