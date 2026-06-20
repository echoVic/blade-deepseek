## Task 6 Report: Node Workflow Host Protocol

### Implementation summary

Implemented a new `orca_runtime::workflow::host` module that provides:

- `HostEvent` with `#[serde(tag = "type", rename_all = "snake_case")]` and the required protocol variants.
- `HostCommand` as a forward-looking protocol enum for future stdin-fed host interaction.
- `WorkflowHost::node_available()` using `node --version`, returning `false` if the command cannot execute or exits unsuccessfully.
- `WorkflowHost::run_collecting_events(&Path, Value)` which:
  - writes the embedded JS host shim from `include_str!("host.mjs")` to the stable temp path `std::env::temp_dir()/orca-workflow-host.mjs`
  - spawns `node <host-file> <script-path> <args-json>`
  - parses JSONL stdout into `HostEvent` values
  - returns an `io::Error` containing the failure text when the host exits nonzero after emitting `WorkflowFailed`

Implemented the JavaScript host shim in `crates/orca-runtime/src/workflow/host.mjs` with only the requested globals:

- `args`
- `agent`
- `parallel`
- `pipeline`
- `phase`

The shim emits JSONL protocol events, returns synthetic `agent()` results for now, and reports `workflow_completed` / `workflow_failed` terminal events.

### TDD evidence

#### RED

1. Added `tests/workflow_host_contract.rs` with the required contract coverage for:
   - phase and agent-call event emission
   - `args` global exposure
2. Ran:

```bash
cargo test --test workflow_host_contract
```

Observed expected failure:

```text
error[E0432]: unresolved import `orca_runtime::workflow::host`
```

This confirmed the test was failing for the missing host surface, as intended.

#### GREEN

After implementing `workflow::host` and exporting it from `workflow/mod.rs`, ran:

```bash
cargo test --test workflow_host_contract
```

Observed passing result:

```text
running 2 tests
test host_exposes_args_global ... ok
test host_emits_phase_and_agent_call_events ... ok

test result: ok. 2 passed; 0 failed
```

Re-ran after `cargo fmt` to verify the formatted tree stayed green.

### Tests run

- `cargo test --test workflow_host_contract` (RED: unresolved import)
- `cargo test --test workflow_host_contract` (GREEN: 2 passed)
- `cargo fmt`
- `cargo test --test workflow_host_contract` (post-format verification: 2 passed)

### Files changed

- Created `crates/orca-runtime/src/workflow/host.rs`
- Created `crates/orca-runtime/src/workflow/host.mjs`
- Modified `crates/orca-runtime/src/workflow/mod.rs`
- Created `tests/workflow_host_contract.rs`

### Self-review

- Kept the implementation inside the allowed ownership boundary.
- Preserved the exact serde-tagged event shape requested by the brief.
- Ensured tests skip cleanly when Node is unavailable by returning early in each contract test.
- Made host-file materialization stable and deterministic via a fixed temp filename.
- Captured `WorkflowFailed` text and surfaced it in the returned Rust error on nonzero host exit.

### Concerns

- `run_collecting_events` currently reads stdout line-by-line and only returns the collected events on success; if a later task needs partial events on failure, the API may want a richer error type carrying both events and failure context.
- The stable temp host path is overwritten on each invocation, which matches the brief and is acceptable for current single-process contract coverage, but future concurrent workflow execution may want an atomic write or versioned strategy.
