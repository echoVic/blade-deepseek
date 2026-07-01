# Server Command Exec Processor Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move server command/exec dispatch out of the generic server router into a focused command-exec processor.

**Architecture:** Keep `server/router.rs` as the operation-family router. Add `server/processors/command_exec.rs` to own decoded `CommandExec`, `CommandExecWrite`, `CommandExecResize`, and `CommandExecTerminate` dispatch details while preserving existing `run_command_exec*` behavior and JSON wire events.

**Tech Stack:** Rust 2024, `orca-runtime`, server JSONL contract tests, architecture tests in `crates/orca-runtime/src/lib.rs`.

## Global Constraints

- Use TDD: write the failing architecture test before production code.
- Preserve command execution output, process-id streaming, stdin write, resize, termination, sandboxing, permission-profile, network proxy, and cleanup behavior.
- Do not move or rewrite the `run_command_exec*` handlers in this slice.
- Keep the router responsible only for identifying operation families and coordinating writer ownership.
- Commit the slice before preparing the patch release.

---

### Task 1: Command Exec Processor Boundary

**Files:**
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/server/router.rs`
- Modify: `crates/orca-runtime/src/server/processors/mod.rs`
- Create: `crates/orca-runtime/src/server/processors/command_exec.rs`
- Modify: `docs/production-roadmap.md`
- Create: `docs/superpowers/plans/2026-07-02-server-command-exec-processor-boundary.md`

- [x] **Step 1: Write the failing architecture test**

Add `server_command_exec_dispatch_is_owned_by_command_exec_processor` in `crates/orca-runtime/src/lib.rs`. The test checks that the router delegates command execution operations to `command_exec::dispatch_command_exec_operation`, and no longer owns the four command-exec `ClientOp` variants.

- [x] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p orca-runtime server_command_exec_dispatch_is_owned_by_command_exec_processor -- --nocapture
```

Expected: FAIL because `src/server/processors/command_exec.rs` does not exist yet.

- [x] **Step 3: Add the command-exec processor**

Create `crates/orca-runtime/src/server/processors/command_exec.rs`. The processor exposes:

```rust
pub(in crate::server::router) fn is_command_exec_operation(op: &ClientOp) -> bool
pub(in crate::server::router) fn dispatch_command_exec_operation<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: &mut W,
) -> io::Result<()>
```

- [x] **Step 4: Route command-exec operations through the processor**

In `crates/orca-runtime/src/server/router.rs`, add the command-exec processor import and delegate matching command-exec operations before the generic `ClientOp` dispatch match.

- [x] **Step 5: Run focused tests**

Run:

```bash
cargo fmt
cargo test -p orca-runtime server_command_exec_dispatch_is_owned_by_command_exec_processor -- --nocapture
cargo test -p orca-runtime server_shell_dispatch_is_owned_by_shell_processor -- --nocapture
cargo test -p orca-runtime server_thread_query_dispatch_is_owned_by_thread_processor -- --nocapture
cargo test -p orca-runtime server_turn_control_dispatch_is_owned_by_turn_processor -- --nocapture
cargo test -p orca-runtime server_operation_dispatch_is_owned_by_router_module -- --nocapture
cargo test --test session_server_contract command_exec -- --nocapture
cargo test --test server_runtime_contract -- --nocapture
```

- [x] **Step 6: Run release-gate verification**

Run:

```bash
cargo fmt -- --check
cargo test --workspace --all-targets
npm --prefix site run build
npm --prefix site run check:seo
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
node scripts/release/real-api-e2e.mjs --timeout-ms 180000
git diff --check
```

- [x] **Step 7: Commit**

Run:

```bash
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/server/router.rs crates/orca-runtime/src/server/processors/mod.rs crates/orca-runtime/src/server/processors/command_exec.rs docs/production-roadmap.md docs/superpowers/plans/2026-07-02-server-command-exec-processor-boundary.md
git commit -m "refactor(server): dispatch command exec operations through processor"
```
