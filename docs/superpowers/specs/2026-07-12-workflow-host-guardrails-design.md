# Workflow Host Guardrails Design

Date: 2026-07-12

## Problem And Evidence

`WorkflowHost` executes user-authored workflow JavaScript in a child Node
process, but the Rust side does not yet own that process as a bounded runtime
operation:

- stdout is read through `BufRead::lines`, so one JSONL event can allocate
  without a frame limit;
- every event is retained in a `Vec<HostEvent>` even though the production
  runner consumes events through a callback and discards the returned vector;
- each `agent_call` spawns another scoped thread, allowing host input to create
  threads faster than the workflow concurrency gate can admit work;
- stderr is collected with `wait_with_output`, so a failing host can allocate
  its complete diagnostic stream;
- a stop request is observed only before or between agent calls; a silent
  promise, pure JavaScript loop, or event-only script cannot be stopped;
- parse, callback, reader, or early-return failures drop a live `Child` without
  an explicit process-group terminate-and-wait obligation.

These are boundary and ownership defects, not presentation truncation defects.
The P0.1 bounded process-output collector does not cover this bidirectional
JSONL protocol.

## Reference Principles

Codex uses `kill_on_drop`, process groups, one-shot cancellation tokens,
bounded head/tail output, and bounded fan-out channels for child execution. The
important property is that one runtime object owns cancellation, output, wait,
and cleanup.

Package 3 separates task stop from UI foregrounding, kills process trees,
reacts to the parent process `exit` rather than waiting for descendant-held
stdio `close`, and places explicit caps on task output. The useful property is
that a task remains controllable even when descendants or output pipes outlive
the immediate shell.

Orca should adopt those ownership properties without copying either runtime or
changing the workflow DSL.

## User Value

Stopping a workflow must stop its Node host even when no agent call is active.
A malformed or noisy workflow must fail with a bounded diagnostic instead of
growing Orca memory or leaving a Node process behind. Valid parallel workflows
continue to run up to their configured concurrency, and CLI, TUI, server,
persisted workflow state, and DSL shapes remain unchanged.

## Target Ownership

One `WorkflowHostSession` owns:

- the child Node process and Unix process group;
- the writable protocol stdin;
- the stdout frame-reader thread and its bounded channel;
- the stderr retained-output reader;
- the fixed agent-call worker pool and bounded admission queue;
- the operation cancellation predicate;
- the terminal event, exit status, and cleanup obligation.

Every return path must either observe a clean child exit or terminate the
process group and wait. Cleanup must happen before reader and worker joins can
block on a live child.

The production `WorkflowRunner` creates one fresh `CancelToken` for the run.
The host cancellation predicate checks both the durable stop request and the
process-local task token. When cancellation is observed, it first cancels the
shared run token, then terminates the host. Concurrent child agents receive the
same one-shot run token instead of unrelated tokens created per call.

## Resource Policy

- Maximum stdout JSONL frame: 1 MiB, including the terminating newline.
- Stdout frame channel: synchronous capacity 8.
- Maximum accepted host events: 8,192 per run.
- Maximum aggregate accepted event bytes: 64 MiB per run.
- Retained stderr: 64 KiB head/tail with an exact omission marker.
- Agent-call workers: the normalized workflow `max_concurrent_agents` value.
- Agent-call queue: twice the worker count, with a minimum capacity of 1.
- Cancellation/control polling: at most 50 ms while waiting for host output.
- Post-terminal child exit grace: 2 seconds, then terminate and wait.

Backpressure is intentional. When the frame or call queue is full, the Node
host blocks on its pipe instead of allocating an unbounded Rust queue or an
unbounded number of threads.

There is no total workflow-duration limit in this slice. Long workflows remain
valid, but they must remain stoppable and resource-bounded.

## Protocol And Compatibility

Existing public `WorkflowHost::run_collecting_events*` methods keep returning
the same event vector for tests and direct callers, subject to the new safety
limits. The production runner uses an internal callback-only mode and no longer
retains duplicate events.

The workflow DSL, host event JSON, host command JSON, CLI commands, TUI rows,
server payloads, persisted state, and final status strings do not change.

Oversized frames, event floods, aggregate-byte overflow, and malformed JSON
fail closed with an `InvalidData` error. Cancellation returns an interrupted
host result; `WorkflowRunner` maps a confirmed stop request to the existing
`stopped` terminal state rather than `failed`.

## Failure Semantics

- stdout EOF before a terminal event is a protocol failure;
- reader I/O errors and worker panics are explicit host failures;
- stderr omission is diagnostic truncation, not a second failure;
- callback failure cancels the run token, terminates the host, and returns the
  original callback error;
- cancellation skips queued agent calls, closes protocol input, terminates the
  host process group, waits, and returns promptly;
- terminal host output is authoritative only after the child exits within the
  grace period; a child that emits terminal output and stays alive is killed;
- pause behavior remains agent-boundary cooperative in this slice; only stop is
  made host-wide.

## Migration

1. Add RED tests for bounded lines, frame/event overflow, stderr retention,
   fixed worker admission, silent-host cancellation, and post-terminal cleanup.
2. Introduce the host session guard and fixed reader/worker channels.
3. Replace `BufRead::lines`, per-call thread spawning, and `wait_with_output`.
4. Add callback-only production event handling.
5. Create one run cancellation token in `WorkflowRunner` and pass it through
   every workflow child-agent runtime.
6. Make durable/process-local stop requests cancel that token and terminate the
   Node host.

## Acceptance Criteria

1. A frame larger than 1 MiB fails without retaining the complete frame.
2. More than 8,192 events or 64 MiB of accepted event data fails explicitly.
3. Agent-call execution never creates more worker threads than configured.
4. A silent workflow stops within a bounded interval after `request_stop`.
5. Active child-agent provider/tool work observes the workflow run token.
6. Host stderr retains at most 64 KiB plus the omission marker.
7. Parse, callback, cancellation, post-terminal hang, and nonzero-exit paths
   leave no workflow Node process or descendant behind.
8. Existing workflow host/runtime/CLI behavior tests remain green.

## Final Deletion Gate

This slice is incomplete while production `WorkflowHost` contains
`BufRead::lines`, `wait_with_output`, one thread spawn per `agent_call`, an
unbounded event vector in the production runner, or any early return that can
skip terminate-and-wait for a live child.

