# Readonly Tool Turn Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move readonly tool batch execution and recording out of `tool_turn.rs` into a focused runtime module without changing tool-turn behavior.

**Architecture:** Keep `tool_turn.rs` responsible for cursoring across provider tool requests, rejecting disallowed child tools, selecting subagent batches, selecting readonly batches, and dispatching normal tool turns. Add `runtime_readonly_tool_turn.rs` to own readonly batch selection, hook-gated parallel execution, result recording, and the readonly tool-turn entrypoint. The readonly module receives grouped `RuntimeReadonlyBatchContext` and `RuntimeReadonlyToolTurnContext` inputs so the new boundary does not introduce another long argument surface.

**Tech Stack:** Rust workspace, `orca-runtime`, existing tool-turn unit tests, architecture ownership tests in `crates/orca-runtime/src/lib.rs`.

---

### Task 1: Lock Readonly Tool-Turn Ownership With A Failing Test

**Files:**
- Modify: `crates/orca-runtime/src/lib.rs`
- Verify: `crates/orca-runtime/src/tool_turn.rs`
- Verify: `crates/orca-runtime/src/runtime_readonly_tool_turn.rs`

- [ ] **Step 1: Write the failing ownership test**

Add a test in `crates/orca-runtime/src/lib.rs`:

```rust
#[test]
fn readonly_tool_turn_is_owned_by_runtime_readonly_module() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let lib_source =
        std::fs::read_to_string(manifest_dir.join("src/lib.rs")).expect("lib source");
    let tool_turn_source =
        std::fs::read_to_string(manifest_dir.join("src/tool_turn.rs")).expect("tool turn source");
    let readonly_source = std::fs::read_to_string(
        manifest_dir.join("src/runtime_readonly_tool_turn.rs"),
    )
    .expect("runtime readonly tool turn source");

    assert!(
        lib_source.contains("pub(crate) mod runtime_readonly_tool_turn;"),
        "runtime crate must declare a focused readonly tool-turn module"
    );
    for marker in [
        "pub(crate) struct RuntimeReadonlyBatchContext",
        "pub(crate) struct RuntimeReadonlyToolTurnContext",
        "pub(crate) fn execute_readonly_batch<W: io::Write>",
        "pub(crate) fn should_run_readonly_batch(",
        "pub(crate) fn collect_readonly_batch(",
        "pub(crate) fn record_readonly_batch_results(",
        "pub(crate) fn run_readonly_tool_turn<W: io::Write>",
    ] {
        assert!(
            readonly_source.contains(marker),
            "runtime_readonly_tool_turn must own readonly detail {marker}"
        );
        assert!(
            !tool_turn_source.contains(marker),
            "tool_turn must not own readonly detail {marker}"
        );
    }
}
```

- [ ] **Step 2: Run the test and confirm RED**

Run:

```bash
cargo test -p orca-runtime readonly_tool_turn_is_owned_by_runtime_readonly_module -- --nocapture
```

Expected: FAIL because `runtime_readonly_tool_turn.rs` is not declared or present.

### Task 2: Extract The Readonly Module

**Files:**
- Create: `crates/orca-runtime/src/runtime_readonly_tool_turn.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/tool_turn.rs`

- [ ] **Step 1: Add module export**

Add near the runtime modules in `crates/orca-runtime/src/lib.rs`:

```rust
pub(crate) mod runtime_readonly_tool_turn;
```

- [ ] **Step 2: Move readonly code**

Move these functions from `tool_turn.rs` into `runtime_readonly_tool_turn.rs`:

```rust
pub(crate) struct RuntimeReadonlyBatchContext<'a, W: io::Write> { ... }
pub(crate) struct RuntimeReadonlyToolTurnContext<'a, W: io::Write> { ... }
pub(crate) fn execute_readonly_batch<W: io::Write>(
    context: RuntimeReadonlyBatchContext<'_, W>,
) -> io::Result<Vec<ToolResult>> { ... }
pub(crate) fn should_run_readonly_batch(...)
pub(crate) fn collect_readonly_batch(...)
pub(crate) fn record_readonly_batch_results(...)
pub(crate) fn run_readonly_tool_turn<W: io::Write>(
    context: RuntimeReadonlyToolTurnContext<'_, W>,
) -> io::Result<ToolTurnOutcome> { ... }
```

The new module imports `ToolTurnOutcome` and `record_tool_result_for_agent` instead of duplicating behavior.

- [ ] **Step 3: Update dispatch imports**

In `tool_turn.rs`, import:

```rust
use crate::runtime_readonly_tool_turn::{
    RuntimeReadonlyToolTurnContext, collect_readonly_batch, run_readonly_tool_turn,
    should_run_readonly_batch,
};
```

Keep `tool_turn.rs` as the owner of `ToolRequestCursor`, `RuntimeToolTurnsContext`, `RuntimeNormalToolTurnContext`, normal tool result folding, and the `run_tool_turns` dispatcher.

- [ ] **Step 4: Run the ownership test and confirm GREEN**

Run:

```bash
cargo test -p orca-runtime readonly_tool_turn_is_owned_by_runtime_readonly_module -- --nocapture
```

Expected: PASS.

### Task 3: Prove Behavior Is Preserved

**Files:**
- Test: `crates/orca-runtime/src/tool_turn.rs`
- Test: `crates/orca-runtime/src/runtime_readonly_tool_turn.rs`

- [ ] **Step 1: Run readonly-focused tests**

Run:

```bash
cargo test -p orca-runtime readonly -- --nocapture
```

Expected: PASS for readonly batch execution, recording, and dispatch coverage.

- [ ] **Step 2: Run focused tool-turn tests**

Run:

```bash
cargo test -p orca-runtime tool_turn -- --nocapture
```

Expected: PASS for tool-turn unit tests.

- [ ] **Step 3: Run focused runtime all-targets tests**

Run:

```bash
cargo test -p orca-runtime --all-targets -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 4: Run formatting and diff checks**

Run:

```bash
cargo fmt -- --check
git diff --check
```

Expected: both exit 0.

- [ ] **Step 5: Run focused clippy**

Run:

```bash
cargo clippy -p orca-runtime --all-targets
```

Expected: exit 0. Existing warnings are acceptable only if clippy exits 0.

### Task 4: Feature Commit

**Files:**
- Stage: `crates/orca-runtime/src/lib.rs`
- Stage: `crates/orca-runtime/src/tool_turn.rs`
- Stage: `crates/orca-runtime/src/runtime_readonly_tool_turn.rs`
- Stage: `docs/superpowers/plans/2026-07-05-readonly-tool-turn-boundary.md`

- [ ] **Step 1: Review diff**

Run:

```bash
git diff --stat
git diff -- crates/orca-runtime/src/lib.rs crates/orca-runtime/src/tool_turn.rs crates/orca-runtime/src/runtime_readonly_tool_turn.rs
```

Expected: diff only moves readonly tool-turn ownership and updates imports/tests.

- [ ] **Step 2: Commit feature**

Run:

```bash
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/tool_turn.rs crates/orca-runtime/src/runtime_readonly_tool_turn.rs docs/superpowers/plans/2026-07-05-readonly-tool-turn-boundary.md
git commit -m "refactor(runtime): extract readonly tool-turn boundary"
```

Expected: one feature commit with no release version changes.
