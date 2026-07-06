# Server Permission Manager Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move server pending-permission request state and runtime permission request handling out of `server.rs` into a focused permission manager module.

**Architecture:** Keep `server.rs` responsible for stdio orchestration, permission response event shaping, session grant persistence, and command/exec retry dispatch. Add `server/permission_manager.rs` to own pending permission storage, runtime permission request registration, command/exec pending request records, and the runtime permission handler used by active thread turns.

**Tech Stack:** Rust, `orca-runtime`, server JSON protocol, runtime permission handler trait, command/exec permission retry flow.

## Global Constraints

- Preserve the existing server wire protocol: `permission_request`, `permission_resolved`, and error events keep the same JSON shape.
- Preserve command/exec retry behavior for network and filesystem permission prompts.
- Preserve runtime `request_permissions` behavior for active thread turns.
- Use TDD: add the failing ownership test first, verify RED, then implement the refactor.
- Keep this as a behavior-compatible architecture slice and release it as one patch version.

---

### Task 1: Permission Manager Boundary

**Files:**
- Create: `crates/orca-runtime/src/server/permission_manager.rs`
- Modify: `crates/orca-runtime/src/server.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `docs/production-roadmap.md`
- Create: `docs/superpowers/plans/2026-07-05-server-permission-manager-boundary.md`

**Interfaces:**
- Consumes: `RuntimePermissionRequestHandler`, `RuntimePermissionResponse`, `PendingCommandExecPermissionRequest` fields currently used by command/exec retry.
- Produces: `PendingPermissionManager`, `PendingPermissionRequest`, `PendingCommandExecPermissionRequest`, and `ServerPermissionRequestHandler`.

- [x] **Step 1: Write the failing ownership test**

Add `server_permission_manager_is_owned_by_permission_manager_module` to `crates/orca-runtime/src/lib.rs`. It must assert that `server.rs` declares `mod permission_manager;`, that `server/permission_manager.rs` owns `PendingPermissionManager`, `PendingPermissionRequest`, `PendingCommandExecPermissionRequest`, and `ServerPermissionRequestHandler`, and that the new module owns runtime permission handling plus command/exec pending insertion.

- [x] **Step 2: Run the test to verify RED**

Run:

```bash
cargo test -p orca-runtime server_permission_manager_is_owned_by_permission_manager_module -- --nocapture
```

Expected: FAIL because `src/server/permission_manager.rs` does not exist yet.

- [x] **Step 3: Move pending permission state into the manager**

Create `crates/orca-runtime/src/server/permission_manager.rs` with `PendingCommandExecPermissionRequest`, `PendingPermissionRequest`, `PendingPermissionManager`, and `ServerPermissionRequestHandler`. Add `mod permission_manager;` to `server.rs` and replace direct `Arc<Mutex<HashMap<...>>>` ownership with `PendingPermissionManager`.

- [x] **Step 4: Route existing server behavior through the manager**

Use manager methods for runtime handler creation, command/exec permission insertion, and permission response removal. Keep `run_permission_respond` in `server.rs` so this slice does not change permission response event shaping, session grant persistence, or command/exec retry execution.

- [x] **Step 5: Verify focused behavior**

Run:

```bash
cargo test -p orca-runtime server_permission_manager_is_owned_by_permission_manager_module -- --nocapture
cargo test --test session_server_contract permission -- --nocapture
cargo test --test session_server_contract command_exec -- --nocapture
cargo test --test session_server_contract turn_control -- --nocapture
cargo test --test session_server_contract turn -- --test-threads=1 --nocapture
cargo fmt -- --check
git diff --check
cargo test -p orca-runtime --all-targets -- --test-threads=1
cargo clippy -p orca-runtime --all-targets
```

Expected: all commands pass. Existing clippy warnings are acceptable only if they are unrelated to this slice.

- [x] **Step 6: Commit**

```bash
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/server.rs crates/orca-runtime/src/server/permission_manager.rs docs/production-roadmap.md docs/superpowers/plans/2026-07-05-server-permission-manager-boundary.md
git commit -m "refactor(server): extract permission manager"
```
