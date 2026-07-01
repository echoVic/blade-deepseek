# Server RuntimeThread Adoption Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move server-mode long-lived agent state behind `RuntimeThread` so `ServerThread` owns only server metadata and delegates session/lifecycle turn execution to the runtime thread boundary.

**Architecture:** `RuntimeThread` remains the owner of `InteractiveSession` and `RuntimeSessionLifecycle`. `ServerThread` stores `thread: RuntimeThread`, reads projection/task data through accessors, and runs turns through `RuntimeThread::run_request` / `run_request_with_cancel`, preserving server protocol behavior while removing duplicated session/lifecycle assembly.

**Tech Stack:** Rust 2024, `orca-runtime`, `RuntimeThread`, `ServerThreadRuntime`, existing server runtime contract tests.

## Global Constraints

- Use TDD: prove the old ownership boundary with a failing test before changing production code.
- Keep protocol, JSONL event names, thread metadata, permission overrides, resume/fork, and cancellation behavior unchanged.
- Keep the feature patch-sized and commit it independently before release prep.
- Run focused server runtime tests plus broader workspace checks before committing.

---

### Task 1: Server RuntimeThread Ownership

**Files:**
- Modify: `crates/orca-runtime/src/thread.rs`
- Modify: `crates/orca-runtime/src/server_runtime.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `docs/production-roadmap.md`
- Test: `crates/orca-runtime/src/lib.rs`
- Test: `tests/server_runtime_contract.rs`

- [x] **Step 1: Write the failing architecture test**

Add `server_thread_runtime_owns_agent_state_through_runtime_thread` in `crates/orca-runtime/src/lib.rs`. It should read `server_runtime.rs` with `include_str!`, assert that `ServerThread` contains `thread: RuntimeThread`, and assert that `server_runtime.rs` does not contain `InteractiveSession`, `RuntimeSessionLifecycle`, or `ThreadTurnExecutor`.

- [x] **Step 2: Run the failing test**

Run:

```bash
cargo test -p orca-runtime server_thread_runtime_owns_agent_state_through_runtime_thread -- --nocapture
```

Expected before implementation: FAIL with `ServerThread must hold runtime-owned agent state through RuntimeThread`.

- [x] **Step 3: Add RuntimeThread resume and request helpers**

In `crates/orca-runtime/src/thread.rs`, add:

- `RuntimeThread::resume_same_thread(config, transcript)` for server resume paths.
- `RuntimeThread::run_request(config, request, writer)` for normal turn requests.
- `RuntimeThread::run_request_with_cancel(config, request, writer, cancel)` for active server turn cancellation and steering.

- [x] **Step 4: Move ServerThread state to RuntimeThread**

In `crates/orca-runtime/src/server_runtime.rs`, replace `thread_id`, `session`, and `lifecycle` fields with `thread: RuntimeThread`. Route projection reads, next-turn id calculation, task registry cloning, active task lookup, persisted turn task start, permission persistence, normal turn execution, and cancel/steer turn execution through `RuntimeThread`.

- [x] **Step 5: Run focused architecture and behavior tests**

Run:

```bash
cargo test -p orca-runtime server_thread_runtime_owns_agent_state_through_runtime_thread -- --nocapture
cargo test --test server_runtime_contract -- --nocapture
cargo test -p orca-runtime runtime_thread -- --nocapture
```

Expected: all pass. The server runtime contract should continue to prove live projection, explicit turn requests, resume/fork, permission override persistence, turn id prediction, and turn execution behavior.

- [x] **Step 6: Update roadmap**

Update `docs/production-roadmap.md` to record that server-mode `ServerThread` now stores agent state through `RuntimeThread`, while TUI/headless RuntimeThread adoption remains open.

- [x] **Step 7: Run full verification**

Run:

```bash
cargo fmt -- --check
cargo test --test server_runtime_contract -- --nocapture
cargo test -p orca-runtime runtime_thread -- --nocapture
cargo test --workspace --all-targets
npm --prefix site run build
npm --prefix site run check:seo
node scripts/release/test-stage-npm.mjs
git diff --check
```

- [ ] **Step 8: Commit feature**

Run:

```bash
git add crates/orca-runtime/src/thread.rs crates/orca-runtime/src/server_runtime.rs crates/orca-runtime/src/lib.rs docs/production-roadmap.md docs/superpowers/plans/2026-07-01-server-runtime-thread-adoption.md
git commit -m "refactor(runtime): route server threads through runtime thread"
```
