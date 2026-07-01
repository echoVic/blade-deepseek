# TUI RuntimeThread Adoption Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move TUI long-lived conversation state behind `RuntimeThread` so TUI, server mode, and headless exec share the same runtime-owned session/lifecycle ownership boundary.

**Architecture:** `TuiConversationSession` remains the TUI compatibility wrapper used by `app.rs`, but it now owns `RuntimeThread` instead of directly storing `InteractiveSession` and `RuntimeSessionLifecycle`. Existing TUI behavior still calls the same bridge helpers; those helpers delegate session and lifecycle access through the runtime thread boundary.

**Tech Stack:** Rust 2024, `orca-tui`, `orca-runtime`, `RuntimeThread`, existing TUI bridge tests.

## Global Constraints

- Keep public TUI behavior stable: keybindings, slash commands, approval prompts, workflow controls, goal mode, and event names must not change.
- Follow TDD: write the architecture guard before changing production code and watch it fail.
- Keep the slice patch-sized: do not move the entire TUI agent loop in this release.

---

### Task 1: TUI RuntimeThread Ownership

**Files:**
- Modify: `crates/orca-runtime/src/thread.rs`
- Modify: `crates/orca-tui/src/bridge.rs`
- Modify: `docs/production-roadmap.md`
- Create: `docs/releases/v0.1.80.md`
- Modify: `site/src/shared.ts`
- Modify: `site/src/changelog/Changelog.tsx`

**Interfaces:**
- Consumes: `RuntimeThread::session`, `RuntimeThread::session_mut`, `RuntimeThread::lifecycle`, `RuntimeThread::lifecycle_mut`
- Produces: `RuntimeThread::start_with_preloaded(config: &RunConfig, title: impl Into<String>, preloaded: Option<SessionTranscript>) -> io::Result<RuntimeThread>`

- [x] **Step 1: Write the failing architecture guard**

Add `tui_session_owns_runtime_thread_boundary` in `crates/orca-tui/src/bridge.rs`. It reads `bridge.rs` with `include_str!`, extracts the `TuiConversationSession` struct, and asserts it contains `runtime: RuntimeThread` while not containing `RuntimeSessionLifecycle` or `InteractiveSession`.

- [x] **Step 2: Run the test to verify RED**

Run:

```bash
cargo test -p orca-tui tui_session_owns_runtime_thread_boundary -- --nocapture
```

Expected before implementation: FAIL with `TUI session must own RuntimeThread instead of rebuilding runtime state locally`.

- [x] **Step 3: Add preloaded RuntimeThread construction**

Add `RuntimeThread::start_with_preloaded` in `crates/orca-runtime/src/thread.rs`, delegating to `InteractiveSession::new_with_preloaded` and `RuntimeThread::from_session`.

- [x] **Step 4: Move TUI wrapper ownership to RuntimeThread**

Change `TuiConversationSession` to store `runtime: RuntimeThread`. Update session, conversation, writer, cost, MCP, hooks, memory, tasks, compaction, and lifecycle helper methods to delegate through `runtime.session()`, `runtime.session_mut()`, `runtime.lifecycle()`, and `runtime.lifecycle_mut()`.

- [x] **Step 5: Run focused TUI and runtime tests**

Run:

```bash
cargo test -p orca-tui tui_session_owns_runtime_thread_boundary -- --nocapture
cargo test -p orca-tui tui_ -- --nocapture
cargo test -p orca-runtime runtime_thread -- --nocapture
cargo test -p orca-runtime headless_run_inner_enters_agent_loop_through_runtime_thread -- --nocapture
cargo test --test runtime_lifecycle_contract controller_turn_started_events_include_agent_task_lifecycle -- --nocapture
```

Expected: all pass.

- [x] **Step 6: Update release docs and public site metadata**

Update roadmap baseline/status, add `docs/releases/v0.1.80.md`, bump the site release list/changelog, update the README pinned install version, and keep structured-data `softwareVersion` current.
