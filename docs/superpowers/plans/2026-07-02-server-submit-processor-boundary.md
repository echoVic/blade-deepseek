# Server Submit Processor Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move server submit-family dispatch out of the generic server router into a focused submit processor.

**Architecture:** Keep `server/router.rs` as the operation-family router. Add `server/processors/submit.rs` to own decoded `Submit`, `ThreadStart`, `ThreadResume`, and `ThreadFork` dispatch details while preserving the existing `run_submit`, `run_thread_submit_async`, `run_thread_start`, `run_thread_resume`, and `run_thread_fork` behavior.

**Tech Stack:** Rust 2024, `orca-runtime`, server JSONL contract tests, architecture tests in `crates/orca-runtime/src/lib.rs`.

---

### Task 1: Submit Processor Boundary

**Files:**
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/server/router.rs`
- Modify: `crates/orca-runtime/src/server/processors/mod.rs`
- Create: `crates/orca-runtime/src/server/processors/submit.rs`
- Modify: `docs/production-roadmap.md`
- Create: `docs/superpowers/plans/2026-07-02-server-submit-processor-boundary.md`

- [x] **Step 1: Write the failing architecture test**

Add `server_submit_dispatch_is_owned_by_submit_processor` in `crates/orca-runtime/src/lib.rs`. The test checks that the router delegates submit-family operations to `submit::dispatch_submit_operation`, and no longer owns the decoded `Submit`, `ThreadStart`, `ThreadResume`, or `ThreadFork` dispatch details.

- [x] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p orca-runtime server_submit_dispatch_is_owned_by_submit_processor -- --nocapture
```

Expected: FAIL because `src/server/processors/submit.rs` does not exist yet.

- [x] **Step 3: Add the submit processor**

Create `crates/orca-runtime/src/server/processors/submit.rs`. The processor exposes:

```rust
pub(in crate::server::router) fn is_submit_operation(op: &ClientOp) -> bool
pub(in crate::server::router) fn dispatch_submit_operation<W: Write + Send + 'static>(
    config: &ServerConfig,
    state: &mut ServerState,
    op: ClientOp,
    id: Value,
    writer: Arc<Mutex<W>>,
) -> io::Result<()>
```

The processor must delegate:

- thread-bound `Submit` to `run_thread_submit_async`
- unbound `Submit` to `run_submit`
- `ThreadStart` to `run_thread_start`
- `ThreadResume` to `run_thread_resume`
- `ThreadFork` to `run_thread_fork`

- [x] **Step 4: Route submit-family operations through the processor**

In `crates/orca-runtime/src/server/router.rs`, add the submit processor import and delegate matching submit-family operations after the already-focused query/control/shell/command/permission processors.

- [x] **Step 5: Run focused tests**

Run:

```bash
cargo fmt
cargo test -p orca-runtime server_submit_dispatch_is_owned_by_submit_processor -- --nocapture
cargo test -p orca-runtime server_operation_dispatch_is_owned_by_router_module -- --nocapture
cargo test --test session_server_contract accepts_submit -- --nocapture
cargo test --test session_server_contract thread_start -- --nocapture
cargo test --test session_server_contract routes_turn_start -- --nocapture
cargo test --test session_server_contract resumes_and_forks -- --nocapture
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
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/server/router.rs crates/orca-runtime/src/server/processors/mod.rs crates/orca-runtime/src/server/processors/submit.rs docs/production-roadmap.md docs/superpowers/plans/2026-07-02-server-submit-processor-boundary.md
git commit -m "refactor(server): dispatch submit operations through processor"
```
