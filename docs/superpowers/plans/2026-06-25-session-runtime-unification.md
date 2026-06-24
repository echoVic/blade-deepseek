# Session Runtime Unification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move interactive session state ownership from `orca-tui` into `orca-runtime` while preserving current behavior.

**Architecture:** Add `orca_runtime::session::InteractiveSession` as the runtime-owned session state object. Keep `orca_tui::bridge::TuiConversationSession` as a temporary compatibility wrapper that delegates to runtime. Do not change public event formats in this release.

**Tech Stack:** Rust 2024, Cargo workspace crates, existing `orca-core`, `orca-runtime`, `orca-tui`, existing release scripts.

## Global Constraints

- Preserve public CLI behavior for `orca exec`, `orca --mode=server`, and TUI.
- Preserve current JSONL event names and payloads.
- Preserve `/goal`, subagent, workflow, approval, memory, and history behavior.
- Write failing tests before production changes.
- Update docs and release notes before release.
- Release `v0.1.31` only after local verification passes.

---

### Task 1: Runtime Interactive Session Type

**Files:**
- Modify: `crates/orca-runtime/src/session.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Test: `crates/orca-runtime/src/session.rs`

**Interfaces:**
- Produces: `InteractiveSession::new_with_preloaded(config, prompt_for_title, preloaded) -> io::Result<Self>`
- Produces: `InteractiveSession::session_id() -> Option<&str>`
- Produces: `InteractiveSession::usage_totals() -> UsageTotals`
- Produces: `InteractiveSession::has_active_workflows() -> bool`
- Produces: `InteractiveSession::append_message(&mut self, &Message)`
- Produces: `InteractiveSession::complete(&mut self, status)`
- Produces: `InteractiveSession::compact(&mut self, config, cwd) -> (usize, usize)`

- [x] Write failing runtime tests for session initialization, resume/fork writer setup, and active workflow detection.
- [x] Run the focused runtime tests and confirm they fail because `InteractiveSession` does not exist.
- [x] Implement `InteractiveSession` by moving state and helper logic currently in `orca-tui/src/bridge.rs`.
- [x] Run focused runtime tests until they pass.

### Task 2: TUI Compatibility Wrapper

**Files:**
- Modify: `crates/orca-tui/src/bridge.rs`
- Test: `crates/orca-tui/src/bridge.rs`

**Interfaces:**
- Consumes: `orca_runtime::session::InteractiveSession`
- Preserves: public `TuiConversationSession` methods used by `app.rs` and bridge tests.

- [x] Write failing TUI tests asserting `TuiConversationSession` delegates session id, usage totals, active workflow state, backtracking, and compaction to runtime.
- [x] Run focused TUI bridge tests and confirm the delegation tests fail.
- [x] Replace TUI-owned fields with a single runtime session field.
- [x] Add narrow wrapper accessors where the existing TUI runner still needs mutable runtime state.
- [x] Run focused TUI bridge tests until they pass.

### Task 3: Behavior Preservation Sweep

**Files:**
- Modify: `crates/orca-tui/src/bridge.rs`
- Modify: `crates/orca-runtime/src/session.rs`
- Test: existing integration tests

**Interfaces:**
- Preserves: `run_agent_for_tui` signature and return statuses.
- Preserves: workflow notification behavior.
- Preserves: goal context replacement.

- [x] Run `cargo test -p orca-tui bridge::tests::tui_session_reuses_conversation_across_submits`.
- [x] Run `cargo test -p orca-tui bridge::tests::tui_session_backtracks_last_user_before_next_submit`.
- [x] Run `cargo test -p orca-tui bridge::tests::tui_workflow_tool_launches_runtime_instead_of_placeholder_executor`.
- [x] Fix only regressions introduced by the session ownership move.

### Task 4: Docs And Release Prep

**Files:**
- Modify: `docs/production-roadmap.md`
- Create: `docs/releases/v0.1.31.md`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `README.md`

**Interfaces:**
- Produces: release notes and version alignment for `v0.1.31`.

- [x] Document the P0 session-runtime boundary and P1 follow-up.
- [x] Bump root package version to `0.1.31`.
- [x] Update `Cargo.lock`.
- [x] Update README pinned install version.
- [x] Add `docs/releases/v0.1.31.md`.

### Task 5: Verification And Release

**Files:**
- No source changes expected after this task unless verification fails.

**Interfaces:**
- Produces: pushed commit and tag `v0.1.31`.
- Produces: verified GitHub Release and npm package.

- [x] Run `cargo fmt -- --check`.
- [x] Run `cargo test --workspace --all-targets`.
- [x] Run `node scripts/release/test-stage-npm.mjs`.
- [x] Run `git diff --check`.
- [ ] Commit P0 implementation and docs.
- [ ] Push `main`.
- [ ] Create and push tag `v0.1.31`.
- [ ] Wait for GitHub Actions release workflow to complete.
- [ ] Run `node scripts/release/verify-published.mjs --version 0.1.31 --repo echoVic/blade-deepseek --package @blade-ai/orca --bin orca`.
