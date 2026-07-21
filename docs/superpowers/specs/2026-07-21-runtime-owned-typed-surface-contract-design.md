# Runtime-Owned Typed Surface Contract Design

- Date: 2026-07-21
- Status: approved; Phase 0A artifact work authorized
- Phase gate: Phase 0B remains blocked until the frozen Phase 0A companion
  bundle is committed and receives explicit written review by exact hashes
- Baseline: `main@50b7698d1` (`v0.2.50`)
- Target: the next Orca patch release, currently `v0.2.51`
- Extends: `docs/architecture/adr/0005-runtime-host-operation-control-plane.md`
- Scope: runtime operation ownership, typed snapshot/replay, durable
  interactions, TUI cutover, ACP cutover, JSONL server convergence,
  documentation, and public release verification

## Decision

Orca will expose one runtime-owned, closed, typed surface contract for every
runtime thread. The TUI is the first client, ACP is the second client, and the
released JSONL server is the final compatibility adapter migrated before the
release closes.

For all three clients, runtime is the only authority for:

- thread and operation admission;
- operation and generation identity;
- cancellation, pause, resume, steer, and background handoff;
- pending interaction identity, response validation, and recovery;
- snapshot, attach, replay, and slow-consumer semantics;
- Goal, task, workflow, usage, context, and settings facts;
- operation finalization and the canonical terminal outcome.

Clients retain presentation, transport, protocol negotiation, and request
correlation. They may project runtime facts, but may not infer or manufacture
runtime facts.

This is not a second actor system and not another executor rewrite.
`RuntimeHost`, `ThreadActor`, the canonical turn executor, the runtime
background registry, and the interaction broker remain the execution owners.
A resident `SurfaceHub` is a passive projection owned by the runtime thread. It
has no independent command authority, task lifecycle, or policy decisions.

The private Rust type names in this document are candidate names. The stated
ownership, ordering, persistence, recovery, and deletion invariants are
normative. Types stay private until TUI and ACP parity proves the contract.

## Why This Is The Next Refactor

The recent release sequence moved execution into runtime but did not finish
the client contract migration:

1. `v0.2.30` moved production TUI turns and workflows onto
   `RuntimeHost`/`ThreadActor` and removed the parallel TUI executor.
2. `v0.2.31` through `v0.2.33` added shared event ordering, durable sequence
   reservation, a semantic journal, stable turn/item identities, canonical
   completed model items, and runtime-owned user admission.
3. `v0.2.34` through `v0.2.36` moved read-only tools, normal tools, and
   subagents under runtime execution ownership.
4. `v0.2.46` repaired Goal control-plane dispatch.
5. `v0.2.47` added ACP as a thin adapter over `RuntimeThreadHandle`,
   `OperationHandle`, and raw `EventEnvelope` observation.
6. `v0.2.49` and `v0.2.50` moved Goal continuation and stopping policy into
   runtime while TUI and ACP continued to project Goal state independently.

The current architecture is therefore one runtime with several client-owned
semantic control planes. Adding another feature to each projector would deepen
that split.

## Source Review

The following local source snapshots were inspected before this decision:

| Reference | Revision | Reusable property | Boundary not to copy |
| --- | --- | --- | --- |
| Codex | `main@56395bddaf` | core `Session`/`ActiveTurn` owns the live turn; TUI reuses app-server request/notification types through an in-process client; running resume serializes snapshot composition with listener attachment; interrupt replies after `TurnAborted` | the full app-server/JSON-RPC projection still mirrors active-turn/listener state, has no general resumable live cursor or `SnapshotRequired`, sends bare core interrupt after an adapter-side id check, and publishes terminal before final rollout flush |
| Claude Code | `@anthropic-ai/claude-code@2.1.88` source-map restoration, cross-checked against local `2.1.206`-`2.1.212` bundles | a newer in-process typed engine exposes input, typed events/result, interrupt, model, permission, and settings control over the shared query generator | REPL, headless `QueryEngine`, SDK NDJSON, and remote bridges retain separate messages, abort, permission, resume, and terminal ownership; `hostOwnsPermissionMode` is an explicit split-control transition |
| Grok Build | local `98c3b2438`, checked against `origin/main@a881e6703` | `SessionActor` plus a mailbox and prompt-id fence owns execution and rejects stale completion; recent `0.2.105`/`0.2.106` tighten cancel races and durable replay | Pager-over-ACP splits cursor/reload projection across server and TUI and reconciles `PromptResponse`, compatibility completion, and persisted `TurnCompleted` as multiple terminal rails |

Codex's migration history is also instructive: it introduced the in-process
app-server, ran a parallel TUI, made transcript/terminal delivery lossless,
deleted the legacy TUI, then added dependency guards and continued removing
legacy core imports across `0.113` through `0.145` development. Orca should copy
that staged parity/deletion discipline, while keeping the shared domain below
the transport and adding stronger cursor, fence, and durability semantics.
Codex `main` has no ACP path. Its inspected experimental ACP branch is itself an
app-server adapter and retains prompt/turn projection, which reinforces ACP as
an external mapping rather than an internal runtime contract.

The Claude checkout is not a Git repository; it is a 2.1.88 npm source-map
restoration without tests, CI, or commit history. The newer local native bundles
confirm direction but provide minified-symbol/byte-offset evidence rather than
source history, so they are corroboration rather than a source-level template.
They show that sharing a query generator or typed engine does not by itself
remove client-owned lifecycle authority. Claude Code has no ACP implementation
in the inspected sources; its external surfaces are SDK NDJSON and remote
WebSocket/SSE/HTTP protocols.

Grok Build's public Git history consists of coarse monorepo snapshots, so recent
changes can prove the current boundary but not the original introduction commit
for `SessionActor` or ACP. Its design validates actor-side prompt fencing and
durable replay classification while also demonstrating the extra reconciliation
created when TUI replay and terminal semantics sit above ACP.

The useful common property is a typed boundary around one runtime owner. None of
the references provides Orca's required durable terminal plus atomic
snapshot/cursor contract, and none supports making ACP the in-process authority.

## Current Structural Defects

### Cancellation can deadlock

`PauseGoalRun` in `runtime_host.rs` requests cancellation, waits for the active
generation to join, and only then replies. The TUI calls
`TuiInteractionBroker::interrupt` only after that reply. A generation blocked
inside a TUI-owned interaction waiter can therefore prevent the join that must
complete before the TUI wakes it.

The corrected cancellation sequence is:

1. persist the pause/cancel intent and identify the exact operation generation;
2. close or transfer that generation's runtime-owned interactions;
3. wake their execution waiters;
4. signal generation cancellation;
5. commit a typed cancel-acceptance event and return its mutation receipt;
6. let the actor loop continue selecting commands and generation completion;
7. join owned workers from the completion branch and run the one runtime
   finalizer asynchronously.

The command handler must not await the generation join inline. Cancel
acceptance and operation terminal completion are distinct results. Acceptance
is returned by the command receipt; completion arrives later through the
surface terminal cursor and operation waiter.

### Running threads cannot be attached atomically

During execution, `ThreadActorState` moves into a blocking generation task and
`ReadSnapshot` returns `RuntimeHostError::OperationActive`. A client cannot
obtain a coherent running snapshot and subscribe without a history/live gap.

The surface projection must remain resident while execution state moves into
the generation. It must be readable without borrowing `RuntimeThread` and
without waiting for execution to finish.

### TUI is still a control plane

The TUI currently owns or mirrors:

- an active `OperationHandle` plus pre-install interrupt/background flags;
- a writable broker with operation identity and interaction waiters;
- generation handler construction for approvals, permissions, user input, and
  MCP elicitation;
- raw `EventEnvelope.payload` reduction into TUI events;
- terminal buffering and foreground cleanup ordering;
- direct history, Goal, task, MCP, workflow, and background continuation
  access;
- duplicate interrupt paths through direct controller calls and queued user
  actions.

Presentation state is valid TUI ownership. These lifecycle facts are not.

### ACP is a second, narrower control plane

The ACP adapter currently owns `current_op` and `cancel_requested`, compensates
for prompt/cancel installation races, observes raw payloads, infers terminal
status, and sends notifications through an unbounded channel.

`flatten_prompt` silently discards every non-text content block, including the
ACP baseline `ResourceLink`. `load_session` preloads a transcript without
streaming an ordered history baseline back to the client. Permission, user
input, MCP elicitation, negotiated filesystem/terminal capabilities, and
runtime recovery do not share the TUI semantics.

### JSONL server still owns interactions and active-turn projection

Server response processors remove a pending waiter before validating its
generation. A stale or invalid response can consume a valid request. The
server also retains active-turn and pending-interaction managers and rebuilds
events by parsing JSON payloads.

The released JSONL shape remains compatible, but those managers cannot remain
semantic owners after the shared surface is complete.

### Terminal publication is not one barrier

Turn code can emit `session.completed` before the actor restores state,
finalizes the writer, adopts background work, and completes the operation
waiter. TUI and ACP compensate locally. Persistence failure in some completion
paths is either flattened into a different outcome or lost.

There must be one actor-owned terminal commit and one client-visible terminal
fact.

### Existing sequence and journals are not a surface cursor

The existing event sequence reserves blocks durably and intentionally permits
gaps. The semantic journal excludes high-volume delta/progress events and
stores `EventEnvelope { payload: Value }`. It is useful legacy evidence but is
not a complete typed snapshot/replay source and cannot be exposed as the
surface cursor.

## Goals

1. Make runtime the sole authority for operation, interaction, replay, and
   terminal semantics.
2. Give all clients one closed typed snapshot and event contract.
3. Make fresh attach and cursor attach gap-free.
4. Make detach and slow-consumer failure independent from operation lifetime.
5. Persist the side-effect boundary and terminal barrier for recorded threads.
6. Make every interaction response fenced, durable, idempotent, and
   first-commit-wins.
7. Preserve TUI behavior while deleting TUI lifecycle ownership.
8. Make ACP a faithful, capability-aware adapter over the same facts.
9. Preserve the released JSONL wire while deleting server lifecycle ownership.
10. Update architecture, user, harness, and release documentation and publish a
    publicly verified patch release and npm package.

## Non-Goals

- Replacing the canonical provider/tool/compaction/hook executor.
- Routing TUI through ACP or stdio.
- Upgrading `agent-client-protocol` while changing ownership.
- Persisting every streamed token delta as a crash-durable event.
- Treating a live cursor as a permanent cross-process log offset.
- Redesigning legacy turn/item identity in the same slice.
- Moving layout, focus, scroll position, selection, theme, or physical TTY
  restoration into runtime.
- Claiming exactly-once model or external-tool execution after arbitrary
  process failure.
- Preserving experimental surface implementations merely because they already
  compile in another worktree.

## Target Ownership

```text
TUI renderer/input -----------\
                               -> RuntimeSurfaceHandle -> ThreadActor
ACP protocol adapter ---------/             |               |
JSONL compatibility adapter --/             |               +-> execution
                                             |               +-> Goal actor
                                             |               +-> broker
                                             |               +-> background registry
                                             +-> SurfaceHub -> snapshot/replay/subscribers
                                             +-> ThreadStore -> durable typed facts
```

| Concern | Sole owner | Client responsibility |
| --- | --- | --- |
| Thread and foreground admission | `ThreadActor` | send typed intent and render result |
| Background operation lifetime | `RuntimeHost` background registry | render status; send fenced control intent |
| Operation/generation identity | runtime operation ledger | retain opaque ids for correlation only |
| Pending interactions and responses | runtime interaction broker plus actor | render request and submit fenced response |
| Goal lifecycle and continuation | one host-injected `GoalRuntimeHandle` | render facts and send typed mutation intent |
| Snapshot, cursor, replay, subscriptions | resident `SurfaceHub` | hold projection and reattach on gap |
| Durable records | `ThreadStore` and Goal/task stores | no direct mutation or before/after diffing |
| TUI presentation | `orca-tui` | layout, input, keybindings, overlays, TTY |
| ACP protocol | ACP adapter | negotiation, standard/extension mapping, correlation |
| JSONL wire compatibility | server adapter | decode/encode released wire shapes |

`SurfaceHub` may serialize access to its reducer and retained suffix, but it
does not have a separate actor command loop and cannot admit, cancel, respond,
or finalize operations. All writable commands route through `ThreadCommand` or
the host command authority.

## Closed Host Surface

Clients also need a closed host facade so session discovery cannot bypass the
thread surface. `RuntimeSurfaceHostHandle` is the only TUI/ACP/server entry for
the following 24 host commands:

```text
SurfaceHostCommand =
  ListSessions
  | SearchSessions
  | ReadSessionMetadata
  | ReadSession
  | ReadThreadPage
  | CreateThread
  | OpenThread
  | LoadThread
  | ForkThread
  | ResolveRunningThread
  | ResumeLatestActiveGoal
  | UpdateSessionMetadata
  | QueryInputCatalog
  | ControlJsonlTurn
  | RememberMemory
  | ReconcileMemoryMutation
  | ReadFolderTrust
  | SetFolderTrust
  | ReconcileFolderTrustRevocation
  | ReadRuntimeSettings
  | UpdateRuntimeSettings
  | ReconcileHostMutation
  | CloseThread
  | ShutdownHost
```

List/search/metadata/page reads return typed immutable catalog or history-page
projections with stable pagination cursors. `ReadSession` returns one coherent
session bundle at one read token; `UpdateSessionMetadata` is the only catalog
metadata mutation; and `QueryInputCatalog` performs host/thread-context-bound
file, directory, skill, plugin, workflow, and resource discovery without giving
a client registry or filesystem authority. `ControlJsonlTurn` resolves a
released JSONL turn id and commits its actor control in one host command, so the
adapter owns neither an active-turn map nor a lookup-then-control race.
Create/load/fork/open returns a `RuntimeSurfaceHandle`, never
`RuntimeThreadHandle` or mutable history/store access. The host registry is
authoritative for live thread identity; the session catalog is authoritative
for durable discovery.

Resolution rules are deterministic:

1. if the canonical session id is live in this host/process registry, open or
   reconnect attaches that actor;
2. if it is not live but has a durable record, load materializes one actor and
   applies the restart matrix before returning a surface;
3. a concurrent create/load for the same canonical id has one registry winner;
   losers attach the winner or receive a typed configuration conflict;
4. a workspace/config identity mismatch never creates a second actor silently;
5. fork always allocates a new canonical session id and starts without an
   active operation while preserving only the explicitly forkable history;
6. cross-process open/load must acquire the thread's process-lifetime ownership
   lease before materializing an actor or applying the restart matrix. If a live
   owner holds it, return `ThreadOwnedElsewhere` or attach through an explicitly
   registered inter-process surface transport; never create a second actor or
   classify its active generation as restart-aborted;
7. a takeover is allowed only after the prior lease is provably gone. It
   increments the durable thread owner epoch before recovery, so a stale actor
   cannot commit or launch another generation.

`ResumeLatestActiveGoal(request_id)` runs under the one process Goal registry and
host session registry. It preserves the existing deterministic latest-active
ordering `(updated_at DESC, created_at DESC, session_id DESC)`,
opens or loads that Goal's canonical thread under the ownership-lease rules,
and idempotently reserves one new Goal operation through the same coordinator
intent. It returns the `RuntimeSurfaceHandle`, Goal change/read receipt, operation
receipt, and waiter as one closed result. A concurrent duplicate is
`AlreadyApplied`; a different active run is a typed conflict. The TUI never
opens a temporary Goal actor, reads the Goal store, or loads history to perform
this selection.

`RememberMemory` is host-scoped because user/project memory affects future
threads. Its closed input is:

```text
RememberMemory {
  request_id,
  scope: User | Project { canonical_root, root_revision },
  note,
  pin_to_thread: Option<SurfaceThreadId>,
}
```

The host validates/normalizes the note and project root. The user/project memory
store serializes cross-process append with a canonical-scope lock, stable record
id, expected revision, atomic replace/sync, and request-id probe; it returns a
memory revision plus immutable display path. When `pin_to_thread` is present, a
host mutation intent links that receipt to `PinnedContextMutation::Add`; retry
probes the memory receipt before
pinning so a partial failure cannot append the note twice. The closed result
distinguishes memory committed/pin committed from memory committed/pin deferred.
With no attached thread it commits only long-term memory; the next operation
loads that memory through runtime-owned context resolution, and TUI keeps no
authoritative pending-pinned buffer.

Memory committed with a missing thread pin returns
`Deferred { state: MemoryPinPending, retry: ReconcileMemoryMutation }`.
Reconciliation accepts only the original request/memory receipt, probes that
record, and retries the idempotent pin; it never appends memory again.

Folder trust is also host-scoped because it controls write/network authority
before a thread exists and can affect multiple live threads. Clients use only:

```text
ReadFolderTrust { path }
SetFolderTrust {
  request_id,
  path,
  expected_trust_revision,
  level: Trusted | Untrusted,
}
```

The host canonicalizes the path against its authoritative cwd/workspace roots
and returns the canonical key, matched ancestor decision, effective level, and
trust revision. The user-owned trust store uses a cross-process lock, expected
revision, stable request id, atomic replace/sync, fail-closed parse behavior, and
a monotonically increasing durable policy epoch. `SetFolderTrust` returns a
post-commit receipt containing old/new effective levels, trust revision, policy
epoch, and whether project configuration needs a fresh thread/reload.

Every trust mutation increments the host policy epoch and invalidates cached
authority fingerprints and Allow receipts whose cwd/root policy depends on the
changed path. Adding trust never widens an existing operation, generation,
attachment grant, or already loaded project configuration; new work must resolve
the new epoch explicitly.

Every process using the trust store holds a renewable `PolicyOwnerLease` and
registers the canonical roots and governed resource supervisors for which it
holds write/network authority. Every generation/tool permit, OS command/proxy
request, filesystem write, and ACP capability delivery carries the observed
policy epoch. Immediately before each new governed side effect, its owner must
prove that epoch still equals the durable store epoch; read/check failure is
fail-closed. A long-lived subprocess, proxy request, or remote terminal cannot
recheck every external action, so it is registered as a revocable resource under
the owner lease.

Removing trust is a cross-process safety barrier:

1. commit the Untrusted decision and new policy epoch, close new affected
   admission locally, and publish a durable invalidation record;
2. every live affected policy owner observes the record, revokes interaction
   grants and write/network permits, cancels governed calls, boundedly
   kills/joins local subprocess trees and proxy work, settles known ACP terminal
   leases, publishes the policy change to its live surfaces, and writes an ack
   identifying any ambiguous resource;
3. the mutating host returns `Committed` only after every non-expired affected
   owner lease has acknowledged the epoch and every registered resource is
   stopped or proved outside the removed authority;
4. a missing owner ack, live resource, unknown remote terminal, or failed
   kill/join returns `Deferred { state: PolicyRevocationPending, retry:
   ReconcileFolderTrustRevocation }` with pending owner/resource identities. It
   never claims completed revocation;
5. lease expiry permits takeover/reconciliation but is not proof that an orphan
   resource stopped. The durable resource record must be probed/killed or remain
   explicitly ambiguous.

Already completed external effects are not undone or hidden. A stale process
cannot launch a new governed effect because the immediate epoch check fails;
`ReconcileFolderTrustRevocation` retries only acknowledgement and resource
cleanup for the original request/epoch, never a new trust mutation.

Runtime defaults used before thread creation are host-owned as well:

```text
RuntimeSettingsTarget =
  HostDefaults
  | Thread(SurfaceThreadId)
  | HostDefaultsAndThread(SurfaceThreadId)

RuntimeSettingsPatch =
  SetModel
  | SetReasoning
  | SetApprovalMode
  | SetWorkspaceRoots
```

`ReadRuntimeSettings` returns the host-default revision plus any attached
thread's effective and pending revisions. `UpdateRuntimeSettings` validates a
closed patch, expected revision, model/mode support, roots, and trust policy;
updates host defaults and, when requested, routes the thread portion through
`SettingsMutation` under one host receipt. New operations capture the committed
settings/policy revisions at admission. A running generation retains its frozen
revision; a settings change cannot silently alter its model or widen authority.
TUI may optimistically render a pending choice, but it updates its effective
presentation/config projection only after the committed receipt and reverts on
rejection.

ACP new/load, TUI startup/session picker/latest-Goal resume, and JSONL
start/resume use this facade. Direct history or Goal-store loading in an adapter
is a deletion-gate violation.

## Closed Surface Domain

The initial contract is private under an `unstable_surface` facade. The
facade exposes reviewed closed values, not `RuntimeThreadHandle`,
`OperationHandle`, `EventEnvelope`, registries, provider DTOs, parser buffers,
or arbitrary `serde_json::Value`.

Candidate identifiers:

```text
SurfaceThreadId
SurfaceOperationId
SurfaceGenerationId
SurfaceOperationFence {
  thread_id,
  thread_owner_epoch,
  operation_id,
  generation_id,
}
SurfaceBackgroundFence { operation_fence, background_owner_token }
SurfaceInteractionId
SurfaceAttachmentId
SurfaceResponseId
SurfaceResponseGrantToken
SurfaceEventId
SurfaceGoalId
```

Every event has one closed scope rather than an operation fence that is
meaningless for thread-wide facts:

```text
SurfaceScope =
  Thread
  | Operation(SurfaceOperationId)
  | Generation(SurfaceOperationFence)
  | Background(SurfaceBackgroundFence)
  | Goal {
      goal_id: SurfaceGoalId,
      causative_generation: Option<SurfaceOperationFence>,
    }
```

Candidate read types:

```text
SurfaceCursor { thread_id, incarnation, next_seq, source_revision }
CursorSourceRevision =
  Recorded { durable_revision }
  | Ephemeral { live_revision }
SurfaceSnapshot {
  cursor,
  thread,
  foreground_operation,
  background_operations,
  items,
  assistant_streams,
  tools,
  plan,
  usage,
  context,
  interactions,
  tasks,
  workflows,
  subagents,
  goal,
  settings,
  mcp_catalog,
  pinned_context,
  session_health,
}
SurfaceEventEnvelope { ordinal, event_id, commit_class, scope, event }
SurfaceCommitBatch {
  cursor_before,
  cursor_after,
  commit_class,
  event_count,
  batch_digest,
  events: NonEmpty<SurfaceEventEnvelope>,
}
SurfaceSubscriptionItem =
  Batch { batch: SurfaceCommitBatch }
  | Gap { required: SnapshotRequired }
  | Sealed { reason: ThreadClosed | HostShutdown }
SurfaceAttachment =
  Fresh { baseline, subscription: Stream<SurfaceSubscriptionItem>, capabilities }
  | Cursor { head, replay: Vec<SurfaceCommitBatch>,
             subscription: Stream<SurfaceSubscriptionItem>, capabilities }
OperationTerminalAtCursor { operation_id, terminal, cursor, commit_class, batch_digest }
SnapshotRequired { reason, retained_from, head }
```

The canonical terminal enum is exhaustive:

```text
OperationTerminal =
  NotAdmitted { reason }
  | Succeeded { usage }
  | Cancelled { reason }
  | BudgetExhausted { budget }
  | Failed { class, message }
  | Panicked { message }
  | JoinFailed { message }
  | AbortedByRuntimeRestart { last_generation }
  | Shutdown { reason }

OperationBudget =
  ModelTokens { limit?, observed? }
  | TurnRequests { scope: AgentLoop | Subagent, limit, observed }
  | GoalTokenBudget { goal_id, limit, observed }
  | WorkflowTokenBudget { workflow_id, limit, observed }
  | MonetaryBudgetUsdMicros { limit, observed }
```

`TerminalCommitFailure` and `FinalizingDegraded` are failure to establish a
terminal, not additional terminal variants. Phase 0A inventories every current
`RunStatus::BudgetExhausted` source into one of these variants before the old
collapsed status can feed a surface terminal; no generic budget string or
catch-all mapping is permitted.

The top-level `SurfaceEvent` enum is exhaustive:

```text
SurfaceEvent =
  Operation(OperationPatch)
  | Item(ItemPatch)
  | Assistant(AssistantPatch)
  | Tool(ToolPatch)
  | Plan(PlanSnapshot)
  | Usage(UsageSnapshot)
  | Context(ContextSnapshot)
  | Interaction(InteractionPatch)
  | Task(TaskPatch)
  | Workflow(WorkflowPatch)
  | Subagent(SubagentPatch)
  | Goal(GoalPatch)
  | Settings(SettingsPatch)
  | McpCatalog(McpCatalogPatch)
  | PinnedContext(PinnedContextPatch)
  | Session(SessionPatch)
```

Each nested patch is itself a closed enum. Phase 0A must freeze every nested
variant and an exhaustive source-to-patch matrix for every current runtime fact
in the separately reviewed two-file companion bundle before RED tests or
production implementation begin. Adding a new authoritative fact requires extending the
closed enum, reducer, snapshot, materializer, replay tests, and all relevant
client mappings in the same change.

The complete `SurfaceCommitBatch` is the only public linearization and delivery
unit. Every envelope in it carries the batch's byte-identical complete
`CommitClass`; ordinals and boundary cursors prove exact membership. The hub
preflights and reduces the whole batch, advances one immutable snapshot, retains
one complete batch, and fans it out atomically. A durable or live prefix is an
incomplete commit and must be repaired or reported as a replay hole; it is never
published as a smaller valid batch.

One complete batch is itself bounded: at most 1,024 events and at most 8 MiB in
the private v1 canonical batch encoding. Those limits are no larger than an
empty subscriber lane. `CommitBatchTooLarge` is a closed precommit failure: the
builder must detect it before a coordinator WAL prepare, source-store mutation,
receipt, cursor advance, or authoritative fact. Splittable stream/progress facts
are chunked into independent complete batches before that point; an indivisible
fact or terminal/finalizer payload must use the contract's bounded typed
representation. The runtime never publishes an oversized batch and never turns
one into an immediate subscriber gap.

The durability/scope matrix is fixed:

| Event family | Required scope | Durable source for recorded threads | Ephemeral-only members |
| --- | --- | --- | --- |
| Operation | Operation or Generation | operation ledger | none |
| Item | Generation or Background | explicit-identity conversation records | partial text before a completed item |
| Assistant | Generation or Background | completed model item | text/reasoning deltas |
| Tool | Generation or Background | requested/completed tool records | progress and output deltas |
| Plan/Usage/Context | Thread or Operation | session writer/coordinator receipt | transient progress only |
| Interaction | Operation or Background | broker ledger | none for recorded threads |
| Task/Workflow/Subagent | Operation, Generation, or Background | store receipt wrapped by coordinator | transient progress only |
| Goal | Goal with optional causative Generation fence | fenced Goal store receipt wrapped by coordinator | display-only progress explicitly marked transient |
| Settings/McpCatalog/PinnedContext | Thread | thread metadata/config receipt | transient server-health details only |
| Session | Thread | coordinator health/shutdown records | connection-local presentation is not a surface event |

Ephemeral-only members are retained for live replay within one incarnation but
are reconstructed from durable completed facts, not claimed as token-perfect
after process restart.

The 22-variant thread surface command vocabulary is exhaustive for this
release:

```text
SurfaceCommand =
  ReserveOperation
  | AdmitReserved
  | CancelOperation
  | CancelSessionCurrent
  | InterruptGeneration
  | PauseGoalOperation
  | ResumeOperation
  | SteerOperation
  | TransferBackground
  | RespondInteraction
  | ReconcileMutation
  | RetryStartCommit
  | RetryProjection
  | RetryFinalization
  | ManualCompact
  | Backtrack
  | TaskControl
  | WorkflowControl
  | GoalMutation
  | SettingsMutation
  | McpCatalogQuery
  | PinnedContextMutation
```

Thread close and host shutdown exist only on `RuntimeSurfaceHostHandle`; the
thread handle cannot create a second routing or receipt path for them.

`RespondInteraction` has one closed selector rather than adapter-specific
commands:

```text
InteractionSelector =
  Exact {
    interaction_id,
    kind,
    response_token,
    response_route_epoch,
    response_grant_token,
    operation_fence,
  }
  | OpaqueRequestId {
      attachment_id,
      opaque_request_id,
      expected_kind,
    }
```

The opaque selector exists only for released transports whose wire request id
does not carry the full fence. Runtime resolves it and validates the attachment,
thread, kind, response token, and operation/generation fence atomically before
committing a response. No adapter may resolve the id to a semantic waiter first.

Attach, cursor attach, detach, snapshot-required recovery, and terminal waiting
are handle operations with closed results; they cannot mutate execution state
except for attachment capabilities.

Unknown or malformed input fails closed at the private ingress. It cannot
partially mutate the snapshot and cannot silently disappear when it represents
an authoritative runtime fact.

Command results use one exact closed algebra, defined normatively only in the
Phase 0A private companion. `RuntimeSurfaceMutationResult` is
`Committed | Deferred | Uncommitted`; `MutationReply<T>` binds a command's closed
value to that result. `Committed` contains the exact ordered requirement/witness
set. The requirement/witness sum includes thread batch-head cursors, remote-owner
acks, typed host receipts, Goal-store receipts, operation-terminal receipts, and
policy-revocation barriers. A local cursor witness names an event member but its
cursor is always the containing complete batch's `cursor_after`.

`Deferred` contains the unique proved acknowledgement subsequence, its exact
nonempty missing complement, and one `DeferredRepair` closed variant. The repair
variant binds state and token together for mutation, local projection,
remote-owner acknowledgement, start commit, finalization, memory pin, policy
revocation, or close/shutdown barrier repair. There is no separately selectable
`state + retry` pair. A repair reuses the original semantic request, target, and
commit identities and may only establish missing witnesses; it cannot enable a
new side effect.

`Uncommitted` is one of four disjoint, exhaustive error classes: invalid, stale,
unavailable, or commit-failed. It carries no cursor, receipt, or durability
claim. `CommitBatchTooLarge` is in the invalid precommit set. A later operation
terminal has its own receipt. Fire-and-forget control commands are not permitted
for operation, interaction, Goal, task, workflow, memory, settings, close, or
shutdown mutations.

Host and thread handles both return this algebra. Host receipts use their owning
typed host revision/target/receipt digest rather than a fabricated thread cursor;
cross-process interaction responses use the exact durable owner acknowledgement
rather than another process's live cursor. The private command matrices and
immutable policy/shutdown plans are the sole exact definitions of required
acknowledgements, legal deferred values, repair equality, and uncommitted codes;
this parent intentionally does not duplicate a second candidate schema.

### Contract-freeze gate

This approved parent design fixes ownership and failure semantics and authorizes
completion of Phase 0A only; candidate type names are not sufficient
implementation input. Before writing Phase 0B RED tests, Phase 0A must freeze,
commit, and obtain explicit written review of one two-file companion bundle:

- `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.md`;
- `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`.

The Markdown and machine-readable manifest are one artifact. Review names the
exact commit plus SHA-256 of both files; changing either file invalidates that
review and reruns all Phase 0A consistency checks. Together they contain:

- every nested `SurfaceEvent` variant and field, required scope, durability
  class, authoritative source, reducer transition, snapshot destination,
  materialization rule, and TUI/ACP/JSONL mapping;
- every `SurfaceCommand` and `SurfaceHostCommand` payload, caller capability,
  required thread-owner/operation/generation/interaction fence, legal source
  state, idempotency identity, mutation target, result disposition/error, and
  emitted fact/receipt;
- the exact closed snapshot/read/page/attach/wait result DTOs and every
  invalid, stale, degraded, gap, and retry outcome;
- reviewed machine-readable source/action inventories and test vectors from
  which Phase 0B builds exhaustive-match and inventory tests for every current
  runtime fact, production TUI action, and mutation-capable slash/callback
  entrypoint.

The artifact may refine candidate names but may not weaken this design's sole
authority, durability, ordering, recovery, or deletion invariants. Any open
payload, `serde_json::Value`, wildcard match, or "implementation decides"
entry fails Phase 0A. The user's approval of this parent design does not waive
the frozen-bundle review: Phase 0B RED evidence starts only after explicit
review of that exact two-file artifact, and no runtime, TUI, ACP, or JSONL
production cutover may begin earlier.

## Canonical Operation Contract

### One identity

Runtime allocates one globally unique `SurfaceOperationId` at request
reservation. It is not a per-actor counter and is never reused after restart.
The same id is used by persistence, admission, execution, generation fences,
cancel, recovery, background transfer, terminal, cursor events, and waiters.

The current actor-local `OperationId` may temporarily exist behind an internal
compatibility adapter, but it may not be exposed as a second client identity or
used to publish a second lifecycle.

### Two-layer lifecycle

An operation can contain multiple execution generations. The operation and
generation state machines are distinct:

```text
Operation:
  Requested -> Admitted -> Terminal
  Requested ------------> Terminal(NotAdmitted)

Generation, repeated zero or more times inside one admitted operation:
  Reserved -----> Started ------> Stopped
      |              |
      |              +----------> Transferred -> Stopped
      +-------------------------> Stopped(NotStarted { reason })
```

The diagram is a closed transition table, not an illustration. The only legal
operation transitions are `Requested -> Admitted`, `Requested ->
Terminal(NotAdmitted)`, `Admitted -> Suspended`, `Admitted -> Finalizing`,
`Suspended -> Admitted` only when the resumed generation's `Started` commit is
established, `Suspended -> Finalizing` through cancel/close/shutdown, an
irrecoverable resumed-generation Started commit, or non-replayable recovery
abort, `Finalizing -> Terminal`, `Finalizing ->
FinalizingDegraded`, and `FinalizingDegraded -> Terminal` only through
`RetryFinalization`; `Terminal` is absorbing. The only legal generation
transitions are `Reserved -> Started`, `Reserved -> Stopped(NotStarted)`,
`Started -> Stopped`, `Started -> Transferred`, `Transferred -> Stopped`, and
no transition out of `Stopped`. Every omitted source/target pair is
`IllegalTransition`, including repeated stop/transfer, `Started -> Reserved`,
`Stopped -> Started`, and any generation patch after its operation is
`Terminal`.

The reducer also enforces these cross-field invariants:

- an operation and every nested fence share one `operation_id`, thread id, and
  thread owner epoch; the first admitted generation is id `0`, has phase
  `Reserved`, no `started_witness`, and no `stop_reason`; later generations are
  contiguous, have a stopped predecessor in the same operation, and retain the
  same logical turn/input identities unless the transition is a Goal outer-turn
  admission explicitly carrying the new identities;
- `OperationPhase::Requested` has no logical turn, generation, terminal, or
  terminalizing intent; `Admitted` has a foreground generation or a committed
  suspension; `Suspended` names the exact stopped generation and its cause;
  `Finalizing`/`FinalizingDegraded` have a fixed finalize intent; and
  `Terminal` has exactly one terminal record and no live reservation, generation,
  interaction, or pending control;
- `GenerationStarted` is valid only for a matching `Reserved` generation and
  freezes that generation's settings revision, policy epoch, replayability,
  required capabilities, and capability fingerprint; generation zero also
  matches the Requested admission receipt. `AgentLoopTurnStarted` uses a matching Started
  generation and the next ordinal; `GenerationTransferred` carries the same
  fence as the resulting background owner;
- `Suspended`/`RecoveryRequired` require a preceding `Stopped` generation with
  `InterruptedResumable` or `RuntimeRestart` (or the explicitly typed provider
  suspension), and `ResumeOperation` must carry that exact stopped fence plus a
  durable replay capsule or current-incarnation live-resume capability;
- the terminalizer is the only code allowed to map a stopped generation to an
  operation terminal: successful completion maps to `Succeeded`, verification
  failure to `Failed(Verification)`, budget exhaustion to the corresponding
  closed `BudgetExhausted` variant, cancellation/pause/shutdown to the exact
  `Cancelled`/`Shutdown` cause, and runtime/persistence/join/panic failures to
  their matching terminal class. `NotAdmitted` is legal only before the first
  generation is Started; an admitted-but-Reserved generation has performed no
  external side effect and may still settle as `NotAdmitted`.
- cancelling a `Requested` operation is the sole control path that directly
  invokes the reservation finalizer. It atomically commits
  `Terminal(NotAdmitted(CancelledBeforeAdmission))` and returns that terminal
  receipt; it never creates a pending terminalization intent.
- an operation-level join settlement failure is a closed finalizer source and
  maps exactly to `Terminal(JoinFailed)`; no adapter may synthesize that outcome.

`Stopped` ends one execution attempt; it does not terminalize the operation. A
resume reserves a fresh monotonically increasing generation under the same
operation. Waiting for interaction, cancellation requested, resume queued,
backgrounded, recovery required, and finalizing are typed runtime states
attached to the appropriate operation or generation. They are not client-owned
phases or alternative terminal rails.

For recorded threads:

- `Requested` is appended before a client-visible request acknowledgement.
- `Admitted` fixes operation, logical turn, and initial generation identity in
  one thread-ledger commit before executor enqueue. An input-bearing User/Goal/
  workflow-followup admission preallocates its item id and appends the pending
  explicit-identity user item in that batch. Manual compaction, backtrack, and
  standalone workflow admission are typed `NotApplicable` and append neither an
  input-item identity nor an Item patch.
- every `Generation::Started` is durably synced immediately before that
  generation's first model, provider, hook, tool, workflow, continuation
  consumption, or other externally visible side effect;
- only the runtime operation finalizer may append `Terminal`, including
  `Terminal(NotAdmitted)`.

Admission may write only declarative, idempotent identity and policy metadata.
Session-start hooks, provider continuation consumption, Goal-turn execution,
task execution state, and tool/workflow launch happen only after the generation
Started barrier.

History-disabled or ephemeral threads use the same in-process state machine but
do not gain a false crash-durability claim. Their executable prompt content is
never persisted merely to enable replay.

Each append carries stable `(operation_id, generation_id?, transition_kind,
commit_id)` data so retry can reconcile an ambiguous write without duplicating
a transition. In the private envelope, `commit_id` is the `commit_id` nested in
`CommitClass`, and `transition_kind` is the closed `SurfaceEvent`/patch
discriminant. A reducer's duplicate key is therefore
`(event_id, commit_class.commit_id)`; an event cannot be reclassified under a
different operation, generation, or transition kind during retry.

### Goal composite operations

A Goal run is one composite operation, not one operation per outer turn and not
an untracked loop inside a generation:

```text
GoalOperationIdentity {
  goal_id,
  goal_run_id,
  operation_id,
}
GoalGenerationIdentity {
  operation_fence,
  goal_outer_turn_id,
  logical_turn_id,
  canonical_input_item_id,
  outer_turn_origin: User | Resume | Continuation | WorkflowNotification,
  attempt: Initial | RecoveryReplacement,
  predecessor_generation?,
  objective_revision,
  outer_turn_count,
}
```

The complete `GoalGenerationIdentity` is stored as one typed value on both the
Goal continuation admission and the matching generation record. The duplicated
operation fence, logical turn, canonical input, objective revision, released
outer-turn count (first started turn is one), and predecessor fence must compare equal. A predecessor
fence can authorize at most one identity; replay of the same identity is
`AlreadyApplied`, while any field mismatch is a stale-identity rejection.

`GoalSet`, an admitted user-authored turn while a Goal is active, attached
`GoalResume`, host `ResumeLatestActiveGoal`, and an admitted durable workflow
notification each allocate a fresh `GoalRunId` and `SurfaceOperationId` in the
same coordinator intent that creates/activates the Goal run. Their first outer
turn records `User`, `Resume`, or `WorkflowNotification` respectively; the
workflow form also carries the durable workflow/result identity used to make
its operation admission idempotent. They never reopen an older Goal run or
operation. Only runtime-owned automatic continuation uses `Continuation` and
retains the existing Goal run and operation.
The first admitted outer turn is that operation's initial generation. Every
later admitted outer turn begins with a fresh monotonically increasing
generation under the same operation and Goal run, with its own outer-turn,
logical-turn, and canonical-input identities. An explicit generic recovery may
allocate a replacement generation for the same outer-turn/turn/input identities;
it is marked `RecoveryReplacement` and is not counted as a new Goal
continuation.

A continuation input is a typed runtime fact containing Goal/objective revision,
prior generation/verification receipt, and released outer-turn count. It is not a TUI
prompt, ACP prompt, synthetic user message, or client-owned XML/string. Runtime
may render provider prompt text from that fact only after the new generation's
Started barrier. A concurrent Goal edit affects only a continuation whose
admission records the edited objective revision.

After each Goal generation joins, runtime performs this actor-owned sequence:

1. reconcile the generation result, usage, verifier, Goal-store, and coordinator
   receipts without publishing a standalone Stopped fact;
2. evaluate one closed `GoalContinuationDecision` against that exact predecessor
   result, Goal state/revision/budget, queued user input, pending interactions,
   workflow ownership, plan mode, terminalizing control intent, and predecessor
   generation fence;
3. on `Admit`, atomically commit one batch containing `Generation::Stopped`, the
   exact outer-turn/usage/verifier settlement, `GoalContinuationAdmitted`, and
   the successor `Generation::Reserved` with its new outer-turn/turn/input
   identities, objective revision, and predecessor fence. The operation remains
   admitted and does not enter its finalizer;
4. commit the successor Started barrier, then and only then resolve its typed
   continuation input and launch it;
5. on `Stop`, atomically commit `Generation::Stopped`, the exact outer-turn
   settlement and Goal status/reason, plus the one `FinalizationStarted` fact.
   No crash-recoverable state contains a stopped current generation without
   either a reserved successor or a finalization disposition.

Automatic `Admit` requires a successfully completed predecessor result, an Active Goal
whose outer-turn settlement says continuation is ready, no terminalizing intent,
no queued user input, no unresolved interaction or workflow owner, and a mode
that permits continuation. A duplicate predecessor fence is `AlreadyApplied`.
There is no fixed outer-turn ceiling; Goal token budget, explicit pause/cancel,
completion/verification, user input, workflow/interaction ownership, and runtime
failure are the stopping authorities.

The `Stop` branch's stop-to-terminal mapping is closed: a completed or deliberately yielded
successful outer turn finalizes `Succeeded` while its Goal patch records
Completed/Paused and the exact reason; Goal token exhaustion finalizes
`BudgetExhausted(GoalTokenBudget)`; cancel/Goal pause finalizes `Cancelled`; an
unsuccessful generation or verification/runtime invariant failure uses its
exact failure terminal. A later `/goal resume` always creates a new Goal run and
operation. Generic `ResumeOperation` is used only for an explicitly visible
Suspended/`RecoveryRequired` operation, including a crash-recovered Goal
generation, and retains that operation's already committed identities.

Restart never automatically admits a new Goal continuation. A crash after a
Stopped predecessor but before the successor reservation completes the old
operation finalizer from durable settlements; a later Goal resume creates a new
run/operation. A crash after a successor is Reserved follows the ordinary
NotStarted recovery row and requires explicit `ResumeOperation`; a Started Goal
generation is never replayed.

### Reservation and pre-admission control

The surface start API is two-stage even when an adapter presents one high-level
prompt call:

| Command | Resident state before | Durable/surface result | Next action |
| --- | --- | --- | --- |
| `ReserveOperation(intent)` | bounded reservation capacity available | operation id, admission lease, and Requested receipt; no user item | adapter may request admission or cancel by id |
| `AdmitReserved(id)` | matching Requested with a valid lease | mark the reservation ready; return queued or Admitted plus initial Generation::Reserved receipt | actor admits the oldest ready reservation when the foreground slot and admission gates are available |
| `CancelOperation(id)` | Requested, Admitted, Reserved, Started, or Suspended operation | Requested returns the committed `Terminal(NotAdmitted(CancelledBeforeAdmission))`; later phases return an operation-cancel acceptance receipt | Requested finalizes synchronously with no control intent; otherwise stop any generation and terminalize asynchronously |
| `InterruptGeneration(fence)` | exact Reserved or Started generation | generation-interrupt acceptance receipt | stop before start or join to Stopped(InterruptedResumable); retain a visible Suspended operation and foreground slot |
| `PauseGoalOperation(goal_id, goal_revision)` | matching active or idle Goal | pause acceptance receipt | atomically persist Goal pause; if a Goal operation is active, resolve it inside runtime, stop its generation, and terminalize it; later Goal resume creates a new operation |
| `ResumeOperation(id)` | Suspended operation or recovery-required admitted operation | fresh Generation::Reserved receipt | commit Started before spawning the new generation |
| `TransferBackground(target)` | matching Requested/Reserved operation or active foreground generation | committed background-on-start intent or durable background-owner receipt | defer transfer until admission/Started when needed; release foreground only after the handoff barrier |
| `RetryStartCommit(token)` | `StartCommitDegraded` with matching owner epoch, generation fence, and Started commit id | Started cursor, terminal path receipt, or explicit still-degraded result | probe/retry only the same Started transition; never allocate another generation |
| `RetryProjection(token)` | ProjectionDegraded with matching commit id | projected cursor/remote ack or explicit retry failure | unblock only the effects authorized by the repaired fact |
| `RetryFinalization(token)` | FinalizingDegraded | terminal commit or explicit retry failure | admit new work only after terminal barrier succeeds |

The background target is closed and preserves the pre-install race identity:

```text
BackgroundTarget =
  ReservedOperation { operation_id, admission_lease }
  | ActiveGeneration { operation_fence }
```

`RuntimeSurfaceHandle::start` may compose reserve and admit, but it must expose
the reservation identity before waiting for admission. The actor can therefore
process a cancel between reservation and admission without an unkeyed
"cancel-next" flag.

Cancel, interrupt, and pause are not aliases:

| State | `CancelOperation` | `InterruptGeneration` | `ResumeOperation` |
| --- | --- | --- | --- |
| Requested | Terminal(NotAdmitted(CancelledBeforeAdmission)) | reject: no generation | reject |
| Admitted + Reserved, not Started | Stopped(NotStarted(Cancelled)) then Terminal(Cancelled) | Stopped(NotStarted(Interrupted)) and Suspended | reserve a fresh generation only when recovery-required |
| Started | signal/join Stopped(Cancelled), then Terminal(Cancelled) | signal/join Stopped(InterruptedResumable), remain Suspended | reject until Stopped is committed |
| Suspended after Stopped | Terminal(Cancelled) | AlreadyApplied | reserve the next generation and leave Suspended only after Started commits |
| Background-owned | signal the exact background owner and terminalize after settlement | reject unless a background-specific control is explicitly supported | reject |
| Terminal/FinalizingDegraded | AlreadyApplied or finalization receipt | stale/reject | reject or `RetryFinalization`, never resume execution |

The actor also persists one control intent while an asynchronous stop/join is in
flight:

```text
TerminalizationCause =
  UserCancel
  | GoalPause
  | HostShutdown
  | ThreadClose

PendingControlIntent =
  Interrupt { generation_fence }
  | Terminalize { cause: TerminalizationCause, operation_id }
  | ResumeStarting { generation_fence }
  | ResumeAfterInterruptedStop { generation_fence }
  | BackgroundOnStart { operation_id, reservation_sequence }
```

Control races use these deterministic rules:

- the first committed `CancelOperation` or `PauseGoalOperation` fixes the
  operation's terminalizing cause; duplicates are `AlreadyApplied`, while a
  different later terminalizing request is rejected with the winning intent;
- a pending Interrupt may be upgraded by a later exact Cancel or Goal pause.
  The upgrade is committed before the join result and changes the post-join
  action from Suspended to finalization. Interrupt, resume, steer, and transfer
  cannot downgrade a terminalizing intent;
- `ResumeStarting` owns exactly one new Reserved generation. A repeated resume
  is `AlreadyApplied`; cancel or interrupt targets that Reserved generation by
  the ordinary state table, so no second generation can be allocated;
- `TransferBackground` accepts either the reserved operation id plus admission
  lease before a generation exists, or the exact generation fence afterward.
  The former commits `BackgroundOnStart` and stays queued; it does not claim a
  background owner or release readiness. When that reservation becomes eligible,
  the actor temporarily acquires the foreground slot, registers a provisional
  owner, commits Started and then Transferred without launching side effects,
  promotes the background owner, releases the slot, and only then launches the
  generation under that owner. Admission rejection or lease expiry produces the
  ordinary `NotAdmitted` terminal instead. A duplicate intent is
  `AlreadyApplied`, and cancel/shutdown may supersede it before launch;
- background transfer is serialized as one actor transition through its commit
  barrier. Mailbox commands are observed after it either commits or fails: an
  exact cancel then targets Background-owned on success or the still-foreground
  generation on failure;
- host shutdown closes admission immediately. It finishes an already committed
  operation terminalizing cause rather than rewriting it, then shutdown-cancels
  every operation without one;
- `CancelSessionCurrent` exists only for a released legacy wire that carries no
  operation id. It resolves the current operation once inside the actor mailbox,
  becomes an exact `CancelOperation`, and is never reevaluated after a newer
  foreground operation appears. TUI and extension-aware clients must use the
  operation id from their reservation receipt or attached snapshot instead;

A Suspended operation remains a durable, visible foreground owner until an
explicit resume, cancel, close, or host shutdown. Generation join may complete
before the resume command; the actor retains the Suspended operation and does
not race to Terminal. Released server `turn/interrupt` is resolved by
`ControlJsonlTurn` and then commits `InterruptGeneration`; user intent that
means stop the whole operation maps to `CancelOperation`.

Requested reservations live in a bounded FIFO separate from the foreground
slot. Each has a stable reservation sequence, lease, and `ready_for_admission`
bit. `AdmitReserved` is an actor-serialized compare-and-admit: it validates the
lease and identity; an admission-blocking degraded/shutdown gate returns before
changing the bit. Otherwise it marks the reservation ready and admits only the
oldest ready reservation when the foreground slot is empty. An earlier
reservation that has not requested admission does not block a later ready one.

When the slot is busy, the command returns a committed `Queued` admission
outcome and retains the Requested operation; the actor automatically admits it
when it becomes the oldest eligible ready reservation. A configuration/policy
conflict terminalizes it as `NotAdmitted`, while an admission-blocking degraded
state returns `Unavailable` without changing its ready bit. Concurrent admit,
cancel, and lease expiry are ordered in the mailbox: the first committed
transition wins and every later command observes it.

The default reservation lease is 30 seconds on the runtime monotonic clock and
is returned in the receipt. Expiry is actor-owned: the reservation finalizer
records `Generation::Stopped(NotStarted(ReservationExpired))` when applicable
and `Terminal(NotAdmitted(ReservationExpired))`. Detach/EOF does not itself
cancel the reservation. Tests use an injected clock; no wall-clock sleep is
required.

### Restart recovery

Recovery never silently restarts external work:

| Last durable fact | Runtime action after restart | Execution rule |
| --- | --- | --- |
| `Requested` without `Admitted` | append `Terminal(NotAdmitted(RuntimeRestart))` through the operation finalizer | create no user item and perform no model/tool I/O |
| replayable `Admitted` whose latest generation is Reserved but not Started | append `Generation::Stopped(NotStarted(RuntimeRestart))` and expose `RecoveryRequired` for the same operation/turn/input identities | require explicit `ResumeOperation` to reserve a fresh generation before committing Started |
| non-replayable `Admitted` whose latest generation never Started | append `Generation::Stopped(NotStarted(RuntimeRestart))`, then `Terminal(AbortedByRuntimeRestart)` | never reconstruct or execute redacted/missing request content |
| Suspended with a replayable/current-live resume replacement Reserved but not Started | append `Stopped(NotStarted(RuntimeRestart))`, rebase the suspension witness to that replacement, and clear ResumeStarting | require another explicit ResumeOperation; never continue the stopped replacement |
| Suspended with a stale/unavailable non-replayable resume replacement Reserved but not Started | append `Stopped(NotStarted(RuntimeRestart))`, then enter the Suspended recovery-abort finalizer | complete `AbortedByRuntimeRestart`; never append Terminal directly from Suspended |
| latest durable generation is `Started` with no matching `Stopped` or `Transferred` and operation has no Terminal | append `Generation::Stopped(RuntimeRestart)` and `Terminal(AbortedByRuntimeRestart)` after recovery settlement | never automatically replay model, tool, provider, hook, or workflow work |
| latest generation is `Stopped(InterruptedResumable)` or `Stopped(NotStarted(RuntimeRestart))`, operation has no finalization intent, and its request capsule is replayable | expose a durable Suspended/`RecoveryRequired` operation | require explicit `ResumeOperation` to reserve a fresh generation; never continue the stopped generation |
| latest generation is resumably Stopped and its non-replayable live capsule is stale/unavailable | enter the Suspended recovery-abort finalizer | never reconstruct redacted/missing request content or append Terminal directly from Suspended |
| latest generation is non-resumably Stopped or a durable finalization intent exists, but Terminal is absent | reconcile idempotent settlement receipts and complete that finalizer | never reopen the operation or rerun execution |
| latest generation is `Transferred` without Terminal | record loss of the in-process background owner, append `Stopped(RuntimeRestart)`, and complete `Terminal(AbortedByRuntimeRestart)` | never assume a host-owned background task survived process restart; external durable workers require a separate ownership contract |
| `Terminal` | retain the committed terminal | never append a second terminal |

Materialization is pure replay and never reclassifies a historical stop against
the new process incarnation. The table's capsule-dependent actions run only
after a complete snapshot is rebuilt and append new fenced recovery batches.
Every newly handled stop is committed atomically with its selected Suspended,
suspension-rebase, or FinalizationStarted disposition.

Pending interaction state may be rendered after restart. Resolving it does not
implicitly restart execution; a separately admitted resume command is required
when execution must continue.

Requested carries only the generation-zero admission replayability receipt.
Each `GenerationRecord` carries the sole executable capsule for its own input,
configuration revision, cwd, workspace roots, permission/policy revision, tool
schema digest, required capabilities, and capability fingerprint. Generation
zero equals the Requested receipt; a Goal continuation freezes a new capsule;
a recovery replacement copies its stopped predecessor exactly. History
redaction or disabled history makes that generation capsule non-replayable
rather than persisting forbidden input.

A persisted Allow receipt is valid in a recovered generation only when its
operation, request/tool digest, cwd, roots, policy revision, executable/artifact
generation, and capability fingerprint all match. Any mismatch creates a fresh
interaction requiring renewed approval. Broker reload also requires an
`expected_thread_id`; a cross-thread or corrupted record fails closed before it
can be projected or executed.

### Background handoff

Backgrounding or provider suspension is not an operation terminal. The
`RuntimeHost` supervisor owns a bounded background registry keyed by thread,
operation, and generation. It retains the cancellation authority, joined task,
completion waiter, durable owner token, and surface publisher scope.

Background handoff has its own barrier: register a provisional owner, append and
sync Transferred, commit that exact fact through the general projection barrier,
promote the owner, and only then release the foreground thread slot. Failure
before the durable transfer removes the provisional entry and leaves the
generation foreground. Failure after the durable transfer cannot roll history
back: runtime retains the foreground slot and provisional task ownership while
it retries/rematerializes projection. If repair cannot succeed, it cancels and
joins the provisional task, appends Stopped(ProjectionFailure), and enters the
one operation finalizer; no newer foreground work is admitted.

When background work settles after a successful handoff, the host sends a
fenced background-settlement command to the owning thread's commit coordinator.
It uses the same settlement, Terminal, and `SurfaceHub::commit_batch` barrier but not
the foreground-slot mutation. Only that later barrier publishes Terminal.

Clients may detach from or hide a background operation. They may not synthesize
a successful foreground terminal to make the UI idle.

## Thread-Scoped Commit Protocol

The contract does not claim that separate JSONL, SQLite, task, workflow, and
Goal stores can be changed by one filesystem transaction. Instead, every
recorded thread has one `RuntimeCommitCoordinator` that provides a write-ahead,
idempotent commit protocol.

The coordinator owns:

- a process-lifetime `ThreadOwnershipLease` plus monotonically increasing
  durable owner epoch for actor/execution authority;
- a separate short-held logical-session commit lock shared by
  plaintext/compressed and active/archive paths;
- a monotonically increasing durable revision and versioned record schema;
- append-batch, short-write rollback, `sync_data`, commit-id probe, and CAS on
  expected durable revision;
- operation/generation records and surface-relevant domain receipts;
- finalize intents identifying every required external-store settlement;
- checkpoint revision and recovery of incomplete commit batches.

Every execution-bearing command, generation fence, Started record, publisher
permit, finalization intent, and terminal commit carries the current thread owner
epoch. The actor must still own the process-lifetime lease when it commits
`Generation::Started`; only after that synced check may its generation perform a
side effect. Lease loss closes admission, revokes publisher permits, signals
owned work, and enters fenced shutdown/finalization. A second process cannot run
the restart matrix until it has acquired the lease and advanced the epoch.

An external process may submit a capability-valid interaction response without
owning the thread execution lease; that narrow exception is governed by the
broker source-head CAS and response-route epoch. It grants no admission,
execution, projection, or terminal authority.

Input-bearing admission writes the operation, turn, explicit input-item
identity, pending user item, and initial generation reservation in one
coordinator batch. A typed NotApplicable maintenance/standalone-workflow
admission writes the operation, turn, and generation without an Item. A stale metadata
rewrite cannot replace records appended after its expected revision. Unknown
new records and unmigrated legacy fields are round-tripped opaquely until their
explicit migration commits; an unrelated task/history rewrite may not erase a
legacy continuation.

Cross-store finalization uses intent and receipts:

1. append `FinalizeIntent(operation_id, expected_revisions, settlement_ids)`;
2. apply each task/workflow/Goal/usage settlement with its stable idempotency
   key and record the returned store receipt;
3. append a coordinator settlement record for each receipt;
4. after every required receipt is present, append and sync Terminal;
5. mark the finalize intent complete in the same terminal batch.

After a crash between domain settlement and Terminal, recovery never reruns the
model or tools. It probes settlement ids, reapplies only missing idempotent
settlements, then completes the terminal barrier. A non-idempotent or ambiguous
settlement leaves the thread `FinalizingDegraded` for explicit repair.

`CursorSourceRevision::Recorded.durable_revision` is the coordinator revision
whose facts the immutable snapshot contains. A committed fact from another store
enters the surface only with its external receipt wrapped by a coordinator
commit. This is the thread-wide linearization boundary; it is not inferred from
unrelated store timestamps or existing reserved event sequence numbers.

For an ephemeral thread, the same API returns
`CommitClass::Ephemeral { incarnation }` and performs an atomic in-memory
commit. Its cursor advances an incarnation-local `live_revision`; it never
returns a durability receipt or claims restart recovery.

Every authoritative fact, not only Terminal, uses one commit/projection barrier:

1. preflight the closed reducer against the current immutable snapshot and
   validate the publisher permit, expected owner epoch, and source revisions;
2. create a stable commit id. For coordinator-local facts, append and sync the
   fact. For Goal/task/workflow/settings or another external store, first append
   `MutationIntent { request_id, kind, expected_revisions, settlement_id,
   projected_fact }`, apply the external mutation with that idempotency key,
   record/probe its receipt, then append and sync the coordinator-wrapped fact;
3. call `SurfaceHub::commit_batch` for that exact commit id and complete batch.
   Retry an expected-head
   advance from the new immutable snapshot without repeating persistence;
4. on a non-retryable projection failure, seal the hub, rematerialize a fresh
   incarnation through the durable revision containing the fact, and verify it;
5. only after local cursor commit or a remote-owner projection acknowledgement
   may runtime return `Committed` or cross an execution-enabling barrier.

Crash recovery probes every incomplete mutation intent and reapplies only a
missing idempotent settlement. An ambiguous or non-idempotent external result
enters `MutationDegraded`; it is never guessed from before/after rereads. The
finalization intent described below is the multi-settlement specialization of
this same protocol, not a separate reliability model.

`ReconcileMutation` accepts only the original request/settlement token. It probes
the external store's stable idempotency key, records a proved receipt or an
explicit irrecoverable result, and then resumes the coordinator/projection
barrier. It never issues a semantically new mutation.

Repeated non-retryable projection failure enters
`ProjectionDegraded { durable_commit_id, fact_kind }`, seals affected
attachments, and blocks admission. `RetryProjection` rematerializes and proves
the fact by commit id. An execution-enabling fact such as Admitted, Started,
Resume, or Transferred cannot launch work or release a slot before that repair.
A safety-reducing fact such as cancel, pause, shutdown, or interaction close
still signals/wakes existing work after its durable commit, but its caller gets
`Deferred` until projection is repaired; it can never enable a new side effect.

If the Started append itself cannot be established, no executor or hook starts.
The actor enters `StartCommitDegraded` while retaining the Reserved generation.
`RetryStartCommit` accepts only the original owner epoch, generation fence, and
stable Started commit id. It first probes that id: a present record proceeds
through the ordinary projection barrier and only then launches the executor; an
absent retryable record is appended again under the same id; an explicit
irrecoverable result records `Stopped(NotStarted(StartCommitFailure))` and enters
the operation finalizer. Cancel or shutdown may choose the terminal path, but it
must probe the same id before deciding whether the generation was Started. An
ambiguous probe remains `Deferred { state: StartCommitDegraded, retry:
RetryStartCommit }`. If even the stopped/terminal path cannot be persisted, the
operation remains admission-blocking and returns the non-fabricated
`Deferred`/commit failure result. No repair allocates a second generation or
launches a side effect before the Started cursor barrier.

For a recorded cross-process interaction response, the owner actor projects the
resolved fact and writes an internal `ProjectionAck(response_commit_id,
thread_owner_epoch, durable_revision)` under the coordinator lock. The responder
waits a bounded interval for that ack and then returns
`ThreadRemoteOwnerAck`; timeout returns
`Deferred { state: OwnerAckPending, retry: RetryProjection }` without
misclassifying a live but slow owner as projection failure. If the owner crashes
after projection but before ack, its successor rematerializes the durable
response and idempotently writes the ack. No cross-process path manufactures
another process's live cursor.

The resident actor control state, commit coordinator handle, broker,
`SurfaceHub`, and finalization state never move into a generation task. The task
receives only its execution capsule and generation-scoped publisher permit. If
the task panics and loses its movable `RuntimeThread` execution state, the actor
still owns the durable control plane and enters `FinalizingDegraded`; it may
reconstruct or close from durable state but cannot admit another generation by
pretending the lost state returned.

## Finalization And Failure Semantics

The operation finalizer is driven by the actor/host completion branch, never by
an inline command handler. It has three fenced entry modes:

- `ReservationFinalizer` owns a matching Requested FIFO entry, has no generation
  or foreground slot, and may produce only `Terminal(NotAdmitted)`;
- `ForegroundFinalizer` may change the foreground slot only when its operation
  and generation still match that slot;
- `BackgroundSettlementFinalizer` removes only its matching host-registry entry
  and cannot restore, clear, or overwrite a newer foreground operation.

All modes share one terminal barrier; steps that require a generation are
vacuous for `ReservationFinalizer`:

1. validate and remove the exact Requested reservation, or obtain the completed
   generation result and join every operation-owned child, or durably transfer
   explicitly supported background ownership;
2. append the finalize intent, close unstarted foreground interactions, and
   reconcile broker/source heads;
3. finish writer persistence and idempotently settle task/workflow/Goal and
   usage receipts;
4. compute the one typed terminal outcome;
5. append and durably sync the terminal record;
6. call one `SurfaceHub::commit_batch` linearization operation whose complete
   batch contains Terminal and any paired settlement/session facts, advances to
   the next batch-boundary cursor, reduces the immutable snapshot, appends one
   complete retained batch, enqueues that batch or a sticky gap to every
   subscriber, and returns the terminal batch cursor;
7. apply only the entry mode's fenced reservation/foreground/background effect
   and recompute admission;
8. wake operation waiters with `OperationTerminalAtCursor`.

Cancellation and Goal pause first perform the interaction-close/wake sequence
described earlier, then join and enter this finalizer.

There is no separate post-commit terminal broadcast. Subscriber delivery is
part of `SurfaceHub::commit_batch`, so a newly admitted operation cannot publish past
the prior terminal before that terminal is ordered in every still-valid lane.

An expected-head advance is a normal optimistic conflict, not projection
failure. The finalizer reloads the current immutable snapshot and retries the
same terminal fact while its permit remains valid; it does not append another
Terminal. If the durable Terminal still cannot be projected because of a
reducer fault, sealed incarnation, invalid permit, or other non-retryable
projection failure, runtime seals the old hub, rematerializes a new incarnation
from the coordinator revision containing Terminal, and verifies that the new
immutable snapshot contains that exact terminal. Existing subscribers receive
sticky `SnapshotRequired`. Only successful rematerialization produces the
terminal cursor and wakes the waiter. Repeated failure enters
`ProjectionDegraded` within the admission-blocking `FinalizingDegraded` state
until explicit repair succeeds.

If a background settlement degrades while a newer foreground operation is
already running, that foreground generation is not overwritten or implicitly
cancelled. Runtime records `BackgroundFinalizingDegraded`, blocks further
admission, lets the current generation reach its next safe completion point,
and then requires both finalizers to establish their terminal/projection
barriers before the thread becomes idle.

Subscriber delivery failure after a durable append cannot turn a completed
operation into a runtime failure. The subscriber recovers through replay or a
fresh snapshot.

Terminal persistence failure is different. It leaves the thread in a sticky
`FinalizingDegraded` state, returns `TerminalCommitFailure` to the waiter, and
blocks new admission until retry or explicit repair establishes the terminal
barrier. Runtime must not fabricate a success, cancellation, or generic failure
terminal that was not durably committed.

## Semantic Ingress And Projection

One private ingress converts runtime domain facts into closed surface events.
It accepts typed runtime values, not serialized JSON. Materialization, durable
replay, retained live replay, and live publication use the same pure reducer.

The ingress must cover or explicitly replace:

- items, assistant/reasoning stream state, and completed model responses;
- tools, file changes, plans, usage, and context;
- interactions and permission profile changes;
- tasks, workflows, subagents, and background work;
- Goal state and post-commit Goal mutation changes;
- model, reasoning, roots, settings, and pinned context;
- operation lifecycle, terminal, faults, and shutdown.

Direct snapshot field assignment and client parsing of `EventEnvelope.payload`
are forbidden in production surface paths.

Legacy `EventEnvelope` and JSONL remain compatibility projections produced at
the external boundary. They are not read back to construct TUI, ACP, or server
authority.

The existing event publication lock may serialize domain event creation, but
it may not call slow client code. Surface reduction and subscriber fan-out use
bounded, non-blocking delivery. Legacy observers disappear as each client
migrates.

`SurfaceHub` is the one live ordering point. It stores an immutable
`Arc<SurfaceSnapshot>`, head cursor, bounded retained suffix, and subscriber
lanes behind one short critical section. Fresh attach clones the `Arc`; it does
not deep-copy a long conversation while holding the lock.

Sources publish only with an unforgeable, capability-limited permit:

- the actor control permit may publish operation admission/control and thread
  facts;
- a generation permit may publish only facts scoped to its exact active fence;
- a background permit may publish only facts scoped to its durable owner token;
- the Goal publisher may publish only post-commit changes carrying the current
  Goal store receipt;
- after takeover, a current-owner Recovery permit may publish only a
  RuntimeRestart Stop for its exact historical fence plus the atomic phase
  disposition; it cannot execute, respond, admit, or publish success;
- only the operation finalizer permit may publish Terminal.

`SurfaceHub::commit_batch` validates every permit and scope, requires the same
complete `CommitClass` on every envelope, checks the expected batch-boundary
head, preflights the pure reducer against the immutable snapshot, and performs
the single linearization described in the finalizer section. Concurrent sources
retry from the new head; stale generation or owner permits fail closed. The hub
orders already-authoritative facts but never decides whether a command is
allowed.

## Attach, Cursor, Replay, And Backpressure

`SurfaceCursor.next_seq` is an exclusive complete-batch publication boundary.
State at cursor `C` contains every event in every batch ending at or before that
boundary. Applying a batch of `N` events spanning `[S,S+N)` atomically advances
the cursor to `S+N`; event ordinals do not form observable intermediate state or
attachable cursors. This is not the existing reserved `EventEnvelope.seq`.

`incarnation` is an opaque random identity created whenever a hub is
materialized or recreated, including process restart and projection reset. The
old hub is sealed before its replacement becomes visible. `durable_revision`
identifies the coordinator revision contained by a recorded immutable snapshot;
`live_revision` orders only an ephemeral snapshot in that incarnation.

Fresh attach is atomic under the `SurfaceHub` lock:

1. clone the immutable current snapshot and head cursor `H`;
2. register the subscriber for event `seq = H.next_seq`;
3. return `SnapshotAtCursor(H)` and the subscription.

Cursor attach is also atomic:

1. validate the exact thread id and current incarnation, require the cursor's
   complete source revision to equal the retained `cursor_before` boundary at
   `C.next_seq`, and ensure the sequence is neither in the future nor older
   than the retained suffix;
2. copy complete retained batches spanning `[C.next_seq, H.next_seq)`; a cursor
   inside a batch is invalid;
3. register live delivery beginning at event `seq = H.next_seq`;
4. return the replay batches and a batch-level subscription.

The caller's existing state at cursor `C` plus atomic replay of every complete
batch spanning `[C.next_seq, H.next_seq)` is the baseline at `H`.
If runtime cannot prove that baseline, it returns `SnapshotRequired`; the
client then performs a fresh attach. Runtime never silently treats a bad cursor
as fresh.

`SnapshotRequired` reasons distinguish at least stale incarnation, expired
suffix, replay hole, slow subscriber, and projection reset. A same-incarnation
future cursor, wrong thread, or impossible recorded/ephemeral source revision is
`InvalidCursor`; a valid thread/incarnation whose retained revision boundary has
expired returns `SnapshotRequired`.

The retained suffix is bounded by both event count and encoded byte estimate,
but eviction occurs only at complete-batch boundaries.
Each subscriber queue is bounded. On overflow runtime records one sticky gap,
detaches that subscriber, and closes its ordered lane. The receiver drains
already admitted events before observing `SnapshotRequired`; a separate
high-priority close channel may not overtake a queued operation terminal.
The event that caused overflow is not promised to that subscriber, and no event
after the gap is delivered on that lane.

Detach removes only attachment capabilities and delivery resources. It does
not cancel an operation, resolve an interaction, release background ownership,
or mutate Goal state.

## Durable Interaction Contract

The runtime broker is the only writable ledger for tool approval, permission,
user input, MCP elicitation, and background approval.

Each interaction follows one closed lifecycle:

```text
Requested -> Resolved
          -> Cancelled
          -> Expired
          -> Transferred -> Resolved | Cancelled | Expired
```

`Transferred` changes the durable recovery/capability owner without resolving
the request. A later response still transitions that same interaction to
Resolved; transfer never creates a second request identity.

Every request carries:

```text
operation fence
interaction id and kind
monotonic interaction revision
response token
owner/capability fence
recovery disposition
expected thread id
authority fingerprint when an Allow can authorize later execution
```

Every response carries the attachment identity, interaction identity/kind,
response token, response-route epoch, per-attachment grant token, unique
response id, and applicable operation/generation fence. Reassigning a route
rotates the epoch and all grant tokens; a late reverse response from an earlier
grant cannot pass even when the same attachment is selected again.

A client supplies only the closed semantic answer. The attachment-bound handle
injects all response identities/tokens plus the answer policy persisted with the
request. For an Allow that can authorize later execution it also injects the
persisted authority fingerprint; the actor re-derives current authority and
compares it before commit. Clients never serialize or choose that fingerprint
or a legacy validation policy.

The actor validates all fences before changing broker state. An invalid,
foreign, stale, or malformed response cannot remove or consume the request.
The first committed valid response wins. For recorded threads the commit is
durably synced; for ephemeral threads it is atomic in-memory and explicitly
labelled ephemeral. Repeating the same response id is idempotent; a different
later response receives `AlreadyResolved`.

For recorded threads, request persistence precedes publication and response
persistence precedes the resolved event and waiter wake. Ephemeral threads
preserve the same order against their in-memory commit. Broker wake
notifications are hints only; the actor reconciles contiguous revisions and
remains the command authority.

Broker materialization is bound to the expected thread and coordinator
revision. It rejects records from another thread, non-contiguous source heads,
unknown capability owners, and an Allow whose authority fingerprint is no
longer valid. Reload never auto-executes a decided response.

Every recorded response commit performs compare-and-append against the broker
source head under the coordinator's session lock. Competing brokers/processes
therefore converge on one canonical response; a loser refreshes and returns
`AlreadyResolved`. The process that owns an execution waiter runs a bounded
source-head watcher/reconciliation tick. External commits only wake that actor
to read contiguous durable revisions; the watcher has no response or execution
authority. This is required so an ACP or server process can resolve an
interaction owned by another live surface without process-local first-wins.

Response authority is a runtime-owned, epoch-fenced route:

```text
InteractionResponseRoute =
  Unassigned { epoch }
  | Exclusive { epoch, attachment_id }
  | SharedFirstCommitWins { epoch, attachment_ids }
```

The default is exclusive. Runtime first chooses the operation-origin attachment
when it has the exact negotiated interaction capability; otherwise it chooses a
compatible fallback by the stable runtime ordering `(role_priority,
attached_at_cursor, attachment_id)`. An adapter cannot claim a request merely by
rendering it. A shared route is allowed only by explicit host policy and grants
each listed attachment a capability for the same epoch; the durable
compare-and-append rule still produces one winner and `AlreadyResolved` for a
later valid response.

Detach removes that attachment's live grant but does not resolve the request.
Reassignment or explicit transfer increments the route epoch and replaces the
entire grant set, so every old capability becomes stale. Background handoff
does not silently preserve or transfer response authority: the actor recomputes
the route and increments the epoch whenever its grant set changes.
After process restart no prior live attachment identity is reusable: broker
materialization advances the route epoch, retains the request's separate durable
unavailable disposition, and leaves the interaction unassigned until a new
capability-valid attachment is selected.

Every request durably selects one unavailable disposition. `FailOperation`
closes the interaction and enters operation finalization with
`ClientCapabilityUnavailable`; `AwaitCapableAttachment { deadline }` keeps it
pending until a capable attachment arrives or the runtime-owned deadline
expires. Loading or detaching a particular client never chooses between those
outcomes. The underlying pending interaction remains owned by runtime throughout.

Workflow results follow the same discipline: a durable `Ready` fact precedes
source acknowledgement, admission fixes one operation/turn/input identity, and
retries are idempotent. No client converts results into XML prompts or creates
an independent continuation ledger.

## Goal Ownership

Within a process, a registry keyed by the canonical Goal store identity owns one
shared `GoalRuntimeHandle` and injects it into every `RuntimeHost` and
`RuntimeThread`. Individual threads or hosts may not lazily create competing
in-memory actors for the same store.

Across processes, the Goal store issues an owner epoch/fencing lease. Every
mutation transaction checks that fence; lease loss makes the actor fail closed
and rematerialize before further mutation. A filesystem lock without a durable
owner epoch is not sufficient proof of single ownership.

Goal mutations return post-commit typed change sets. Clients do not perform
before/after rereads to reconstruct created, updated, removed, paused, or
completed facts.

Goal pause uses the runtime interaction-close/cancel/join/finalize sequence.

## TUI Client Contract

The TUI uses an in-process `RuntimeSurfaceHandle`. It owns:

- composer input, key routing, layout, rendering, scrollback presentation, and
  overlays;
- physical terminal lifecycle and desktop notifications;
- a presentation model derived only from surface snapshots and events;
- correlation needed to render accepted/rejected commands.

It does not own an operation handle, cancellation token, writable interaction
broker, generation handler factory, terminal buffer, recovery controller,
direct history/Goal/task/MCP registry mutation, or raw event reducer.

The current production `UserAction` enum has this frozen authority matrix:

| `UserAction` | Owner and route | Required result |
| --- | --- | --- |
| `Submit` | runtime `ReserveOperation(UserPrompt)` then `AdmitReserved` | Requested/admission receipt, then terminal waiter |
| `SubmitWithMentions` | same route with closed `SurfaceInputBindingRequest` descriptors | same; no TUI resource expansion |
| `SubmitWorkflowNotification` | runtime reserve/admit with durable workflow-result id as idempotency identity | one operation identity and terminal waiter |
| `RunWorkflow` | `WorkflowControl::Launch` | committed launch/operation receipt; no local workflow owner |
| `SetModel` | host `UpdateRuntimeSettings(SetModel, HostDefaultsAndThread?)` | post-commit host/thread settings revision; current generation remains frozen |
| `Remember` | host `RememberMemory` with preserved `User`/`Project` scope and optional current thread id | idempotent long-term memory receipt plus optional post-commit pinned-context cursor/deferred pin result |
| `Compact` | `ManualCompact` | operation receipt and terminal waiter |
| `GoalShow` | presentation-only read of attached `goal` snapshot | no mutation and no direct Goal-store read |
| `GoalSet` | `GoalMutation::SetAndRun` | one coordinator intent covers the Goal change and newly reserved Goal operation |
| `GoalEdit` | `GoalMutation::Edit` | post-commit Goal change set/cursor |
| `GoalClear` | `GoalMutation::Clear` | post-commit Goal change set/cursor |
| `GoalPause` | `PauseGoalOperation` addressed by Goal id/revision | runtime atomically pauses the Goal and terminalizes its active Goal operation if one exists; the TUI never chooses the operation |
| `GoalResume` | attached thread: `GoalMutation::ResumeAndRun`; no attached thread: host `ResumeLatestActiveGoal` | one coordinator intent activates/selects the Goal and reserves a new Goal operation; never resume an old generation |
| `ResolveBackgroundApproval` | `RespondInteraction` with exact granted route | committed response receipt; continuation admission remains runtime-owned |
| `StopTask` | `TaskControl::Stop` | post-commit task change/cursor |
| `ForegroundTask` | `TaskControl::Foreground` | post-commit ownership/presentation fact; no TUI registry access |
| `RespondToInteraction` | `RespondInteraction` with exact selector/grant | committed, stale, or already-resolved receipt |
| `Backtrack` | `Backtrack` | post-commit history/snapshot cursor plus restored input projection |
| `BackgroundCurrentTurn` | `TransferBackground` with the reserved operation id/lease before generation creation, otherwise the latest observed exact fence | committed queued intent followed by handoff cursor, or stale/rejected receipt; readiness changes only at handoff/terminal |
| `Interrupt` | exact `CancelOperation` using the reserved operation id from this TUI submission or the operation id in its attached snapshot | committed cancel acceptance, then operation Terminal cursor so a fresh submit is allowed; it cannot target another attachment's queued/newer operation |
| `Cancel` | host `ShutdownHost` for the app-level quit action | bounded shutdown receipt before terminal restoration |

Phase 3 must add two explicitly addressed recovery actions before TUI cutover;
they are not aliases of the app-level `Cancel` or ordinary Ctrl-C `Interrupt`:

| Required target `UserAction` | Owner and route | Required result |
| --- | --- | --- |
| `ResumeOperation { operation_id }` | runtime `ResumeOperation` for the exact visible Suspended/`RecoveryRequired` operation | fresh Reserved/Started receipts and the existing operation waiter |
| `CancelOperation { operation_id }` | runtime `CancelOperation` for that exact visible operation | committed cancel acceptance and operation Terminal cursor |

Fresh attach or restart that exposes Suspended/`RecoveryRequired` must render
these recovery controls. Until the user chooses one, the foreground slot stays
truthfully occupied and new submit is unavailable. The actions are added to the
same machine-readable inventory in the commit that adds their enum variants.

The Phase 0A manifest classifies the current-action table and separately records
the two required target additions. Phase 0B's inventory test compares that
manifest with the current Rust enum. Phase 3 makes the target rows active in the
same commit that adds their enum variants. Adding or renaming any action without
classifying it then fails the build. Slash commands, key bindings,
startup/session-picker actions, and workflow callbacks may translate into these
actions or call the same closed
surface/host routes; they cannot introduce an unclassified mutation path.
Session replacement is `Detach` plus host `OpenThread`/`LoadThread` and `Attach`;
explicit thread close is host `CloseThread`. Mention/fuzzy search is local only
when it reads the attached immutable snapshot, otherwise it uses the closed host
`QueryInputCatalog` projection with an exact host/thread context. MCP catalog
inspection may additionally use the thread-scoped `McpCatalogQuery`; neither
path gives the TUI a writable catalog or filesystem handle. Every mutation
receives the typed receipt named above.

Phase 3 also replaces `UserAction::Remember(String)` with
`Remember { scope: User | Project, note }`. Slash parsing may recognize the
existing `project:` syntax, but it must preserve the scope in that typed action
and may not call `remember_user`, `remember_project`, or any memory writer before
the host receipt. The Phase 0A entrypoint inventory includes mutation-capable
slash commands and callbacks as well as the `UserAction` enum, so a pre-action
side effect fails the boundary gate.

`/trust show` calls host `ReadFolderTrust`; `/trust add` and `/trust remove`
call `SetFolderTrust(Trusted|Untrusted)` with the host-returned revision. The
slash handler renders the receipt but never imports or calls `folder_trust`
storage. Trust add does not promise to upgrade the current thread, and trust
remove does not wait for a restart to revoke future write/network authority.
JSONL `command/exec` and every thread/tool admission read the same host policy
revision; no adapter caches a separate trust decision.

`/model`, `/mode`, `/plan`, reasoning controls, and workspace-root changes use
`ReadRuntimeSettings`/`UpdateRuntimeSettings`; `/config` reads the returned
projection. Slash handlers do not mutate the authoritative `RunConfig` or shared
config before a receipt. Any local copy is presentation cache only and is
reconciled from the returned revision.

MCP mention/resource/template data comes from the read-only `mcp_catalog`
snapshot/patch and `McpCatalogQuery`; TUI never imports the writable
`McpRegistry`. Local transcript fuzzy search may stay presentation-only when it
uses only the attached immutable snapshot.

Selecting a file, skill, plugin, MCP resource/template, or other mention creates
a closed `SurfaceInputBindingRequest` descriptor containing its kind, opaque
catalog identity, observed catalog/config revision, and user-visible label. TUI
passes that descriptor inside `ReserveOperation`; it never expands it into prompt
text or performs the authoritative read. Runtime validates roots/capabilities at
admission, preserves the binding request in the generation replayability capsule,
and creates only a
pending audit Item. It resolves content and promotes that Item to canonical
conversation input only after
the exact generation Started barrier under its cancellation and authority
fence. Resolution failure is a typed operation failure. Thus remote resource
I/O cannot occur before an operation id exists or escape cancellation.

Immediate cancel and background actions send one correlated typed command.
Pre-admission action is addressed to the reserved operation id; it cannot fall
through to the next operation.

Session replacement detaches the old projection and attaches the new one. It
does not cancel old work unless the user separately sends an explicit cancel.
For an ordinary foreground operation the composer becomes ready only after its
terminal cursor is reduced. After a successful background transfer it becomes
ready at the committed handoff/admission-available cursor; it does not wait for
the background operation's later terminal. If validation or reservation fails
before Requested exists, the composer becomes ready after the closed
`Uncommitted` rejection result; it must not wait for a terminal that cannot
exist. A committed queued reservation renders as queued and remains explicitly
cancellable until admission or its `NotAdmitted` terminal.

Lifecycle causes are distinct:

| Cause | Attachment action | Runtime operation action |
| --- | --- | --- |
| TUI view/session replacement | detach the old view | none unless an explicit cancel was sent |
| TUI correlated Ctrl-C | keep attachment | send exact `CancelOperation` for the reservation/snapshot operation id; never infer "current" in the adapter |
| ACP standard cancel | keep attachment | runtime targets the operation bound to that ACP prompt RPC, including after background handoff; it never targets a newer foreground operation |
| embedded multi-connection ACP EOF | detach that connection's attachments and fail its local RPC waiters | operations continue or wait under runtime routing policy |
| `orca --mode acp` stdio EOF/write failure | detach the sole connection, fail its local RPC waiters, then request host shutdown | cancel, join, and finalize all host-owned work before process exit |
| JSONL sole-connection EOF/normal close/read/write/flush failure | close ingress, retire opaque routes/direct responders, detach, and stop/join compatibility services before requesting host shutdown | cancel, join, and finalize every host-owned thread/operation; no orphan one-shot thread or command-exec/shell/search task |
| explicit thread close | detach after acknowledgement | cancel, join, finalize, and close that thread |
| normal TUI process quit | stop accepting input | explicit host shutdown cancels, joins, and finalizes all owned work before terminal restoration |
| process crash | no clean detach assumption | use the restart matrix; never auto-replay Started work |
| host shutdown | revoke attachments after shutdown receipt | close interactions, cancel/join operations and background work, commit terminals or report degraded shutdown |

No path may leave an orphan interaction waiter, child task, or background owner.

## ACP Client Contract

The ownership migration stays on `agent-client-protocol = 0.10.4`. An SDK or
schema upgrade is a separate release slice.

An `AcpConnectionSupervisor` owns one late-bound client proxy, negotiated
capabilities, ordered writer, bounded Orca ingress lane, attachment set,
inbound sequence, and physical-write acknowledgements. Session agents receive a
restricted clone; they do not own the connection or notification queue.

The upstream 0.10.4 RPC loop cannot satisfy this contract: it independently
spawns request/notification handlers, `session_notification().await` returns
after enqueueing to an unbounded channel, and the I/O task neither flushes nor
propagates a failed `write_all`. Phase 0 therefore adds a repo-local
`AcpRpcFacade` pinned to the 0.10.4 schema/types. Its read loop numbers messages
before dispatch and awaits capacity in the bounded ingress lane; it never drops
or reorders prompt/cancel/control input when full. Its total outgoing lane is
bounded and ordered, and every wire message carries a write acknowledgement. It
calls `write_all` and `flush`, then
acknowledges the corresponding cursor/request only after both succeed. A write
failure fails outstanding acknowledgements and reverse requests, terminates the
I/O task, seals the connection, and joins the supervisor. No call to the
upstream `session_notification()` API is accepted as a write barrier.

The facade uses a framed reader with a maximum inbound line size, a maximum
encoded outbound message size, and both message-count and total-byte budgets for
ingress, outgoing, and load/terminal gate buffers. A bounded prelude scanner may
classify an oversize inbound line only when it can recover its JSON-RPC kind and
bounded id/method without allocating the rejected body:

| Inbound oversize class | Required behavior |
| --- | --- |
| request with a recoverable id | do not dispatch; send stable `RequestTooLarge` error for that id when the error frame fits, otherwise seal |
| notification | send no response; seal the connection and apply the ordinary detach/stdio-shutdown policy |
| response to a pending interaction reverse request | send no response; retire and tombstone that RequestId, fail its local waiter, revoke the attachment grant, and let runtime reroute/apply the persisted unavailable disposition |
| response to a pending filesystem/terminal capability call | send no response; classify the call through its written/ambiguous ledger state below, tombstone the RequestId, and settle/fail the owning tool without interaction-route operations |
| unknown/duplicate response id | send no response; seal because correlation cannot be proved |
| malformed, unclassifiable, or over-limit id/method prelude | seal without parsing or echoing attacker-controlled data |

JSON-RPC responses and notifications are never answered with error responses.
An adapter size limit never rewrites or independently terminalizes a runtime
operation. Oversize output is classified before any bytes are written:

| Outbound frame class | Required behavior |
| --- | --- |
| handshake/validation result or error with no runtime mutation | replace it with the stable `ResponseTooLarge` RPC error when that error fits; otherwise seal the connection |
| command result after a runtime fact committed | fail the local RPC/ack and seal or detach its attachment; preserve the committed fact and never compensate through ACP |
| load baseline before `ResponseWriting` | fail the open load with stable `BaselineTooLarge`, detach that attachment, preserve any host materialization/restart settlement already committed, and perform no compensating runtime mutation |
| live update or extension event | mark that attachment lane gapped, fail an open correlated RPC with reload-required when possible, otherwise seal/detach; emit no later frame on the lane |
| interaction reverse request | retire and tombstone the local request, revoke its attachment grant, and let runtime reroute or apply the request's already-persisted unavailable disposition |
| filesystem/terminal capability request | fail it as `FailedBeforeWrite`, settle the owning tool with no external-effect claim, and never invoke interaction routing |
| terminal update or prompt response after runtime Terminal commit | fail the local RPC and seal/detach as required; retain the committed runtime Terminal unchanged |
| connection-scoped notification/request with no session operation | seal the connection after failing its local acknowledgement/waiter |

If replacement-error bytes do not fit, any bytes may already have been written,
or frame class cannot be proved, the facade seals rather than guessing. On an
interaction path, only the runtime-owned persisted unavailable disposition may
later fail an operation. On a filesystem/terminal path, only runtime-owned tool
settlement of the typed capability-ledger outcome may do so. The ACP transport
itself never chooses or writes an operation terminal.

Every write/flush has a configurable deadline driven by an injected clock.
Timeout is the same first-failure path as `write_all` error: cancel the shared
connection token, stop reads and new handlers, close all lanes, fail all pending
write acknowledgements and local RPC/reverse-request waiters, and join the
reader, writer, handler, and attachment tasks. The supervisor itself has a
bounded join deadline and aborts any remaining owned task before returning, so
an `AsyncWrite` that stays Pending cannot block stdio shutdown forever.

The connection supervisor itself only detaches attachments on failure; it does
not infer operation cancellation. The `orca --mode acp` stdio owner has exactly
one connection, so EOF or writer failure then invokes `ShutdownHost`, waits for
cancel/join/finalization, and exits. Only a future or embedded multi-connection
host may keep runtime operations alive after one connection reaches EOF.

Closing an interaction immediately retires its local ACP reverse request and
adds a bounded, expiring RequestId tombstone before any best-effort peer cancel
notification. The supervisor never waits for 0.10.4 peer cancellation support,
never reuses a RequestId within the connection, and discards a late response by
the tombstone plus response-route fence.

Negotiated client filesystem and terminal methods use a runtime-owned
capability-call ledger plus a bounded supervisor transport mirror; they are not
interactions and have no response grant or unavailable disposition:

```text
AcpCapabilityCallKind =
  ReadTextFile
  | WriteTextFile
  | TerminalCreate
  | TerminalOutput
  | TerminalWaitForExit
  | TerminalKill
  | TerminalRelease

AcpCapabilityCallState =
  Prepared
  | DeliveryPossible
  | WrittenAwaitingResponse
  | Completed { result: Success | RemoteError, response_digest }
  | FailedBeforeWrite { error }
  | ObservationUnavailable { error }
  | ExternalEffectAmbiguous { effect_kind, error }

RemoteTerminalLeaseState =
  Live { terminal_id, owner_fence }
  | KillPending { terminal_id, owner_fence }
  | ReleasePending { terminal_id, owner_fence }
  | Released
  | IdentityUnknown { create_call_id }
  | CleanupAmbiguous { terminal_id?, owner_fence }
```

Every call carries a unique non-reused call/RequestId, ACP session id, exact
operation/generation fence, capability revision, trust policy epoch, method,
normalized arguments digest, and owning tool-call id. For `WriteTextFile`, `TerminalCreate`,
`TerminalKill`, and `TerminalRelease` on a recorded thread, the coordinator
commits `CapabilityCallIntent(Prepared)` before the call enters the writer. It
then commits and projects `DeliveryPossible` before allowing the writer to emit
any byte; this conservative transition is the side-effect barrier. The intent contains a
target/argument digest, not replayable secret content, and is never used to
automatically repeat the call. Ephemeral threads use the same atomic in-memory
states without a crash-durability claim.

A call may fail as `FailedBeforeWrite` only while durable/in-memory state is
still `Prepared` and the writer proves it received no permit. Once
`DeliveryPossible` commits, neither the facade nor tool runner automatically
retries the call: a full newline may reach the peer even when
`write_all`/`flush` later fails. Physical write acknowledgement moves it to
`WrittenAwaitingResponse`; one matching decoded response atomically commits
`Completed` and any terminal lease before the tool waiter is released.

Response loss, an oversize response, timeout, or connection failure after
possible delivery is classified by method:

- read, terminal output, and wait-for-exit become `ObservationUnavailable`;
  runtime may issue a new explicitly fenced observation call, but it is a new
  identity and may observe newer state;
- file write becomes `ExternalEffectAmbiguous(FileWrite)`; the owning tool and
  operation record an ambiguous external effect and fail closed without an
  automatic rewrite;
- terminal create without a decoded terminal id becomes
  `IdentityUnknown`; the operation records an ambiguous remote process and the
  connection/session health is degraded;
- kill or release after possible delivery becomes `CleanupAmbiguous`; runtime
  never claims that the remote command was killed or released.

Connection sealing walks the bounded ledger and completes every call exactly
once with the appropriate typed outcome before dropping local waiters. The
owning runtime tool path commits that outcome and any external-effect/cleanup
health fact before its generation can finalize; the supervisor itself does not
choose an operation terminal.

Restart scans incomplete recorded capability intents before thread admission.
`Prepared` is failed with proof of no dispatch. `DeliveryPossible` or
`WrittenAwaitingResponse` without a completed response becomes
`ExternalEffectAmbiguous(FileWrite)`, `IdentityUnknown`, or `CleanupAmbiguous`
according to the frozen method matrix, and is never replayed. Thus a crash in
the interval between remote delivery and response/lease registration remains a
visible durable fact rather than disappearing with the connection task.

A decoded `terminal/create` response is registered as a
`RemoteTerminalLease::Live` under the exact generation fence before the tool can
use or publish its id. For a recorded thread that registration is a
coordinator-wrapped Tool/Session fact; after restart, any non-Released lease from
the dead connection becomes `CleanupAmbiguous` and is never reused. Generation
cancel, tool settlement, operation
finalization, or clean connection shutdown drives a bounded kill-then-release
sequence for every known live lease. `Live -> KillPending`; kill success moves
only to `ReleasePending`, never `Released`. A terminal already known to have
exited also enters `ReleasePending`. Only a successful `terminal/release`
response moves the lease to `Released`. A kill or release error, response loss,
timeout, or disconnect moves it to `CleanupAmbiguous` and is not automatically
retried. ACP 0.10.4 has no list/discover method and does not guarantee cleanup of a
create whose response
was lost, so Orca does not fabricate that guarantee: it seals/detaches the
connection, publishes `RemoteTerminalIdentityUnknown` session health, and
requires operator/client reconciliation before claiming clean remote-resource
shutdown. A client-specific stronger cleanup extension may remove that degraded
state only after Phase 4A specifies and proves it.

Capability outcomes feed the runtime-owned tool settlement and typed Tool/
Session patches before generation completion. They never directly resolve an
interaction or manufacture an operation terminal. Phase 4A fixtures cover every
method at failure-before-write, partial/write-acknowledged, response, oversize,
cancel, and disconnect boundaries, and prove zero automatic retry for ambiguous
side effects.

Standard ACP carries, where supported by 0.10.4:

- initialize/authenticate and capability negotiation;
- session new/load, prompt, cancel, and session updates;
- agent message/thought, tool, plan, and permission requests/updates;
- exact model/mode/configuration options supported by both sides;
- negotiated client filesystem and terminal operations.

Prompt content is handled in original block order:

| ACP content | Runtime ingress rule |
| --- | --- |
| `Text` | preserve text and block order exactly |
| `ResourceLink` | preserve the typed link; validate URI scheme, negotiated client capability, workspace roots, and read authority before resolution |
| embedded text resource | accept only supported text MIME/encoding and preserve its resource identity |
| image/blob/audio or unknown block | accept only when an advertised runtime/model capability has an explicit typed mapping; otherwise reject the prompt before Requested |

`new_session` and `load_session` map declared MCP servers and additional
directories through runtime configuration validation. They are not ignored or
stored only in the adapter. Unsupported content, URI, server, directory, or
capability use returns an explicit protocol error before operation reservation;
it is never silently discarded or flattened into misleading text.

Standard ACP prompt and cancel remain session-scoped, but their binding is not
"whatever is foreground now." The facade allocates
`AcpPromptBindingId { connection_id, session_id, inbound_seq }` when the prompt
line is decoded and sends prompt/cancel ingress through one per-session ordered
lane. For a prompt already read from the wire, that lane must complete
`ReserveOperation(origin_binding)` and durably bind its operation id before a
later-read cancel is handled. This closes the 0.10.4 handler-spawn race without
an adapter `cancel_requested` flag.

Each ACP connection/session has at most one unresolved standard prompt binding.
Background handoff does not clear it, and another surface may start a newer
foreground operation without changing it. `session/cancel` targets the exact
operation bound to that prompt RPC even when it is now background-owned; a
second standard prompt on the same ACP session is rejected until the bound
prompt reaches its true terminal and its response is written, or transport
failure retires the RPC and a fresh load establishes a new attachment. An
extension-aware cancel may carry an exact operation fence. The adapter retains
only transport correlation; runtime owns the binding-to-operation mapping and
all cancellation authority.

The transport binding state is closed and separate from runtime operation state:

```text
AcpPromptBindingState =
  Bound
  | TerminalGated { terminal_cursor }
  | ResponseWriting
  | Completed
  | TransportRetired { reason, operation_id, request_id_tombstone }
```

A subscriber gap, oversize frame, or connection failure before the prompt
response commits transitions the binding exactly once to `TransportRetired`.
When the writer is still usable, the ordered lane first drains already-admitted
pre-gap frames, writes one stable correlated reload/transport error for the
original prompt request id, obtains its physical acknowledgement, and then
retires/tombstones the id and detaches the invalid attachment. On writer failure
it retires locally while sealing. The runtime operation continues, waits,
reroutes interactions, or terminalizes only under runtime policy; no later
`PromptResponse` is emitted for the retired id. A late terminal is visible only
after a fresh attach/load through that new projection, never as a second response
to the old RPC.

The runtime terminal commit is the prompt/cancel race linearization point. A
cancel admitted before Terminal fixes a cancelled outcome, including the
pre-admission `NotAdmitted(CancelledBeforeAdmission)` case. A cancel read after
Terminal is committed receives `AlreadyApplied` and cannot rewrite success or
another terminal merely because the response bytes are still pending. The
per-session sequencer ensures a cancel line read before prompt reservation is
not overtaken by handler scheduling; it cannot reverse an already committed
runtime terminal.

Interaction mapping is normative:

| Runtime interaction | ACP 0.10.4 mapping | Route/failure rule |
| --- | --- | --- |
| provider tool approval | `session/request_permission` with exact options and response-token metadata | use the runtime response route; on no grant apply the persisted unavailable disposition |
| runtime permission request | `session/request_permission`; map only exact grant scopes | reject an unrepresentable scope without widening authority, otherwise use the persisted route/disposition |
| user input | negotiated logical `orca/surface/v1/request_input` reverse request | route to another capable attachment or apply the persisted unavailable disposition |
| MCP elicitation | negotiated logical `orca/surface/v1/request_elicitation` reverse request | same route/disposition rule; never flatten to chat text |
| background approval | standard permission request when exactly representable, otherwise negotiated extension | keep runtime ownership and apply its route; never infer approval from connection state |

If operation cancellation closes an interaction, the supervisor cancels or
retires the pending ACP reverse request. A late response is stale and cannot
consume a newer interaction. Absent a transport-retirement condition, the
ordinary prompt RPC remains pending through background transfer and completes
only at the true operation terminal.

Terminal mapping is normative:

| Runtime result | ACP result |
| --- | --- |
| `Terminal(Succeeded)` | `PromptResponse(EndTurn)` |
| `Terminal(Cancelled)` | `PromptResponse(Cancelled)` |
| `Terminal(BudgetExhausted(ModelTokens))` | `PromptResponse(MaxTokens)` |
| `Terminal(BudgetExhausted(TurnRequests { scope: AgentLoop }))` | `PromptResponse(MaxTurnRequests)` |
| Goal/workflow token, subagent-turn, or monetary `BudgetExhausted` | prompt RPC error with stable `orca_budget_exhausted` data carrying the closed budget kind; do not mislabel it as an ACP token/turn limit |
| `Terminal(NotAdmitted(CancelledBeforeAdmission))` | `PromptResponse(Cancelled)` |
| other `Terminal(NotAdmitted)` | prompt RPC error with stable Orca error data |
| execution/panic/restart-abort terminal | prompt RPC error; never `EndTurn` |
| `Terminal(Shutdown)` while the connection is writable | `PromptResponse(Cancelled)`; transport loss may instead make the local RPC fail |
| background transfer or waiting interaction | no prompt response yet; publish truthful active/waiting update |
| `FinalizingDegraded` or terminal commit failure | fail the local RPC with terminal-commit error after its receipt; do not fabricate a terminal |

An `AcpPromptCompletionGate` prevents the prompt waiter from racing the surface
projector. After `OperationTerminalAtCursor`, the ACP projector must consume and
encode every standard update and negotiated extension terminal through that
cursor, then obtain `flush_through(terminal_cursor)`. A gap or writer failure
before that acknowledgement wins over the prompt result and takes the
`TransportRetired` path. Only after the flush acknowledgement may the facade enqueue the
`PromptResponse`; physical acknowledgement of that response completes the
binding. The only alternative is the mutually exclusive `TransportRetired`
path above, which tombstones the request id and forbids a later prompt response.
Thus final updates cannot be overtaken by EndTurn/Cancelled.

ACP load always starts with a fresh runtime attachment because standard ACP has
no live cursor. Runtime fully materializes the conversation, tools, plan,
pending interactions, Goal/task/workflow state, and current operation before
projection. The wire baseline is capability-dependent:

- standard ACP receives the complete ordered projection representable by
  0.10.4, including conversation/tool/plan updates and any exactly representable
  pending permission request;
- a negotiated Orca extension receives the additional Goal/task/workflow,
  operation/generation, interaction, terminal, and cursor facts;
- an unrepresentable pending interaction is not flattened or discarded. If it
  is already routed to another capable attachment it remains there. Otherwise
  runtime applies the interaction's persisted `FailOperation` or
  `AwaitCapableAttachment` disposition; the ACP adapter does not choose.

`load_session` calls `flush_through(baseline_cursor)` for every message in that
client's representable projection and yields its JSON-RPC result only after the
baseline's physical write acknowledgement. The facade tags the resulting load
response with the same per-session baseline gate; only the physical
acknowledgement of that response releases buffered live events. Therefore no
live update can overtake the load response, and live delivery resumes at the
next surface sequence.
"Complete load" means a complete runtime materialization plus a complete
capability-valid projection, not pretending standard ACP can encode Orca-only
state.

The load gate has explicit wire states `BaselineStreaming -> BaselineFlushed ->
ResponseWriting -> ResponseCommitted -> Live`; `ResponseCommitted` means the
entire success response completed `write_all` and `flush`. A buffered-live gap
before `ResponseWriting` returns the reload-required RPC error. Once any success
response bytes may have been written, that response cannot be replaced: a gap
during `ResponseWriting` seals the connection and lets the client observe
transport failure/reconnect. A gap after `ResponseCommitted` follows the normal
extension/extensionless attachment rule. No path emits both success and error
for one JSON-RPC id.

The optional public extension namespace is `orca.runtimeSurface.v1`. A client
advertises version `1` in initialize `_meta`; the agent echoes the selected
version and never sends extension requests to a client that did not negotiate
it. In the 0.10.4 API the logical extension method names below are passed without
a leading underscore, while the frozen JSON-RPC wire names have the SDK-required
leading underscore. The repo-local facade preserves that mapping. Version 1
reserves:

- `_meta.orca.runtimeSurface.v1` on load/update/prompt values for cursor,
  operation/generation, interaction token, and terminal receipt data;
- logical `orca/surface/v1/request_input`, wire
  `_orca/surface/v1/request_input`, and logical
  `orca/surface/v1/request_elicitation`, wire
  `_orca/surface/v1/request_elicitation`, as reverse requests;
- logical `orca/surface/v1/gap`, wire `_orca/surface/v1/gap`, for
  `SnapshotRequired`;
- logical `orca/surface/v1/control`, wire `_orca/surface/v1/control`, for exact
  fenced control intents not represented by standard ACP.

Unknown versions fall back to standard ACP with no partial extension fields.
Before any Phase 4 production implementation, Phase 4A freezes the public
compatibility contract in a separately reviewed protocol document, checked-in
JSON Schemas, and canonical wire fixtures. It defines every method direction,
request/result/error shape, required/optional/null field, version negotiation,
standard fallback, stable error code/data, unknown method/version/field rule,
numeric/string bound, cursor/token opacity rule, and maximum frame behavior.
The schemas cover initialize metadata, load/update/prompt metadata, both reverse
requests, gap, and control. Fixtures include accepted and rejected examples and
byte-exact logical-name to leading-underscore wire-name cases. Internal Rust
candidate types are never serialized automatically. Phase 4B code and tests may
begin only after explicit written review of Phase 4A; changing the schema after
that review requires a compatibility review and, when incompatible, a new
namespace version.

For an extensionless client, a slow-consumer gap fails an open prompt or load RPC
with the stable reload-required error, physically acknowledges that sole error
when possible, and retires its prompt/load binding before detaching. With no open
correlated RPC, it seals and detaches that session attachment so the next prompt
fails until a fresh `load_session`; it does not fabricate a response or event.
No update or terminal after the gap is delivered on the invalid lane. An
extension-aware client first receives wire method `_orca/surface/v1/gap`; when a
prompt/load RPC is open, the ordered correlated error and binding retirement
still follow. It uses the same fresh-load rule unless a future protocol version
explicitly supports cursor resume.

The repo-local facade's total outgoing lane is bounded. This does not claim that
the remote peer or its transport buffers are bounded; slow/failing peer behavior
is covered by the writer acknowledgement and shutdown tests.

## JSONL Server Contract

The server migrates after ACP. Its released request and event JSON shapes stay
compatible. Internally it attaches to the same surface and maps typed commands,
snapshots, events, interactions, and terminals to JSONL.

The server may maintain transport request correlation, but it no longer owns:

- active operation/generation truth;
- pending permission, user-input, or MCP waiters;
- response consumption rules;
- runtime terminal inference;
- raw JSONL-to-runtime semantic reconstruction.

A single `JsonlServerSupervisor` owns connection teardown. EOF, normal close,
read failure, and write/flush failure all converge on the same ordered close
sequence defined by the private contract, including bounded cleanup of the
remaining non-surface command-exec/shell/search services before the final
`ShutdownHost` barrier. Individual processors cannot start a second shutdown
rail.

The legacy response wire currently carries only `requestId`. For
`user_input/respond` and `mcp_elicitation/respond`, the adapter supplies its
attachment capability from the connection-scoped surface handle and calls the
bound interaction responder with the closed opaque selector. For
`permission/respond`, it MUST instead call the host-owned opaque permission
router below; the adapter may not construct a thread selector or probe an owner.
The selected runtime owner atomically validates the exact pending interaction,
stored route/grant, thread, kind, token, and operation/generation fence before
consumption. A stable response id is derived from connection identity, inbound
RPC id, opaque request id, and normalized response body so an identical
transport retry is idempotent. The adapter stores no semantic waiter or
generation mirror.

`permission/respond` shares one released wire method between thread interactions
and non-thread `command/exec`. A host-owned `OpaquePermissionRouter` therefore
allocates one collision-free connection namespace and registers a closed route
before emitting each request:

```text
OpaquePermissionRoute =
  ThreadInteraction {
    thread_id, interaction_id, response_route_epoch, response_grant_token,
  }
  | CommandExecPermission { service_request_id, service_fence }
```

On response the router looks up but does not remove the route, delegates to the
exact owner, and tombstones it only after that owner returns a committed or
terminal result. A committed tombstone stores a safe permission-resolution
receipt and a private keyed digest: the identical RPC/body identity replays that
receipt, while a different body or RPC identity cannot consume or overwrite it.
Validation/persistence failure leaves the published route retryable by the same
transport response id. Repair tokens remain inside the router. The adapter never
probes two managers or infers the owner from request text; the router contains
transport routing capabilities, not a semantic waiter.

Route registration and request publication are one closed state machine. A route
is registered before encoding, becomes published only after `write_all + flush`
acknowledgement, and is transport-retired on encoding/write/flush failure. That
retirement atomically revokes a thread response grant and lets runtime reroute or
apply the persisted unavailable disposition, or fails the exact command/exec
service fence before execution. A response commit racing the write failure wins
by compare-and-set and is never compensated; a possibly partial request id is
tombstoned before the connection is sealed.

If a server surface subscription gaps, behavior depends only on released
transport correlation:

- when a long-lived correlated RPC is still open, the adapter returns its
  existing JSONL error shape with the exact message `thread surface snapshot
  required; reconnect and resume the thread`, permanently retires that RPC
  binding, and emits no post-gap update for its id;
- when only an asynchronous event stream remains and there is no request id to
  answer, the adapter drains already-admitted pre-gap writes, seals and closes
  the connection/stream, and requires the existing reconnect plus
  `thread/resume` path. It never fabricates a response or invents an event name.

Resume uses a fresh runtime snapshot and projects the baseline through existing
compatible events. Phase 0 freezes differential fixtures only for flows actually
reachable in `v0.2.50`. The new gap branches are verified with adapter
fault-injection contract tests that require existing error shapes and exact
close ordering; they are not falsely described as a black-box `v0.2.50`
differential case.

### Frozen JSONL compatibility refinements

The Phase 0A companion freezes these refinements for the Phase 5 adapter; they
are not implementation choices:

- released `thread/list`, `thread/search`, `thread/read`, turn/item page reads,
  metadata update, start, resume, and fork map respectively to the closed host
  catalog/read/materialization commands. `thread/read` uses one coherent
  `ReadSession` token, and only the released metadata decoder may request the
  stable-id `LegacyLastWriteWins` precondition;
- `turn/interrupt`, `turn/resume`, and `turn/steer` use
  `ControlJsonlTurn`, which performs legacy-turn lookup and actor control as one
  runtime-owned command. The adapter retains RPC correlation only. Released
  `v0.2.50` has no `thread/close` method: that request keeps the existing
  unsupported-method error and never maps to host `CloseThread`;
- `turn/start` with a thread id resolves only a live thread and never cold-loads
  one. A busy thread takes the actor's pre-Requested immediate rejection branch
  and preserves the existing active-turn error. Stateless submit creates one
  ephemeral, noncatalogued, one-shot thread and emits no `thread_started`;
- released permission-profile and permission-update values decode into closed
  settings patches. Resume applies them in its atomic load/settings receipt;
  turn start commits them before Requested and freezes the resulting settings
  and policy revisions. Failure emits the existing correlated error and no
  operation fact;
- a `permission/respond` route created by the released JSONL v0.2.50 adapter may
  use the capability-bound legacy permission validation mode. It preserves any
  well-formed returned profile, including values not present in the request,
  while still validating route, fence, policy epoch, and closed value shape.
  Native typed responders use requested-subset validation;
- a released `mcp_elicitation/respond` accept uses its capability-bound legacy
  opaque-content mode: any bounded closed JSON value is accepted without schema
  validation, and omitted `contentJson` becomes `{}`. Native typed Form answers
  remain recursively schema-checked;
- one released turn is one `SurfaceOperation`; `turn/resume` may start a
  replacement generation under that same operation but never creates a second
  operation. Every legacy `turn_started`, including later agent-loop iterations,
  projects from `OperationPatch::AgentLoopTurnStarted`; generation Started is
  only the once-per-generation execution barrier;
- `turn_completed` is projected only from runtime Terminal:
  `Succeeded -> success`, verification failure to `verification_failed`, legacy
  approval-required failure to `approval_required`, other failure/panic/join or
  restart abort to `failed`, cancel/shutdown to `cancelled`, and budget exhaustion
  to `budget_exhausted`. NotAdmitted and pre-start failures retain one correlated
  error and suppress both start and completion;
- image, local-image, incomplete-skill, and untagged string inputs that the
  released decoder accepted but discarded remain explicit
  `LegacyAcceptedDropped` cases for this wire version; Phase 5 may not silently
  claim typed support or begin rejecting them;
- the frozen ordering corpus remains authoritative, including assistant/tool/
  workflow item ordering, `turn_controlled` before a steer-created user item,
  and `turn_completed` as the final frame for its RPC id. Surface-only facts use
  `NoLegacyProjection`; the adapter invents no cursor, recovery, Goal, settings,
  health, or capability event names.

`PendingPermissionManager` currently also serves non-thread `command/exec`.
Phase 5 splits that responsibility: thread-bound permission moves to the
surface; command/exec retains a separate bounded, request-fenced transport
service behind the one opaque router above unless it is independently designed
as a host-scoped operation. Shell-manager transport is an explicit non-goal and
is not deleted merely because the thread surface converges. Released
thread/session list, search, coherent read, metadata, page-read, start, resume,
fork, and turn-control operations do migrate to `RuntimeSurfaceHostHandle`;
mention/fuzzy search that uses thread/session data uses its typed read
projections rather than a server history owner. Host `CloseThread` remains a
closed command for actual clients, but is not exposed through a nonexistent
released JSONL `thread/close` method.

Legacy `EventEnvelope` serialization remains available for compatibility and
stored history readers. It is derived from typed runtime facts and is not the
surface source of truth.

Compatibility is proved against a frozen `v0.2.50` request/response corpus, not
only tests compiled with the new implementation. The differential gate
normalizes JSON object key order and documented volatile ids/timestamps, then
requires the same field set, values, error shape, request correlation, and
per-request event ordering. Any intentional additive field is versioned and
documented rather than hidden by normalization.

## Migration Plan

### Phase 0: Contract freeze and RED evidence

- This parent design is approved on a clean `v0.2.50` worktree, and Phase 0A
  artifact completion is authorized.
- Phase 0A freezes and commits the two-file companion bundle named above,
  including exact nested patches, the 22 thread commands, the 24 host commands,
  fences/results, DTOs, source inventory, JSONL compatibility vectors, and the
  machine-readable TUI action matrix. It then obtains explicit written review
  against the exact file hashes. No Phase 0B RED test or production
  implementation begins before that review gate.
- Phase 0B starts only from the reviewed bundle and converts its invariants
  into compile-time inventories and behavior tests. Any post-review artifact
  edit returns the work to Phase 0A consistency checks.
- Convert the identified P0 deadlock, stale response consumption, terminal
  ordering, attach race, and client mirror gaps into behavior tests.
- Prove the repo-local ACP 0.10.4 facade can preserve read order across
  prompt/cancel, acknowledge only after `write_all` plus `flush`, and propagate
  writer failure. This feasibility RED/GREEN precedes the ACP cutover plan.
- Treat experimental p13 types/reducer/tests as evidence only; do not
  cherry-pick its independent sequencer/control plane.

### Phase 1: Minimum resident control core

- Add `RuntimeCommitCoordinator`, globally unique operation ids, the two-stage
  reservation/admission API, operation/generation records, and explicit input
  identity append.
- Add the resident actor control slot, minimal `SurfaceHub` operation/
  interaction/terminal lane, and operation waiter receipts.
- Move all five interaction kinds to the actor-owned broker and route existing
  clients through temporary presentation/transport bridges.
- Make cancellation commit acceptance and wake interactions without inline
  join; drive join/finalization from the actor completion branch.
- Add restart, authority-fingerprint, cross-thread broker, partial-write,
  cross-store finalize-intent, background handoff, and terminal failure tests.

Deletion gate: no surface UUID plus actor-local client identity, no direct
admission-error terminal helper, no second completion rail, and no new client
can receive a writable broker or generation handler. Temporary legacy bridges
may encode or present requests but cannot consume a response, decide terminal
state, or own a waiter. Their file deletion waits for the corresponding
vertical cutover.

### Phase 2: Complete private semantic surface

- Expand the closed event/snapshot domain, pure reducer, materializer,
  coordinator records/checkpoints, retained suffix, and bounded subscribers.
- Implement Arc-based fresh attach and half-open cursor replay atomically.
- Route item/tool/plan/usage/context/task/workflow/subagent/Goal/settings/pinned
  facts through one typed ingress and scoped publisher permits.
- Inject one Goal handle from the process registry and return post-commit
  changes carrying the Goal store receipt.
- Give standalone workflows their own operation id and optional parent link;
  generation-launched workflows use a child fence, and result follow-up uses
  one idempotently admitted operation identity.
- Preserve unknown legacy continuation data opaquely until its explicit typed
  migration commits.

Deletion gate: materialize/replay/live use one reducer; no production surface
path reconstructs state from raw payloads or direct snapshot assignment; no
independent interaction/workflow reconciler can publish authority outside the
runtime mailbox and commit coordinator.

### Phase 3: TUI vertical cutover

- Build the in-process surface client and presentation projection.
- Switch startup/load/submit/cancel/pause/resume/steer/background,
  interactions, manual compaction, backtrack, thread close/shutdown, Goal,
  task stop/foreground, workflows/results, settings, model, reasoning, roots,
  long-term memory, folder trust, and pinned context as one ownership migration.
- Enforce the frozen `UserAction` authority matrix and inventory test.
- Preserve keyboard, rendering, approval, background, session-picker, and
  DeepSeek behavior with parity tests and PTY harnesses.

Delete after parity:

- `operation_controller.rs`;
- `interaction_broker.rs`;
- `hosted_runtime.rs` terminal authority;
- `runtime_interaction_adapter*` and generation handler factories;
- `runtime_event_projection.rs` raw reduction;
- `RuntimePendingInteractionStore` TUI assembly;
- `background_approval.rs`, lifecycle authority in `background_tasks.rs`, and
  `workflow_notifications.rs` continuation ownership;
- direct runtime history/Goal/task/MCP registry mutation in session-picker,
  slash-menu, and slash-command paths;
- direct `memory::remember_user`/`remember_project` calls and TUI-owned pending
  pinned-context buffering;
- direct `folder_trust` reads/writes from slash handlers or presentation code;
- direct mutation of runtime-effective `RunConfig`/shared config from TUI
  model/mode/plan/root handlers before a settings receipt;
- direct history/session-store list, search, metadata, load, fork, and transcript
  reads in startup, resume, and session-picker paths.

An import boundary guard rejects `OperationHandle`, `RuntimeThreadHandle`,
`RuntimeHostHandle`, direct `history`/`SessionStore`, `GoalRuntimeHandle`,
`TaskRegistry`, `McpRegistry`, direct memory or folder-trust stores/writers,
writable runtime-config handles, writable broker/store, generation handler, and
`EventEnvelope.payload` in production TUI runtime paths. TUI may import only the
closed host/thread surface facades for runtime/session data. Behavior tests
remain the primary proof.

### Phase 4: ACP vertical cutover

- Phase 4A commits the reviewed public protocol document, JSON Schemas, and
  canonical wire fixtures described above. No Phase 4 production adapter work
  begins until this independent compatibility gate passes.
- Phase 4B performs the ACP cutover against that frozen schema.
- Attach ACP sessions to the same surface.
- Add the repo-local 0.10.4 RPC facade, connection supervisor, per-session
  inbound sequencer, prompt binding, client capability proxy, physical writer
  barrier, baseline-before-load-response, and live-after-baseline.
- Implement the content, interaction, cancel, terminal, gap, standard fallback,
  MCP server, and additional-directory matrices in this design.
- Add reverse-scheduled prompt/cancel, slow/failing writer, stdio and embedded
  EOF, multi-session detach, late response, and joined shutdown tests.

Delete after parity:

- `OrcaAcpAgent.host: RuntimeHostHandle` and
  `SessionEntry.thread: RuntimeThreadHandle`, replacing them with
  `RuntimeSurfaceHostHandle` and `RuntimeSurfaceHandle` only;
- `SessionEntry.current_op` and `cancel_requested`;
- raw `acp/event_map.rs` authority;
- direct transcript preload as load semantics;
- local stop-reason/terminal inference;
- the runtime-owned unbounded ACP notification channel.

An ACP import boundary guard covers every production `acp/**` module, including
same-crate private imports. It rejects `RuntimeHostHandle`,
`RuntimeThreadHandle`, `OperationHandle`, raw history/session stores,
`EventEnvelope` payload access, writable brokers/registries, generation
handlers, and legacy terminal/outcome inference. ACP may import only the closed
surface host/thread facades, closed ACP transport DTOs, and pure presentation
mappers. Rust visibility alone is not accepted as the boundary proof.

### Phase 5: JSONL server convergence

- Move released thread/session list, search, coherent `thread/read`, metadata,
  turn/item-page read, start, resume, fork, resolve-running, and turn-control
  paths onto `RuntimeSurfaceHostHandle` and per-thread typed reads. The released
  `v0.2.50` wire has no `thread/close`; preserve its unsupported-method error
  rather than adding a compatibility mapping to host `CloseThread`.
- Replace active-turn and thread-bound pending-interaction ownership with
  `ControlJsonlTurn`, surface commands, and the opaque request-id adapter.
- Split non-thread command/exec permission service behind the single
  host-owned opaque permission router before deleting the shared pending
  manager.
- Route command/exec folder-trust reads through the same durable policy epoch;
  do not retain a server-local trust cache/store path.
- Project typed surface updates onto the released JSONL contract.
- Preserve the frozen settings-before-Requested, one-operation/multi-generation,
  `AgentLoopTurnStarted`, total terminal, `LegacyAcceptedDropped`, exact gap,
  opaque-permission-router, and event-ordering refinements in the companion
  artifact.
- Add the frozen `v0.2.50` semantic differential corpus and gap/resume tests.
- Preserve all existing request ids, event names, field/error shapes, and
  ordering covered by both the current suite and differential corpus.

Delete after parity:

- `ServerActiveTurnRegistry` or equivalent active authority;
- `ServerThreadRuntime` live-map/start/resume/fork authority and adapter-direct
  `SessionStore`/history lifecycle reads;
- pending permission/user-input/MCP waiter managers and response processors'
  thread-operation ownership;
- server raw-payload semantic projector except the final compatibility encoder.

### Phase 6: Documentation and release

- Update `README.md`, the architecture ADR, production roadmap, harness
  contract, Goal and ACP/server documentation, troubleshooting,
  `docs/release-process.md`, and release notes.
- Rebase onto current `main`, rerun every gate, integrate onto a clean `main`,
  and choose the next available patch version if it is no longer `v0.2.51`.
- Update Rust, npm, lockfile, site, and release metadata consistently.
- Push `main`, verify `HEAD == origin/main`, create the tag at that exact commit,
  and push the tag. The tag target must be an ancestor of `origin/main`.
- Require every Release workflow job and Pages deployment for the release SHA
  to succeed; missing npm credentials are a hard release failure.
- Verify the GitHub Release commit and assets, checksums, npm package matrix,
  installed npm binary, public site/install script, and real DeepSeek TUI, ACP,
  and server behavior from public artifacts.

The goal is not complete at local green tests or tag creation. It closes only
after public artifact verification succeeds.

## Behavior And Failure Test Matrix

### Operation and terminal

- canonical id exists before preparation and is shared by every lifecycle fact;
- reserve returns the id before admission and pre-admission cancel terminalizes
  that id without a user item;
- concurrent ready reservations have one actor-ordered foreground winner; busy
  reservations remain queued, and admit/cancel/expiry races have one committed
  outcome;
- every generation Started sync is observed before hooks, continuation
  consumption, executor spawn, or another side-effect probe;
- a stopped generation can resume under the same operation without publishing
  an operation terminal;
- duplicate and competing interrupt/cancel/pause/resume/transfer controls obey
  the committed-intent precedence table while join remains in flight;
- immediate cancel cannot target the following submit;
- cancel/Goal pause acceptance is cursor-visible before command response, wakes
  interaction waiters before join, and leaves the actor command loop responsive;
- join panic, writer failure, task settlement failure, and terminal append
  failure produce their exact non-fabricated outcomes;
- Started append failure performs no side effect, and nonterminal projection
  failure blocks enabling effects until retry/rematerialization proves the
  durable fact;
- operation terminal is durable and cursor-visible before waiter success;
- background handoff releases foreground only after its transfer barrier and is
  active state until the later terminal barrier;
- crash between domain settlement and terminal append reconciles receipts
  without rerunning execution;
- restart never invokes the executor for Started-without-Terminal work;
- restart exposes resumably Stopped replayable work only as explicit
  `RecoveryRequired`, finishes non-resumable stopped finalization, and aborts a
  host-owned Transferred generation without pretending its task survived;
- a second process cannot materialize or restart-abort a thread whose ownership
  lease is live; after proven takeover, the old owner epoch cannot start,
  publish, settle, or terminalize work.

### Attach and replay

- fresh attach during every operation phase has no history/live gap;
- every retained replay split converges to the continuously reduced snapshot;
- stale, future, expired, hole, and wrong-incarnation cursors are distinct;
- terminal cannot be overtaken by subscription close;
- slow and fast subscribers do not affect each other or runtime completion;
- detach does not cancel work or resolve interactions;
- after crash between durable append and live publication, a new-incarnation
  fresh snapshot contains the fact once; no cross-incarnation event-delivery
  exactly-once claim is made.

### Interaction and Goal

- stale/foreign response cannot consume a valid waiter;
- two responders yield one durable winner and one `AlreadyResolved`;
- two independent recorded brokers committing different responses converge by
  source-head CAS, and the owner actor observes the external commit;
- repeated response id is idempotent;
- broker reload rejects the wrong thread, non-contiguous revision, and changed
  authority fingerprint;
- an Allow is re-requested after cwd/roots/policy/tool/artifact change;
- all interaction kinds survive view replacement;
- restart rendering does not auto-resume execution;
- multiple threads/hosts in one process share the Goal registry, and a stale
  cross-process Goal owner epoch cannot commit;
- crash between any Goal/task/workflow/settings external-store mutation and its
  coordinator wrapper is reconciled by intent/idempotency receipt, while an
  ambiguous non-idempotent result remains `MutationDegraded`;
- Goal mutation result equals the committed change set.

### TUI

- existing approval, permission, input, MCP, background, Goal, workflow,
  compaction, tool, subagent, paste, mouse, and alternate-screen behavior;
- cancel/startup/background races use one correlated command;
- session replacement and reattach converge to continuous presentation;
- view detach, explicit close, normal quit, crash, and host shutdown follow the
  distinct lifecycle table with no orphan waiter/child/background owner;
- the complete `UserAction` authority matrix has no unclassified mutation;
- `/remember` preserves user/project scope, is idempotent across memory-commit/
  thread-pin failure, and performs no TUI-side memory write before its host
  receipt;
- `/trust` uses host reads/CAS mutations; add never widens live authority, while
  remove rotates the policy epoch; stale side-effect fences fail closed, all live
  cross-process owners ack, and subprocess/proxy/ACP resources stop before a
  committed receipt, otherwise the result remains revocation-pending;
- model/mode/plan/reasoning/root changes update host/thread settings only after
  revisioned receipts and never mutate a running generation's frozen config;
- composer becomes ready after the terminal cursor barrier for an ordinary
  foreground operation, or after the committed handoff/admission-available
  cursor for a successfully backgrounded operation;
- a pre-Requested rejection restores readiness from its closed uncommitted
  result, while a queued reservation remains visible and cancellable;
- boundary tests reject direct lifecycle-owner imports, supplemented by behavior
  tests rather than treated as proof alone.

### ACP

- new/load/prompt/cancel and update ordering;
- a prompt line read before cancel is reserved and bound before that cancel is
  admitted even when the 0.10.4 handler scheduler would run them in reverse;
- standard cancel targets its bound prompt operation after background handoff
  and cannot cancel a newer foreground operation from another attachment;
- complete runtime materialization plus complete capability-valid wire baseline
  before load response, with live updates after it;
- ordered Text/ResourceLink/embedded text plus URI/root/authority checks;
- explicit unsupported content/capability errors;
- all five interaction mappings and late-response cancellation;
- exclusive/shared response-route epochs, detach/transfer revocation, and
  persisted unavailable dispositions;
- success/cancel/budget/failure/restart/shutdown/background/degraded terminal
  mapping, including writable-connection
  `Terminal(Shutdown) -> PromptResponse(Cancelled)`;
- model-token versus agent-turn versus unrepresentable budget mapping;
- prompt transport retirement emits at most one correlated error and never a
  later `PromptResponse` for the same id;
- oversize inbound request/notification/interaction-response/capability-response
  classes obey JSON-RPC directionality;
- every filesystem/terminal capability method covers failure before write,
  possible delivery, response loss/oversize, cancellation, and disconnect with
  no automatic side-effect retry; known terminal leases release or report
  cleanup ambiguity, and unknown create identity remains visible degraded state;
- crash after durable `DeliveryPossible` but before response/lease registration
  recovers the exact ambiguous capability fact and never replays the call;
- extension negotiation, unknown-version fallback, and extensionless gap/load;
- cancel followed immediately by a fresh prompt;
- `write_all` plus `flush` acknowledgement, slow/failed writer, stdio EOF host
  shutdown, embedded EOF detach, and no surviving child task;
- ordinary clients work without Orca extensions.

### Server and compatibility

- existing `session_server_contract` behavior remains semantically compatible;
- frozen `v0.2.50` differential corpus preserves normalized field/error shapes,
  request correlation, and per-request ordering;
- interaction response validation happens before durable consumption;
- deterministic legacy response-id retry is idempotent without a server waiter;
- typed mapping requires no JSONL round trip;
- resume returns a coherent snapshot/replay projection;
- a correlated gap ends the existing RPC, while an uncorrelated asynchronous
  gap seals the stream and requires reconnect without a fabricated response;
- command/exec permission remains correctly fenced after thread permission
  manager separation;
- JSONL legacy readers load old sessions and new versioned records safely.

## Verification Gates

Each phase follows RED, focused GREEN, ownership/deletion review, and a
semantically complete commit. Shared changes additionally run the relevant
cross-client suites before the next phase.

The deterministic local release gate is:

```sh
cargo fetch --locked
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked --offline -- --test-threads=1
cargo clippy --workspace --all-targets --locked --offline
git diff --check
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
node scripts/release/test-real-api-e2e.mjs
node scripts/release/test-real-api-tui-approval-recovery.mjs
node scripts/release/test-real-api-acp-surface.mjs
node scripts/release/test-real-api-server-approval-recovery.mjs
node scripts/release/verify-version-sync.mjs
npm --prefix site run build
npm --prefix site run check:seo
```

The three focused `test-real-api-*` scripts are Phase 6 deliverables. They use
fake deterministic peers to prove their harness failure oracles before real
credentials are used. Each real harness accepts `--bin <absolute-path>`, exits
nonzero on a missing expected event or surviving child process, and prints one
stable success sentinel only after cleanup.

With `DEEPSEEK_API_KEY` configured, pre-tag validation runs:

```sh
cargo build --release --locked --bin orca
ORCA_BIN="$PWD/target/release/orca"
test -x "$ORCA_BIN"
node scripts/release/real-api-e2e.mjs --orca-bin "$ORCA_BIN" --skip-build
node scripts/release/real-api-tui-approval-recovery.mjs --bin "$ORCA_BIN"
node scripts/release/real-api-acp-surface.mjs --bin "$ORCA_BIN"
node scripts/release/real-api-server-approval-recovery.mjs --bin "$ORCA_BIN"
```

These prove TUI PTY approval/cancel/reattach, ACP new/load/prompt/cancel and
baseline ordering, and server interaction recovery respectively. The existing
real API harness continues to prove provider, Goal, CLI, history, and server
baselines; its fake self-test is not substituted for a credentialed run.

The tag workflow is updated before release to run:

```sh
cargo test --workspace --all-targets --locked -- --test-threads=1
```

on a clean runner. It must not rely on `--offline` without a preceding fetch.
The release workflow contains an `npm-auth` preflight job that requires a
non-empty `NPM_TOKEN` and successful `npm whoami`. The formal `release` job
depends on `npm-auth` as well as build/version, so missing or revoked npm
credentials cannot leave a public GitHub Release with no npm package. A separate
manual token-check workflow may remain diagnostic but is not the release gate.

The npm job publishes the exact five `.tgz` files produced by the staging step,
native variants first and the main package last. It never repacks the package
directories during publish. The same immutable tarballs are uploaded as
workflow artifacts and then GitHub Release assets.

The workflow adds a final `verify` job with
`needs: [release, npm, npm-release-assets]` and
`if: ${{ always() && github.ref_type == 'tag' }}`. Its first step asserts that
all three `needs.*.result` values are exactly `success`; dependency failure or
skip therefore fails verification instead of silently skipping the job.
`npm-release-assets` uploads all five npm tarballs before this job starts; the
complete published verifier never runs earlier inside the `npm` job. The final
Actions job has no DeepSeek credential requirement and verifies only public
metadata, assets, npm payloads, digests, installability, and binary version. A
missing asset is failure, not success.

`verify-version-sync.mjs` checks at least `Cargo.toml`, `Cargo.lock`,
`npm/orca/package.json`, pinned README install examples, `site/src/shared.ts`,
both changelog summary maps, `site/index.html` softwareVersion, sitemap/release
URLs, the production roadmap, and `docs/releases/vX.Y.Z.md`.

`verify-published.mjs` is extended to prove:

- the tag and Release target the pushed `main` release SHA and are published,
  non-draft, and non-prerelease;
- four native archives, four `.sha256` files, and five npm tarballs exist on
  the Release;
- each Release npm tarball's SHA-512 matches npm registry `dist.integrity` for
  its exact package version, and its complete packed file tree matches the
  registry tarball;
- downloaded native archive checksums pass and the current-platform archive
  runs `orca --version`;
- `@blade-ai/orca@X.Y.Z` and the four native versions
  `@blade-ai/orca@X.Y.Z-darwin-arm64`,
  `@blade-ai/orca@X.Y.Z-darwin-x64`,
  `@blade-ai/orca@X.Y.Z-linux-arm64`, and
  `@blade-ai/orca@X.Y.Z-linux-x64` are public;
- the main npm package's four optional-dependency alias keys point exactly to
  `npm:@blade-ai/orca@X.Y.Z-<platform>`;
- each unpacked npm native payload binary has the same SHA-256 as the binary in
  its corresponding GitHub native archive, preventing an older same-version npm
  payload from passing a version-only check;
- a clean temporary npm installation exposes the expected binary and version.

After Actions publication verification succeeds, release completion requires a
separate credentialed command:

```sh
node scripts/release/verify-public-real-api.mjs \
  --version X.Y.Z \
  --require-deepseek-api-key
```

That script fails when `DEEPSEEK_API_KEY` is absent, installs the exact public
npm version into a clean temporary directory, resolves its absolute binary
path, and passes that path to the TUI, ACP, and server real-API harnesses. It is
not an optional branch of the no-credential Actions verifier. Completion also
requires every job in the tag Release workflow to report `success`, the Pages
workflow to succeed for the release SHA, and public checks that the
homepage/changelog show the version, the Release link is correct, and the
public `install.sh` installs that tag.

Existing warnings and generated-cache churn are separated from regressions.
No broad release claim is made from a focused test alone.

## Rejected Alternatives

### Continue patching TUI and ACP projectors

This is locally cheaper but preserves multiple operation, interaction,
recovery, and terminal state machines. It cannot satisfy the ownership goal.

### Route TUI through ACP

ACP does not express Orca's durable cursor, snapshot barrier, exact operation
terminal, Goal lifecycle, background ownership, or complete interaction model.
Grok Build demonstrates the terminal and replay compensation this creates.

### Add an independent SurfaceActor

Another actor would compete with `ThreadActor` for command and finalization
authority. A passive resident hub gives attach/replay without a second control
plane.

### Expose `EventEnvelope<Value>` as the shared protocol

Raw payload parsing lets every client silently define a different contract and
cannot provide exhaustive mapping or compile-time deletion pressure.

### Cherry-pick the experimental p13 surface wholesale

That branch provides useful reducer and race-test ideas, but its in-memory
sequencer, independent identities, manual snapshot reconstruction, and dual
terminal paths do not satisfy this contract. Only isolated tests and pure
reducer ideas may be ported after review.

## Completion Criteria

The refactor is complete only when all of the following are true:

1. Runtime owns one globally unique operation identity, explicit per-generation
   Started barriers, and one operation terminal barrier for every migrated
   client operation.
2. TUI, ACP, and server cannot admit, cancel, resolve, recover, or terminalize
   work outside the runtime surface command authority.
3. Fresh attach, cursor replay, detach, slow consumers, and restart behavior
   pass the stated failure tests.
4. Every interaction kind uses the fenced broker; first recorded durable or
   ephemeral atomic response wins without stale consumption or stale-authority
   reuse.
5. TUI and ACP deletion lists and authority-matrix forbidden imports are absent
   from production ownership paths.
6. Server pending/active ownership is removed while its released wire remains
   compatible.
7. Raw payload parsing remains only in explicit legacy compatibility readers or
   encoders, not client semantic paths.
8. Focused, workspace, PTY, real DeepSeek, ACP, server, npm, site, and release
   gates pass.
9. Architecture, harness, user, and release documentation describe the shipped
   contract accurately.
10. The tag equals the pushed `main` release SHA; all Release and Pages jobs,
    the formal GitHub Release/assets/checksums, npm package matrix, installed
    public binary, site/install script, and public-artifact TUI/ACP/server
    behavior are independently verified.
