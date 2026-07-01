# Headless RuntimeThread Adoption Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route the headless controller entry point through `RuntimeThread` so headless and server-mode turns share the same runtime-owned session/lifecycle boundary.

**Architecture:** `RuntimeThread` owns `InteractiveSession` and `RuntimeSessionLifecycle`; `run_inner` keeps headless-only session start/end hook and notification behavior, then delegates the agent turn to `RuntimeThread::run_request`. `ThreadTurnRequest` keeps its default `session.completed` emission for server/TUI callers, while headless disables that emission so it can preserve the legacy SessionEnd-hook-before-completed ordering.

**Tech Stack:** Rust 2024, `orca-runtime`, `orca-core::EventSink`, existing controller/runtime lifecycle contract tests.

---

### Task 1: Headless RuntimeThread Ownership

**Files:**
- Modify: `crates/orca-runtime/src/controller.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/session.rs`
- Modify: `crates/orca-core/src/event_sink.rs`
- Modify: `tests/runtime_lifecycle_contract.rs`
- Modify: `docs/production-roadmap.md`

- [x] **Step 1: Write failing architecture test**

Add `headless_run_inner_enters_agent_loop_through_runtime_thread` in `crates/orca-runtime/src/lib.rs`. It asserts that the `run_inner` body contains `RuntimeThread::start` and `.run_request(` and no longer directly creates `RuntimeSessionLifecycle::new(new_run_id())`, `TaskRegistry::new_for_cwd`, or `run_agent_loop(`.

- [x] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p orca-runtime headless_run_inner_enters_agent_loop_through_runtime_thread -- --nocapture
```

Expected before implementation: FAIL with `headless run_inner must create long-lived agent state through RuntimeThread`.

- [x] **Step 3: Route headless turn execution through RuntimeThread**

Update `run_inner` to create `RuntimeThread::start(&config, &prompt)`, emit headless `session.started`, run SessionStart hook through `thread.session().hooks()`, delegate the turn with `thread.run_request`, run SessionEnd hook, then emit `session.completed`.

- [x] **Step 4: Preserve writer and event ordering**

Expose `EventSink::writer_mut` so headless can emit pre-turn events, borrow the same writer for `RuntimeThread::run_request`, then resume post-turn event emission. Add `ThreadTurnRequest::with_session_completed_event(false)` for headless so `ThreadTurnExecutor` does not emit `session.completed` before SessionEnd hooks.

- [x] **Step 5: Update behavior coverage**

Extend `controller_turn_started_events_include_agent_task_lifecycle` to assert that headless `session.started`, `turn.started`, and `session.completed` share the same `runId`.

- [x] **Step 6: Run focused verification**

Run:

```bash
cargo fmt -- --check
cargo test -p orca-runtime runtime_thread -- --nocapture
cargo test -p orca-runtime headless_run_inner_enters_agent_loop_through_runtime_thread -- --nocapture
cargo test --test runtime_lifecycle_contract controller_turn_started_events_include_agent_task_lifecycle -- --nocapture
cargo test --test server_runtime_contract -- --nocapture
```

- [ ] **Step 7: Run full verification and commit**

Run:

```bash
cargo test --workspace --all-targets
npm --prefix site run build
npm --prefix site run check:seo
node scripts/release/test-stage-npm.mjs
git diff --check
```

Then commit:

```bash
git add crates/orca-core/src/event_sink.rs crates/orca-runtime/src/controller.rs crates/orca-runtime/src/lib.rs crates/orca-runtime/src/session.rs tests/runtime_lifecycle_contract.rs docs/production-roadmap.md docs/superpowers/plans/2026-07-01-headless-runtime-thread-adoption.md
git commit -m "refactor(runtime): route headless turns through runtime thread"
```
