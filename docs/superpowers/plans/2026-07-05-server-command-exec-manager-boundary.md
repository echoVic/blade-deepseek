# Server Command Exec Manager Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move command/exec process state ownership out of `server.rs` into a focused server command-exec manager module.

**Architecture:** Keep `server/processors/command_exec.rs` responsible for decoded protocol dispatch and keep `server.rs` responsible for high-level server orchestration. Add `server/command_exec_manager.rs` to own active process records, duplicate process-id checks, streaming drain behavior, write/resize/terminate helpers, and drain outcomes used by permission retry paths.

**Tech Stack:** Rust, Cargo tests, ownership contract tests in `crates/orca-runtime/src/lib.rs`, command/exec server contract tests in `tests/session_server_contract.rs`.

## Global Constraints

- Preserve existing server-mode JSON wire events and `command/exec` method names.
- Preserve command/exec network and filesystem permission retry behavior.
- Use TDD: add the ownership contract test before moving production code.
- Do not rewrite command sandbox resolution in this slice.
- Commit this slice separately before preparing the patch release.

---

### Task 1: Command Exec Manager Module

**Files:**
- Create: `crates/orca-runtime/src/server/command_exec_manager.rs`
- Modify: `crates/orca-runtime/src/server.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `docs/superpowers/plans/2026-07-05-server-command-exec-manager-boundary.md`

**Interfaces:**
- Consumes: `RuntimeShellSessionManager`, `RuntimeNetworkBlockReport`, `RuntimeNetworkProxy`, `SandboxDenialDiagnostic`, `PendingCommandExecPermissionRequest`, `protocol::write_server_event`, `ServerEvent`, and existing output-cap helpers.
- Produces: `CommandExecManager`, `CommandExecProcess`, and `CommandExecDrainOutcome` for `server.rs` command/exec handlers.

- [x] **Step 1: Write the failing ownership test**

Add `server_command_exec_manager_is_owned_by_command_exec_manager_module` to `crates/orca-runtime/src/lib.rs`. It must assert that `server.rs` declares `mod command_exec_manager;`, that `server/command_exec_manager.rs` owns `CommandExecManager`, `CommandExecProcess`, and `CommandExecDrainOutcome`, and that `server.rs` no longer defines those types.

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p orca-runtime server_command_exec_manager_is_owned_by_command_exec_manager_module -- --nocapture`

Expected: FAIL because `src/server/command_exec_manager.rs` does not exist and `server.rs` still owns the manager types.

- [x] **Step 3: Move the manager implementation**

Create `crates/orca-runtime/src/server/command_exec_manager.rs` with the moved process record, manager, and drain outcome. Add `mod command_exec_manager;` to `server.rs` and import the moved types. Keep helper functions that are still used broadly by command/exec startup and sandbox code in `server.rs`.

- [x] **Step 4: Run focused ownership and manager tests**

Run:

```bash
cargo test -p orca-runtime server_command_exec_manager_is_owned_by_command_exec_manager_module -- --nocapture
cargo test -p orca-runtime command_exec_manager_rejects_duplicate_active_process_id_until_removed -- --nocapture
```

Expected: both tests PASS.

- [x] **Step 5: Run command/exec contract coverage**

Run:

```bash
cargo test --test session_server_contract command_exec -- --nocapture
cargo test -p orca-runtime server_command_exec_dispatch_is_owned_by_command_exec_processor -- --nocapture
```

Expected: command/exec server contract tests and existing processor ownership test PASS.

- [x] **Step 6: Commit implementation and plan**

Run:

```bash
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/server.rs crates/orca-runtime/src/server/command_exec_manager.rs docs/superpowers/plans/2026-07-05-server-command-exec-manager-boundary.md
git commit -m "refactor(server): extract command exec manager"
```
