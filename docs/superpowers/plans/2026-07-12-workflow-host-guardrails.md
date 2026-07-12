# Workflow Host Guardrails Implementation Plan

**Goal:** Make the workflow Node host stoppable and bounded at every JSONL,
thread-admission, diagnostic-output, and child-process boundary.

**Architecture:** A `WorkflowHostSession` owns the process group, protocol
pipes, fixed worker pool, bounded channels, run cancellation, terminal status,
and cleanup. The production runner consumes callbacks without retaining a
duplicate event history.

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
- [ ] Add RED bounded-frame and aggregate event admission tests.
- [ ] Add RED fixed agent-worker admission tests.
- [ ] Add RED bounded-stderr and post-terminal child cleanup tests.
- [ ] Add RED silent-workflow stop and child-agent cancellation tests.
- [ ] Implement the host session guard, process group, readers, and channels.
- [ ] Replace per-call thread spawning with a fixed worker pool.
- [ ] Add callback-only production event handling.
- [ ] Thread one run cancellation token through WorkflowRunner child agents.
- [ ] Remove `BufRead::lines`, `wait_with_output`, and unguarded child returns.
- [ ] Run focused host/runtime/CLI tests and inspect process cleanup.
- [ ] Run the complete workspace gate, Clippy, formatting, diff check, and
  static deletion scans.
- [ ] Commit implementation and final verification separately.

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
