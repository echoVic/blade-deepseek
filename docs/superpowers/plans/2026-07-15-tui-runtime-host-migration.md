# P0.3e TUI Runtime Host Migration Plan

- Status: Active; P0.3e1 complete, P0.3e2 next
- Date: 2026-07-15
- Base: `35c6361c1cbb49e557f8738b6b1feef88af1b9d8`
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

### P0.3e2: Typed Interaction Broker And Dispatcher

1. Replace borrowed `Receiver`/`RefCell` interaction adapters with owned,
   thread-safe handlers.
2. Keep the controller dispatch loop active while a turn or interaction wait
   runs.
3. Fence permission, user-input, and MCP responses by operation generation.
4. Prove interrupt and shutdown wake every pending waiter.

### P0.3e3: Actor-Owned TUI Session And Operation Control

1. Create/resume/fork the TUI conversation inside one `RuntimeHost`.
2. Route TUI turns, manual compaction, idle mutations, task queries, and
   terminal waits through typed handles.
3. Delete `TuiConversationSession` ownership of `RuntimeThread` and delete UI
   `OperationCancellation`.
4. Preserve goal and background continuation task identity and usage.

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
2. Responses are delivered only to the matching active generation.
3. Duplicate, stale, cancelled, and shutdown responses fail closed and wake
   waiters.
4. TUI interrupt remains responsive while approval, permission, user-input, or
   MCP waits are active.

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
