# Workflow Host Guardrails Implementation Plan

**Goal:** Make the workflow Node host stoppable and bounded at every JSONL,
thread-admission, diagnostic-output, and child-process boundary.

**Architecture:** One lexical host run scope uses RAII child/file owners for the
process group and generated module, plus fixed readers, a bounded
multi-consumer worker queue, shared run cancellation, terminal status, and
cleanup. The production runner consumes callbacks without retaining a duplicate
event history.

## Constraints

- Preserve workflow DSL, CLI, TUI, server, persisted-state, and event shapes.
- Preserve configured parallel workflow behavior through bounded worker
  admission and backpressure.
- Do not add an arbitrary total workflow duration limit.
- Do not change release metadata or publish from this branch.
- Do not treat pause as process-wide suspension in this slice.
- Delete the unbounded/unguarded production paths in the same slice.

## Tasks

- [x] Audit WorkflowHost process, frame, event, thread, stop, and stderr owners.
- [x] Compare Codex child ownership and Package 3 task-stop/output policies.
- [x] Define resource limits, compatibility, failure semantics, and deletion
  gates.
- [x] Add RED bounded-frame and aggregate event admission tests.
- [x] Add RED fixed agent-worker admission tests.
- [x] Add RED bounded-stderr and post-terminal child cleanup tests.
- [x] Add RED silent-workflow stop and child-agent cancellation tests.
- [x] Implement child/file guards, process-group cleanup, readers, and channels.
- [x] Replace per-call thread spawning with a fixed worker pool.
- [x] Add callback-only production event handling.
- [x] Thread one run cancellation token through WorkflowRunner child agents.
- [x] Remove `BufRead::lines`, `wait_with_output`, and unguarded child returns.
- [x] Run focused host/runtime/CLI tests and inspect process cleanup.
- [x] Run the complete workspace gate, Clippy, formatting, diff check, and
  static deletion scans.
- [x] Commit implementation and final verification separately.

## Implemented Outcome

- Host stdout and Rust-to-Node command frames are capped at 1 MiB.
- Event count/bytes, stderr retention, frame admission, and worker admission are
  bounded.
- Node emits synchronously, and agent calls use a bounded multi-consumer queue
  instead of one thread per call.
- Production runner callbacks no longer retain a duplicate event vector.
- Durable stop, task stop, pause waits, synthetic hold waits, active child
  agents, and unawaited terminal agents share one run cancellation token.
- `WorkflowHostChild` owns terminate/wait on every return; clean parent exit also
  clears process-group descendants that hold inherited pipes.
- Generated `orca-workflow-host-*.mjs` files are removed by an RAII guard.
- Cancelled admitted child agents persist as `cancelled` rather than stale
  `running` records; never-started queue entries are discarded before a running
  record exists.

## RED Verification

```bash
cargo test -p orca-runtime workflow::host::tests -- --nocapture
cargo test --test workflow_host_contract -- --nocapture
cargo test --test workflow_runtime_contract workflow_runner_stops -- --nocapture
```

## Final Verification

```bash
cargo test -p orca-runtime workflow::host::tests -- --nocapture
cargo test --test workflow_host_contract -- --test-threads=1
cargo test --test workflow_runtime_contract -- --test-threads=1
cargo test --test workflow_cli_contract -- --test-threads=1
cargo test --workspace --all-targets --locked --offline -- --test-threads=1
cargo clippy --workspace --all-targets --locked --offline
cargo fmt --all -- --check
git diff --check
```

After each cancellation/failure test, verify no process whose command contains
the test worktree or generated `orca-workflow-host-*` path remains.

## Verification Evidence

- Workflow unit tests: 27/27; host subset: 9/9.
- `workflow_host_contract`: 26/26.
- `workflow_runtime_contract`: 45/45.
- `workflow_cli_contract`: 10/10.
- `orca-runtime --lib`: 666/666.
- `cargo test --workspace --all-targets --locked --offline -- --test-threads=1`:
  exit 0, including the previously hanging permission-profile network-policy
  contract.
- Workspace Clippy: exit 0 with existing warnings.
- Formatting, diff check, static deletion scan, and residual-process scan: pass.
