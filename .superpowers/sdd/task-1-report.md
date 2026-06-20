# Task 1 Report: Core Workflow And Task Types

## Implementation Summary

Implemented the shared core workflow and task type surfaces for Orca-compatible Dynamic workflows:

- Added `orca_core::workflow_types` with `WorkflowInput`, `WorkflowOutput`, `WorkflowMeta`, `WorkflowRunStatus`, `WorkflowAgentStatus`, and `WorkflowRunState`.
- Added `orca_core::task_types` with `TaskStatus`, `TaskType`, and `BackgroundTaskSummary`.
- Exported the new modules from `crates/orca-core/src/lib.rs`.
- Added `ToolName::Workflow` in `crates/orca-core/src/tool_types.rs` with `as_str() == "Workflow"` and `from_str("Workflow")`.
- Added the contract test file `tests/workflow_types_contract.rs`.

## Tests

Focused verification:

```bash
cargo test --test workflow_types_contract
```

Result: 5 tests passed.

## TDD Evidence

### RED

Before implementation, the contract test failed to compile with unresolved imports:

- `orca_core::task_types`
- `orca_core::workflow_types`

That confirmed the test was exercising missing public API surface, not existing behavior.

### GREEN

After adding the modules and enum variant, the same test suite passed cleanly:

- `workflow_input_accepts_official_fields`
- `workflow_output_serializes_claude_compatible_shape`
- `workflow_tool_name_round_trips`
- `background_task_summary_matches_sdk_names`
- `workflow_status_serializes_snake_case`

## Files Changed

- `crates/orca-core/src/lib.rs`
- `crates/orca-core/src/tool_types.rs`
- `crates/orca-core/src/task_types.rs`
- `crates/orca-core/src/workflow_types.rs`
- `tests/workflow_types_contract.rs`

## Self-Review

- The new types use serde rename rules aligned with the brief and the contract test.
- Public exports are limited to the requested core type modules and the Workflow tool name.
- No runtime, config, event, registry, CLI, or JS host behavior was added.

## Concerns

- None for this task. The implementation stays within the requested scope and the focused contract test is green.

## Review Fix

Resolved the follow-up review note for `ToolName::from_str`:

- Added support for the lowercase `"workflow"` alias in `crates/orca-core/src/tool_types.rs`.
- Extended `tests/workflow_types_contract.rs` so the round-trip test now checks `ToolName::from_str("workflow") == Some(ToolName::Workflow)`.

Verification:

```bash
cargo test --test workflow_types_contract
```

Result: finished successfully, 5 tests passed, 0 failed.
