# ADR 0005: Runtime Host Operation Control Plane

- Status: Accepted; P0.3a implemented but not released
- Date: 2026-07-15
- Roadmap: P0.3 Runtime Operation Host and canonical turn executor

## Context

Orca has improved cancellation and worker cleanup, but it still has multiple
owners for one conceptual agent operation.

- `orca-tui::agent_runner` owns a TUI-only provider, tool, compaction, hook,
  and turn loop plus an outer agent thread and provider workers.
- `RuntimeThread` exposes borrowed synchronous mutation and delegates each
  request to `ThreadTurnExecutor`.
- `run_thread_turn_inner_with_events` has separate branches for caller-owned
  and internally-created `EventFactory` values, duplicating turn assembly.
- The server moves a whole `ServerThread` into a per-turn OS thread while
  `ActiveTurnManager` separately owns generation, cancellation, resume, join,
  and reclamation state.
- `orca-runtime/src/lib.rs` contains more than one thousand
  `include_str!`/`contains` source-shape checks. These tests can preserve
  obsolete names and file layouts without proving cancellation, joining, or
  terminal delivery.

The result is an architecture defect, not a local cancellation defect. There
is no shared runtime handle that can prove all of the following for TUI,
server, and headless callers:

1. exactly one owner admits commands for a thread;
2. every operation has a fresh cancellation scope;
3. stale commands cannot affect a newer operation;
4. completion is available independently of event subscribers;
5. shutdown cancels, waits for, and reclaims active work;
6. thread state is owned either by the actor or by its one active task, never
   borrowed by an external surface during a turn.

Current Codex main reinforces this ownership model: one session submission
loop owns thread commands, a running turn owns its cancellation token and task
handle, completion has an independent notification, and `shutdown_and_wait`
waits for the session loop. Codex steering also fences input by the expected
turn id. Claude Code `2.1.210` no longer ships inspectable implementation
source in its npm package, so current internal ownership cannot be independently
confirmed there. The local restored `2.1.88` source used one conversation-owned
`QueryEngine` and explicit abort control. Orca adopts the evidence-backed
ownership properties, not either implementation.

## Decision

Introduce the P0.3 control plane in `orca-runtime`:

### RuntimeHost

`RuntimeHost` owns one process Tokio runtime and a supervisor task. Dropping or
shutting down the host sends a typed shutdown command and joins the supervisor
thread. The host never abandons actors or operation tasks.

The host command mailbox is bounded. Thread creation returns a cloneable
`RuntimeThreadHandle`; callers do not receive mutable access to the actor-owned
`RuntimeThread`.

### ThreadActor

One `ThreadActor` owns one conversation and serializes a bounded typed command
mailbox. In this slice it accepts:

- `StartTurn`;
- `InterruptOperation`;
- `ReadState`;
- `ShutdownThread`.

The actor is the sole authority for the current operation id and generation.
It rejects a second start while an operation is active. Future P0.4 input
commands will use the same operation id fence instead of creating another
mailbox owner.

`StartTurn` carries an owned `HostedTurnRequest`. It accepts only thread-safe,
owned interaction and event handlers. The legacy `ThreadTurnRequest` can still
carry borrowed TUI handlers, so it is constructed only inside the operation
task and is never falsely marked or treated as `Send`.

When idle, the actor owns `RuntimeThread`. While a turn runs, ownership moves
into one joined operation task. The actor then owns the task handle, fresh
cancel scope, operation id, and terminal completion. When the task finishes,
the thread returns to the actor before another turn can start.

### OperationHandle And Terminal Completion

Starting a turn returns an `OperationHandle` containing the thread id,
operation id, command authority, and a shared one-shot completion cell. The
completion cell records one typed terminal outcome and supports waiting without
reading runtime events.

Dropping an `OperationHandle` or an event receiver does not cancel the
operation. Cancellation requires an explicit typed interrupt command.

Terminal outcomes distinguish at least:

- completed with `RunStatus`;
- execution failure;
- operation task panic or join failure.

An interrupt request is only an acknowledgement that the actor cancelled the
matching scope. The terminal outcome is published after the operation task has
actually stopped and returned its owned thread state.

### Initial Turn Kernel

P0.3a delegates turn execution to the existing
`RuntimeThread -> ThreadTurnExecutor` path through one narrow runtime-owned
kernel interface. This is a migration boundary, not a second agent loop. It
must not assemble provider, tool, compaction, hook, or event behavior itself.

P0.5 will replace the legacy borrowed executor internals with the canonical
actor-owned turn executor. The kernel boundary remains only if it continues to
represent a useful testable execution contract after that replacement.

## User Value

This slice establishes the behavior needed for reliable TUI task control:

- an interrupt can target exactly the operation shown as running;
- returning control to the UI cannot precede task cleanup and join;
- renderer or event-subscriber loss cannot silently cancel work or lose the
  authoritative terminal state;
- thread shutdown has a joined, testable path instead of relying on detached
  worker lifetime;
- the next TUI migration can use a runtime handle without preserving the TUI's
  current outer cancellation owner.

P0.3a is a foundation slice and is not a release point by itself. A release
requires a migrated surface with a user-visible reliability improvement.

## Ownership Model

At every instant, ownership is one of these states:

```text
RuntimeHost supervisor
  -> ThreadActor
       -> idle: RuntimeThread
       -> running: ActiveOperation
            -> OperationScope
            -> joined task handle
            -> RuntimeThread (inside task)
            -> terminal completion cell
```

No external caller owns `RuntimeThread`, the operation join handle, or the
cancel token. Handles carry command authority only.

## External Compatibility

P0.3a does not change:

- CLI arguments or exit codes;
- TUI key bindings, transcript behavior, or permission flow;
- server JSONL request or event shapes;
- persisted thread/session formats;
- provider retry, streaming, compaction, or tool semantics.

Existing surface execution paths remain temporarily available until their
individual migrations pass behavior parity tests. They must not be extended
with new lifecycle state that belongs in the host.

## Migration Sequence

1. Add the host, actor, typed commands, operation handle, and terminal cell
   behind behavior tests.
2. Run the existing legacy `ThreadTurnExecutor` through the actor-owned task.
3. Migrate server active turns to `RuntimeThreadHandle`; delete server-owned
   generation, cancellation, and reaper state.
4. Add generation-fenced same-turn and next-turn input admission in P0.4.
5. Move the canonical turn executor under the actor in P0.5.
6. Move provider awaiting into the runtime and delete the synchronous provider
   compatibility worker in P0.6.
7. Migrate headless and TUI execution; delete the TUI provider/tool loop and
   outer operation cancellation owner in P0.7.

The temporary state is explicit: P0.3a may coexist with old surface entry
points, but there must be only one owner inside each path. No surface may wrap
the new host with another generation or join state and call that final.

## P0.3a Acceptance Criteria

1. Host and actor command channels have explicit finite capacities.
2. Starting a turn returns a stable operation id and an independent completion
   handle.
3. A concurrent `StartTurn` is rejected without replacing or cancelling the
   active operation.
4. Interrupting operation A cannot cancel operation B after B starts.
5. Dropping operation/thread handles does not cancel work; event-consumer loss
   is a typed execution failure, not cancellation.
6. An interrupt terminal is published only after the operation task exits and
   its thread state is reclaimed.
7. `ShutdownThread` cancels and joins active work before actor termination.
8. `RuntimeHost::shutdown` waits for every thread actor; `Drop` cannot detach
   the supervisor.
9. Operation completion is written exactly once, including execution error and
   task panic paths.
10. Tests inspect behavior and public state, not source text or symbol names.
11. Existing `RuntimeThread`, controller, server, and TUI focused tests pass.
12. The shared-runtime full gate passes before the slice is committed.

## Final Deletion Gates

The P0 architecture stage is not complete until these old owners are removed:

- server `ActiveTurnManager` generation/cancel/reaper ownership after server
  migration;
- TUI `OperationCancellation` and the outer agent/provider worker ownership
  after TUI migration;
- public borrowed `RuntimeThread` mutation paths once all surfaces use actor
  commands;
- duplicated event-factory branches and turn-loop assembly after the canonical
  executor lands;
- obsolete source-shape assertions replaced by ownership and lifecycle tests;
- the synchronous provider compatibility worker after async provider execution
  moves under the runtime.

## Rejected Alternatives

### Add Another Surface Wrapper

Wrapping the current TUI and server workers in a host while leaving generation,
cancel, and join ownership in each surface would create another compatibility
layer and two facts for terminal state. Rejected.

### Use Unbounded Channels

Operation control traffic is small and must remain bounded under stalled or
misbehaving clients. Unbounded command or event queues would move lifecycle
risk into memory growth. Rejected.

### Cancel On Subscriber Disconnect

Event delivery is observation, not operation ownership. A renderer restart or
client detach must not be interpreted as an explicit user interrupt. Rejected.

### Keep Actor State Borrowed By The Operation

Borrowing `&mut RuntimeThread` across an operation prevents the actor from
owning a joinable `'static` task and obscures shutdown ownership. The thread is
moved into the task and returned on completion instead. Rejected.

## Verification

- Nine focused behavior tests cover concurrent start rejection, stale
  interrupts, independent completion, typed event-subscriber failure, executor
  panic recovery, explicit thread shutdown, host shutdown across actors, host
  `Drop`, and delegation to the existing `ThreadTurnExecutor`.
- `cargo test -p orca-runtime --all-targets -- --test-threads=1` passed with
  778 runtime unit tests, nine host tests, and 12 task-output integration tests.
- `cargo test --workspace --all-targets -- --test-threads=1` passed, including
  130 server contracts and 467 TUI tests.
- `cargo clippy --workspace --all-targets` passed with the repository's existing
  warnings. The new runtime-host implementation and tests introduce no Clippy
  warning.
- A real DeepSeek smoke was intentionally not run for P0.3a because no
  production CLI, server, or TUI path executes through the host yet. It becomes
  mandatory when the first production surface migrates.
