# P0.3e TUI Runtime Host Migration Plan

- Status: P0.3e1 through P0.3e4c complete; integrated into main for v0.2.30
- Date: 2026-07-15
- Base: `35c6361c1cbb49e557f8738b6b1feef88af1b9d8` (latest `origin/main` at P0.3e4c validation)
- Branch: `codex/tui-runtime-host-migration`
- ADR: `docs/architecture/adr/0005-runtime-host-operation-control-plane.md`

## Goal

Move the production TUI onto the process-owned `RuntimeHost` control plane
without regressing interrupt, approval, user-input, MCP elicitation, goal,
history, workflow, or background-current-turn behavior. The finished path must
have one owner for the live thread and operation lifecycle, one typed command
path, and no detached TUI worker.

## User Value

The migration is complete only when it changes reliability that a TUI user can
observe:

- interrupt addresses the exact operation shown as running and cleanup finishes
  before the operation becomes terminal;
- closing the TUI cancels and joins the agent, provider, interaction, and
  background-current-turn work it started;
- approval, user-input, and MCP responses cannot reach a cancelled or replaced
  generation;
- backgrounding a provider turn still lets the user submit new work, but the
  background request remains cancellable, visible, and joined at shutdown;
- TUI, server, and headless turns use the same DeepSeek streaming, retry,
  compaction, tool, hook, and terminal behavior.

## Current Structural Problems And Evidence

This is an architecture defect rather than a local shortcut bug.

1. `crates/orca-tui/src/app.rs` spawns `_agent_handle` and never joins it. The
   UI owns a separate `OperationCancellation`, sends `UserAction::Interrupt`,
   and assumes the agent loop will eventually observe the token.
2. `TuiConversationSession` in `crates/orca-tui/src/bridge.rs` directly owns a
   mutable `RuntimeThread` and exposes session, lifecycle, conversation,
   history writer, task registry, extension, compaction, and turn mutation to
   the surface.
3. `run_agent_for_tui_with_notification_queue` in
   `crates/orca-tui/src/agent_runner.rs` assembles its own provider, compaction,
   usage, hook, tool, approval, event, task, and turn loop beside the canonical
   `RuntimeThread -> ThreadTurnExecutor` path.
4. `spawn_provider_stream` and `spawn_background_provider_completion` use
   untracked `thread::spawn` calls. Backgrounding transfers only a receiver;
   neither provider nor completion join ownership is retained for shutdown.
5. TUI interaction adapters borrow one `Receiver<UserAction>` plus
   `RefCell<VecDeque<UserAction>>`. They work only because the surface loop and
   interaction wait run synchronously on the same thread; they cannot be owned
   by an actor generation or fenced independently.
6. Background-current-turn is a real product constraint, not incidental code.
   It releases the conversation for another submit while a cloned provider
   request continues. Migrating directly to the current host would either
   block this workflow or leave a second lifecycle owner around the actor.

## Target Ownership And Module Boundaries

### TUI UI Thread

The render/input thread owns presentation state and typed `TuiCommand`
submission only. It does not own a cancel token, runtime thread, provider
worker, pending-interaction waiter, or join handle.

### TUI Runtime Controller

One controller owns the TUI action dispatcher, one `RuntimeHost`, the current
`RuntimeThreadHandle`, and the current `OperationHandle`. It remains responsive
while an operation runs and routes:

- submit, interrupt, resume, steer, and shutdown to actor commands;
- approval, permission, user-input, and MCP responses to the interaction
  broker;
- idle thread mutations through explicit typed actor commands;
- operation completion back to presentation without treating interrupt
  acknowledgement as terminal completion.

The controller itself has one joined lifetime owned by the TUI entrypoint.

### Runtime Host And Thread Actor

`RuntimeHost` owns the process runtime and background-task supervisor. One
`ThreadActor` permanently owns the live `RuntimeThread`, persistent event
sequence, active operation, generation cancel scope, and operation join.

The actor executes the canonical runtime turn path. A surface-specific
executor wrapper is acceptable only as a migration step if it removes an
existing owner and has an explicit deletion gate; it is not the final design.

### TUI Interaction Broker

The broker owns pending typed interaction records. Each record carries request
identity, operation/generation fence, response channel, and terminal state.
The controller validates the fence through the active operation before
delivering a response. Cancellation and shutdown remove the record and wake
the waiter. No response path reads or mutates a raw generation integer.

### Background Provider Tasks

Background-current-turn is modeled as an explicit runtime-owned task handoff.
The supervisor owns the provider request, completion consumer, cancellation
scope, and join. Returning the `RuntimeThread` to the actor does not detach
that work. Task completion publishes one typed terminal and persists usage or
pending continuation exactly once. Host shutdown cancels and joins all such
tasks.

## External Compatibility

The migration preserves:

- CLI arguments, TUI key bindings, status transitions, transcript rendering,
  approval choices, user-input and MCP flows;
- background-current-turn, task foreground/stop, goals, workflow
  notifications, manual compaction, remember, backtrack, model changes, and
  session picker behavior;
- server/JSONL methods and events;
- persisted session, goal, task, workflow, and permission formats;
- DeepSeek model selection, streaming deltas, retry, reasoning, compaction,
  tool-call, hook, usage, and budget behavior.

Any intentional behavior change needs a separate acceptance test and release
note. No wire or persistence migration is part of P0.3e.

## Migration Sequence

### P0.3e1: Joined TUI Worker Supervision (Complete)

1. Add RED behavior tests proving TUI exit joins the agent controller and that
   foreground/background provider tasks expose cancel and join ownership.
2. Replace the ignored agent handle with an owned runtime-controller lifetime.
3. Replace detached provider and completion spawns with a bounded supervisor
   that cancels and joins every admitted task.
4. Preserve background-current-turn behavior and task/usage terminals.

Checkpoint result:

- `TuiAgentRuntime` owns cancellation, shutdown admission, the agent join, and
  one bounded `TuiTaskSupervisor`;
- shutdown closes operation admission before a non-blocking controller wake,
  so a full action mailbox cannot block exit or admit a late operation;
- foreground provider tasks join locally, while background provider and
  completion ownership transfers together into the supervisor;
- auto-memory uses the cancellable provider path and is supervised;
- background stop and provider completion settle atomically, with stop winning
  and already incurred usage preserved even when the provider terminal was
  queued before the completion consumer observed stop;
- event receiver disconnect and terminal restoration happen before runtime
  join on normal exit and unwinding.

The temporary TUI loop, direct session ownership, operation cancellation,
borrowed interaction handlers, and detached workflow notification watchers
remain intentionally visible deletion targets for P0.3e2 through P0.3e4.

This slice retains the existing TUI turn loop temporarily, but removes its
detached lifetime. It is independently valuable because TUI shutdown no longer
leaks work, and it creates the transfer boundary required by the host.

### P0.3e2: Typed Interaction Broker And Dispatcher (Complete)

This slice removes the raw action mailbox from every interaction waiter. One
joined `TuiActionDispatcher` owns the raw UI receiver for its entire lifetime.
It routes ordinary agent commands into a bounded internal mailbox while
handling interaction responses, interrupt, shutdown, and
background-current-turn directly. A full agent-command mailbox therefore
cannot prevent a response or interrupt from reaching the active operation.

One `TuiInteractionBroker` is the live waiter authority. Its key contains the
`OperationId`, request id, and interaction kind. `RuntimePendingInteractionStore`
remains only a session/UI projection during this slice; insertion or removal
there never delivers a response. A `TuiOperationScope` activates one operation
fence and clears all of that operation's waiters on drop. Broker shutdown
closes admission, removes all waiters, and wakes them before the dispatcher and
agent threads are joined.

Migration order:

1. Add broker and dispatcher behavior tests for duplicate registration, stale
   operation responses, request-id reuse, interrupt, shutdown, and command
   mailbox backpressure.
2. Route typed approval, permission, user-input, and MCP responses through the
   broker. Events and responses carry the operation fence that was active when
   the waiter was registered.
3. Replace borrowed `Receiver`/`RefCell` interaction adapters with owned
   `Send + Sync` handlers backed by broker waiters.
4. Move background-current-turn polling off the raw receiver and onto the
   operation controller.
5. Store user-input versus MCP elicitation explicitly in `AppState`, then fix
   composer submit and `Ctrl-C` behavior against that typed state.

Temporary state: the synchronous TUI agent loop and UI-side
`OperationCancellation` remain until P0.3e3, but neither owns or consumes the
raw action receiver. P0.3e2 is incomplete until production interaction handlers
have no lifetime parameter, no `Receiver<UserAction>`, and no
`RefCell<VecDeque<UserAction>>`; `poll_background_current_turn_for_tui` must not
read an action receiver; and `TuiAgentRuntime` must join both dispatcher and
agent threads.

Implementation checkpoint:

- one joined `TuiActionDispatcher` exclusively owns the raw UI receiver and
  routes ordinary commands through a bounded mailbox plus bounded backlog;
- interaction responses, interrupt, shutdown, and background-current-turn
  bypass ordinary command backpressure through `TuiOperationController`;
- `TuiInteractionBroker` is the only response authority and atomically
  deactivates an interrupted or completed operation before waking its waiters,
  so cancelled operations cannot admit a late waiter;
- approval, permission, user-input, and MCP handlers are owned `Send + Sync`
  values, while `RuntimePendingInteractionStore` remains projection-only;
- the old unfenced approval/user-input/MCP action variants and runtime-event
  approval projection are deleted; actionable UI events carry a typed
  operation/request/kind fence;
- `AppState` distinguishes user input from MCP elicitation, composer submit
  sends the matching typed response, terminal completion clears stale
  interaction presentation, and `Ctrl-C` interrupts both waiting states;
- background-current-turn polling reads only operation-scoped control, and
  the background/approval agent-loop behavior tests now run through the real
  dispatcher boundary.

Final verification followed a fresh `git fetch origin` and rebase onto the
latest `origin/main` (a no-op because the validated base remained current).
Focused tests passed with `orca-core` 144/144, `orca-runtime` 769/769 plus
runtime-host 18/18 and task-output 12/12, and `orca-tui` 508/508. The serial
workspace all-targets gate and workspace Clippy passed with only pre-existing
warnings. The release real-API harness tests passed, and the live DeepSeek
smoke verified provider summary, CLI, history replay and repair, server and
thread memory, active-turn resume/control, thread read and metadata updates,
list filters and search, and turn/item pagination within the `$0.02` budget.

### P0.3e3: Actor-Owned TUI Session And Operation Control

Structural problem and evidence:

- `bridge::TuiConversationSession` still stores `runtime: RuntimeThread` and
  exposes mutable conversation, history writer, lifecycle, cost, extension,
  and compaction access to the surface loop;
- `agent_loop_thread_with_registry` owns `Option<TuiConversationSession>` and
  performs create/resume, idle mutation, turn execution, and terminal handling
  synchronously beside the runtime actor used by server and headless;
- the UI shortcut path receives `OperationCancellation` directly, so UI state
  can acknowledge cancellation without an actor-fenced `OperationId` or joined
  `OperationCompletion` terminal;
- resume/fork startup and goal/background continuation rely on direct session
  mutation, so merely wrapping submit in `RuntimeHost` would leave two owners.

Target ownership and module boundary:

- `TuiAgentRuntime` owns one `RuntimeHost`, one joined TUI command controller,
  and at most one `RuntimeThreadHandle`; the handle is command authority, not a
  mutable session lease;
- one `ThreadActor` permanently owns the TUI `RuntimeThread`, including resumed
  history, MCP registry, conversation, writer, lifecycle, usage, task registry,
  and thread extensions;
- the TUI controller retains only the current `OperationHandle` identity and
  completion cell. Interrupt sends the handle's typed operation command, then
  UI terminal state follows the joined completion rather than acknowledgement;
- session startup, idle reads/mutations, manual compaction, and turns use typed
  host commands. No closure command or type-erased mutable session escape is
  accepted as the final boundary;
- during P0.3e3 only, a `TuiThreadOperationExecutor` may invoke the existing TUI
  turn kernel while borrowing `&mut RuntimeThread` inside the actor-owned task.
  It may not store the thread, cancel token, join handle, or a second operation
  state. P0.3e4 deletes this executor when the canonical runtime turn and
  background handoff replace the old TUI kernel.

TUI user value:

- `Ctrl-C` targets the exact actor operation shown as active and the next turn
  cannot start until cancelled work has returned the thread to the actor;
- resume/fork, goals, manual compaction, task controls, and model changes keep
  one conversation and usage fact instead of racing a surface-owned session;
- TUI shutdown joins the host-owned operation and thread actor before terminal
  restoration completes.

External compatibility remains unchanged for CLI arguments, TUI keys and
status transitions, transcript/history format, goals/tasks/workflows,
server/JSONL protocol, MCP behavior, model selection, provider semantics, and
background-current-turn behavior.

Migration checkpoints:

1. **P0.3e3a - actor session capabilities.** Add behavior-first runtime-host
   support for preloaded/resumed startup with an injected MCP registry and the
   typed idle session reads/mutations required by the TUI. Prove commands are
   rejected while an operation owns the thread and preserve session identity,
   conversation, task registry, and usage.
2. **P0.3e3b - production TUI host controller.** Start the TUI thread in one
   host, route submits and manual compaction through `OperationHandle`, wait on
   `OperationCompletion`, and keep the dispatcher responsive while the actor
   operation runs. Preserve goal and background continuation behavior.
3. **P0.3e3c - delete surface owners.** Replace `TuiConversationSession` with a
   handle/projection boundary, delete UI/controller `OperationCancellation`,
   and replace source-shape ownership assertions with actor behavior tests.

P0.3e3a implementation checkpoint:

- `RuntimeThreadStartRequest` moves the config, title, optional preloaded
  transcript, and optional initialized MCP registry into host startup, so
  resume/fork construction occurs before the actor becomes externally visible;
- `RuntimeThreadMutation` provides typed model, pinned-context, goal-context,
  and skill-context mutation, while backtrack remains a typed result command;
- idle snapshots expose an immutable cloned conversation plus aggregate usage,
  completion error, active workflow state, and lifecycle task identity without
  leaking mutable session ownership;
- `InteractiveSession` now retains resumed usage as a separate baseline from
  the current foreground `CostTracker`. Actor projections return baseline plus
  current usage, while history `Usage` records remain post-resume foreground
  snapshots and therefore cannot double-count the persisted `UsageBaseline`;
- mutations, backtrack, and snapshots are rejected with the active
  `OperationId` while the operation task owns the thread.

Final verification followed a fresh fetch and no-op rebase onto the current
`origin/main`. Runtime-host 21/21 and `orca-runtime` 769/769 passed after the
rebase, followed by the serial workspace all-targets gate and workspace Clippy
with only pre-existing warnings. The release harness checks and live DeepSeek
E2E passed within the `$0.02` budget, including provider summary, CLI, history
replay and repair, server/thread memory, active-turn resume/control, metadata,
list filters/search, and turn/item pagination.

P0.3e3b implementation checkpoint:

- production `TuiAgentRuntime` now starts and owns one `RuntimeHost`; the TUI
  command loop retains only a cloneable `RuntimeHostHandle` and at most one
  `RuntimeThreadHandle`, while the actor permanently owns the live
  `RuntimeThread`;
- submit, manual compaction, and approved background continuation start typed
  `HostedOperationKind` requests and are controlled through `OperationHandle`;
  model, pinned context, goal context, backtrack, snapshots, task registry, and
  session identity use typed actor commands or immutable handles;
- `TuiOperationController` production mode owns only the current
  `Arc<OperationHandle>`. Interrupt and background-current-turn requests that
  arrive during activation are retained and applied after installation, while
  shutdown interrupts the installed handle and wakes activation waiters;
- `TuiThreadOperationExecutor` borrows the actor-owned thread through
  `TuiHostedConversationSession`. TUI pending-interaction projection and the
  shared usage ledger live in thread `ExtensionData`, so they survive turns
  without becoming a second thread owner;
- each operation has a fenced result and joined event relay. Nonterminal events
  stream immediately, but `SessionCompleted` and `Compacted` are held until
  `OperationCompletion` has joined and the controller has cleared the matching
  operation; provider/background/workflow events that outlive the foreground
  operation route directly to the UI channel instead of retaining the relay;
- background goal usage uses shared exactly-once accounting, including the
  completion-before-foreground race. Hosted admission failures publish a
  terminal UI event, failed session startup rejects and restores the submitted
  prompt, preloaded history is retained on startup failure, and command-mailbox
  overflow rejects user submissions with their original prompt;
- TUI shutdown now has a combined behavior test proving an active hosted
  operation is cancelled and joined before runtime teardown completes.

The remaining P0.3e3c deletion targets are explicit: remove the test-only
`TuiConversationSession` and local `OperationCancellation` path, collapse the
dual hosted/legacy `TuiSession` adapter into the actor-borrowed boundary, delete
the legacy agent-loop helpers, and replace the remaining source-shape ownership
tests with behavior tests. `TuiThreadOperationExecutor` and the TUI turn kernel
remain temporary until P0.3e4 replaces them with the canonical runtime turn.

P0.3e3c slice contract:

- Structural problem and evidence: production turns use actor-owned
  `RuntimeHost`, but tests can still construct a surface-owned
  `TuiConversationSession`, start a local `OperationCancellation`, or execute
  `agent_loop_thread_with_registry`. This leaves two ownership models and lets
  cancellation, interaction, task, goal, and persistence tests pass without
  exercising the production control plane. A source-text assertion additionally
  protects the obsolete owner instead of observable behavior.
- Target ownership and module boundary: `TuiSession<'_>` is the only TUI
  turn-kernel adapter and always borrows an actor-owned or test-owned
  `RuntimeThread`; it never constructs or stores one. The
  `TuiOperationController` is host-backed only and tracks an
  `Arc<OperationHandle>`. High-level TUI tests start the same hosted controller
  as production. Low-level kernel tests may construct a local `RuntimeThread`,
  but must borrow it through the single hosted adapter for the duration of the
  call.
- TUI user value: cancellation, shutdown, interaction fencing, goal resume,
  and task/background behavior are now verified against the same actor and
  joined-operation lifecycle users run. This removes false confidence from a
  second test-only loop and protects the user-visible guarantee that a terminal
  state is emitted only after the addressed operation has completed cleanup.
- External compatibility: no CLI, key binding, TUI event, JSONL, persistence,
  goal/task/workflow, MCP, model, or DeepSeek provider behavior changes in this
  slice.
- Migration order and temporary state: first add reusable hosted and borrowed
  kernel test harnesses; then migrate behavior tests; then delete
  `TuiConversationSession`, local controller construction, compatibility
  cancellation traits, and legacy app loop/session/goal helpers. The
  `TuiThreadOperationExecutor` and TUI-specific turn kernel remain the only
  intentional P0.3e4 transition state.
- Acceptance: no `TuiConversationSession`, `TuiOperationScope`, local
  `OperationCancellation`, `agent_loop_thread`, or source-shape assertion for
  the deleted session/cancellation owner remains in `orca-tui`; production and
  high-level tests use the hosted runtime; low-level tests use the borrowed
  adapter; focused TUI tests, the serial workspace gate, workspace Clippy, real
  DeepSeek harness, and PTY TUI smoke pass.
- Final deletion gate for this slice: there is one session adapter, one
  operation-control authority, and no test-only path that can own a live TUI
  conversation or acknowledge cancellation independently of
  `OperationCompletion`.

P0.3e3c implementation checkpoint:

- `TuiSession<'_>` is the single session adapter. It borrows `&mut
  RuntimeThread` for one kernel call and cannot construct, retain, or replace
  the actor-owned thread;
- `TuiOperationController` is host-backed only and retains at most one
  `Arc<OperationHandle>`. `TuiTurnControl` carries the operation fence but no
  cancel token;
- the local `OperationCancellation`, `TuiOperationScope`, compatibility
  interrupt implementation, legacy session constructors, legacy app
  agent-loop/session/goal helpers, and dual hosted/local controller branches
  are deleted;
- production and high-level app tests run through `RuntimeHost`,
  `TuiAgentRuntime`, and `TuiThreadOperationExecutor`; low-level kernel tests
  own a `RuntimeThread` only in the test harness and borrow it through
  `TuiSession` for each call;
- synchronous, silent, and asynchronous child-agent execution explicitly
  inherits the current `TuiTurnControl`, preserving the addressed operation
  fence through nested work;
- hosted behavior tests now prove interrupt, shutdown/join, operation
  replacement rejection, interaction cleanup, background continuation, goal
  accounting, and terminal ordering without constructing the deleted owner or
  asserting its source-text shape.

Verification ran after a fresh fetch confirmed `origin/main` was still
`35c6361c1`, so the branch was already based on the current main. Formatting,
`cargo check -p orca-tui`, TUI 510/510, `orca-runtime` 769/769 plus runtime-host
21/21 and task-output 12/12, the serial workspace all-targets gate, and
workspace Clippy completed successfully; Clippy retained the repository's
non-deny warning baseline. The release harness contract and complete real
DeepSeek E2E passed within the `$0.02` budget, covering provider summary,
CLI/history replay and repair, server/thread memory, active-turn resume/control,
metadata, list filters/search, and turn/item pagination. A separate real PTY
TUI run through the production hosted path returned `ORCA_TUI_HOSTED_OK`,
displayed live context and usage (`7.0k` tokens, `$0.0031`), and restored the
terminal through the normal double-`Ctrl-C` exit flow.

P0.3e3 is incomplete until production TUI code contains no directly owned
`RuntimeThread`, `InteractiveSession`, `OperationCancellation`, operation cancel
token, or operation join handle outside `RuntimeHost`; every terminal UI
transition must be attributable to a matching `OperationCompletion`.

### P0.3e4: Canonical Turn And Background Handoff

Structural problem and evidence:

- the actor-owned `TuiThreadOperationExecutor` still invokes
  `run_agent_for_tui_with_event_factory`, which duplicates provider streaming,
  compaction, tool scheduling, subagents, hooks, usage, task settlement, and
  terminal mapping beside canonical `ThreadTurnExecutor`;
- canonical turn execution accepts permission, user-input, and MCP handlers,
  but interactive tool approval is still hard-coded to
  `RuntimeConfigApprovalHandler`. A TUI operation-fenced approval broker cannot
  therefore own canonical approval waits, and an interrupted wait is currently
  surfaced as a generic tool failure;
- canonical runtime events do not yet carry every TUI projection needed for
  turn/context state, streaming shell output, and committed file-change
  previews;
- background-current-turn still transfers a TUI-owned provider worker and
  completion closure into `TuiTaskSupervisor`, while canonical provider turns
  are synchronous and cannot hand an in-flight request back to the host;
- canonical non-waiting workflow execution leaves completion joins outside the
  host-owned task model. Deleting the TUI loop before those ownership gaps are
  closed would regress task visibility and shutdown cleanup.

Target ownership and module boundary:

- `ThreadTurnExecutor` is the only provider/tool/compaction/hook turn kernel for
  TUI, server, and headless surfaces;
- `HostedGenerationHandlers` is the typed operation-generation interaction
  bundle and owns approval, permission, user-input, and MCP handlers. Each
  handler is fenced to the generation cancel scope and cannot resolve a newer
  operation;
- canonical runtime emits the typed events required by TUI presentation. The
  TUI adapter only projects runtime events and never re-executes tool or
  lifecycle logic;
- the runtime host supervisor owns provider and workflow background tasks,
  including cancellation, join, terminal publication, persisted usage, and
  resumable continuation state;
- background-current-turn is a typed host handoff of an in-flight provider
  phase, not a detached worker or cloned session. Returning the thread to the
  actor cannot orphan the provider request.

TUI user value:

- approvals, permission prompts, user input, MCP elicitation, interrupts, and
  terminal state use the same operation fence even after the canonical switch;
- DeepSeek streaming, retry, compaction, tool-call recovery, hooks, subagents,
  budget enforcement, and persistence cannot drift between TUI and server;
- backgrounded turns and workflows remain visible, cancellable, foregroundable,
  and joined at shutdown with exactly one usage and terminal settlement.

External compatibility remains unchanged for CLI arguments, key bindings,
TUI status/transcript behavior, server/JSONL shapes, persistence, goals, tasks,
workflows, MCP, model routing, and provider semantics. New runtime events may be
added only as backward-compatible typed events needed to preserve existing TUI
presentation.

Migration checkpoints:

1. **P0.3e4a - generation-scoped canonical approval.** Add
   `RuntimeApprovalHandler` to `HostedGenerationHandlers`, `ThreadTurnRequest`,
   and the grouped runtime interaction/capability contexts. Canonical tool
   execution uses the injected handler when present and maps an interrupted
   generation-scoped wait to `RunStatus::Cancelled`. Behavior tests must prove
   an injected approval controls execution, interrupt wakes the wait, and a
   stale generation handler cannot resolve a later operation. This is an
   internal reliability slice that removes the first blocker to running the TUI
   on the canonical kernel; it does not add a compatibility loop.
2. **P0.3e4b - canonical foreground TUI turn.** Move goal-tool exposure,
   main-session task/backtrack metadata, context state, committed diff, shell
   output, and TUI event projection into typed canonical request/event
   boundaries. Run production foreground TUI turns through
   `ThreadTurnExecutor` while retaining the explicit background handoff as the
   final temporary path.
3. **P0.3e4c - host-owned background handoff and deletion.** Move in-flight
   provider and workflow task ownership into the runtime host, route
   background continuation through canonical `RuntimeTurnContinuation`, then
   delete `TuiThreadOperationExecutor`, the TUI provider/tool loop,
   `TuiTaskSupervisor`, provider worker facade, and remaining source-shape tests
   that protect those paths.

P0.3e4a acceptance:

- the approval handler travels through one named generation interaction bundle
  instead of a parallel TUI-only parameter;
- request-level fallback preserves CLI/server behavior when no generation
  handler is installed;
- an allowed injected approval executes the requested tool once and publishes
  one successful terminal;
- interrupting a generation-scoped approval wait publishes one cancelled tool
  terminal and one cancelled operation terminal, then the next operation can
  use a fresh handler;
- focused runtime-host, controller/tool, and TUI interaction tests pass before
  the full shared-runtime gate.

P0.3e4a implementation checkpoint:

- `HostedGenerationHandlers` now carries approval beside permission,
  user-input, and MCP handlers. `HostedTurnRequest` resolves the active
  generation handler first, preserves the request-level handler as the
  CLI/server compatibility fallback, and leaves config-backed approval as the
  canonical final fallback;
- `ThreadTurnRequest` installs the selected approval handler into
  `RuntimeTurnInteractionState`. The handler then travels through the named
  step capability and normal tool-turn interaction snapshots to
  `ToolExecutionContext`; no second approval state or TUI-specific execution
  branch was introduced;
- canonical interactive approval maps an interrupted handler or cancelled
  generation token to one cancelled tool terminal and `RunStatus::Cancelled`
  instead of relabelling the wait as a generic failed tool;
- runtime-host behavior tests prove generation injection, request fallback,
  interrupt wakeup, one cancelled tool terminal, operation cancellation, and a
  fresh handler on the next operation in the same actor-owned thread;
- the Mock provider now scopes tool-result completion to the current user turn
  rather than all historical turns. This repairs the shared multi-turn
  validation path that previously hid the second operation behind a stale tool
  result; real provider behavior and external protocol shapes are unchanged.

Verification followed a fresh `git fetch origin`; `origin/main` remained
`35c6361c1`, so the branch was already based on current main and rebase was a
no-op. Formatting and diff checks passed, followed by `orca-provider` 164/164,
runtime-host 24/24, `orca-runtime` 769/769, and `orca-tui` 510/510. The serial
workspace all-targets gate and workspace Clippy completed successfully with the
repository's existing non-deny warning baseline. The release harness contract
and complete real DeepSeek E2E passed within the `$0.02` budget, including
provider summary, CLI, history replay and repair, server/thread memory,
active-turn resume/control, metadata, list filters/search, and turn/item
pagination.

P0.3e4b slice contract:

- **Current structural problem and evidence.** The canonical runtime emits
  compaction, usage, and tool terminal events, but it does not emit the current
  context budget or live shell output, and its tool terminal drops the
  committed `FileChangePreview`. `HostedTurnRequest` stores goal-tool,
  task-description, and backtrack metadata without forwarding those semantics
  into `ThreadTurnRequest`. Finally, provider execution is synchronous, so a
  background-current-turn request cannot release the actor-owned thread without
  being mislabeled as a completed operation.
- **Target ownership and boundary.** `ThreadTurnRequest` owns prompt placement
  and tool-schema policy; canonical runtime events own every presentation fact
  needed by the TUI; provider execution returns a typed completed-or-suspended
  result; the runtime host owns operation terminal publication. The TUI only
  projects events and requests a typed background handoff.
- **TUI user value.** The canonical path keeps the context meter accurate,
  preserves committed file diffs, streams shell output before completion,
  keeps workflow notifications out of backtrack history, exposes goal tools
  only during goal mode, and retains non-blocking background-current-turn
  behavior without orphaning provider work.
- **External compatibility.** CLI arguments, TUI keys and transcript behavior,
  server/JSONL event consumers, persisted messages/tasks, and provider request
  semantics remain compatible. `context.updated` and `tool.output.delta` are
  additive event types; `tool.call.completed.diff` is an additive payload
  field.
- **Migration and temporary state.** P0.3e4b1 adds canonical event fidelity;
  P0.3e4b2 moves prompt/tool/task metadata into typed canonical requests;
  P0.3e4b3 adds the typed provider suspension result and switches production
  foreground execution. The existing TUI background completion supervisor is
  the only intentional temporary owner until P0.3e4c moves it into the host.
- **Acceptance.** Each checkpoint starts with behavior RED tests and passes its
  focused crates before commit. P0.3e4b as a whole additionally passes the
  serial workspace gate, workspace Clippy, the real DeepSeek harness, and a
  production PTY TUI smoke covering streamed output, context/usage, interrupt,
  and normal terminal restoration.
- **Deletion gate.** P0.3e4b3 removes production foreground calls to
  `run_agent_for_tui_with_event_factory`. P0.3e4c then deletes
  `TuiThreadOperationExecutor`, `ProviderStreamTask`, the duplicated TUI
  provider/tool/compaction loop, and `TuiTaskSupervisor` after the host owns the
  final background handoff.

P0.3e4b1 acceptance:

- canonical execution emits one `context.updated` before each provider turn
  with the same wire-token budget used for compaction;
- canonical bash execution emits ordered `tool.output.delta` chunks before its
  `tool.call.completed` terminal and preserves cancellation/join behavior;
- canonical edit/write terminals carry the committed preview captured by the
  tool result rather than rereading later workspace state;
- the TUI runtime projection maps all three typed facts without TUI-side tool
  execution or filesystem reads;
- `orca-core`, `orca-runtime`, runtime-host, and `orca-tui` focused tests pass.

P0.3e4b1 checkpoint result:

- canonical context pressure now emits `context.updated` before `turn.started`,
  using the same wire-token calculation and soft limit as compaction;
- canonical bash execution projects observed stdout/stderr chunks in order as
  `tool.output.delta` while retaining the shell session's existing
  cancel/wait/join owner;
- edit and write completion events project the committed `FileChangePreview`
  already captured by the tool result, with no post-execution filesystem read;
- the TUI runtime adapter maps context budget, live tool output, and committed
  diff directly from typed runtime events;
- formatting and diff checks passed, followed by `orca-core` 147/147,
  `orca-runtime` 770/770, runtime-host 27/27, task-output 12/12, and `orca-tui`
  513/513. Focused all-targets Clippy completed successfully with only the
  repository's existing non-deny warning baseline.

P0.3e4b2 slice contract:

- **User value.** User turns remain backtrackable, workflow notifications stay
  out of user backtrack history, goal turns expose the goal lifecycle tools,
  and the task panel shows one main-session task with the correct label and
  terminal state after the canonical foreground switch.
- **Architecture value.** `ThreadTurnRequest` becomes the single owner of
  prompt placement and root tool-schema mode. `ThreadActor` allocates an
  opt-in main-session task from request metadata, assigns the same task id to
  runtime lifecycle, and the canonical turn emits task projection events. The
  temporary TUI executor no longer defines these semantics independently.
- **External compatibility.** Default CLI/server requests retain ordinary user
  prompt placement, the standard root tool schema, and no additional
  main-session task projection. TUI request construction explicitly opts into
  goal tools and main-session task tracking. Persisted message and task formats
  remain unchanged.
- **Migration state.** This slice forwards the typed semantics through the
  default canonical executor and keeps the temporary TUI executor consuming the
  same `HostedTurnRequest` until P0.3e4b3 switches production foreground turns.
  The old TUI prompt insertion, tool-schema construction, and task lifecycle
  helpers remain deletion targets, not parallel long-term authorities.
- **Acceptance.** Behavior tests first fail because canonical hosted workflow
  notifications are appended as ordinary user turns, goal requests use the
  standard schema, and task metadata produces no canonical task lifecycle.
  After implementation: prompt placement is one typed mutually exclusive mode;
  a pinned workflow notification cannot replace the preceding user backtrack
  target; goal mode alone exposes goal tools; an opted-in main-session task uses
  one actor-assigned id across task registry, lifecycle, running event, and
  terminal event; default hosted requests do not add task events; focused
  `orca-provider`, `orca-runtime`, runtime-host, and `orca-tui` tests pass.
- **Deletion gate.** P0.3e4b3 must remove the production TUI call sites that
  still insert prompts, construct goal schemas, and start/finish main-session
  tasks inside `run_agent_for_tui_with_event_factory`.

P0.3e4b2 checkpoint result:

- RED behavior tests proved the three missing canonical semantics: goal mode
  had no tool-policy constructor, a pinned workflow notification replaced the
  preceding backtrackable user turn, and task metadata emitted no canonical
  task lifecycle events;
- `ThreadTurnRequest` now owns mutually exclusive backtrackable-user,
  pinned-user, and existing-turn prompt placement plus standard-versus-goal
  root tool mode;
- `ThreadActor` creates an opt-in main-session task before execution, assigns
  its id to runtime lifecycle and the canonical request, and canonical task
  events use that same registry id through running and terminal settlement;
- `PreparedThreadTurn` is the shared writer/event-factory execution path, so
  task settlement, provider execution, verifier behavior, and terminal commit
  no longer have two canonical implementations;
- task event delivery failure settles the registry task before returning the
  canonical I/O error, while terminal task settlement preserves stop-wins
  semantics through `apply_main_session_terminal_update`;
- production TUI request construction opts every visible turn into task
  tracking, exposes and accounts goal tools only while a goal is active, and
  preserves workflow notifications as pinned rather than backtrackable input;
- the temporary TUI executor adopts the actor-created task id instead of
  creating a second main-session task. `TuiMainSessionTaskStart` is an explicit
  P0.3e4b3 deletion target, not a second long-term lifecycle authority;
- formatting and diff checks passed, followed by `orca-provider` 164/164,
  `orca-runtime` 771/771, runtime-host 31/31, task-output 12/12, and `orca-tui`
  515/515. Focused all-targets Clippy and the combined runtime/TUI all-targets
  check completed successfully with only the repository's existing non-deny
  warning baseline.

P0.3e4b3 slice contract:

- **User value.** Foreground TUI turns use the same DeepSeek streaming,
  compaction, retry, hooks, tools, subagents, permissions, persistence, usage,
  and terminal semantics as server/headless turns. `Ctrl-B` still releases the
  TUI while the exact in-flight provider request continues, and `Ctrl-C` still
  cancels and joins the addressed foreground generation.
- **Architecture value.** `orca-provider` owns one transferable streaming-call
  handle whose drop path cancels and joins the provider worker. Canonical
  runtime propagates one typed completed-or-provider-suspended outcome from the
  provider step through the turn kernel. The TUI executor installs fenced
  canonical interaction handlers and projects canonical events; it no longer
  runs a second foreground provider/tool/compaction loop.
- **External compatibility.** CLI/server/headless callers keep blocking turn
  behavior and existing event/persistence shapes. TUI keys, interaction UI,
  task panel, goal accounting, workflow notification ordering, background
  approval continuation, and aggregate budget behavior remain unchanged.
- **Migration state.** A suspended provider handle is temporarily transferred
  to the existing bounded `TuiTaskSupervisor`, which retains current background
  task settlement and foregrounding behavior. P0.3e4c moves that owner into the
  runtime host and deletes the supervisor-facing provider completion path.
- **Acceptance.** RED tests first prove the provider API cannot transfer an
  in-flight stream, canonical turns cannot return suspension, and production
  TUI foreground turns still enter `run_agent_for_tui_with_event_factory`.
  After implementation: the provider stream handle preserves ordered delivery,
  cancellation, and exactly-once join; a canonical hosted slow provider turn
  returns a typed suspension without committing session/task terminal state;
  the suspended request can finish, be stopped, or be foregrounded once; TUI
  approvals, permission requests, user input, and MCP elicitation use the
  operation fence; normal foreground turns publish canonical context, stream,
  tool, task, usage, and terminal events; focused provider/runtime/TUI tests
  pass.
- **Deletion gate.** Production `HostedOperationKind::Turn` contains no call to
  `run_agent_for_tui_with_event_factory`, and `TuiMainSessionTaskStart` is
  deleted. `ProviderStreamTask` and the duplicate TUI foreground loop remain
  only as test/legacy deletion targets until P0.3e4c removes their final
  background consumers.

P0.3e4b3 checkpoint result:

- `orca-provider` now owns one transferable `ProviderStreamingCall`. Ordered
  deliveries acknowledge consumption before the worker advances, normal
  completion joins exactly once, and explicit cancellation, callback panic, or
  drop cancels and joins the provider worker. The blocking streaming facade now
  consumes the same transferable implementation instead of owning a second
  worker lifecycle.
- canonical runtime propagates `ThreadTurnOutcome::ProviderSuspended` from the
  provider step through the turn kernel, controller, and `RuntimeThread`
  without committing task, lifecycle, verifier, or session terminal state.
  Production `HostedOperationKind::Turn` runs this canonical executor and
  installs operation-fenced approval, permission, user-input, and MCP
  elicitation handlers after hosted activation.
- `Ctrl-B` transfers the exact in-flight provider request into the bounded TUI
  supervisor. Behavior tests prove the canonical request can complete in the
  background, be stopped and joined once, or return to the foreground once;
  cancellation does not emit a misleading error and does not poison the next
  generation.
- operation-scoped event relay holds canonical terminal events until the actor
  operation is joined. Runtime errors, host admission failures, and background
  handoff failures therefore publish one fenced terminal after operation
  cleanup, while shutdown cancels and joins the active operation.
- production no longer calls `run_agent_for_tui_with_event_factory`,
  `TuiMainSessionTaskStart` is deleted, and the old TUI foreground
  provider/tool/compaction loop is test-only. `ProviderStreamTask`, that
  duplicate test loop, and `TuiTaskSupervisor` remain explicit P0.3e4c deletion
  targets rather than long-term parallel owners.
- a fresh `git fetch origin` confirmed `origin/main` remained `35c6361c1`, so
  the branch was already based on current main and rebase was a no-op. Formatting
  and diff checks passed, targeted all-targets Clippy completed with only the
  repository's existing non-deny warning baseline, and the serial all-targets
  gate passed: `orca-provider` 167/167, `orca-runtime` 769/769, runtime-host
  32/32, task-output 12/12, and `orca-tui` 523/523.
- the complete P0.3e4b workspace gate, workspace Clippy, real DeepSeek harness,
  and production PTY TUI smoke remain the next checkpoint before P0.3e4c or any
  push/release decision.

P0.3e4b stage validation result:

- after the P0.3e4b3 commit, `cargo test --workspace --all-targets --
  --test-threads=1` completed with exit 0, including `orca-runtime` 769/769,
  runtime-host 32/32, task-output 12/12, and `orca-tui` 523/523. `cargo clippy
  --workspace --all-targets` also completed with exit 0 and only the existing
  non-deny warning baseline;
- `node scripts/release/test-real-api-e2e.mjs` passed, followed by the complete
  `$0.02` real DeepSeek harness. Provider summary, CLI, malformed-history replay
  and compatibility repair, server submit, thread memory, active-turn
  resume/control, thread read/metadata, list filters/search, and turn/item
  pagination all reached their expected successful sentinels;
- a production TUI run in a real PTY streamed canonical reasoning and answer
  deltas, returned `ORCA_TUI_CANONICAL_OK`, displayed `context 94%`, `6.7k
  tokens`, and `$0.0003`, then restored cursor, keyboard mode, bracketed paste,
  mouse modes, and the alternate screen through the normal double-`Ctrl-C`
  exit path;
- a second production PTY run interrupted a long live DeepSeek stream with
  `Ctrl-C`, released the foreground operation, accepted a fresh submit, returned
  `ORCA_TUI_AFTER_INTERRUPT_OK`, updated usage to `9.0k tokens` and `$0.0023`,
  and restored the terminal on exit. The current product retains the interrupted
  user message in conversation history, so a terse follow-up
  can be interpreted as additive rather than superseding; sharper cancellation
  supersession UX is a separate product candidate, not a reason to add a second
  lifecycle or conversation owner during this migration;
- a final `git fetch origin` confirmed `origin/main` remained `35c6361c1`; the
  branch stayed linearly based on current main. P0.3e4b is therefore complete.
  P0.3e4c must now move background handoff ownership into the runtime host and
  delete `TuiThreadOperationExecutor`, `ProviderStreamTask`, the duplicate test
  foreground loop, and `TuiTaskSupervisor` before any push or release decision.

P0.3e4c slice contract:

- **Current structural problem and evidence.** `ThreadActor` moves its entire
  `ThreadActorState` into one blocking generation, while
  `ThreadOperationExecutor::run_turn` returns only `io::Result<RunStatus>`.
  The host therefore cannot distinguish a completed turn from a canonical
  provider suspension or regain the thread while retaining the in-flight
  provider request. `TuiThreadOperationExecutor` currently receives
  `ThreadTurnOutcome::ProviderSuspended`, marks the task backgrounded, clones
  the history writer and usage ledger, transfers budget admission, and spawns
  `spawn_background_provider_task_completion` in `TuiTaskSupervisor`. That
  completion path separately owns provider cancellation/join, task settlement,
  history writes, usage and goal accounting, foreground replay, approval
  continuation, and completion notices. Host shutdown joins only actor
  generations, so this TUI supervisor remains a second lifecycle authority.
- **Target ownership and module boundary.** A typed host execution result
  distinguishes a completed operation from a background handoff. The handoff
  returns `ThreadActorState` immediately and moves the provider suspension plus
  its typed settlement context into a bounded runtime-host background task.
  The host owns admission, cancellation, join, task terminal, history and
  usage settlement, pending continuation, and completion publication. The
  actor can admit the next foreground turn while the background task remains
  addressable through the existing task id. Foregrounding or approved
  continuation consumes one `RuntimeTurnContinuation` exactly once and starts
  a normal actor generation; no background worker borrows or mutates the live
  `RuntimeThread`.
- **TUI user value.** `Ctrl-B` releases the exact provider request without
  making it unowned; the user can immediately submit the next turn, stop the
  background task, or foreground it once. Closing the TUI cancels and joins
  both active and background provider/workflow work before terminal restore.
  A stopped, completed, foregrounded, or approval-pending task publishes one
  terminal and contributes usage once, even when those actions race.
- **External compatibility.** CLI arguments, TUI keys and transcript/status
  behavior, task ids and task panel actions, goals, workflow notifications,
  server/JSONL events, persisted history/task/usage formats, MCP behavior,
  model selection, and DeepSeek request semantics remain unchanged. New
  runtime-host result and background-task types are internal typed boundaries;
  no wire or persistence migration is introduced.
- **Migration order and temporary state.** First add runtime-host RED behavior
  tests for suspension ownership, actor reuse, bounded admission, stop,
  foreground, shutdown join, exactly-once terminal/usage settlement, and stale
  continuation rejection. Then replace the executor's `RunStatus` result with
  the typed completed-or-backgrounded result and add the host supervisor. Next
  move canonical provider completion and `RuntimeTurnContinuation` resumption
  into runtime, route TUI task controls and event projection through host APIs,
  and move non-waiting workflow joins under the same host-owned task boundary.
  Finally delete the TUI executor, supervisor, provider facade, duplicate loop,
  and source-shape tests in the same complete slice. No compatibility adapter
  introduced here may remain after the deletion checkpoint.
- **Acceptance.** Runtime-host behavior tests prove: a suspended provider is
  host-owned while the actor accepts a second turn; task stop and host/thread
  shutdown cancel and join it; capacity is bounded and closes before shutdown;
  completion, stop, foreground, and approval races settle task/history/usage
  once; foreground and approved continuation are one-shot and stale requests
  cannot resume a newer operation; the next foreground generation remains
  usable after every background terminal. TUI behavior tests prove background,
  foreground, stop, approval continuation, goal accounting, workflow notices,
  interrupt, and shutdown through the production host path. Focused provider,
  runtime-host, runtime, and TUI tests pass before the serial workspace gate,
  workspace Clippy, real DeepSeek harness, and production PTY validation.
- **Final deletion gate.** `orca-tui` contains no
  `TuiThreadOperationExecutor`, `TuiTaskSupervisor`, `TuiTaskSpawner`,
  `ProviderStreamTask`, `run_agent_for_tui_with_event_factory`, or background
  provider completion loop. `TuiAgentRuntime` owns one `RuntimeHost` and no
  separate task supervisor. Runtime host is the only owner of active and
  background operation joins, and there is one typed continuation path and one
  source of task, history, conversation, and usage settlement.

P0.3e4c implementation checkpoint:

- `ThreadTurnOutcome` and `ThreadOperationOutcome` now carry an opaque typed
  background-workflow batch for both completed and provider-suspended turns.
  `wait=false` no longer drops workflow handles; direct non-host callers join
  them synchronously, while the runtime host adopts them into its bounded
  background registry.
- `RuntimeThreadHandle::launch_workflow` is the typed saved-workflow command.
  It preserves the existing TUI tool/task/workflow event projection, starts the
  next foreground turn immediately, and gives host shutdown cancellation and
  join ownership. Capacity exhaustion rejects before creating a workflow task.
- Workflow workers publish progress and exactly one completion/failure event,
  request task stop on host cancellation, and are joined during actor/host
  shutdown. Tests cover turn-launched and saved workflows, next-turn reuse,
  terminal-event uniqueness, capacity exhaustion, and fast shutdown join.
- `TuiThreadOperationExecutor`, `TuiTaskSupervisor`, `TuiTaskSpawner`, the TUI
  provider/tool/workflow/subagent loop modules, the old saved-workflow watcher,
  and their source-shape tests are deleted. Production `/workflow` now submits
  a typed `HostedWorkflowRequest` to the runtime host.
- The runtime lifecycle operation ID and the `TaskRegistry` main-session task
  ID are now distinct. This fixes a server regression where persisted request
  ID `turn-1` was incorrectly used to settle the registry task. The server
  regression test now has a two-second deadline and reports the hosted
  operation terminal instead of polling forever.
- Focused validation passed: runtime-host 39/39, hosted TUI behavior 28/28,
  approved background tool continuation 2/2, runtime interaction adapter 6/6,
  and the complete `orca-runtime` all-targets gate.
- The serial workspace all-targets gate and workspace Clippy completed with
  exit 0; Clippy retained only the repository's existing non-deny warning
  baseline. Formatting and diff checks also passed.
- The release harness contract and complete `$0.02` real DeepSeek E2E passed,
  covering provider summary, CLI, history replay and repair, server submit and
  thread memory, active-turn resume/control, thread read and metadata, list
  filters/search, and turn/item pagination.
- Production PTY validation streamed a live `deepseek-v4-flash` response and
  returned `ORCA_TUI_RUNTIME_HOST_OK` with live context state. A second run
  interrupted a long live stream with `Ctrl-C`, accepted a fresh submit, and
  returned `ORCA_TUI_AFTER_RUNTIME_HOST_INTERRUPT_OK`. Both runs restored the
  cursor, keyboard mode, bracketed paste, mouse modes, and alternate screen
  through the normal consecutive double-`Ctrl-C` exit path.
- A final feature-worktree fetch confirmed `origin/main` remained `35c6361c1`.
  The feature branch was then rebased over the clean local-main Goal-notice
  commit `28fb80987`, reran TUI 389/389, the serial workspace gate, Clippy, and
  the real DeepSeek harness, and fast-forwarded main. Main release validation
  repeated the locked/offline workspace and Clippy gates, site/SEO and release
  helpers, the complete real DeepSeek harness, and an `orca 0.2.30` production
  PTY run returning `ORCA_TUI_V030_OK` with normal terminal restoration.

## Slice Acceptance Criteria

### P0.3e1

1. Dropping or explicitly shutting down the TUI runtime cancels and joins the
   agent loop.
2. Every foreground or background provider worker has a traceable owner and a
   joined terminal.
3. Background-current-turn still returns control before provider completion,
   accepts a next submit, records one task terminal, and accounts usage once.
4. Worker admission is bounded and shutdown cannot create a new worker.
5. Focused TUI worker/background tests, full TUI tests, workspace gate, and
   Clippy pass without new warnings.

### P0.3e2

1. Interaction handlers are owned `Send + Sync` values with no borrowed action
   receiver or `RefCell` queue.
2. Duplicate request keys cannot replace a live waiter. A response is delivered
   only when operation id, request id, and interaction kind all match the
   active waiter; a delayed response cannot hit a reused id in a new operation.
3. Interrupt wakes approval, permission, user-input, and MCP waiters. Shutdown
   closes admission, wakes every waiter, rejects late requests/responses, and
   joins the dispatcher.
4. Interaction response and interrupt routing remains responsive while the
   bounded ordinary-command mailbox is full.
5. MCP composer submit sends an MCP response, and `Ctrl-C` during approval or
   input wait cancels the active interaction instead of entering idle quit
   confirmation.
6. Background-current-turn no longer consumes the raw UI action receiver.

### P0.3e3

1. One actor permanently owns the TUI `RuntimeThread`.
2. TUI interrupt targets an `OperationId`; terminal UI state follows joined
   `OperationCompletion`.
3. TUI entrypoint and controller own no operation cancel token or operation
   join handle outside the host.
4. Session mutations and reads use typed actor commands and preserve history,
   goals, tasks, usage, and model behavior.

### P0.3e4

1. TUI/server/headless execute through one canonical turn path.
2. Background provider tasks are host-owned, cancellable, joined, and have one
   terminal and one usage commit.
3. The old TUI provider/tool loop and direct execution helpers are absent.
4. TUI focused behavior tests, full serial workspace gate, workspace Clippy,
   and real DeepSeek TUI/headless/server smokes pass.

## Final Deletion Gate

P0.3e is incomplete while production TUI code contains any of the following:

- an unjoined agent, provider, or completion `thread::spawn`;
- `OperationCancellation` as the TUI turn-control authority;
- direct mutable ownership of `RuntimeThread` or `InteractiveSession`;
- a TUI-specific provider/tool/compaction/hook agent loop;
- a detached TUI workflow notification watcher owning a workflow launch;
- borrowed interaction handlers that consume the controller action receiver;
- background work without cancel, join, and exactly-one terminal ownership;
- two long-lived sources for active operation, task, usage, or conversation
  state.

Intermediate commits must state which remaining item they remove next. No
release is cut until a target stage has passed its deletion gate and provides
the user-visible reliability improvement.

## Verification Ladder

Each slice runs RED behavior tests first, then its focused tests. A slice that
touches shared runtime, task lifecycle, event protocol, persistence, or
provider ownership also runs:

```text
cargo test --workspace --all-targets -- --test-threads=1
cargo clippy --workspace --all-targets
node scripts/release/test-real-api-e2e.mjs
node scripts/release/real-api-e2e.mjs --max-budget 0.02 --timeout-ms 300000
```

P0.3e1 passed this ladder with `orca-core` 143/143, `orca-runtime` 769/769,
and `orca-tui` 506/506 tests; the serial workspace all-targets gate; workspace
Clippy with only pre-existing warnings; the release real-API smoke; and the
complete DeepSeek harness. The real harness verified provider summary,
CLI/history replay and repair, server submit/thread memory, active-turn
resume/control, thread read/metadata, list filters/search, and turn/item
pagination.

Before each commit and before final integration, fetch and rebase the latest
`origin/main`, then rerun affected focused tests. Final integration and any
release happen only from the clean main worktree.
