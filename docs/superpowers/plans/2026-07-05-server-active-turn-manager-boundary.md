# Server Active Turn Manager Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move server active-turn lifecycle state out of `server.rs` into a focused active-turn manager module.

**Architecture:** Keep `server.rs` responsible for stdio orchestration and protocol event emission, while `server/processors/turn.rs` continues to own decoded turn-control dispatch. Add `server/active_turn_manager.rs` to own active turn controls, running turn join handles, finished-turn reclamation, thread-specific reclaim waits, and completed-turn metadata merge for session-scoped permission grants.

**Tech Stack:** Rust, Cargo tests, ownership contract tests in `crates/orca-runtime/src/lib.rs`, server-mode turn control contracts in `tests/session_server_contract.rs`.

## Global Constraints

- Preserve existing server-mode JSON events for `turn_started`, `turn_controlled`, `item_started`, and `turn_completed`.
- Preserve interrupt, resume, steer, completed-turn rejection, and thread mismatch behavior.
- Preserve session-scoped filesystem and network permission grant metadata when an active turn finishes.
- Use TDD: add the ownership contract before moving production code.
- Commit this slice separately before preparing the patch release.

---

### Task 1: Active Turn Manager Module

**Files:**
- Create: `crates/orca-runtime/src/server/active_turn_manager.rs`
- Modify: `crates/orca-runtime/src/server.rs`
- Modify: `crates/orca-runtime/src/server/processors/submit.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `docs/superpowers/plans/2026-07-05-server-active-turn-manager-boundary.md`

**Interfaces:**
- Consumes: `CancelToken`, `ThreadSteerHandle`, `ServerThread`, `ServerThreadRuntime`, `ThreadMetadataPatch`, and active session permission grant metadata.
- Produces: `ActiveTurnManager`, `ActiveTurnControl`, and `ActiveTurnHandle` for server orchestration and submit/control call sites.

- [x] **Step 1: Write the failing ownership test**

Add `server_active_turn_manager_is_owned_by_active_turn_manager_module` to `crates/orca-runtime/src/lib.rs`. It must assert that `server.rs` declares `mod active_turn_manager;`, that `server/active_turn_manager.rs` owns `ActiveTurnManager`, `ActiveTurnControl`, `ActiveTurnHandle`, and completed-turn metadata merge, and that `server.rs` no longer defines those types.

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p orca-runtime server_active_turn_manager_is_owned_by_active_turn_manager_module -- --nocapture`

Expected: FAIL because `src/server/active_turn_manager.rs` does not exist yet.

- [x] **Step 3: Move active-turn state and reclamation**

Create `crates/orca-runtime/src/server/active_turn_manager.rs` with the active turn control record, running handle wrapper, manager container, `join_all`, `reclaim_finished`, `reclaim_finished_thread`, `has_thread`, and `apply_session_permission_grant`. Add `mod active_turn_manager;` to `server.rs` and replace the raw `HashMap` plus running handle vector with `ActiveTurnManager`.

- [x] **Step 4: Update submit/control call sites**

Replace direct `active_turns.values()`, `active_turns.insert(...)`, and `running_turns.push(...)` usage with `ActiveTurnManager` methods. Keep `run_turn_control` event emission in `server.rs` so this slice only moves state ownership and lifecycle bookkeeping.

- [x] **Step 5: Run focused ownership and turn-control coverage**

Run:

```bash
cargo test -p orca-runtime server_active_turn_manager_is_owned_by_active_turn_manager_module -- --nocapture
cargo test --test session_server_contract active_turn -- --nocapture
cargo test --test session_server_contract turn_control -- --nocapture
cargo test --test session_server_contract server_mode_interrupt -- --nocapture
cargo test --test session_server_contract server_mode_steers -- --nocapture
cargo test --test session_server_contract server_mode_resumes_active_thread_turn_before_cancellation_checkpoint -- --nocapture
```

Expected: ownership, active-turn id, idle/completed/thread-mismatch turn controls, interrupt, steer, and resume contracts PASS.

- [x] **Step 6: Commit implementation and plan**

Run:

```bash
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/server.rs crates/orca-runtime/src/server/active_turn_manager.rs crates/orca-runtime/src/server/processors/submit.rs docs/superpowers/plans/2026-07-05-server-active-turn-manager-boundary.md
git commit -m "refactor(server): extract active turn manager"
```
