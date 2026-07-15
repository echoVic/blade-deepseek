# P0.3e TUI Runtime Host Migration Plan

- Status: Active; P0.3e1, P0.3e2, P0.3e3a, and P0.3e3b complete; P0.3e3c next
- Date: 2026-07-15
- Base: `35c6361c1cbb49e557f8738b6b1feef88af1b9d8` (latest `origin/main` at P0.3e3a validation)
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

Verification ran after a fresh fetch confirmed `origin/main` was still
`35c6361c1`, so the branch was already rebased on the current main. Clean
`cargo check -p orca-tui`, hosted/action-dispatcher focused tests, TUI 518/518,
`orca-runtime` 769/769 plus runtime-host 21/21 and task-output 12/12, the serial
workspace all-targets gate, and workspace Clippy all passed; Clippy reported
only the repository's pre-existing warning set. The release harness contract
and complete real DeepSeek E2E passed within the `$0.02` budget, covering
provider summary, CLI/history replay and repair, server/thread memory,
active-turn resume/control, metadata, list filters/search, and turn/item
pagination. A real 120x40 PTY TUI run through the production hosted path
returned `ORCA_TUI_HOSTED_OK`, displayed live usage/context state, and exited
cleanly through the normal double-`Ctrl-C` flow.

P0.3e3 is incomplete until production TUI code contains no directly owned
`RuntimeThread`, `InteractiveSession`, `OperationCancellation`, operation cancel
token, or operation join handle outside `RuntimeHost`; every terminal UI
transition must be attributable to a matching `OperationCompletion`.

### P0.3e4: Canonical Turn And Background Handoff

1. Move background provider admission and completion under the runtime host.
2. Run TUI turns through the same canonical executor as server and headless.
3. Delete the TUI provider/tool/compaction/hook loop and its provider worker
   facade.
4. Replace detached TUI workflow notification watchers with host-owned task
   events and joined ownership.
5. Remove temporary adapter APIs and source-shape tests that protect deleted
   ownership.

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
