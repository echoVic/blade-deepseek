# Runtime Special Dispatch Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move runtime-special tool dispatch classification and small runtime-special executors out of `lifecycle.rs` into a focused `runtime_special.rs` module.

**Architecture:** Keep `RuntimeToolActorContext` as the runtime state holder for lifecycle and permission overlay. Add `runtime_special.rs` to own `RuntimeSpecialToolDispatch`, classification by `ToolName`, and the runtime-special helpers that execute `request_permissions`, workflow IPC, subagent status, task list, task stop, and workflow draft preview creation. Leave heavyweight workflow and subagent execution orchestration in the existing execution modules.

**Tech Stack:** Rust 2024, `orca-runtime`, runtime architecture tests in `crates/orca-runtime/src/lib.rs`, existing runtime/tool/server/TUI contract tests.

---

### Task 1: Runtime Special Dispatch Boundary

**Files:**
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/lifecycle.rs`
- Modify: `crates/orca-runtime/src/tool_execution.rs`
- Create: `crates/orca-runtime/src/runtime_special.rs`
- Modify: `docs/production-roadmap.md`
- Create: `docs/superpowers/plans/2026-07-02-runtime-special-dispatch-boundary.md`

- [x] **Step 1: Write the failing architecture test**

Add `runtime_special_dispatch_is_owned_by_runtime_special_module` in `crates/orca-runtime/src/lib.rs`. The test reads `src/runtime_special.rs` and checks that it owns:

```rust
pub enum RuntimeSpecialToolDispatch
pub fn classify_dispatch
pub fn execute_request_permissions_tool
pub fn execute_request_permissions_tool_with_handler
pub fn execute_workflow_ipc_tool
pub fn execute_subagent_status_tool
pub fn execute_task_list_tool
pub fn execute_task_stop_tool
pub fn execute_workflow_draft_tool
```

The same test checks that `src/lifecycle.rs` no longer owns those runtime-special details, while `src/lib.rs` declares `pub(crate) mod runtime_special;`.

- [x] **Step 2: Run the focused RED test**

Run:

```bash
cargo test -p orca-runtime runtime_special_dispatch_is_owned_by_runtime_special_module -- --nocapture
```

Expected: FAIL because `src/runtime_special.rs` does not exist yet.

- [x] **Step 3: Add `runtime_special.rs`**

Create `crates/orca-runtime/src/runtime_special.rs` and move the runtime-special enum, classification, small executors, and parsing/projection helpers out of `lifecycle.rs`.

The new module exposes the same public API currently used by `tool_execution.rs`:

```rust
pub enum RuntimeSpecialToolDispatch { ... }

impl RuntimeToolActorContext {
    pub fn classify_dispatch(&self, request: &ToolRequest) -> RuntimeSpecialToolDispatch { ... }
    pub fn execute_request_permissions_tool(&mut self, request: &ToolRequest) -> ToolResult { ... }
    pub fn execute_request_permissions_tool_with_handler(
        &mut self,
        request: &ToolRequest,
        handler: &dyn RuntimePermissionRequestHandler,
    ) -> ToolResult { ... }
    pub fn execute_workflow_ipc_tool(
        &mut self,
        request: &ToolRequest,
        workflow_ipc: Option<&dyn RuntimeWorkflowIpc>,
    ) -> ToolResult { ... }
    pub fn execute_subagent_status_tool(
        &mut self,
        request: &ToolRequest,
        lookup: &dyn RuntimeSubagentStatusLookup,
    ) -> ToolResult { ... }
    pub fn execute_task_list_tool(
        &mut self,
        request: &ToolRequest,
        task_registry: &TaskRegistry,
    ) -> ToolResult { ... }
    pub fn execute_task_stop_tool(
        &mut self,
        request: &ToolRequest,
        task_registry: &TaskRegistry,
    ) -> ToolResult { ... }
    pub fn execute_workflow_draft_tool(
        &mut self,
        request: &ToolRequest,
        draft_request: RuntimeWorkflowDraftRequest<'_>,
    ) -> std::io::Result<ToolResult> { ... }
}
```

- [x] **Step 4: Wire the module without behavior changes**

In `crates/orca-runtime/src/lib.rs`, add:

```rust
pub(crate) mod runtime_special;
```

In `crates/orca-runtime/src/lifecycle.rs`, keep the shared runtime traits and state types, but remove the runtime-special enum, dispatch classification, and small executor method bodies. Make only the fields needed by the sibling `runtime_special` module `pub(crate)`.

In `crates/orca-runtime/src/tool_execution.rs`, import `RuntimeSpecialToolDispatch` and `RuntimeWorkflowDraftRequest` from `crate::runtime_special` instead of `crate::lifecycle`.

- [x] **Step 5: Run focused GREEN tests**

Run:

```bash
cargo fmt
cargo test -p orca-runtime runtime_special_dispatch_is_owned_by_runtime_special_module -- --nocapture
cargo test --test runtime_lifecycle_contract tool_actor_context_classifies_runtime_special_tool_dispatch -- --nocapture
cargo test --test runtime_lifecycle_contract tool_actor_context_reports_request_permissions_network_domain_grants -- --nocapture
cargo test --test runtime_lifecycle_contract tool_actor_context_executes_workflow_ipc_against_runtime_trait -- --nocapture
cargo test --test runtime_lifecycle_contract tool_actor_context_executes_subagent_status_against_runtime_lookup -- --nocapture
cargo test --test runtime_lifecycle_contract tool_actor_context_stops_running_task_by_task_id -- --nocapture
cargo test --test runtime_lifecycle_contract tool_actor_context_lists_tasks_with_package3_shape -- --nocapture
cargo test --test runtime_lifecycle_contract -- --nocapture
cargo test --test tool_contract -- --nocapture
```

- [x] **Step 6: Run release-gate verification**

Run:

```bash
cargo check --workspace --all-targets
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
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/lifecycle.rs crates/orca-runtime/src/tool_execution.rs crates/orca-runtime/src/runtime_special.rs docs/production-roadmap.md docs/superpowers/plans/2026-07-02-runtime-special-dispatch-boundary.md
git commit -m "refactor(runtime): move runtime-special dispatch into focused module"
```
