# Server Turn Processor Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move server turn-control dispatch out of the generic server router into a focused turn processor.

**Architecture:** Keep `server/router.rs` as the operation-family router. Add `server/processors/turn.rs` to own decoded `TurnInterrupt`, `TurnResume`, and `TurnSteer` dispatch details while preserving the existing `run_turn_control` behavior and JSON wire events.

**Tech Stack:** Rust 2024, `orca-runtime`, server JSONL contract tests, architecture tests in `crates/orca-runtime/src/lib.rs`.

## Global Constraints

- Use TDD: write the failing architecture test before production code.
- Preserve existing `turn_controlled` events and steer item emission.
- Do not move or rewrite `run_turn_control` in this slice.
- Keep active/completed/idle turn semantics unchanged.
- Commit the slice before preparing the patch release.

---

### Task 1: Turn Control Processor Boundary

**Files:**
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/server/router.rs`
- Modify: `crates/orca-runtime/src/server/processors/mod.rs`
- Create: `crates/orca-runtime/src/server/processors/turn.rs`
- Modify: `docs/production-roadmap.md`
- Create: `docs/superpowers/plans/2026-07-02-server-turn-processor-boundary.md`

- [x] **Step 1: Write the failing architecture test**

Add `server_turn_control_dispatch_is_owned_by_turn_processor` in `crates/orca-runtime/src/lib.rs`. The test checks that the router delegates turn control operations to `turn::dispatch_control_operation`, and no longer owns the three turn-control `ClientOp` variants.

- [x] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p orca-runtime server_turn_control_dispatch_is_owned_by_turn_processor -- --nocapture
```

Expected: FAIL because `src/server/processors/turn.rs` does not exist yet.

- [x] **Step 3: Add the turn processor**

Create `crates/orca-runtime/src/server/processors/turn.rs`. The processor exposes:

```rust
pub(in crate::server::router) fn is_control_operation(op: &ClientOp) -> bool
pub(in crate::server::router) fn dispatch_control_operation<W: Write + Send + 'static>(
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: Arc<Mutex<W>>,
) -> io::Result<()>
```

- [x] **Step 4: Route turn control operations through the processor**

In `crates/orca-runtime/src/server/router.rs`, add the turn processor import and delegate matching turn-control operations before the generic `ClientOp` dispatch match.

- [x] **Step 5: Run focused tests**

Run:

```bash
cargo fmt
cargo test -p orca-runtime server_turn_control_dispatch_is_owned_by_turn_processor -- --nocapture
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
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/server/router.rs crates/orca-runtime/src/server/processors/mod.rs crates/orca-runtime/src/server/processors/turn.rs docs/production-roadmap.md docs/superpowers/plans/2026-07-02-server-turn-processor-boundary.md
git commit -m "refactor(server): dispatch turn controls through processor"
```
