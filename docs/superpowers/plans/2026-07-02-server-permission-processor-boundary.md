# Server Permission Processor Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move server permission response dispatch out of the generic server router into a focused permission processor.

**Architecture:** Keep `server/router.rs` as the operation-family router. Add `server/processors/permission.rs` to own decoded `PermissionRespond` dispatch details while preserving the existing `run_permission_respond` behavior for turn/session grants, strict auto-review, network grants/denials, and filesystem overlays.

**Tech Stack:** Rust 2024, `orca-runtime`, server JSONL contract tests, architecture tests in `crates/orca-runtime/src/lib.rs`.

## Global Constraints

- Use TDD: write the failing architecture test before production code.
- Preserve `permission/respond` JSON protocol shape and all permission grant/deny behavior.
- Do not move or rewrite `run_permission_respond` in this slice.
- Keep router responsible only for identifying operation families and coordinating writer ownership.
- Commit the slice before preparing the patch release.

---

### Task 1: Permission Processor Boundary

**Files:**
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/server/router.rs`
- Modify: `crates/orca-runtime/src/server/processors/mod.rs`
- Create: `crates/orca-runtime/src/server/processors/permission.rs`
- Modify: `docs/production-roadmap.md`
- Create: `docs/superpowers/plans/2026-07-02-server-permission-processor-boundary.md`

- [x] **Step 1: Write the failing architecture test**

Add `server_permission_dispatch_is_owned_by_permission_processor` in `crates/orca-runtime/src/lib.rs`. The test checks that the router delegates permission operations to `permission::dispatch_permission_operation`, and no longer owns the `PermissionRespond` dispatch details.

- [x] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p orca-runtime server_permission_dispatch_is_owned_by_permission_processor -- --nocapture
```

Expected: FAIL because `src/server/processors/permission.rs` does not exist yet.

- [x] **Step 3: Add the permission processor**

Create `crates/orca-runtime/src/server/processors/permission.rs`. The processor exposes:

```rust
pub(in crate::server::router) fn is_permission_operation(op: &ClientOp) -> bool
pub(in crate::server::router) fn dispatch_permission_operation<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: &mut W,
) -> io::Result<()>
```

- [x] **Step 4: Route permission operations through the processor**

In `crates/orca-runtime/src/server/router.rs`, add the permission processor import and delegate matching permission operations before the generic `ClientOp` dispatch match.

- [x] **Step 5: Run focused tests**

Run:

```bash
cargo fmt
cargo test -p orca-runtime server_permission_dispatch_is_owned_by_permission_processor -- --nocapture
cargo test -p orca-runtime server_command_exec_dispatch_is_owned_by_command_exec_processor -- --nocapture
cargo test -p orca-runtime server_shell_dispatch_is_owned_by_shell_processor -- --nocapture
cargo test -p orca-runtime server_operation_dispatch_is_owned_by_router_module -- --nocapture
cargo test --test session_server_contract request_permissions -- --nocapture
cargo test --test session_server_contract network -- --nocapture
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
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/server/router.rs crates/orca-runtime/src/server/processors/mod.rs crates/orca-runtime/src/server/processors/permission.rs docs/production-roadmap.md docs/superpowers/plans/2026-07-02-server-permission-processor-boundary.md
git commit -m "refactor(server): dispatch permission operations through processor"
```
