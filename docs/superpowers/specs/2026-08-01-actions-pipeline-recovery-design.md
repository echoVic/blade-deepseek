# GitHub Actions Pipeline Recovery Design

Date: 2026-08-01
Status: design approved; written-spec review pending
Scope: restore every currently failing pipeline on `main`, specifically the Windows and Release workflows

## Objective

Restore the repository's GitHub Actions pipelines without weakening their behavioral gates. Completion requires all of the following:

1. The Release workflow must compile and run the Linux workspace test job successfully.
2. The Windows workflow must pass its x64 and native ARM64 jobs, including the full `workflow_cli_contract` suite.
3. `orca workflow run` must return after the background worker reports a durable launch, while the workflow continues independently.
4. Stop, pause, resume, and clone commands must still operate on a live persisted workflow run.
5. The fix must not skip tests, raise timing thresholds until the failure disappears, move the existing `v0.3.0` tag, or rewrite published history.

## Observed Failures

### Release run 30621714917

The `test` job fails while compiling `orca-tools` on Ubuntu, before any workspace test executes:

```text
error[E0603]: function import `plain_bash_command` is private
  --> crates/orca-tools/src/sandbox/mod.rs:74:33
warning: function cannot return without recursing
  --> crates/orca-tools/src/sandbox/mod.rs:73:1
```

The later `verify` failure is a consequence rather than a second defect: `build`, `release`, and npm publication jobs are skipped because the test dependency failed, so `Require every publication rail` receives `skipped` for every result.

### Windows run 30621701901

Both `native-x64` and `native-arm64` compile successfully and pass the explicit platform gates. Both then fail the full suite in the same three tests:

- `workflow_run_returns_before_slow_workflow_completes`
- `workflow_stop_requests_real_background_stop`
- `workflow_pause_resume_and_clone_control_persisted_run`

Each failure observes the persisted workflow in `completed` before it can interact with an active run.

### Follow-up PR run 30649340706

PR #16 changes the test's active-state classification. The follow-up Windows run disproves that hypothesis:

- x64 reports that `workflow run` blocked for about 6.86 seconds while the mock provider delay was 6 seconds.
- x64 and ARM64 still report a first observed status sequence of only `completed`.
- Both architectures still fail the same three tests.

This evidence rules out the active-status allowlist as the root cause. The production launch command is not externally observable as complete until the long-running worker finishes.

## Root Causes

### Non-macOS sandbox name resolution

`crates/orca-tools/src/sandbox/mod.rs` defines an outer wrapper named `plain_bash_command`. Its non-macOS `platform` module imports the outer module with `use super::*`. Platform implementations of the same name are conditionally compiled for Windows and fallback targets, but there is no Linux implementation.

On Linux, the glob import therefore makes the outer wrapper visible as `platform::plain_bash_command`. The wrapper calls that imported symbol, which resolves back to itself. Rust diagnoses both the private import and unconditional recursion.

The defect is hidden on macOS because the macOS platform module has its own concrete function, and on Windows because the Windows-specific function exists.

### Windows workflow worker handle inheritance

The standalone CLI starts a long-lived workflow worker and reads one launch-response line from a dedicated worker stdout pipe. The CLI then returns without waiting for the worker, which is the intended background contract.

On Windows, `CreateProcessW` is invoked with handle inheritance enabled by the Rust process implementation. The CLI itself may have inheritable standard handles, especially when a caller uses `Command::output()` as the integration tests do. Although the worker receives an explicit dedicated stdout pipe, it can also inherit the CLI's original captured stdout handle as an unrelated extra handle.

The CLI process exits, but the caller cannot observe EOF on its capture pipe while the worker still owns the inherited duplicate. `Command::output()` therefore waits until the six-second workflow finishes. When it finally returns, the first state query correctly finds `completed`.

This explains all observed facts at once:

- the launch JSON is valid;
- elapsed time tracks the provider delay;
- the persisted status sequence begins at `completed`;
- the issue is stable on both Windows architectures; and
- the same command behaves asynchronously on Unix, where unrelated descriptors are close-on-exec.

## Considered Approaches

### 1. Production-boundary repair (selected)

Fix Linux platform dispatch explicitly and prevent workflow workers from inheriting the launching CLI's unrelated standard handles on Windows. Keep the behavioral tests strict.

Advantages:

- repairs both production defects at their source;
- preserves the asynchronous CLI contract;
- retains meaningful pause and stop coverage; and
- leaves the workflow and release gates strong.

Cost: requires a small Windows-specific process-spawn boundary and focused platform tests.

### 2. Test-only timing or polling changes

Wait longer, accept `completed`, or remove the elapsed-time assertion.

Rejected because it would make the tests pass while `workflow run` still blocks for the full background operation on Windows. Pause and stop would no longer test a live task.

### 3. CI exclusions

Skip the three workflow tests on Windows or remove them from the full-suite jobs.

Rejected because it hides a real user-visible behavior defect and weakens the platform-readiness gate.

## Design

### Linux sandbox dispatch

Add a concrete Linux `platform::plain_bash_command` implementation beside the other Linux platform functions. It will create the intended unsandboxed `sh -c` command and set the requested working directory. The outer wrapper remains responsible for applying `prepare_non_interactive_command`.

Avoid routing through an imported outer name. Tests and compile checks must prove the platform module always owns a concrete implementation for each supported target family.

### Windows workflow spawn boundary

Introduce a narrowly scoped helper for spawning the workflow worker. On Windows it will:

1. inspect the launching process's current standard handles;
2. temporarily clear `HANDLE_FLAG_INHERIT` only for valid inheritable parent standard handles;
3. spawn the worker, whose explicitly configured API-key stdin and launch-response stdout handles remain inheritable;
4. restore every modified parent handle flag before returning, including on spawn failure; and
5. serialize this temporary mutation with a process-wide lock.

On non-Windows targets the helper delegates directly to `Command::spawn`. The helper is used only by the standalone workflow-worker launch boundary. It does not detach the worker from Orca's durable task ownership or change the workflow runner's internal process-tree controls.

If implementation investigation shows that a stable Windows handle allowlist is available without recreating command-line quoting and environment construction, prefer that allowlist. The required observable contract remains the same: only handles intentionally configured for the worker may cross this spawn boundary.

### Failure handling

- Failure to inspect or change a valid parent handle is treated as a launch error rather than silently returning to unsafe inheritance.
- Parent handle flags are restored through an RAII guard on every return path.
- Existing API-key handoff failure handling continues to terminate and reap the worker.
- The launch-response reader continues to require a complete newline-terminated JSON response before returning success.

## Tests

### Sandbox regression

- Compile `orca-tools` for the native Linux target in CI.
- Exercise `plain_bash_command` through a unit or contract test that checks it invokes a real platform shell rather than recursing.
- Run `cargo check -p orca-tools --all-targets --locked` locally on macOS and rely on the Release Ubuntu job for the native Linux cfg boundary.

### Windows handle regression

Add a Windows-only process contract that launches a short-lived parent under captured output, has it spawn a longer-lived child through the new boundary, and asserts the parent capture reaches EOF before the child exits. This directly covers the failure mechanism rather than relying only on workflow timing.

Retain the workflow integration contract with a deterministic six-second mock-provider delay:

- assert `workflow run` returns before the delayed model call completes;
- observe a non-terminal persisted state;
- issue stop and pause while the run is active; and
- verify stop, paused/resumed completion, and clone behavior.

Diagnostics should include elapsed launch time, last/observed statuses, stderr, and persisted state when an assertion fails.

## Verification and Delivery

Local verification will include focused tests, formatting, diff checks, and the broadest practical workspace gate:

```text
cargo fmt --all -- --check
cargo check -p orca-tools --all-targets --locked
cargo test --test workflow_cli_contract --locked -- --test-threads=1 --nocapture
cargo test --workspace --all-targets --locked -- --test-threads=1
git diff --check
```

The change will be committed on the current `main` branch and pushed only after local verification. Remote completion requires:

1. the push-triggered Windows workflow is green for `native-x64` and `native-arm64`;
2. a manual Release run for the new commit passes the Linux test job and every build target; and
3. the Release manual run does not publish or move `v0.3.0`, because it is a `workflow_dispatch` validation rather than a tag push.

If a remote-only failure remains, use its exact job log to revise the hypothesis. Do not stack unrelated timing adjustments.
