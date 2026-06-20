# Task 3 Report: Workflow Events And Config

## Implementation summary

Implemented the shared workflow event/config surface for Task 3 without adding workflow runtime behavior:

- Added workflow lifecycle `EventType` variants in `crates/orca-core/src/event_schema.rs`.
- Added `EventFactory` helpers for:
  - `workflow_started`
  - `workflow_agent_completed`
  - `workflow_completed`
  - `workflow_result_available`
- Added `WorkflowConfig` to `crates/orca-core/src/config/mod.rs` and wired it into `RunConfig`.
- Added workflow file-config parsing in `crates/orca-core/src/config/file.rs`, including alias support for:
  - `disableWorkflows`
  - `enableWorkflows`
  - `workflowKeywordTriggerEnabled`
- Updated `RunConfig` constructor sites to pass normalized workflow config from file config.
- Added contract coverage in `tests/workflow_events_contract.rs`.

## TDD evidence

### RED

Ran:

```bash
cargo test --test workflow_events_contract
```

Initial failure was as expected:

- unresolved import: `orca_core::config::WorkflowConfig`
- missing `EventType::WorkflowStarted`
- missing `EventType::WorkflowResultAvailable`
- missing `EventFactory::workflow_started`
- missing `FileConfig.workflows`

This confirmed the contract test was exercising the intended missing surface.

### GREEN

Re-ran:

```bash
cargo test --test workflow_events_contract
```

Result:

- 5 passed
- 0 failed

## Tests run

```bash
cargo test --test workflow_events_contract
cargo test -p orca-core config:: -- --nocapture
cargo test -p orca-core event_schema:: -- --nocapture
cargo test -p orca-core event_sink:: -- --nocapture
```

Observed results:

- `workflow_events_contract`: 5 passed
- `orca-core` config-focused tests: 21 passed
- `orca-core` event_schema tests: 7 passed
- `orca-core` event_sink tests: 2 passed

## Files changed

Task-scoped files:

- `crates/orca-core/src/event_schema.rs`
- `crates/orca-core/src/config/mod.rs`
- `crates/orca-core/src/config/file.rs`
- `src/cli.rs`
- `crates/orca-tui/src/bridge.rs`
- `crates/orca-runtime/src/controller.rs`
- `tests/workflow_events_contract.rs`

Additional compile-through file:

- `crates/orca-core/src/event_sink.rs`

## Self-review

- Kept changes limited to event/config contracts and config wiring.
- Did not add workflow runtime execution, task registry changes, JS host work, CLI workflow commands, or workflow tool execution.
- Preserved camelCase payload keys in workflow event payloads.
- Added the required workflow defaults:
  - `enabled = true`
  - `max_concurrent_agents = 16`
  - `max_agents_per_run = 1000`
  - `keyword_trigger_enabled = true`
- Alias handling matches the brief and is covered by tests.
- `RunConfig` constructor updates were applied at all known compile sites called out in the task context.

## Concerns

- `crates/orca-core/src/event_sink.rs` needed a minimal update even though it was not listed in the original ownership set, because adding `EventType` variants broke its exhaustive match. No workflow runtime behavior was added there; only text rendering arms for the new variants.

---

## Review follow-up

Adjusted the workflow config path to remove the out-of-scope clamping behavior and keep only alias resolution plus defaulting:

- Removed `WorkflowConfig::normalized()` and its 64 / 10,000 caps.
- Replaced `WorkflowFileConfig::normalized()` with `WorkflowFileConfig::resolved()`, which only applies:
  - `disableWorkflows`
  - `enableWorkflows`
  - `workflowKeywordTriggerEnabled`
- Updated constructor sites in `src/cli.rs` to use `resolved()` instead of `normalized()`.
- Updated contract and config tests to stop calling `.normalized()`.
- Added a regression test that confirms workflow numeric values are preserved above the former clamp thresholds.

## Verification

Ran:

```bash
cargo test --test workflow_events_contract
cargo test -p orca-core config::
```

Results:

- `cargo test --test workflow_events_contract`: 5 passed, 0 failed
- `cargo test -p orca-core config::`: 22 passed, 0 failed
