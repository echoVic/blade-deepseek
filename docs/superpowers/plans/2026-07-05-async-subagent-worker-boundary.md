# Async Subagent Worker Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move async subagent worker launch/run ownership out of `subagent_execution.rs` into a focused runtime module without changing async task behavior.

**Architecture:** Keep synchronous subagent execution, batch execution, schema validation, and shared worktree result formatting in `subagent_execution.rs`. Add `subagent_async_worker.rs` for async worker entrypoint execution, process spawning, task-registry completion/failure persistence, usage payload shaping, and async worktree handoff. `execute_subagent_tool` delegates async mode to the new module.

**Tech Stack:** Rust workspace, `orca-runtime`, task registry JSON persistence, existing subagent contract tests, TDD ownership tests in `crates/orca-runtime/src/lib.rs`.

---

### Task 1: Lock Async Worker Ownership With A Failing Test

**Files:**
- Modify: `crates/orca-runtime/src/lib.rs`
- Verify: `crates/orca-runtime/src/subagent_execution.rs`
- Verify: `crates/orca-runtime/src/subagent_async_worker.rs`

- [ ] **Step 1: Write the failing ownership test**

Add a test beside the existing architecture ownership tests:

```rust
#[test]
fn async_subagent_worker_is_owned_by_async_worker_module() {
    let lib_source = include_str!("lib.rs");
    let subagent_execution_source = include_str!("subagent_execution.rs");
    let async_worker_source = include_str!("subagent_async_worker.rs");

    assert!(
        lib_source.contains("pub mod subagent_async_worker;"),
        "orca-runtime must expose the async subagent worker module"
    );
    for marker in [
        "pub fn run_async_subagent_worker(",
        "fn spawn_async_subagent_worker(",
        "fn async_subagent_result_payload(",
    ] {
        assert!(
            !subagent_execution_source.contains(marker),
            "subagent_execution must not own async worker detail {marker}"
        );
    }
    for marker in [
        "pub fn run_async_subagent_worker(",
        "pub(crate) fn run_async_subagent_worker_with_executor(",
        "pub(crate) fn launch_async_subagent(",
        "fn spawn_async_subagent_worker(",
        "fn async_subagent_result_payload(",
        ".arg(\"subagent-worker\")",
        "TaskRegistry::new_for_cwd",
        "mark_worker_spawned",
        "complete_with_usage",
        "fail_with_usage",
    ] {
        assert!(
            async_worker_source.contains(marker),
            "subagent_async_worker must own async worker detail {marker}"
        );
    }
}
```

- [ ] **Step 2: Run the test and confirm RED**

Run: `cargo test -p orca-runtime async_subagent_worker_is_owned_by_async_worker_module -- --nocapture`

Expected: FAIL at compile time because `subagent_async_worker.rs` does not exist yet.

- [ ] **Step 3: Commit nothing**

Keep this as an uncommitted TDD red state before implementation.

### Task 2: Extract The Async Worker Module

**Files:**
- Create: `crates/orca-runtime/src/subagent_async_worker.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/subagent_execution.rs`
- Modify: `src/cli/subagent_worker.rs` or the file found by `rg "run_async_subagent_worker"`

- [ ] **Step 1: Add module export**

Add:

```rust
pub mod subagent_async_worker;
```

near `pub mod subagent_execution;` in `crates/orca-runtime/src/lib.rs`.

- [ ] **Step 2: Move async worker code**

Move these items from `subagent_execution.rs` into `subagent_async_worker.rs`:

```rust
pub struct AsyncSubagentWorktree { ... }
pub fn run_async_subagent_worker(...) -> i32 { ... }
pub(crate) fn run_async_subagent_worker_with_executor(...) -> i32 { ... }
pub(crate) fn launch_async_subagent(...) -> tool_types::ToolResult { ... }
fn spawn_async_subagent_worker(...) -> Result<u32, String> { ... }
fn usage_totals_if_non_empty(...) -> Option<UsageTotals> { ... }
fn async_subagent_result_payload(...) -> String { ... }
```

- [ ] **Step 3: Keep shared helpers shared**

Change these helpers in `subagent_execution.rs` to `pub(crate)` so the async worker can reuse the exact schema and worktree behavior:

```rust
pub(crate) fn append_worktree_outcome(...)
pub(crate) fn validate_subagent_output_schema(...)
```

Leave `subagent_output_value` private in `subagent_execution.rs`.

- [ ] **Step 4: Update async call sites**

In `subagent_execution.rs`, import and call:

```rust
use crate::subagent_async_worker::launch_async_subagent;
```

Update the CLI worker entrypoint found by `rg "run_async_subagent_worker"` to use:

```rust
orca_runtime::subagent_async_worker::run_async_subagent_worker(...)
```

- [ ] **Step 5: Run the ownership test and confirm GREEN**

Run: `cargo test -p orca-runtime async_subagent_worker_is_owned_by_async_worker_module -- --nocapture`

Expected: PASS.

### Task 3: Prove Behavior Is Preserved

**Files:**
- Test: `tests/subagent_contract.rs`
- Test: `crates/orca-runtime/src/subagent_execution.rs`
- Test: `crates/orca-runtime/src/lib.rs`

- [ ] **Step 1: Run async subagent contract tests**

Run: `cargo test --test subagent_contract async_subagent -- --nocapture`

Expected: PASS for async launch/completion/schema behavior.

- [ ] **Step 2: Run the status contract test**

Run: `cargo test --test subagent_contract subagent_status_can_read_persisted_async_handle -- --nocapture`

Expected: PASS.

- [ ] **Step 3: Run focused runtime tests**

Run: `cargo test -p orca-runtime --all-targets -- --test-threads=1`

Expected: PASS.

- [ ] **Step 4: Run formatting and whitespace checks**

Run:

```bash
cargo fmt -- --check
git diff --check
```

Expected: both exit 0.

- [ ] **Step 5: Run focused clippy**

Run: `cargo clippy -p orca-runtime --all-targets`

Expected: exit 0. Existing warnings are acceptable only if clippy exits 0.

### Task 4: Feature Commit

**Files:**
- Stage: `crates/orca-runtime/src/lib.rs`
- Stage: `crates/orca-runtime/src/subagent_execution.rs`
- Stage: `crates/orca-runtime/src/subagent_async_worker.rs`
- Stage: async worker CLI call-site found by `rg`
- Stage: `docs/superpowers/plans/2026-07-05-async-subagent-worker-boundary.md`

- [ ] **Step 1: Review diff**

Run: `git diff --stat && git diff -- crates/orca-runtime/src/lib.rs crates/orca-runtime/src/subagent_execution.rs crates/orca-runtime/src/subagent_async_worker.rs`

Expected: diff only moves async worker ownership and exposes shared helpers.

- [ ] **Step 2: Commit feature**

Run:

```bash
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/subagent_execution.rs crates/orca-runtime/src/subagent_async_worker.rs docs/superpowers/plans/2026-07-05-async-subagent-worker-boundary.md
git add "$(rg -l "run_async_subagent_worker" src crates/orca-runtime/src | rg -v "subagent_execution|subagent_async_worker")"
git commit -m "refactor(subagent): extract async worker boundary"
```

Expected: one feature commit with no release version changes.
