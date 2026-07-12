# Server Contract Harness Lifecycle Design

Date: 2026-07-12

## Problem And Evidence

`tests/session_server_contract.rs` starts real `orca --mode server` processes,
then reads JSONL events directly from a blocking `BufReader<ChildStdout>`. The
shared helpers loop until one expected event appears, with no deadline and only
partial recognition of terminal errors.

The full release gate exposed the concrete failure:

- `server_mode_bash_inherits_thread_active_permission_profile_network_policy`
  waits only for `permission_request`;
- in a restricted host, `RuntimeNetworkProxy::start_with_block_reporter` can
  fail before the proxy-backed shell starts;
- the server emits `tool_completed(status=failed)` and then
  `turn_completed(status=success)`;
- the helper ignores both terminal events and blocks forever waiting for more
  stdout;
- when the test process is interrupted, the spawned Orca server can remain
  alive because `OrcaChild` has no `Drop` cleanup.

This is a validation-boundary defect, not evidence that the server turn actor
deadlocked. It also explains why the release gate previously remained stuck for
more than 20 minutes while the server had no command child process.

## User Value

This slice prevents Orca release verification from hanging indefinitely or
leaving server processes behind. That directly protects TUI reliability work:
runtime, permission, cancellation, and process-lifecycle changes can only ship
when their real server contracts terminate deterministically and report the
event that invalidated an expectation.

## Target Ownership

Introduce one test-only `ServerTestClient` boundary that owns:

- the Orca server child process and its isolated `ORCA_HOME`;
- server stdin;
- a stdout reader worker;
- a bounded event channel;
- the recent protocol/noise transcript used for diagnostics;
- the default event deadline;
- graceful shutdown and forced process-group cleanup.

The stdout worker is the only blocking reader. Test threads receive parsed
events with `recv_timeout`, so every protocol wait has a deadline independent
of whether the child remains alive with stdout open.

`ServerTestClient::wait_for` accepts a typed expectation containing request id,
event name, and optional event predicate. It returns the matching event or a
structured test error. While waiting, it must fail immediately when the same
request reaches a terminal state that makes the expectation impossible.

At minimum, these are impossible terminals:

- matching `error` before any different expected event;
- matching `turn_completed` before an expected turn-scoped interaction or tool
  event;
- matching failed/cancelled/indeterminate `tool_completed` before a permission
  request that execution would have produced;
- stdout EOF or reader failure.

The error includes the expectation, terminal reason, child status when known,
and bounded recent protocol/noise lines.

## Cleanup Semantics

The client starts Orca in its own process group. Explicit shutdown and `Drop`
use one idempotent path:

1. close server stdin;
2. wait for a short graceful-exit deadline;
3. if still alive, signal the owned process group;
4. wait for the child so it cannot become a zombie;
5. join the stdout worker after the child closes stdout.

No test failure or panic may leave the direct Orca server child alive. This
test boundary does not claim to replace the production `ProcessSupervisor`
required to own independently grouped command, MCP, workflow, and tool
descendants. That remains a separate P0 runtime slice.

## External Compatibility

There is no production behavior, CLI, TUI, server JSONL, or persisted-history
change. The harness consumes the existing protocol exactly as a real client
does. Production tests continue to launch the built `orca` binary rather than
calling private server functions.

## Migration

1. Add RED unit coverage for deadline diagnostics, impossible-terminal
   detection, and idempotent child cleanup.
2. Introduce `tests/support/server_test_client.rs` without changing production
   code.
3. Migrate the two network-permission turn tests that exposed the defect.
4. Migrate the shared event helper paths used by the rest of
   `session_server_contract` so no JSONL expectation can block without a
   deadline.
5. Delete `OrcaChild`, direct `BufReader<ChildStdout>` event loops, and the old
   unbounded `read_until_*` helpers once their final caller moves.

The final state must not retain two long-term event-reading implementations.
If a raw byte-oriented test genuinely needs direct stdout access, it must use a
separate explicitly bounded helper and document why parsed protocol events are
insufficient.

## Acceptance Criteria

1. Waiting for a missing event fails within the configured deadline and prints
   bounded observed events.
2. A matching `error`, failed tool terminal, or completed turn fails an
   impossible expectation immediately rather than waiting for timeout.
3. Dropping a client with an open server process closes, kills if needed, waits,
   and joins without leaving the direct child alive.
4. The previously hanging network-profile test either observes the expected
   permission request or fails promptly with the actual proxy-start terminal;
   it never hangs.
5. Existing server JSONL behavior remains unchanged.
6. Focused harness and `session_server_contract` tests pass using an isolated
   Cargo target directory.
7. The workspace full gate terminates without leftover test-owned Orca server
   processes.

## Final Deletion Gate

This slice is complete only when `session_server_contract.rs` has no direct
blocking event read whose lifetime is controlled solely by child stdout. Any
remaining raw read must have an explicit deadline and cleanup owner. Source
shape assertions are not acceptance evidence; tests must exercise real timeout,
terminal, and process-reaping behavior.
