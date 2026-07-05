# Runtime Lifecycle Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move Orca runtime task/turn lifecycle state and event payload helpers out of `lifecycle.rs` into a focused `runtime_lifecycle` module while preserving the existing public import path.

**Architecture:** `crates/orca-runtime/src/runtime_lifecycle.rs` owns pure lifecycle state machine types. `crates/orca-runtime/src/lifecycle.rs` re-exports those types and keeps actor/tool/hook behavior for follow-up slices. Ownership contract tests enforce that lifecycle state machine definitions no longer live in `lifecycle.rs`.

**Tech Stack:** Rust, Cargo tests, existing ownership contract tests in `crates/orca-runtime/src/lib.rs`, focused runtime contract tests in `tests/runtime_lifecycle_contract.rs`.

## Global Constraints

- Use TDD: add the ownership contract test before moving production code.
- Preserve existing downstream imports from `orca_runtime::lifecycle::*`.
- Keep this slice behavior-preserving: no task id, event payload, or status mapping changes.
- Do not start release prep until focused and package-level verification pass.

---

### Task 1: Extract Runtime Lifecycle State Machine

**Files:**
- Create: `crates/orca-runtime/src/runtime_lifecycle.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/lifecycle.rs`
- Test: `crates/orca-runtime/src/lib.rs`

**Interfaces:**
- Consumes: existing `RuntimeSessionLifecycle`, `RuntimeTaskLifecycle`, `RuntimeTaskKind`, `RuntimeTaskStatus`, `RuntimeTurnLifecycle`, `RuntimeTurnRunner`, `RuntimeStartedTurn`, `RuntimeAdvancedTurn`.
- Produces: same types re-exported through `orca_runtime::lifecycle::*`.

- [x] **Step 1: Write the failing ownership test**

Add a test named `runtime_lifecycle_state_machine_is_owned_by_runtime_lifecycle_module` to `crates/orca-runtime/src/lib.rs`. It must assert that `lib.rs` declares `mod runtime_lifecycle;`, that `runtime_lifecycle.rs` owns the lifecycle structs/enums/impls, and that `lifecycle.rs` no longer owns them.

- [x] **Step 2: Run the failing test**

Run: `cargo test -p orca-runtime runtime_lifecycle_state_machine_is_owned_by_runtime_lifecycle_module -- --nocapture`

Expected: FAIL because `runtime_lifecycle.rs` does not exist yet.

- [x] **Step 3: Move the lifecycle state machine**

Create `crates/orca-runtime/src/runtime_lifecycle.rs` with the moved lifecycle state machine types and impls. Add `mod runtime_lifecycle;` to `lib.rs`. Add `pub use crate::runtime_lifecycle::{...};` to `lifecycle.rs`.

- [x] **Step 4: Run focused verification**

Run: `cargo test -p orca-runtime runtime_lifecycle_state_machine_is_owned_by_runtime_lifecycle_module -- --nocapture`

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
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/lifecycle.rs crates/orca-runtime/src/runtime_lifecycle.rs docs/superpowers/plans/2026-07-05-runtime-lifecycle-boundary.md
git commit -m "refactor(runtime): extract lifecycle state module"
```
