# Server Shell Processor Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move server shell-session dispatch out of the generic server router into a focused shell processor.

**Architecture:** Keep `server/router.rs` as the operation-family router. Add `server/processors/shell.rs` to own decoded `ShellStart`, `ShellWrite`, `ShellUpdate`, `ShellClose`, `ShellResize`, `ShellList`, `ShellRead`, and `ShellKill` dispatch details while preserving existing `run_shell_*` behavior and JSON wire events.

**Tech Stack:** Rust 2024, `orca-runtime`, server JSONL contract tests, architecture tests in `crates/orca-runtime/src/lib.rs`.

## Global Constraints

- Use TDD: write the failing architecture test before production code.
- Preserve existing `shell_started`, `shell_updated`, `shell_listed`, `shell_output_delta`, and `shell_exited` event behavior.
- Do not move or rewrite the `run_shell_*` handlers in this slice.
- Keep PTY, pipe fallback, shell list/update/read/kill, and task-stop behavior unchanged.
- Commit the slice before preparing the patch release.

---

### Task 1: Shell Processor Boundary

**Files:**
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/server/router.rs`
- Modify: `crates/orca-runtime/src/server/processors/mod.rs`
- Create: `crates/orca-runtime/src/server/processors/shell.rs`
- Modify: `docs/production-roadmap.md`
- Create: `docs/superpowers/plans/2026-07-02-server-shell-processor-boundary.md`

- [x] **Step 1: Write the failing architecture test**

Add `server_shell_dispatch_is_owned_by_shell_processor` in `crates/orca-runtime/src/lib.rs`. The test checks that the router delegates shell operations to `shell::dispatch_shell_operation`, and no longer owns the eight shell `ClientOp` variants.

- [x] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p orca-runtime server_shell_dispatch_is_owned_by_shell_processor -- --nocapture
```

Expected: FAIL because `src/server/processors/shell.rs` does not exist yet.

- [x] **Step 3: Add the shell processor**

Create `crates/orca-runtime/src/server/processors/shell.rs`. The processor exposes:

```rust
pub(in crate::server::router) fn is_shell_operation(op: &ClientOp) -> bool
pub(in crate::server::router) fn dispatch_shell_operation<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: &mut W,
) -> io::Result<()>
```

- [x] **Step 4: Route shell operations through the processor**

In `crates/orca-runtime/src/server/router.rs`, add the shell processor import and delegate matching shell operations before the generic `ClientOp` dispatch match.

- [x] **Step 5: Run focused tests**

Run:

```bash
cargo fmt
cargo test -p orca-runtime server_shell_dispatch_is_owned_by_shell_processor -- --nocapture
cargo test -p orca-runtime server_thread_query_dispatch_is_owned_by_thread_processor -- --nocapture
cargo test -p orca-runtime server_turn_control_dispatch_is_owned_by_turn_processor -- --nocapture
cargo test -p orca-runtime server_operation_dispatch_is_owned_by_router_module -- --nocapture
cargo test --test shell_session_contract -- --nocapture
cargo test --test server_runtime_contract -- --nocapture
cargo test --test session_server_contract runtime_shell_session -- --nocapture
cargo test --test session_server_contract shell_pty -- --nocapture
cargo test --test session_server_contract resize_for_pipe_shell_session -- --nocapture
```

- [ ] **Step 6: Run release-gate verification**

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

- [ ] **Step 7: Commit**

Run:

```bash
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/server/router.rs crates/orca-runtime/src/server/processors/mod.rs crates/orca-runtime/src/server/processors/shell.rs docs/production-roadmap.md docs/superpowers/plans/2026-07-02-server-shell-processor-boundary.md
git commit -m "refactor(server): dispatch shell operations through processor"
```
