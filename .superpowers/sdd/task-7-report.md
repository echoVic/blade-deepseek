## Task 7 Report

### Implementation summary
- Added `crates/orca-runtime/src/agent_child.rs` with public `ChildAgentRequest`, `ChildAgentResult`, and `run_child_agent`.
- Reused the new child-agent runner from both `execute_subagent_batch` and `execute_subagent_tool` in `crates/orca-runtime/src/controller.rs`.
- Kept `run_agent_loop` in `controller.rs` and kept controller-owned event emission behavior in place.
- Preserved the existing tool-result formatting in `subagent_execution_to_tool_result`.
- Exported the new module from `crates/orca-runtime/src/lib.rs`.

### Tests run
- `cargo test --test subagent_contract`
- `cargo test --test agent_loop_contract --test tool_contract`

### Files changed
- `crates/orca-runtime/src/agent_child.rs`
- `crates/orca-runtime/src/lib.rs`
- `crates/orca-runtime/src/controller.rs`

### Self-review
- Confirmed the shared runner only factors child config/model override and child cost tracking; it does not move `subagent.started`, `subagent.completed`, or `tool.call.completed` emission out of controller.
- Confirmed both single-subagent and batch-subagent paths now use the same `run_child_agent` surface.
- Confirmed existing subagent tool result strings remain unchanged.
- Added focused unit coverage in `agent_child.rs` for subagent model override handling.
- Verified requested integration contracts still pass after the refactor.

### Concerns
- `cargo fmt -- crates/orca-runtime/src/agent_child.rs crates/orca-runtime/src/lib.rs crates/orca-runtime/src/controller.rs` reformatted some existing code inside `controller.rs`, but the staged scope remains limited to Task 7-owned files.
