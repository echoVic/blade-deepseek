# Task 2 Report: Workflow Tool Schema And Mock Provider Trigger

## Implementation Summary

Completed the Task 2 scope without implementing workflow runtime execution.

Changes made:
- Registered `Workflow` as a builtin tool in `crates/orca-tools/src/registry.rs`
- Added the official workflow input schema fields:
  - `script`
  - `name`
  - `description`
  - `title`
  - `args`
  - `scriptPath`
  - `resumeFromRunId`
- Added a direct-execution failure path for `BuiltinExecutor::Workflow` that returns:
  - `Workflow must be executed by the runtime controller`
- Extended `crates/orca-provider/src/lib.rs` mock prompt parsing so prompts beginning with `workflow ` emit a `ToolRequest` named `Workflow`
- Added `tests/workflow_tool_contract.rs` to lock the schema registration and mock provider request behavior

## TDD Evidence

### RED

First run of:

```bash
cargo test --test workflow_tool_contract
```

Result:
- `workflow_schema_is_registered_with_official_fields` failed because `Workflow tool registered` was not found
- `mock_provider_can_request_workflow_tool` failed because no `tool.call.requested` event was emitted

### GREEN

After the minimal registry and mock-provider changes, reran:

```bash
cargo test --test workflow_tool_contract
```

Result:
- `workflow_schema_is_registered_with_official_fields ... ok`
- `mock_provider_can_request_workflow_tool ... ok`

## Tests Run

- `cargo test --test workflow_tool_contract`

## Files Changed

- `crates/orca-tools/src/registry.rs`
- `crates/orca-provider/src/lib.rs`
- `tests/workflow_tool_contract.rs`

## Self-Review

- Scope stayed within the requested surfaces only; no controller/runtime/CLI/config/event files were edited
- The schema matches the official fields from the task brief
- The mock provider now produces a `Workflow` request for the `workflow ` prefix
- Direct workflow execution remains intentionally unsupported here and fails with a controller-only message

## Concerns

- The task intentionally leaves runtime workflow execution unimplemented, so any direct execution path will still fail after request emission
- The mock-provider test is intentionally event-based and does not assert process exit success, per the task clarification
