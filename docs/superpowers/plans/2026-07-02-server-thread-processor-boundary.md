# Server Thread Processor Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move synchronous server thread query and metadata dispatch out of the generic server router into a focused thread processor.

**Architecture:** Keep `server/router.rs` as the operation-family router. Add `server/processors/thread.rs` to own decoded `ThreadRead`, `ThreadList`, `ThreadSearch`, `ThreadTurnsList`, `ThreadItemsList`, and `ThreadMetadataUpdate` dispatch details while preserving the existing handler functions and JSON wire behavior.

**Tech Stack:** Rust 2024, `orca-runtime`, server JSONL contract tests, architecture tests in `crates/orca-runtime/src/lib.rs`.

## Global Constraints

- Use TDD: write the failing architecture test before production code.
- Preserve the legacy flat JSON wire format and all existing thread events.
- Do not move the underlying `run_thread_*` handler implementations in this slice.
- Preserve finished-turn reclamation before read/turn/item projections.
- Commit the slice before preparing the patch release.

---

### Task 1: Thread Query Processor Boundary

**Files:**
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/server/router.rs`
- Create: `crates/orca-runtime/src/server/processors/mod.rs`
- Create: `crates/orca-runtime/src/server/processors/thread.rs`
- Modify: `docs/production-roadmap.md`
- Create: `docs/superpowers/plans/2026-07-02-server-thread-processor-boundary.md`

- [x] **Step 1: Write the failing architecture test**

Add `server_thread_query_dispatch_is_owned_by_thread_processor` in `crates/orca-runtime/src/lib.rs`. The test checks that the router declares processor modules, delegates thread query operations to `thread::dispatch_query_operation`, and no longer owns the six thread query/metadata `ClientOp` variants.

- [x] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p orca-runtime server_thread_query_dispatch_is_owned_by_thread_processor -- --nocapture
```

Expected: FAIL because `src/server/processors/thread.rs` does not exist yet.

- [x] **Step 3: Add the thread processor**

Create `crates/orca-runtime/src/server/processors/mod.rs` and `crates/orca-runtime/src/server/processors/thread.rs`. The processor exposes:

```rust
pub(in crate::server::router) fn is_query_operation(op: &ClientOp) -> bool
pub(in crate::server::router) fn dispatch_query_operation<W: Write>(
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: &mut W,
) -> io::Result<()>
```

- [x] **Step 4: Route thread query operations through the processor**

In `crates/orca-runtime/src/server/router.rs`, add the processor module and delegate matching thread query/metadata operations before the generic `ClientOp` dispatch match.

- [x] **Step 5: Run focused tests**

Run:

```bash
cargo fmt
cargo test -p orca-runtime server_thread_query_dispatch_is_owned_by_thread_processor -- --nocapture
cargo test -p orca-runtime server_operation_dispatch_is_owned_by_router_module -- --nocapture
cargo test --test server_runtime_contract -- --nocapture
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
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/server/router.rs crates/orca-runtime/src/server/processors/mod.rs crates/orca-runtime/src/server/processors/thread.rs docs/production-roadmap.md docs/superpowers/plans/2026-07-02-server-thread-processor-boundary.md
git commit -m "refactor(server): dispatch thread queries through processor"
```
