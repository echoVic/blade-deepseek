# ADR 0005: Runtime Host Operation Control Plane

- Status: Accepted; P0.3a through P0.3c implemented but unreleased
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
mailbox. P0.3a accepts:

- `StartTurn`;
- `InterruptOperation`;
- `ReadState`;
- `ShutdownThread`.

The actor is the sole authority for the current logical operation id. It
rejects a second start while an operation is active. P0.3c extends the same
mailbox with actor-owned generation, resume, steer, and generation-admission
commands instead of creating another surface control plane.

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
            -> terminal completion cell
            -> actor-owned request and steer queue
            -> ActiveGeneration
                 -> GenerationFence
                 -> fresh cancel token
                 -> joined task handle
                 -> RuntimeThread + EventFactory + writer (inside task)
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
3. Add actor-owned logical-turn generations and typed resume, steer, and
   generation-fenced input admission.
4. Migrate server active turns to `RuntimeThreadHandle`; delete server-owned
   generation, cancellation, resume mailbox, and reaper state.
5. Move the canonical turn executor under the actor in P0.5.
6. Move provider awaiting into the runtime and delete the synchronous provider
   compatibility worker in P0.6.
7. Migrate headless and TUI execution; delete the TUI provider/tool loop and
   outer operation cancellation owner in P0.7.

The temporary state is explicit: P0.3a may coexist with old surface entry
points, but there must be only one owner inside each path. No surface may wrap
the new host with another generation or join state and call that final.

## P0.3b: Actor-Owned Session And Event Lifecycle

### Structural Problem And Evidence

P0.3a deliberately left event and session ownership outside the host. The
legacy host executor calls `RuntimeThread::run_request_with_cancel`, which
creates a new `EventFactory` for each operation. The headless controller owns a
second lifecycle envelope around that path: it constructs `RuntimeThread` and
`EventFactory`, emits `session.started`, runs `SessionStart`, suppresses the
turn-level terminal, runs `SessionEnd`, and emits `session.completed`.

Moving headless onto P0.3a unchanged would therefore either reset event
sequence numbers inside the actor or preserve a controller-owned outer session
around a host-owned turn. Both outcomes leave two lifecycle facts and make the
first production migration untrustworthy.

### Target Ownership And Module Boundary

`ThreadActor` owns one idle state bundle containing both `RuntimeThread` and a
persistent `EventFactory`. The same bundle moves into the one joined operation
task and returns to the actor before another operation is admitted. The
operation executor receives the actor-owned factory together with the cancel
scope; it may emit turn events but may not create a replacement factory.

`HostedTurnRequest` carries a typed operation envelope:

- `Turn` preserves the existing turn-level completion option for legacy
  callers while they migrate;
- `HeadlessSession` makes the host the sole owner of `session.started`,
  `SessionStart`, turn execution, `SessionEnd`, and the final
  `session.completed` event.

The headless controller becomes a synchronous client of `RuntimeHost`. Its
public writer APIs continue to accept borrowed writers. A bounded,
acknowledged event relay bridges the host-owned operation task to that writer:
each flushed event is accepted only after the caller writer succeeds, and a
downstream write error is returned through the relay so the operation records
typed execution failure rather than reporting cancellation.

The host exposes immutable thread-start diagnostics needed by the controller,
but never mutable `RuntimeThread`, `InteractiveSession`, event-factory, cancel,
or join ownership.

### TUI User Value

Headless is the smallest production path that can prove the runtime host owns a
complete operation rather than only wrapping an internal function. This
removes the event-sequence and session-envelope ambiguity that would otherwise
be carried into the TUI migration. It directly lowers the risk that a later TUI
interrupt returns before cleanup, that renderer replacement resets event
identity, or that terminal state differs between the runtime and the surface.

### External Compatibility

P0.3b keeps CLI arguments, exit codes, text output, JSONL event names and
payloads, event ordering, persisted session format, provider behavior, and
desktop-notification behavior compatible. `run_to_writer` and
`run_to_writer_with_options` retain borrowed-writer support. Server and TUI
execution ownership do not migrate in this slice.

### Migration Order And Temporary State

1. Add behavior tests for persistent actor event sequence, host-owned session
   ordering, typed writer failure, and shutdown/join behavior.
2. Move `EventFactory` into the actor state bundle and pass it through the
   operation executor with the operation cancel token.
3. Add the typed headless session envelope and bounded acknowledged writer
   relay.
4. Replace `controller::run_inner` with host startup, one hosted headless
   operation, relay draining, terminal mapping, and host shutdown.
5. Delete direct headless `RuntimeThread`, `EventFactory`, hook, and session
   terminal ownership in the controller.

The server and TUI remain temporary legacy surface owners until their own
vertical migrations. They may use the actor-owned event sequence later, but
must not add another session envelope around it.

## P0.3c: Actor-Owned Logical Turn Generations And Input Admission

### Structural Problem And Evidence

P0.3b made one host operation joinable, but it still treats every executor run
as the complete operation. That boundary cannot replace the production server
control plane:

- `server::ActiveTurnControl` owns a resettable generation record containing
  the generation id, cancel token, and command-admission flag;
- `run_thread_submit_async` owns a second loop that waits for one generation,
  checks a separate resume mailbox, creates the next cancel token, and reruns
  the same persisted turn;
- `ActiveTurnManager` separately owns the worker join handle, reclamation,
  shutdown reaper, generation validation, steer queue, and session permission
  metadata;
- permission, user-input, and MCP replies ask that manager whether their
  captured generation is still active, while turn steer bypasses the host and
  pushes directly into a shared `ThreadSteerHandle`;
- the server writer buffers terminal-looking JSONL lines so an interrupted
  generation cannot publish the logical turn terminal before a queued resume.

Migrating the server before moving those responsibilities would leave
`ActiveTurnManager` around `RuntimeHost` as a permanent second control plane.
The actor would own an operation task while the server still decided which
generation is current, whether input is accepted, and when the turn is really
terminal. That contradicts the P0 ownership model.

Current Codex keeps expected-turn validation inside the session that owns the
active task. Its app-server maps `expectedTurnId` into
`Session::steer_input`, which atomically checks the active task and turn id
before queuing input. Orca should preserve that ownership property while also
keeping its explicit interrupted-generation resume semantics.

### Target Ownership And Module Boundary

One `OperationId` identifies the actor-owned logical turn for its complete
lifetime. Each executor attempt has an opaque monotonically increasing
`GenerationId`, starting at zero, and a typed `GenerationFence` containing both
ids.

`ThreadActor` exclusively owns:

- logical-turn admission and the one authoritative `OperationCompletion`;
- current generation id, fresh cancel token, and joined task handle;
- interrupt and resume admission, including duplicate-resume coalescing;
- the steer queue and the rule for when same-turn input is accepted;
- validation of generation-scoped permission, user-input, and MCP replies;
- the decision to start a replacement generation only after the previous task
  has joined and returned thread, event, request, and writer ownership.

The actor command mailbox gains typed operations for resume, steer, generation
validation, state reads, and shutdown. Every result identifies the logical
operation and, where relevant, the generation it observed. A stale logical
operation id or generation fence can never affect the current generation.

Interrupt, resume, and steer are logical-turn commands: an accepted command
intentionally targets whichever generation is current for the matching
`OperationId`. Permission, user-input, and MCP replies are generation-scoped
and must present the exact `GenerationFence` captured when the request was
created. This distinction lets a user keep controlling one resumed turn while
preventing an answer produced for generation N from entering generation N+1.

The executor receives its `GenerationFence` and an actor-created steer handle.
`HostedTurnRequest` no longer accepts an externally supplied steer handle that
could bypass actor admission. A resumed generation runs the same owned request
with the existing-turn marker set, so the original user prompt is not appended
again.

An interrupt cancels only the current generation. Resume is admissible only
after that interrupt and is queued on the logical turn; the replacement
generation is not spawned until the cancelled generation has joined. The
logical operation completion remains empty across that transition and is
written exactly once after the final generation joins.

### TUI User Value

This slice removes the lifecycle race that would otherwise reach the TUI when
pause/resume and same-turn input move onto the host:

- Resume cannot start a new provider/tool loop while the previous one still
  owns the conversation, writer, or resources.
- A stale turn control cannot mutate a newer logical operation, and a delayed
  permission, user-input, or MCP answer cannot mutate a newer generation of
  the same turn.
- The UI can distinguish "interrupt accepted", "resume queued", and "turn
  terminal" instead of treating cancellation acknowledgement as cleanup.
- The eventual TUI migration can delete its outer cancellation owner rather
  than wrapping the actor in another resettable scope.

P0.3c is still a foundation slice and is not a release point. Its concrete
product value is eliminating the resume/input race before the server and TUI
adopt the host.

### External Compatibility

P0.3c does not change CLI arguments, TUI interaction flow, server JSONL request
or event shapes, persisted session format, or provider/tool behavior. Headless
sessions remain single-generation and explicitly reject resume while retaining
the P0.3b session envelope.

The new host command/result types are runtime-internal migration APIs. Existing
headless callers keep `start_turn`, interrupt, wait, and shutdown behavior.

### Migration Order And Temporary State

1. Add RED runtime-host behavior tests for generation identity, join-before-
   resume, duplicate resume, stale fences, steer admission, prompt reuse, one
   logical terminal, and shutdown.
2. Add typed generation ids, fences, command results, and state snapshots.
3. Move the steer queue into `ThreadActor` and remove the external host-request
   steer-handle escape hatch.
4. Return owned thread/event/request/writer state from each generation task and
   let the actor either start the joined replacement or publish the final
   logical terminal.
5. Keep server production execution on its legacy control plane until its
   adapter can be replaced vertically in the next slice.

The temporary boundary is explicit: P0.3c makes the host the complete source of
truth only for host-run logical turns. The legacy server still owns its old
turns until migration, and its JSONL terminal buffering remains there only
until the server adapter consumes actor generation outcomes. No new server or
TUI lifecycle state may be added beside it.

### P0.3c Acceptance Criteria

1. One logical operation id remains stable while generation ids increase from
   zero and every generation receives a fresh cancellation token.
2. Resume is rejected before interrupt, duplicate accepted resumes coalesce,
   and generation N+1 cannot enter the executor until generation N has exited
   and joined.
3. A generation fence is accepted only for the current, command-accepting
   generation; cancellation, replacement, completion, and a newer operation
   make the old fence stale.
4. Steer input is accepted only for the matching active logical turn before
   cancellation, reaches the actor-owned queue once, and cannot be injected by
   retaining an external handle.
5. A resumed generation is marked as the existing turn and does not append or
   execute a duplicate initial user prompt.
6. Intermediate cancelled generations never complete the logical operation;
   exactly one authoritative `OperationTerminal` is published after the final
   generation joins.
7. Thread or host shutdown cancels and joins the current generation, ignores a
   queued resume, and publishes one terminal before returning.
8. Headless session ordering, event sequence, hooks, writer-failure behavior,
   and existing P0.3a/P0.3b ownership tests remain compatible.
9. Focused runtime-host and cancellation tests pass, followed by the shared
   runtime full gate and workspace Clippy with no new warnings.

### P0.3c Deletion Gate

This slice is incomplete if `ThreadActor` delegates generation replacement to
an external loop, exposes its cancel token or steer queue for direct mutation,
or completes the logical operation before the final joined generation. The
server `ActiveTurnManager`, generation writer, and legacy worker loop are not
deleted in P0.3c because the production server does not migrate in this slice;
their deletion is the mandatory completion gate of the immediately following
server migration.

### P0.3c Verification

- Seventeen runtime-host behavior tests cover logical operation and generation
  identity, fresh cancellation scopes, join-before-resume, duplicate resume,
  stale generation rejection, actor-owned steer admission, task lifecycle
  reopening, one logical terminal, headless resume rejection, and shutdown.
- A focused controller behavior test proves an existing-turn generation does
  not append the original user prompt again.
- `cargo test -p orca-runtime --all-targets -- --test-threads=1` passes with
  780 runtime unit tests, 17 runtime-host tests, and 12 task-output tests.
- `cargo test --workspace --all-targets -- --test-threads=1` passes, including
  130 server contracts and 467 TUI tests.
- `cargo clippy --workspace --all-targets` passes with the repository's
  existing warnings and no warning in the P0.3c implementation or tests.
- The real DeepSeek release harness passes provider summary, headless CLI,
  malformed-history resume and repair, server, server thread memory, active
  turn resume, turn controls, metadata, list/search, and paginated read gates.

### P0.3b Acceptance Criteria

1. Events emitted by consecutive operations on one actor have one run id and a
   strictly contiguous sequence.
2. A headless session emits one `session.started` before turn events and one
   `session.completed` after `SessionStart` and `SessionEnd` each execute once
   in order.
3. Existing headless JSONL and text contracts remain compatible.
4. A downstream writer or event subscriber failure completes as
   `OperationOutcome::ExecutionFailed`, never `Cancelled`.
5. Host or thread shutdown cancels, joins, and publishes one terminal while
   preserving ownership of the event/thread state until task exit.
6. The headless controller no longer constructs or mutates `RuntimeThread`,
   `InteractiveSession`, or `EventFactory`, and no longer emits session
   lifecycle events or runs session hooks.
7. Focused runtime-host, controller, lifecycle, and exec JSONL tests pass;
   shared-runtime and full workspace serial gates pass.
8. Workspace Clippy passes without new warnings.
9. A real DeepSeek headless JSONL smoke passes through `RuntimeHost` and shows
   one contiguous session sequence and one successful terminal.

### P0.3b Deletion Gate

This slice is incomplete until the old headless session envelope is deleted
from `controller::run_inner`. A helper that keeps controller-owned hooks or
events beside the host is not an acceptable final state. The legacy
turn-without-host APIs remain only for server/TUI and focused internal callers;
their deletion gates remain the later surface migrations listed above.

### P0.3b Verification

- Thirteen runtime-host behavior tests cover actor-owned event continuity,
  headless session and hook order, direct and relayed writer failure,
  headless shutdown ordering, and all P0.3a cancellation/join terminals.
- Controller behavior tests execute the migrated headless path and verify one
  contiguous session lifecycle plus borrowed-writer failure propagation. The
  obsolete source-shape test requiring `RuntimeThread::start` inside
  `run_inner` was deleted.
- The 14 exec JSONL contracts and the focused runtime-lifecycle controller
  contract pass with the existing wire shapes, ordering, exit codes, and
  contiguous sequence assertions.
- `cargo test -p orca-runtime --all-targets -- --test-threads=1` passes with
  779 runtime unit tests, 13 runtime-host tests, and 12 task-output tests.
- `cargo test --workspace --all-targets -- --test-threads=1` passes, including
  130 server contracts and 467 TUI tests.
- `cargo clippy --workspace --all-targets` passes with the repository's
  existing warnings and no warning in the P0.3b implementation or tests.
- The real DeepSeek release harness passes both the headless CLI sentinel and
  malformed-history resume sentinel through `RuntimeHost`; the repaired legacy
  tool call remains non-reexecuted.

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
