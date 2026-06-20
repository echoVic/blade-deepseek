# Task 5 Report: Script Resolution And Run State Store

## Implementation summary

Implemented workflow scaffolding in `orca-runtime` for:

- Workflow script resolution with precedence `scriptPath` -> `script` -> `name`
- Upward project workflow lookup via `.claude/workflows/<name>.js`
- Optional user workflow lookup via `resolve_workflow_script_with_user_dir`
- Inline/path/named script persistence under `session_dir/workflows/scripts/<meta.name>.js`
- Narrow `export const meta = { ... }` parser supporting:
  - single-quoted strings
  - double-quoted strings
  - empty `phases: []`
  - `InvalidData` errors when `name`, `description`, or `phases` is missing
- SHA-256 script digest generation
- Minimal `WorkflowStateStore` scaffolding with:
  - `new`
  - `run_dir`
  - `transcript_dir`
  - `create_run`
  - `load_run`
  - `write_state`
  - `load_state`
  - `record_agent_completed`
  - `cached_agent_result`
- Minimal serializable `WorkflowAgentCacheRecord` sidecar cache keyed by `call_path + input_hash`

## TDD evidence

### RED

Added `tests/workflow_script_contract.rs` first, covering:

- inline script persistence and meta extraction
- `scriptPath` precedence over inline `script`
- nearest project workflow lookup beating user workflow lookup
- missing meta field error shape
- state store run-state and cache roundtrip

Ran:

```bash
cargo test --test workflow_script_contract
```

Observed failure:

- `could not find workflow in orca_runtime`

This confirmed the new contract was failing for the expected missing implementation reason.

### GREEN

Implemented the workflow module, resolver, state store, dependency wiring, and exports.

Re-ran:

```bash
cargo test --test workflow_script_contract
```

Observed result:

- 5 tests passed, 0 failed

## Verification

After formatting, ran fresh verification commands:

```bash
cargo test --test workflow_script_contract --test workflow_types_contract
cargo test -p orca-runtime workflow
```

Observed results:

- `workflow_script_contract`: 5 passed
- `workflow_types_contract`: 5 passed
- `orca-runtime` filtered workflow/unit coverage: 2 passed

## Files changed

- `Cargo.toml`
- `Cargo.lock`
- `crates/orca-runtime/Cargo.toml`
- `crates/orca-runtime/src/lib.rs`
- `crates/orca-runtime/src/workflow/mod.rs`
- `crates/orca-runtime/src/workflow/script.rs`
- `crates/orca-runtime/src/workflow/state.rs`
- `tests/workflow_script_contract.rs`

## Self-review

- Kept scope limited to script resolution and state-store scaffolding only.
- Did not touch controller, tools, provider, CLI, TUI, or event integration.
- Used a deliberately narrow parser rather than inventing a partial JS evaluator.
- Added the extra interface-listed state-store methods even though the later skeleton omitted them.
- Stored agent cache separately from `WorkflowRunState` to avoid forcing premature schema decisions into the core run-state type.

## Concerns

- The meta parser is intentionally narrow and only supports the static literal shape described in the task brief. More dynamic JavaScript meta declarations will need a real parser or JS-host-backed extraction in later tasks.
- The cache key currently concatenates `call_path` and `input_hash` with `:`. That is compile-safe and fine for now, but later tasks may want a more explicit structured key format if agent call paths become more expressive.
