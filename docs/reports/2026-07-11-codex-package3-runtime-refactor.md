# Codex And Package 3 Runtime Refactor Priorities

Date: 2026-07-11

## Decision

Orca should stop treating more argument bundles and source-file extractions as
the primary architecture work. The runtime already contains many small helper
modules, but execution ownership still spans the TUI, server managers,
`RuntimeThread`, task snapshots, and the synchronous provider facade.

The next architecture line should establish one process runtime, one actor per
thread, and one owned operation task for each turn-like operation. That task
must own its cancellation scope, child operations, interaction waiters, event
sequencer, terminal outcome, and cleanup.

The recommended order is:

1. close every tool invocation with exactly one truthful terminal result and
   explicit interrupt/retry semantics;
2. replace resettable cancellation with one-shot operation scopes, stable
   operation identities, and separate current-work versus whole-agent stop;
3. introduce a runtime host and thread actors as the control plane, initially
   delegating to the existing executor;
4. move accepted input, same-turn steer, next-turn input, and background
   notifications behind the actor's generation-fenced input state machine;
5. move one canonical turn executor under actor-owned operation tasks;
6. call the async provider directly and migrate server, headless, and TUI
   surfaces onto runtime handles;
7. add a semantic execution journal with stable IDs and transactional context
   checkpoints;
8. make interactions and task control durable, then add leases and fencing;
9. add a bounded capability catalog for deferred tool discovery;
10. move workflow, subagent, goal, and scheduled-loop recovery onto that control
   plane.

This is not a recommendation for a big-bang rewrite. Each row below is a
shippable compatibility slice, but all new work should move toward the same
owner boundary.

## Approaches Considered

### Recommended: compatibility-layer strangler

Introduce the host, actor, operation, and journal boundaries behind the
existing CLI/server/TUI contracts. Migrate one production surface at a time,
then delete the old kernel and facade as soon as their final caller moves.

This permits a complete internal redesign while preserving bisectability,
history compatibility, and release evidence. It also makes every deletion
conditional on a behavioral parity gate rather than on confidence in a large
rewrite.

### Rejected: continue incremental call-surface extraction

More request/I/O/services structs can reduce argument counts, but they do not
decide who owns execution. This path would make the current synchronous kernels
tidier while preserving their duplicate cancellation, workers, and terminal
semantics.

### Rejected: one-shot rewrite

Replacing runtime, storage, protocol, and TUI orchestration in one branch would
temporarily remove duplicate paths, but it would combine transport, replay,
interaction, workflow, and UI regressions into one unreviewable change. The
`v0.2.16` release already demonstrates why independent release slices are
valuable: transport cancellation and tool-argument validity were verified
without simultaneously changing journal compatibility.

## Reference Baselines

- Codex: a clean local checkout at `main@5c19155cbd` during this review.
- Package 3: a source-map-restored snapshot of
  `@anthropic-ai/claude-code@2.1.88`, not a Git repository or release
  dependency.
- Orca: tagged and pushed `v0.2.16` at `main@4e6502704`, including the async
  DeepSeek transport compatibility slice and pre-side-effect tool validation.

## Deeper Codex Findings

### Submission and execution are separate objects

`Codex` and `CodexThread` expose command submission and event reception. The
submitted operation is not the object that executes a turn. `SessionTask`
defines task-specific `run` and `abort` behavior, while `RunningTask` owns the
turn context, one-shot cancellation token, done notification, abort-on-drop
handle, and task implementation.

Replacement and interruption follow one ordered path:

1. take the active task out of shared state;
2. cancel it;
3. wait for a bounded graceful shutdown;
4. force-abort the task if it is still alive;
5. run task-specific cleanup;
6. persist the interrupted-turn marker;
7. emit and flush one terminal turn event.

Relevant reference points:

- command handle and submission loop:
  `codex-rs/core/src/session/mod.rs:387`,
  `codex-rs/core/src/session/handlers.rs:714`,
  `codex-rs/core/src/codex_thread.rs:162`;
- task ownership and ordered abort:
  `codex-rs/core/src/tasks/mod.rs:206`,
  `codex-rs/core/src/tasks/mod.rs:314`,
  `codex-rs/core/src/tasks/mod.rs:492`,
  `codex-rs/core/src/tasks/mod.rs:834`;
- running task state:
  `codex-rs/core/src/state/turn.rs:72`;
- async provider turn and cancellable stream:
  `codex-rs/core/src/session/turn.rs:1970`,
  `codex-rs/core/src/client.rs:1957`.

The reusable principle is ownership, not Codex's exact types. A runtime task is
the sole owner of execution resources and terminal completion.

### Tool-call closure is a model-context invariant

Codex does not merely delete a function call when its output is missing.
`ContextManager` normalizes every model-bound history so each call has an
output and each output has a call. Missing outputs become deterministic
`aborted` outputs whose IDs are derived with UUID v5 from the source item ID.
Repeated normalization therefore preserves prompt-cache identity.

Relevant reference points:

- call/output normalization:
  `codex-rs/core/src/context_manager/history.rs:355`,
  `codex-rs/core/src/context_manager/normalize.rs:18`;
- deterministic synthetic output IDs:
  `codex-rs/core/src/context_manager/normalize.rs:130`;
- canonical typed items and public projection:
  `codex-rs/protocol/src/items.rs:39`,
  `codex-rs/app-server-protocol/src/protocol/v2/item.rs:222`.

Orca should go one step further for new executions: persist the real terminal
outcome at the cancellation boundary. Deterministic synthesis should remain a
compatibility repair for old or crash-truncated history, not the normal write
path.

### Rollout replay is segmented by explicit boundaries

Codex reconstructs history from append-only rollout records, explicit
turn-start/turn-terminal boundaries, replacement-history checkpoints, and
stable response items. Reverse replay can stop once it has enough metadata and
a surviving checkpoint, then replay only the required suffix forward.

Relevant reference points:

- rollout recorder boundary: `codex-rs/core/src/rollout.rs:1`;
- replay segmentation and checkpoints:
  `codex-rs/core/src/session/rollout_reconstruction.rs:44`,
  `codex-rs/core/src/session/rollout_reconstruction.rs:112`;
- prompt history invariants:
  `codex-rs/core/src/context_manager/history.rs:120`.

Orca should reuse explicit boundaries and replayable facts. It should not copy
the exact rollout schema or put every concern into one oversized core crate.

### Codex is not the durability endpoint

Codex's pending approvals, user input, permission requests, and MCP
elicitations are still in-memory one-shot senders in `TurnState`. They are good
same-process interaction references, but not a durable continuation design.
Orca should not mistake their presence for crash recovery.

### Input delivery is a state machine, not a string side channel

Codex separates turn-local steered input from session-scoped inter-agent mail.
It also records whether late mailbox input may still join the current turn or
must wait for the next one. A steer is accepted only for the expected active
regular turn; review and compaction tasks reject it. After visible terminal
output, late child mail is fenced into the next turn unless explicit same-turn
work reopens delivery.

Relevant reference points:

- turn input and mailbox queues:
  `codex-rs/core/src/session/input_queue.rs:12`,
  `codex-rs/core/src/session/input_queue.rs:34`;
- current-turn versus next-turn delivery:
  `codex-rs/core/src/state/turn.rs:35`,
  `codex-rs/core/src/session/input_queue.rs:126`;
- expected-turn validation and steerability:
  `codex-rs/core/src/session/mod.rs:3857`;
- sampling-boundary drain rules:
  `codex-rs/core/src/session/turn.rs:218`.

Orca already exposes `turn/steer`, but `ThreadSteerHandle` is only a
process-local `Vec<String>` and the server validates a turn ID rather than an
operation generation. Workflow notifications and TUI queued input are separate
queues. The actor migration should preserve these capabilities while replacing
the separate queues with typed admitted input records: `AcceptedTurnInput`,
`SameTurnSteer`, `NextTurnInput`, and `BackgroundNotification`.

## Deeper Package 3 Findings

### One conversation engine, but not a one-shot turn owner

`QueryEngine` centralizes the headless query lifecycle, conversation messages,
file cache, usage, provider loop, tool results, and transcript writes. This is
useful evidence that TUI and headless execution should share one kernel.

However, its `AbortController` is owned by the conversation engine and aborted
in place. That is not the target cancellation design for Orca. Orca should take
the unified engine idea but create a fresh, one-shot scope per operation.

Relevant reference points:

- query engine lifecycle: `src/QueryEngine.ts:176`;
- long-lived abort controller: `src/QueryEngine.ts:184`,
  `src/QueryEngine.ts:1158`;
- queue generation guard: `src/utils/QueryGuard.ts:29`.

`QueryGuard` contributes one useful detail: generation fencing prevents a stale
`finally` block from cleaning up a newer query. Orca needs the same property in
its operation ID and supervisor generation, enforced by ownership rather than
UI state.

### Accepted input is persisted before provider work

Package 3 records the accepted user message before entering the model query
loop. If the process dies before the first provider response, resume still has
an authoritative user-turn boundary.

Reference: `src/QueryEngine.ts:436`.

Orca's journal should make operation acceptance and user input durable before
starting provider or tool side effects.

### Tool closure exists both at runtime and at compatibility boundaries

On streaming abort, Package 3 drains its tool executor so queued and in-flight
tool uses receive synthetic interrupted results. On unexpected query failure,
it also emits missing tool results. Before an API request, defensive pairing
repairs missing results, removes orphan results, and deduplicates IDs.

Relevant reference points:

- interrupted result synthesis: `src/query.ts:123`,
  `src/query.ts:955`, `src/query.ts:1011`;
- defensive pairing: `src/utils/messages.ts:5118`;
- streaming tool ownership:
  `src/services/tools/StreamingToolExecutor.ts:126`;
- read-only concurrency partitioning:
  `src/services/tools/toolOrchestration.ts:20`.

Orca already has conservative tool scheduling. The missing piece is not another
partitioner; it is invocation ownership and terminal closure across cancel,
panic, process loss, and replay.

### Subagent state is isolated while cache-critical input is stable

Package 3 creates child abort controllers, clones mutable file state, and makes
shared mutation an explicit opt-in. It also carries cache-critical prompt
inputs unchanged into forks so subagents can reuse the parent prefix.

Reference: `src/utils/forkedAgent.ts:253`.

Orca should reuse parent-to-child cancellation, isolated mutable state, and
stable context identity. It should not copy the broad `ToolUseContext` object or
its callback-heavy mutation surface.

### Durable intent and worker ownership are distinct

Scheduled tasks are persisted independently of the process that currently
drives them. An exclusive lock identifies an owner, passive sessions probe for
liveness, and a stale owner can be replaced without every session firing the
same task.

Relevant reference points:

- scheduler ownership: `src/utils/cronTasksLock.ts:100`;
- takeover loop: `src/utils/cronScheduler.ts:406`;
- live-session registry: `src/utils/concurrentSessions.ts:49`.

The reusable principle is a durable control record plus a fenced worker lease.
Orca should not copy PID files as its authority. PID and process start time are
diagnostics; a lease epoch or fencing token must decide who may commit.

### Foregrounding, interrupting work, and terminating an agent are different

Package 3 distinguishes the controller that stops a teammate's current turn
from the controller that terminates the whole teammate. Foreground/background
navigation changes presentation ownership without changing execution
ownership. This is more precise than one shared cancellation flag.

Relevant reference points:

- whole-agent and current-work controllers:
  `src/tasks/InProcessTeammateTask/types.ts:36`;
- current-work-only interruption from task navigation:
  `src/hooks/useBackgroundTaskNavigation.ts:157`;
- foreground/background view handoff:
  `src/hooks/useSessionBackgrounding.ts:42`,
  `src/hooks/useSessionBackgrounding.ts:76`.

Orca should model `DetachSubscriber`, `InterruptOperation`, `StopAgent`, and
`ShutdownThread` as different commands. Parent cancellation may cascade to
children, but moving a task to the background must never replace or reset its
operation token.

### Tool definitions carry control semantics, not only schemas

Package 3 lets tools declare whether an interrupt may cancel them or must wait
for completion. Its streaming executor closes every queued or executing tool
with a synthetic result when interruption is allowed. Orca already has the
better foundation for this in `ToolSpec`: capabilities, exposure,
`concurrent_safe`, and result semantics. It should extend that registry rather
than add more name-based checks.

Add two explicit dimensions:

- `InterruptSemantics`: `CooperativeCancel`, `WaitForTerminal`, or
  `DetachAndObserve`;
- `ReplaySemantics`: `SafeToRetry`, `IdempotentWithKey`, or
  `IndeterminateAfterStart`.

These values drive cancellation, recovery, and UI text. They do not claim that
a process can roll back an external side effect.

`DetachAndObserve` is a target ownership policy, not permission to fire and
forget work during P0. It becomes executable only when `RuntimeHost` can
atomically adopt the invocation ID, join handle, cleanup obligation, and
terminal commit. Until that handoff exists, the runtime must use
`WaitForTerminal` or fail closed.

### The command queue is an admission mechanism

Package 3 uses one queue with `now`, `next`, and `later` priorities and a query
generation guard so a stale `finally` block cannot release a reservation owned
by a newer query. The useful part is not its React store. It is the invariant
that dequeue, reservation, execution generation, and release agree on one
owner.

Relevant reference points:

- unified queue subscription and priorities:
  `src/hooks/useQueueProcessor.ts:17`;
- generation fencing: `src/utils/QueryGuard.ts:29`;
- accepted input persisted before provider work: `src/QueryEngine.ts:436`.

In Orca, the thread actor should append an `InputAdmitted` semantic record before
acknowledging a state-changing submission. P0.4 seeds this narrow record in the
same append-only stream that P1.1 later expands into the full execution journal;
it must not create a throwaway queue file or wait for the full journal redesign.
A stale generation may finish cleanup, but it may not drain input, publish a
terminal record, or clear the new operation.

This durability rule applies to resumable threads. An explicitly ephemeral
thread may acknowledge only process-local admission, must advertise that it is
not recoverable, and must not persist prompt text behind
`HistoryMode::Disabled`.

### Deferred tools are now a correctness concern

Orca counts tool schemas in its wire-equivalent context budget and already has
`ToolExposure::Deferred`, but every registered tool is currently direct. The
DeepSeek adapter truncates advertised tools to the first 128. Once MCP and
external tools cross that boundary, a configured tool can become model-invisible
based only on registry order.

Package 3 and Codex both provide references for deferred capability discovery.
Orca should implement a provider-neutral local catalog rather than copy
Anthropic's `tool_reference` wire type:

1. keep a small, stable base schema containing `tool_search`;
2. search bounded name/description/capability metadata locally;
3. persist discovered tool identities in the operation context;
4. advertise the selected schemas on the next sampling request in deterministic
   order;
5. fail explicitly when a required capability cannot fit, instead of silently
   truncating it.

This belongs after stable operation and tool identities, but before a plugin
marketplace or large remote-tool ecosystem.

## Current Orca Gaps

| Gap | Current evidence | Consequence |
|-----|------------------|-------------|
| Two main execution kernels | TUI owns provider/tool/compaction loops in `crates/orca-tui/src/agent_runner.rs:838`; runtime defines `ThreadTurnExecutor` in `crates/orca-runtime/src/controller.rs:75` and enters it at `:481` | Cancellation, replay, and tool fixes do not automatically cover every surface |
| Lifecycle records do not own execution | `RuntimeTaskLifecycle` is descriptive in `runtime_lifecycle.rs:12`; `RuntimeThread::run_request` is synchronous in `thread.rs:104` | There is no shared handle that guarantees cancel, join, cleanup, and one terminal outcome |
| Resettable shared cancellation | `CancelToken::reset` is in `orca-core/src/cancel.rs:20`; TUI and server reuse it, including `server/processors/turn.rs:86` | A stale operation can be re-enabled and `turn/resume` has ambiguous semantics |
| Input admission is split by surface | runtime steer is a `Vec<String>` in `lifecycle.rs:97`; server owns active-turn controls; TUI owns workflow notification queues | Accepted input, same-turn steer, next-turn input, and late child output have no single ordering or generation owner |
| Nested surface-specific workers | Server moves a whole thread into `std::thread::spawn` at `server.rs:1787`; TUI starts an agent thread and another provider thread; the provider facade creates a Tokio runtime per call | Shutdown, replacement, resource reclamation, and backpressure have different contracts |
| Incomplete tool turns are discarded | `normalize_tool_boundaries` keeps a tool-bearing assistant only when every result is present in `orca-core/src/conversation.rs:307` | Resume can lose completed context or repeat a mutating side effect |
| Tool terminal taxonomy is too weak | `ToolStatus` has completed/failed/denied/not-implemented only in `orca-core/src/tool_types.rs:494` | Cancellation and crash-unknown outcomes are collapsed into misleading success/failure text |
| Tool control metadata is incomplete | `ToolSpec` has capability, exposure, result semantics, and concurrency safety in `orca-core/src/tool_types.rs:468`, but no interrupt or replay policy | The runtime cannot decide whether to cancel, wait, detach, or mark an interrupted mutating tool indeterminate |
| Event ordering is local, not canonical | `EventFactory` owns a local `seq` at `event_schema.rs:161`; TUI creates a second factory for the same run at `agent_runner.rs:1064` | Duplicate sequence numbers and cross-child ordering make replay ambiguous |
| Durable history is message-shaped | `SessionRecord` stores messages, completion, compaction, usage, and plan only in `thread_store/types.rs:81` | Turn, operation, interaction, invocation, and control transitions cannot be reconstructed |
| IDs are derived from current position | turn and item IDs are rebuilt as `turn-N` and `item-N` in `thread_store/projection.rs:408` | Compaction, repair, or filtering can change public identity across reads |
| Corrupt middle records are skipped | `read_records` tolerates a partial final line but also silently continues past malformed middle lines in `thread_store/writer.rs:35` | That policy is acceptable only for legacy best-effort history, not a control journal |
| Task persistence is not worker recovery | `TaskRegistry` persists snapshots and recreates fresh control tokens in `tasks.rs:25` and `:1117` | A stop/resume snapshot does not prove that the old worker stopped or the new worker owns commits |
| Pending interactions are process-local | `RuntimePendingInteractionStore` is an in-memory `HashMap` in `runtime_pending_interaction.rs:216`; server managers use channels | Approval and user-input continuations disappear with the process |
| Dependency direction does not enforce the boundary | `orca-tui` directly depends on provider, tools, MCP, approval, and runtime crates | The TUI can continue growing a second runtime even if helper functions move |
| Architecture tests overfit source text | `orca-runtime/src/lib.rs` is over 6,000 lines and contains many `include_str!().contains(...)` assertions | Tests prove spelling and file placement more often than ownership, cancellation, or replay behavior |
| Goal execution remains a TUI loop | goal state is persisted in `goals.rs`, but automatic continuation is `app.rs:2847` | Headless/server recovery and cost control do not share an orchestrator |
| Deferred tool support is scaffold-only | `ToolExposure::Deferred` exists, all registry entries are direct, and `deepseek_http.rs:668` truncates after 128 tools | Large MCP/external-tool sets silently lose capabilities and destabilize the prompt prefix |
| Compaction commit is not atomic | `session.rs:585` replaces live conversation before best-effort count and summary records are appended separately | A crash or write failure can leave live state, replayed state, and summary state disagreeing |

The v0.2.16 provider facade is intentionally temporary. It now owns async HTTP
request/body cancellation and joins its worker, but runtime still calls the
synchronous facade and the TUI adds another outer worker. It should not grow
into a second runtime.

## Target Architecture

### Process host

`RuntimeHost` owns the process Tokio runtime, thread registry, supervisor,
journal services, and outgoing event subscriptions. It exposes cloneable
handles; callers never borrow an `InteractiveSession` or a writer across a
turn.

### Thread actor

One `ThreadActor` owns each conversation, thread extensions, config snapshot,
stable ID allocator, current operation handle, and thread journal writer. A
bounded command channel serializes:

- `StartTurn`;
- `InterruptOperation`;
- `SteerOperation`;
- `ResolveInteraction`;
- `AdmitNextTurnInput`;
- `AdmitBackgroundNotification`;
- `ReadSnapshot`;
- `ShutdownThread`.

The actor may run child work concurrently, but every authoritative state
transition returns through the actor, which checks the operation generation and
assigns the next thread sequence number.

The input side has explicit delivery phases. Inputs admitted for an active
operation carry `expected_operation_id`; inputs intended for a later turn are
durable thread mail. A visible terminal answer closes same-turn mailbox
delivery. Late child output remains queued unless an explicit steer reopens the
current operation before its terminal commit.

### Operation task

Each turn, compaction, workflow, subagent, or long-running tool gets a fresh
`OperationScope` with:

- stable operation and parent IDs;
- a one-shot `tokio_util::sync::CancellationToken`;
- child scopes;
- owned task handle;
- completion signal independent of event consumers;
- typed terminal outcome;
- cleanup hook and deadline;
- operation-local interaction registry.

Operation scope and agent lifetime are separate. A background subagent has an
agent lifetime scope and creates a fresh child operation for each turn. Stopping
current work cancels only that child operation; stopping the agent cancels the
lifetime scope and all descendants. Detaching a UI subscriber cancels neither.

`turn/resume` must never reset a cancelled token. Compatibility RPCs may keep
their current wire shape, but resuming an interrupted turn creates a new
operation with `resumed_from`, or rejects if the operation is still active.

### Provider boundary

`orca-provider` should expose only async provider work. It must not create a
thread or Tokio runtime. The first migration can keep a callback or bounded
async channel; a custom `Stream` trait is not required to remove the ownership
problem.

The runtime awaits provider completion directly and forwards provisional deltas
through a bounded live-event channel. Consumer disconnect cancels that
subscription, not the authoritative operation terminal signal.

### Two event planes

Not every token delta needs a synchronous durable write.

1. **Semantic journal:** operation accepted/started/terminal, completed model
   item, invocation state changes, interaction request/resolution, compaction
   commit, task control, goal accounting, and checkpoints. These facts are the
   recovery source of truth.
2. **Live stream:** reasoning/message deltas, progress, spinners, and other
   provisional presentation events. These may be coalesced or dropped for a
   lagging subscriber; completed semantic items may not.

The actor sequences both planes. Authoritative events are appended before they
are projected. A live delta can appear before durability, but recovery never
depends on reconstructing partial text from those deltas.

### Transactional context checkpoints

Compaction builds a candidate projection without mutating the authoritative
conversation. The actor then appends one checkpoint record containing the
compaction ID, source sequence, retained item IDs, summary state, and context
window metadata. Only after append acceptance does it swap the live projection
and publish `context.compacted`.

This fixes a current Orca weakness: conversation replacement happens before
best-effort compaction and summary records, and those records are correlated by
message counts. Counts are not stable identities. Legacy count-based records
remain readable, while all new checkpoints use item IDs.

### Capability catalog

`ToolSpec` remains the canonical registry record. The runtime derives a bounded
base catalog from direct tools and searchable metadata from deferred tools.
Discovery changes the operation's capability snapshot, not the global registry,
so a child or retry cannot silently change an already running request's tool
surface. Catalog revisions and selected tool IDs become part of step context and
trace metadata.

### Storage and projections

The semantic journal remains append-only and portable. Rebuildable projectors
derive:

- model conversation history;
- public thread/turn/item views;
- task and goal summaries;
- pending interactions;
- usage and cost totals;
- search/index rows.

The new reader must accept only a truncated final record as recoverable. A
checksum or framed length plus contiguous sequence detects middle corruption;
middle corruption is quarantined or fails closed instead of being skipped.

SQLite is justified later as a rebuildable index and transactional lease store,
not as a label placed on the existing task snapshots. JSONL semantic records
remain the portable source of truth.

### Dependency direction

The intended compile-time shape is:

```text
orca-core        stable value types, IDs, commands, events, item schemas
    ^
    |
orca-provider    async provider adapter only
orca-tools       tool implementations only
    ^
    |
orca-runtime     host, actors, operations, journal, interactions, supervisor
    ^
    |
orca-tui         runtime commands + event projection, no provider/tool execution
app server       protocol validation + runtime commands + serialization
CLI              composition root
```

The exact crate names need not change immediately. The enforceable end state is
that `orca-tui` no longer depends directly on `orca-provider`, `orca-tools`,
`orca-mcp`, or `orca-approval`.

## Required Invariants

Architecture work is complete only when tests can prove these properties:

1. every accepted operation has exactly one terminal outcome;
2. cancellation is monotonic and idempotent; it is never reset;
3. a stale operation generation cannot mutate a newer thread state;
4. parent terminal completion waits for required child cleanup or records a
   forced-abort outcome;
5. every tool invocation has exactly one terminal result;
6. a crash-recovered running mutating tool is `indeterminate`, not falsely
   reported as cancelled or safe to retry;
7. a stale operation cannot drain or clear input admitted for a newer
   generation;
8. same-turn steer, next-turn input, and late child mail have explicit delivery
   phases;
9. no event is authoritative before its semantic record is append-accepted;
10. thread journal sequence is contiguous and assigned by one owner;
11. interaction resolution is idempotent by interaction ID;
12. a stale lease holder cannot commit after takeover;
13. compaction changes the live projection only after its checkpoint is
    append-accepted;
14. a capability snapshot never exceeds provider limits through silent
    truncation;
15. replaying the same journal produces byte-equivalent canonical projections;
16. TUI, server, and headless commands execute through the same turn kernel.

## Ranked Refactor Plan

Completed baseline: `v0.2.16` is no longer active architecture work. Keep its
joined synchronous provider facade frozen as a compatibility boundary and
delete it when the final synchronous production caller moves to the async
provider API.

| Priority | Slice | Main outcome | Risk | Prerequisites |
|----------|-------|--------------|------|---------------|
| P0.1 | Invocation truth and tool control metadata | Add cancelled/indeterminate terminals plus interrupt and replay semantics; preserve every call with exactly one result; repair legacy history deterministically | Transcript compatibility and accidental retry of mutating tools | None |
| P0.2 | Stable operation identity and stop hierarchy | One-shot `OperationScope`; separate detach, interrupt current operation, stop agent, and shutdown thread; no token reset | `turn/resume` semantics and background-task control change | P0.1 for truthful cancellation history |
| P0.3 | `RuntimeHost` and `ThreadActor` control plane | One bounded command owner for active operation, generation, input, terminal completion, and sequencing; initially delegate execution to the legacy `ThreadTurnExecutor` | Borrowed session state must move behind owned commands without changing behavior | P0.2 |
| P0.4 | Generation-fenced input admission | Through the actor, seed `InputAdmitted` records for resumable threads before provider work; distinguish same-turn steer, next-turn input, and background notification; stale generations cannot drain or clear newer input | Ordering changes at answer and child-mail boundaries; ephemeral threads remain explicitly non-resumable | P0.3 |
| P0.5 | Canonical actor-owned `ThreadTurnExecutor` | Move turn, compaction, interactions, and child-operation ownership under one operation task while retaining current surface contracts | Large parity surface and cleanup ordering | P0.3-P0.4 |
| P0.6 | Async provider through runtime | Runtime awaits `call_streaming_async`; remove per-call provider runtime/thread from production paths | Event sink backpressure and `Send` boundaries | P0.5 |
| P0.7 | Surface convergence | Migrate server, then headless, then TUI to runtime handles; delete the TUI provider/tool loop and direct execution dependencies | Broad behavior parity across surfaces | P0.5-P0.6 |
| P1.1 | Semantic execution journal, stable IDs, transactional compaction | Append replayable operation/item/invocation facts; one sequencer; install context checkpoints before projection swaps | Migration, write amplification, and corruption policy | Stable P0 identities and one kernel |
| P1.2 | Async `ToolCallRuntime` | Per-invocation task owner, concurrency permit, approval state, output stream, cancellation, cleanup, and terminal CAS | Shell/MCP teardown and blocking tool adapters | P0.1, P0.5, P1.1 |
| P1.3 | Durable interaction broker | Permission, tool approval, user input, and MCP elicitation become persisted idempotent request/response records | Expiry, duplicate, and ownership policy | P1.1-P1.2 |
| P1.4 | Context identity and deferred capability catalog | Stable cache-critical prefix, operation-scoped tool snapshot, local `tool_search`, deterministic selection, and explicit provider-cap errors | Loaded-tool changes can invalidate cache or strand old history | P1.1 and canonical tool IDs |
| P1.5 | Unified supervisor, leases, and fencing | Merge active turns, task workers, subagents, workflows, shell, and background work under cancellation trees and owner epochs | Incorrect takeover or stale commit | P0.5, P1.1 |
| P2.1 | Checkpointable workflow/subagent resume | Resume the same run from a safe cursor with idempotency keys instead of replaying only cached completions | External side effects may be indeterminate | P1.1-P1.5 |
| P2.2 | Runtime goal and schedule orchestrator | Move goal cursor, attempt, usage, tool attempts, lease, continuation, cadence, overlap, missed-run, expiry, and budget policy out of TUI | Runaway retries, duplicate fires, and cost policy | P0.7, P1.1, P1.5 |
| P2.3 | App-server dependency inversion | Processors depend on runtime handles, stores, and outgoing interfaces instead of full mutable `ServerState` | Request ordering and response timing | P0.7, P1.1 |
| P2.4 | Workspace mutation checkpoints | Optionally snapshot files touched by first-party edit/write tools and expose explicit rewind; never claim rollback of shell, MCP, network, or external effects | Disk usage and false safety if scope is unclear | Stable invocation IDs and P1.2 |
| P3 | Crate cleanup and product expansion | Remove source shims/source-text tests; consider CLI/store crate splits; then add plugins, remote control, LSP, and richer PTY support | Feature-specific | P0-P2 |

## Recommended Release Slices

Preserve the repository's patch-release cadence:

1. `v0.2.17`: tool terminal taxonomy, interrupt/replay metadata,
   interrupted/indeterminate closure, and deterministic legacy repair.
2. `v0.2.18`: one-shot operation scopes, typed operation terminal outcomes,
   stop hierarchy, and replacement for token-reset `turn/resume`.
3. `v0.2.19`: seed `RuntimeHost`, `ThreadActor`, and the narrow
   `InputAdmitted` semantic record, then route generation-fenced current/next-turn
   input through that owner while execution still delegates to the existing
   turn executor.
4. `v0.2.20`: move the canonical runtime turn and compaction under the owned
   operation task.
5. `v0.2.21`: call the async provider directly; migrate server and headless, and
   remove their compatibility provider workers.
6. `v0.2.22`: migrate TUI to command/event handles; delete its provider/tool
   loop and direct provider dependency.
7. `v0.2.23`: introduce semantic journal v2, stable item IDs, transactional
   context checkpoints, and replay equivalence while dual-reading legacy JSONL.
8. `v0.2.24+`: durable interactions, `ToolCallRuntime`, capability catalog,
   supervisor leases, then workflow/subagent/goal recovery.

Do not assign dates to later releases until the preceding ownership invariant
is proven. Version numbers are dependency markers, not permission to merge an
unfinished slice.

## Compatibility And Migration

### Existing history

- Keep reading the current message-shaped session JSONL.
- Derive deterministic legacy operation/turn/item IDs from session ID, record
  position, and tool call ID without rewriting old files.
- Normalize incomplete legacy tool boundaries in memory with an explicit
  compatibility-repair marker.
- Start journal v2 only for new operations; replay can project both formats
  into one canonical item model.
- Never silently reinterpret a previously running mutating tool as not having
  executed. Use an `indeterminate` terminal and require state inspection before
  retry.

### Wire protocol

- Preserve existing server method names and payloads through P0.
- Add operation IDs and terminal reasons as additive fields first.
- Map legacy `turn/resume` to a new continuation operation; do not resurrect a
  cancelled operation.
- Keep old event shapes as projections until stable journal/item IDs are ready.

### Runtime migration

- Keep the v0.2.16 synchronous provider facade only while a production caller
  still requires it.
- Migrate server before TUI because it already uses `RuntimeThread` and has the
  smaller semantic distance to the canonical executor.
- Keep the TUI UI loop synchronous if desired; only execution moves behind a
  runtime handle.
- Remove the compatibility worker in the same slice that removes its final
  production caller.

## Verification Gates

Every slice needs behavioral tests, not only source-placement assertions:

- cancellation before request, during retry wait, during body streaming,
  during approval, during tool execution, and during cleanup;
- current-operation interrupt versus whole-agent stop versus subscriber detach;
- callback/event-consumer panic or disconnect;
- replacement while an old worker is finishing;
- stale steer, stale cleanup, late child mail, and next-turn input ordering;
- exactly one operation terminal and exactly one invocation terminal;
- process-kill fault injection after `tool.running` but before terminal commit;
- compaction failure before checkpoint append and after checkpoint append but
  before live projection swap;
- replay equivalence between live projection and cold reconstruction;
- final partial record recovery and middle-record corruption rejection;
- duplicate interaction response and stale lease-holder commit rejection;
- deferred-tool selection stability, provider tool-cap overflow, and replay
  after a discovered tool disappears;
- server, headless, and TUI contract tests against the same mock turn script;
- compile-time dependency checks proving the TUI cannot call provider or tool
  execution directly.

As each ownership boundary becomes compile-enforced, retire the corresponding
`include_str!().contains(...)` architecture assertions and move remaining tests
next to the behavior they protect.

## What Not To Copy

- Codex's exact core crate layout or rollout schema.
- Codex's process-local interaction waiters as a durability design.
- Package 3's reusable conversation-level `AbortController`.
- Package 3's broad callback-heavy `ToolUseContext`.
- Package 3's React command queue as the runtime source of truth.
- Anthropic-specific `tool_reference` blocks; DeepSeek needs a local catalog
  and deterministic next-request schema selection.
- React/Ink waiters, Zod object layout, PID files, or package-specific JSONL.
- Per-token synchronous fsync.
- A generic event-sourcing framework before Orca's own semantic records are
  stable.

## Deferred Product References

These are useful references, but none should displace P0-P2 ownership and
recovery work.

| Priority | Capability | Reference | Orca direction |
|----------|------------|-----------|----------------|
| P2.5 | Content-addressed tool output store | Package 3 `src/utils/toolResultStorage.ts` and `mcpOutputStorage.ts`; Codex function-output truncation | Persist full large outputs as bounded artifacts and place typed previews plus references in model context; do this after stable invocation/item IDs |
| P2.6 | Replay-native tracing and fault diagnostics | Codex rollout trace/OpenTelemetry; Package 3 Perfetto and session tracing | Correlate thread, operation, provider attempt, invocation, interaction, and lease epoch without making telemetry authoritative |
| P3.1 | Plugin and capability manifests | Codex app-server plugin/marketplace processors; Package 3 `PluginInstallationManager` | Extend the existing skill system with signed/validated manifests and capability declarations only after runtime/tool interfaces stabilize |
| P3.2 | Remote control and reattachment | Codex app-server; Package 3 `RemoteSessionManager`, permission bridge, and WebSocket transport | Put a network transport in front of the same runtime handles and durable interactions; never create a remote-only execution kernel |
| P3.3 | LSP diagnostics service | Package 3 `LSPDiagnosticRegistry` and `LSPServerManager` | Add a project-scoped diagnostic actor whose snapshots are bounded context artifacts and whose processes live under the supervisor |
| P3.4 | Memory consolidation | Codex memory pipeline; Package 3 SessionMemory and auto-consolidation services | Derive candidate memories from completed semantic journal ranges, then require explicit bounded storage and provenance |
| P3.5 | Structured output contracts | Package 3 QueryEngine structured-output enforcement; Codex typed protocol items | Validate terminal agent output against a schema at the runtime boundary, with repair attempts represented as child operations |
| P3.6 | Richer shell and worktree isolation | Codex unified exec, sandbox, and agent worktree controls | Move PTY/process ownership and worktree leases under the supervisor before adding more terminal fidelity or automatic worktree workflows |

Prompt suggestions, voice, decorative task-panel parity, plugin marketplace UX,
and additional media readers are lower priority. They increase surface area but
do not improve correctness, recovery, or execution ownership.

## Non-Goals

- Do not rewrite visual TUI components before the operation host exists.
- Do not add SQLite merely to call task snapshots durable.
- Do not expose workflow checkpoint resume until mutating-tool indeterminacy and
  idempotency policy are defined.
- Do not call file snapshots transactional rollback; they cover only known
  first-party file mutations, never arbitrary shell or external effects.
- Do not enable deferred tools by assigning `ToolExposure::Deferred` alone;
  discovery, stable IDs, history compatibility, and provider-cap failures must
  land together.
- Do not expand the synchronous provider facade after v0.2.16.
- Do not merge goal, workflow, operation, and task status enums; their control
  semantics are different.
- Do not split more crates merely to move lines. Split only after the ownership
  API is stable enough for the compiler to enforce the dependency direction.
