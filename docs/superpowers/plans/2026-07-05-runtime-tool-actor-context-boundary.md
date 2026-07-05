# Runtime Tool Actor Context Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the runtime tool actor context out of `lifecycle.rs` into a focused `runtime_tool_actor` module while preserving the existing public import path.

**Architecture:** `crates/orca-runtime/src/runtime_tool_actor.rs` owns `RuntimeToolActorContext` and its adapter methods. `crates/orca-runtime/src/lifecycle.rs` keeps the lower-level task actor, permission overlay, hook helpers, and user-input parsing, and re-exports `RuntimeToolActorContext` for downstream compatibility.

**Tech Stack:** Rust, Cargo tests, ownership contract tests in `crates/orca-runtime/src/lib.rs`, focused runtime contract tests in `tests/runtime_lifecycle_contract.rs`.

---

### Task 1: Extract Runtime Tool Actor Context

**Files:**
- Create: `crates/orca-runtime/src/runtime_tool_actor.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/lifecycle.rs`
- Test: `crates/orca-runtime/src/lib.rs`

**Interfaces:**
- Consumes: existing `RuntimeToolActorContext` constructor, approval/hook/user-input helpers, normal-tool execution helpers, `RuntimeTaskActor`, `TurnPermissionOverlay`, and `RuntimeNormalToolInvocation`.
- Produces: the same `RuntimeToolActorContext` re-exported through `orca_runtime::lifecycle::*`.

- [x] **Step 1: Write the failing ownership test**

Add `runtime_tool_actor_context_is_owned_by_runtime_tool_actor_module` to `crates/orca-runtime/src/lib.rs`. It must assert that `lib.rs` declares `mod runtime_tool_actor;`, that `runtime_tool_actor.rs` owns `RuntimeToolActorContext` and its impl, and that `lifecycle.rs` re-exports rather than owns the context.

- [x] **Step 2: Run the failing test**

Run: `cargo test -p orca-runtime runtime_tool_actor_context_is_owned_by_runtime_tool_actor_module -- --nocapture`

Expected: FAIL because `runtime_tool_actor.rs` does not exist yet.

- [x] **Step 3: Move the context**

Create `crates/orca-runtime/src/runtime_tool_actor.rs` with the moved `RuntimeToolActorContext` type and impl. Add `mod runtime_tool_actor;` to `lib.rs`. Add `pub use crate::runtime_tool_actor::RuntimeToolActorContext;` to `lifecycle.rs`. Remove the old struct and impl from `lifecycle.rs`.

- [x] **Step 4: Run focused verification**

Run: `cargo test -p orca-runtime runtime_tool_actor_context_is_owned_by_runtime_tool_actor_module -- --nocapture`

Expected: PASS.

Run: `cargo test --test runtime_lifecycle_contract -- --nocapture`

Expected: PASS.

- [x] **Step 5: Run package verification**

Run: `cargo fmt -- --check`

Expected: PASS.

Run: `git diff --check`

Expected: PASS.

Run: `cargo test -p orca-runtime --all-targets -- --test-threads=1`

Expected: PASS.

- [x] **Step 6: Commit**

Run:

```bash
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/lifecycle.rs crates/orca-runtime/src/runtime_tool_actor.rs docs/superpowers/plans/2026-07-05-runtime-tool-actor-context-boundary.md
git commit -m "refactor(runtime): extract tool actor context module"
```
