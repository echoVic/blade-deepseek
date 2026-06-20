# Task 10 Report: Controller Integration And Background Launch

## Implementation Summary

Implemented runtime execution for the built-in `Workflow` tool so the controller now treats it as a runtime-special tool instead of letting it fall through to the placeholder tool executor.

Main behavior added:

- `Workflow` tool calls in the controller now parse `WorkflowInput`, launch a background workflow through `WorkflowRunner`, and return a `tool.call.completed` payload whose `output` is serialized `WorkflowOutput`.
- The returned `WorkflowOutput` now uses `status: "async_launched"` and `taskType: "local_workflow"` for controller-driven async launches.
- JSONL runtime sessions now emit `workflow.started` immediately and then observe background workflow completion before `session.completed`, emitting `workflow.completed` plus `workflow.result.available` on success and `workflow.failed` on failure.
- Server mode now maps workflow runtime events into protocol events.

## TDD Evidence

### Red

Added the end-to-end test `workflow_tool_launches_background_task_and_returns_output` to `tests/workflow_tool_contract.rs` first.

Ran:

```bash
cargo test --test workflow_tool_contract workflow_tool_launches_background_task_and_returns_output
```

Observed expected failure:

- `tool.call.completed.payload.status` was `"failed"` instead of `"completed"`.
- This matched the known pre-integration behavior where `Workflow` still resolved to the placeholder executor path.

### Green

After the controller/runner/event changes, re-ran:

```bash
cargo test --test workflow_tool_contract workflow_tool_launches_background_task_and_returns_output
```

Result:

- Test passed.
- Verified the JSONL contract sees `tool.call.completed` with serialized async workflow output and workflow lifecycle events.

## Files Changed

Primary task files:

- `crates/orca-runtime/src/controller.rs`
- `crates/orca-runtime/src/server.rs`
- `tests/workflow_tool_contract.rs`

Additional files changed because they were strictly necessary for controller integration:

- `crates/orca-runtime/src/workflow/runner.rs`
- `crates/orca-runtime/src/workflow/mod.rs`
- `crates/orca-core/src/event_schema.rs`

## Tests Run

Required tests:

```bash
cargo test --test workflow_tool_contract workflow_tool_launches_background_task_and_returns_output
```

- Passed after implementation.

```bash
cargo test --test workflow_tool_contract
```

- Passed: 3 passed, 0 failed.

```bash
cargo test --test subagent_contract --test agent_loop_contract --test session_server_contract
```

- Passed:
  - `agent_loop_contract`: 2 passed
  - `session_server_contract`: 1 passed
  - `subagent_contract`: 3 passed

## Notes On Dirty Owned Files

Before this task, the owned files `crates/orca-runtime/src/server.rs` and `tests/workflow_tool_contract.rs` already had import-order-only dirty hunks.

- Those files still needed functional task edits, so they were necessarily part of the scoped change set.
- The import-order hunks remain part of the staged/committed file versions because the files were touched for task behavior and then formatted.
- I am not counting those import-order-only lines as Task 10 behavior changes.

## Self-Review

What I checked:

- `Workflow` execution is now intercepted in the runtime controller path and no longer goes through the placeholder executor.
- Existing subagent behavior remains intact; subagent regression tests passed unchanged.
- Tool approval and tool result semantics remain intact; workflow still emits a standard completed tool result containing serialized output.
- JSONL exec/server paths now surface workflow lifecycle events without changing the non-JSONL background-task behavior.

Design choices worth calling out:

- I kept background workflow observation scoped to JSONL output, which satisfies the exec contract test and keeps interactive/TUI-oriented behavior background-compatible.
- `WorkflowRunner` gained a narrow async-launch seam so the controller can return task/run identifiers immediately while the workflow continues on a background thread.

## Concerns

- `observe_background_workflows` only joins background workflow threads for JSONL output. That is intentional for this contract, but it means non-JSONL paths keep detached background workflow execution semantics.
- Implementing immediate async launch required small, targeted changes outside the three primary files, specifically in `workflow/runner.rs`, `workflow/mod.rs`, and `event_schema.rs`.
