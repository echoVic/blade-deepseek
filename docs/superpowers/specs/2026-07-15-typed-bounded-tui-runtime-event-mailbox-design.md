# Typed Bounded TUI Runtime Event Mailbox Design

Date: 2026-07-15

## User Value

Long DeepSeek responses, tool streams, workflow progress, and background task
updates must not let a stalled or slow TUI renderer grow memory without a
limit. Assistant output, approval prompts, errors, and terminal status must
remain complete and ordered when the renderer catches up. Cancelling or
finishing a turn must not depend on parsing an internal JSON copy of an event
that already existed as a typed runtime value.

## Current Structural Problems And Evidence

The production TUI creates its `TuiEvent` and `UserAction` lanes with
unbounded `std::sync::mpsc::channel`. Provider streaming, tool output,
subagents, workflows, mention search, background completion, and runtime
projection all clone the event sender. The renderer drains only a bounded
number of events per scheduler iteration, so producer traffic can accumulate
without a queue ceiling while the terminal is slow or paused.

`crates/orca-tui/src/agent_runner.rs::TuiRuntimeEventWriter` is a second
boundary defect. Runtime compaction and runtime-owned background continuation
serialize an `EventEnvelope` as JSONL into this writer. The writer retains an
unbounded partial frame, reparses the JSON into a TUI-local envelope type, then
maps it back into `TuiEvent`. This is a weak internal protocol and makes a
serialization buffer, rather than the runtime event type, the source of truth.

These are event-admission and ownership defects. Adding a final transcript
limit does not bound the queue that precedes it, and adding a byte check to the
JSON buffer would preserve the unnecessary encode/decode boundary.

## Target Ownership And Module Boundaries

`orca_core::event_sink::EventSink` remains the wire-output owner. It gains one
optional owned `EventObserver` callback. `emit` gives the observer the same
typed `EventEnvelope` that is written as JSONL or text; CLI and server callers
without an observer keep their current output byte-for-byte.

The TUI owns a `TuiRuntimeEventObserver`. It maps the typed envelope through
`tui_event_from_runtime_event` and sends the resulting `TuiEvent`. It does not
serialize, buffer, deserialize, or recreate runtime event metadata.

`orca-tui::channels` owns the production mailbox constructors and channel
types:

- TUI event capacity: 256 events.
- User action capacity: 64 actions.
- Both lanes use blocking bounded admission.
- No admitted event or action is silently dropped.
- Sender clones do not own receiver lifetime; receiver drop wakes blocked
  producers with a disconnected result.

The renderer remains the only event receiver. The agent-loop thread remains
the only user-action receiver. Backpressure can pause a producer, but it cannot
freeze the renderer because producers run outside the render loop. A full
action queue requires 64 unconsumed commands; blocking the UI at that boundary
is preferable to accepting an unbounded control backlog with stale semantics.

This slice does not claim that TUI turn orchestration is fully runtime-owned.
It removes the weak event transport and unbounded lanes that currently block a
clean runtime-owned turn host.

## TUI And Architecture Benefits

- A paused terminal has a fixed event backlog instead of proportional memory
  growth.
- Approval, error, completion, and assistant events retain FIFO order and are
  never rejected merely because deltas arrived first.
- Runtime compaction and background continuation deliver the exact typed event
  object to the TUI.
- Queue ownership, capacity, sender failure, and receiver lifetime become
  explicit and testable.
- The later TUI/runtime turn-loop convergence can use the same typed observer
  instead of inheriting a JSON compatibility bridge.

## External Compatibility

CLI arguments, TUI keys and flows, server/JSONL events, persisted JSONL,
permission interactions, tool output, workflow notifications, DeepSeek retry
and compaction behavior, and public Rust event serialization remain unchanged.
The deliberate behavior change is producer backpressure after a fixed number
of unconsumed TUI events or actions.

## Migration Order And Temporary State

1. Add RED behavior tests for typed event observation and bounded event/action
   admission.
2. Add the observer hook to `EventSink` without changing existing wire output.
3. Replace `TuiRuntimeEventWriter` with `TuiRuntimeEventObserver` in runtime
   compaction and background-continuation paths.
4. Introduce the typed bounded TUI channel module and migrate every production
   event/action endpoint to it.
5. Delete the JSON envelope parser, partial-frame buffer, and its source-shape
   or partial-JSON tests.
6. Run focused event, interaction, agent-runner, scheduler, and TUI tests, then
   the complete workspace and real DeepSeek gates.

There is no release state where both the old JSON TUI bridge and the typed
observer remain as independent production paths.

## Acceptance Criteria

1. An `EventSink` observer receives the exact run id, sequence, timestamp,
   event type, and payload that the writer serializes.
2. Observer failure is returned to the operation; writer-only callers retain
   current JSONL and text behavior.
3. Runtime compaction and background continuation reach TUI projection without
   JSON serialization or deserialization.
4. Event admission never exceeds 256 queued values and user-action admission
   never exceeds 64 queued values.
5. A producer blocked at capacity resumes after one receive, and a terminal or
   control value is delivered rather than dropped.
6. Dropping a receiver wakes blocked producers and allows their owner threads
   to terminate.
7. Slow-consumer stress tests preserve complete assistant text and terminal
   status with bounded queue length.
8. Existing TUI approval, user-input, background-turn, compaction, tool,
   workflow, and rendering behavior tests remain green.
9. CLI/server JSONL contract tests, full workspace tests, Clippy, formatting,
   site/release checks, and the real DeepSeek gate pass before release.

## Deletion Gate

The slice is incomplete while production TUI code contains
`TuiRuntimeEventWriter`, `TuiRuntimeEventEnvelope`, an unbounded event/action
constructor, or a JSON deserialize step between runtime `EventEnvelope` and
`TuiEvent`. Tests must prove queue and observer behavior rather than asserting
that source files contain ownership marker strings.

## Candidate Verification

The implementation deleted every item in the deletion gate. After rebasing
onto the latest published `main`, focused `EventSink`, runtime compaction, TUI
mailbox, observer, background-continuation, and silent-child tests passed. The
complete serial workspace suite, all-target Clippy with the repository's
existing warnings, site build and SEO checks, all release-script tests, and the
real DeepSeek provider/CLI/history/server/thread gate also passed. Remote
Actions and published-artifact verification remain release gates rather than
implementation gates.
