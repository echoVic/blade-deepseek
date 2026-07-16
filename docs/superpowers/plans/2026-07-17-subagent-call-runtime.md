# P1.2c Runtime-Owned Subagent Calls Plan

- Status: implemented, full-gate and real-DeepSeek verified; public v0.2.36
  release pending
- Base: `354ec9668297d184cec702569a7a9de170656bb0` (`v0.2.35`)
- Branch: `codex/subagent-call-runtime-p12c`

## User Value, Architecture Value, And Slice Acceptance

P1.2c gives foreground subagents one runtime-owned invocation lifetime. For a
TUI user, interrupting a synchronous delegated task must reach every started
child, wait for provider, tool, process, worktree, and thread cleanup, close each
visible subagent row once, and return the session to an idle state that accepts
the next prompt. A child panic must become a truthful failed or indeterminate
subagent terminal instead of escaping as a RuntimeHost generation panic.

Async delegation has a different user contract. Launching an async subagent
must create one durable background task, expose that task to the TUI immediately,
and return control to the foreground turn. Cancelling a later foreground turn
must not stop the durable task. The task remains explicitly stoppable through
`task_stop` and the TUI task panel.

The architecture value is one final owner for synchronous subagent execution:
owned invocation inputs, admission, started state, child cancellation scope,
worker spawn, panic classification, usage return, worktree completion, join, and
exactly-once terminal selection. Child-agent modules continue to own the actual
agent loop. Tool validation, approval, pre/post hooks, event publication,
conversation/history recording, and sibling closure remain at the canonical
turn boundary.

This slice is accepted only when behavior tests prove all of the following:

- cancellation before synchronous admission never starts the child and returns
  a cancelled-before-start tool result;
- interrupting one synchronous child signals its child loop and RuntimeHost
  waits for cleanup and join before the operation terminal or next prompt;
- interrupting a synchronous batch signals every started child, prevents later
  admission, joins every worker, and records results in provider request order;
- natural child completion racing cancellation wins as the observed result;
- a worker panic after admission produces one indeterminate tool terminal and
  one failed subagent lifecycle terminal without panicking RuntimeHost;
- worktree completion runs after success, failure, cancellation, and child panic,
  preserving dirty worktrees and removing clean worktrees;
- started-event or later event delivery failure cannot return while a child
  worker, process, transport, or worktree owner remains alive;
- schema validation, usage merging, lifecycle turn counts, and batch terminal
  folding remain identical for TUI, server, headless, and JSONL callers;
- async launch does not emit an unpaired foreground `subagent.started` event;
- async launch publishes the existing typed `task.status.updated` event for the
  durable subagent task so the TUI task panel can show and stop it immediately;
- the async worker cannot mutate the persisted task until the parent registry
  has atomically adopted its real PID and running state;
- foreground interrupt after async launch does not stop the background task;
- CLI arguments, tool schema, `subagent.started` / `subagent.completed` payloads
  for synchronous calls, `task_stop`, persisted task shape, and provider request
  behavior remain compatible.

## Structural Problem And Evidence

`subagent_execution.rs` currently owns three incompatible lifecycle paths.

1. `execute_subagent_tool` creates and publishes a lifecycle, then runs a
   synchronous child loop inline on the RuntimeHost generation. It owns
   worktree cleanup, schema validation, usage merging, result formatting, and
   terminal publication. A child panic escapes this boundary, and a worktree
   guard held by the unwinding frame has no cleanup owner.
2. `execute_subagent_batch` creates its own `thread::scope`, per-child lifecycle,
   worker handles, joins, panic conversion, event-error handling, usage merging,
   and result folding. This is a second invocation runtime embedded in the turn
   policy module. Its result formatting duplicates the single-call path.
3. Async mode delegates to `subagent_async_worker`, which launches a durable
   process and persists through `TaskRegistry`. However, the shared
   `execute_subagent_tool` publishes `subagent.started` before it checks the
   mode, then returns after launch without a matching `subagent.completed`.
   The TUI therefore creates a foreground subagent row that remains running even
   though ownership moved to a background task.

The async launch handshake also has a data-consistency race. The parent writes a
placeholder worker PID, spawns the process, and only then adopts the real child.
The worker immediately loads its own `TaskRegistry`, marks the task running, and
may complete. The parent adoption path then persists its older in-memory task
map with the real PID. A fast worker can therefore have running or terminal task
state overwritten by the parent's stale queued record. The same parent registry
does not automatically observe the worker process's later in-memory updates.

Existing tests cover batch joining after an event error and batch panic
conversion, but single and batch ownership are not tested through one boundary.
Many `crates/orca-runtime/src/lib.rs` assertions inspect source strings and
module placement rather than cancellation, join, terminal, and cleanup behavior.

This is an ownership and data-consistency defect, not a missing cancellation
check. Adding `catch_unwind` to the inline path or one more async event special
case would leave the duplicate runtimes, stale launch handshake, and split
terminal authority intact.

## Reference Findings

Codex routes spawned agents through session-scoped `AgentControl`. The tool
handler validates and presents the call, while the control boundary owns
registry state, execution capacity, spawn identity, status, interrupt routing,
and cleanup. Orca should adopt that ownership property without copying Codex's
async task graph or thread model.

Claude Code's async agent lifecycle owns progress, abort handling, terminal
transition, notification, and cleanup together. It deliberately distinguishes
foreground request cancellation from explicit background-agent termination and
commits terminal task state before optional cleanup that may hang. Orca should
preserve the foreground/background distinction while retaining its durable
process worker and `task_stop` semantics.

## Target Ownership And Module Boundaries

Add `runtime_subagent_call.rs` as the subagent implementation of the existing
`RuntimeToolCallRuntime` concept. It may define an `impl RuntimeToolCallRuntime`
outside `runtime_tool_call.rs`, but it must not introduce a second public
scheduler or a wrapper around the old batch owner.

The owned synchronous invocation contains:

- the effective `ToolRequest` and parsed `SubagentRequest`;
- the child `RunConfig` snapshot, including remaining foreground budget;
- cwd, project instructions, memory, MCP registry, hooks, workflow IPC, and the
  child-agent executor;
- isolation and output-schema inputs;
- one runtime lifecycle and started task snapshot;
- optional worktree guard and its final `WorktreeOutcome`;
- one child result, usage tracker, typed terminal, and join handle.

`RuntimeToolCallRuntime` owns synchronous admission in provider order. It checks
parent cancellation before starting a child, creates the lifecycle, asks the
canonical publication boundary to publish the typed started snapshot, and only
then spawns the worker. A publication failure prevents that invocation from
starting, stops later admission, and still joins all earlier workers.

Each worker owns child-agent implementation execution only. It creates a private
event factory and sink because nested child deltas are intentionally disabled,
passes the invocation-scoped cancellation token into the existing canonical
agent loop, catches child execution panics, finishes worktree ownership, selects
one typed terminal, and returns usage plus the terminal lifecycle snapshot.

`subagent_execution` remains the turn integration boundary. It owns validation,
pre/post hooks, public requested/completed events, conversation and durable
history recording, sibling closure, and provider-order result folding. It must
not own threads, joins, child loops, panic classification, worktree cleanup,
schema validation, or a second result-formatting path after this slice.

Async mode remains a durable process task, not a child of the foreground
invocation runtime. `subagent_async_worker` owns worker configuration, process
spawn, child-agent execution, durable progress/terminal persistence, usage, and
worktree handoff. `TaskRegistry` owns the process handle, stop request, process
tree termination, wait, and terminal stop state.

The async launch boundary returns a typed `AsyncSubagentLaunchOutput` containing
the tool result and accepted task summary. The turn publication boundary emits
the existing `task.status.updated` event for that summary. It does not emit a
foreground synchronous subagent lifecycle event.

## Cancellation, Panic, And Terminal Rules

- Parent cancellation before synchronous admission returns cancelled before
  start and does not publish `subagent.started`.
- Parent cancellation after admission signals each invocation's child token.
  The runtime waits for the observed child terminal and joins the worker.
- An observed child result wins a race with cancellation. Cancellation alone
  must not relabel completed child side effects.
- A panic before the started boundary is failed before start. A panic after the
  started boundary is indeterminate for the tool result and failed for the
  subagent lifecycle, with guidance to inspect external state before retrying.
- Worktree completion runs outside the child panic frame. Cleanup failure after
  child execution produces a failed-after-start terminal and preserves the child
  error or output as diagnostic context.
- Event publication failure never executes an unstarted child. It does not
  detach already-started work; the runtime joins before returning the error.
- Each accepted synchronous tool request produces at most one
  `subagent.started`, exactly one `subagent.completed`, exactly one
  `tool.call.completed`, one conversation result, and one durable result.
- Async task cancellation remains explicit through `task_stop`; foreground
  operation cancellation does not signal its durable worker.

## Async Adoption And Persistence Rules

The parent registry is the launch owner until the child process is adopted.
`TaskRegistry::adopt_subagent_worker` must atomically persist the real worker PID,
running status, and start timestamp before the worker may update progress or
terminal state.

The worker must wait for persisted ownership matching both its task id and its
own PID, then reload the adopted task state before calling `mark_running` or
writing progress. It must fail closed on an adoption timeout. This creates one
ordered handoff:

`queued reservation -> spawned process -> parent adoption -> worker updates`.

P1.2c does not introduce a second IPC terminal ledger or make foreground turn
events the durable task truth. Cross-process live progress observation,
lease/fencing, stale-owner takeover, and task-wide publication belong to P1.4.
The current persisted task schema remains unchanged.

## External Compatibility

- Keep CLI arguments, TUI keys and foreground flow, server methods, JSONL event
  names and wrappers, tool names and schemas, provider call ids, and persisted
  record fields unchanged.
- Keep synchronous `subagent.started` and `subagent.completed` payloads,
  lifecycle task ids, status labels, output schema errors, worktree messages,
  and provider-order batch results unchanged.
- Async calls stop emitting an invalid unpaired foreground
  `subagent.started`. They additionally emit the already-public
  `task.status.updated` event for the accepted background task.
- Keep `subagent_status` and `task_stop` identifiers and payloads unchanged.
- Keep async tasks independent from later foreground operation cancellation.
- Do not change workflow child execution, saved workflows, background provider
  handoff, goal continuation, or task persistence format in this slice.

## Migration Sequence And Temporary State

1. Commit this architecture plan from the verified `v0.2.35` baseline.
2. Add RED behavior tests for synchronous pre-admission cancellation, in-flight
   cancellation and join, completion race, single panic isolation, batch cleanup,
   worktree cleanup after panic, started-event failure, and RuntimeHost
   interrupt/next-submit recovery.
3. Add RED async tests proving the unpaired foreground lifecycle, immediate TUI
   task projection, adoption ordering, fast-worker terminal preservation, and
   foreground/background cancellation separation.
4. Implement the owned synchronous subagent invocation in
   `runtime_subagent_call.rs` and route single plus batch execution through it.
5. Move schema validation, worktree completion, lifecycle terminal selection,
   usage return, and result formatting into the runtime-owned output model.
6. Replace `execute_subagent_batch` scoped-thread ownership and the inline single
   child loop. Keep `subagent_execution` only as the turn integration boundary.
7. Make async parent adoption atomic, gate worker updates on the adopted PID,
   return a typed task summary, and publish `task.status.updated` instead of a
   foreground subagent lifecycle.
8. Delete source-shape tests that protect the old subagent owner and replace
   them with compiler and behavior coverage.
9. Update roadmap and release notes, rebase latest main, run focused runtime,
   TaskRegistry, TUI, server, and contract tests, then the serial workspace gate,
   Clippy, site/release helpers, and real DeepSeek verification.

The implementation commit must not leave both inline and runtime-owned
synchronous subagent execution in production. The async process worker remains
the durable background owner and is related through typed launch/adoption
outputs, not wrapped in a foreground worker owner.

## Stage Validation

### Plan checkpoint

- this document records structural evidence, target ownership, TUI value,
  compatibility, cancellation and persistence rules, migration, acceptance, and
  deletion targets;
- the branch and worktree are based on fetched `main` at public `v0.2.35`.

### RED checkpoint

- a single injected child panic escapes the current per-subagent boundary;
- async launch produces `subagent.started` without `subagent.completed`;
- a fast async worker can update persisted state before parent adoption writes
  its stale queued snapshot;
- tests inspect tasks, joins, cleanup markers, task summaries, public events,
  conversation/history results, and next-turn admission rather than source text.

### Runtime owner checkpoint

- focused runtime tests cover single and batch pre-admission cancel, in-flight
  cancel, completion race, panic, event failure, worktree cleanup, terminal once,
  provider ordering, and usage merge;
- RuntimeHost and server behavior tests prove interrupt waits for child cleanup
  and the next turn starts only after join;
- async launch/adoption tests prove PID fencing, fast completion preservation,
  TUI task projection, explicit `task_stop`, and foreground cancellation
  independence;
- synchronous CLI/TUI/JSONL contract tests retain current payloads and ordering.

### Release checkpoint

- `cargo test --workspace --all-targets -- --test-threads=1` passes;
- `cargo clippy --workspace --all-targets` passes with no new warnings;
- the established rustfmt baseline remains unchanged;
- site, SEO, npm staging, public-verifier, and real-API helper tests pass;
- real DeepSeek provider, CLI, history, server, interruption, and subagent
  regressions pass;
- roadmap, plan status, changelog, and `v0.2.36` release notes describe the final
  subagent owner and the remaining P1.4 task-supervisor boundary;
- main, tag, GitHub Release, npm package, executable smoke, release assets, and
  public changelog are verified before worktree cleanup.

Focused implementation validation completed on 2026-07-17:

- `cargo check -p orca-runtime --tests` passes after removing the generic
  foreground `child_executor` from tool-turn, provider-cycle, iteration, and
  turn-loop ownership;
- synchronous pre-admission cancellation starts no child or lifecycle, and a
  child executor panic becomes one indeterminate tool terminal plus one failed
  subagent lifecycle terminal;
- RuntimeHost interruption observes `subagent.completed`, waits for the owned
  worker and child cleanup, and then accepts the next prompt;
- clean worktree isolation is finished after a child panic and leaves only the
  parent worktree registered;
- all 14 subagent batch tests preserve cancellation, provider order, joined
  cleanup, event-error behavior, and panic isolation;
- async adoption atomically persists the real PID and Running state, rejects a
  late adoption over a terminal task, and lets the parent reaper refresh the
  worker's persisted terminal state;
- foreground interruption leaves a registered async subagent task and worker
  running, while explicit task stop remains the durable cancellation owner;
- all 50 RuntimeHost tests, 12 JSONL subagent contracts, task-status server wire
  projection, and TUI task/subagent projection tests pass;
- async launch emits `task.status.updated` for the durable task and no unmatched
  foreground `subagent.started` event.

Full release validation completed on 2026-07-17:

- all 800 runtime library tests, 50 RuntimeHost integration tests, 12 JSONL
  subagent contracts, and focused TUI task/subagent projection tests pass;
- `cargo test --workspace --all-targets -- --test-threads=1` passes;
- `cargo clippy --workspace --all-targets` passes with the established warning
  baseline and no new P1.2c warning;
- the site production build and SEO check pass after installing the declared
  site dependencies;
- npm staging, published-verifier, and real-API helper self-tests pass;
- the complete real DeepSeek release harness passes provider, CLI, history,
  server, interruption, persistence, and resume verification;
- a dedicated real DeepSeek synchronous-subagent smoke returns
  `ORCA_REAL_SUBAGENT_CHILD_OK` from the child and
  `ORCA_REAL_SUBAGENT_PARENT_OK` from the parent, with paired successful
  `subagent.started` / `subagent.completed` events and a successful session
  terminal.

## Completed Deletion Targets

P1.2c deleted or made non-owning all of the following:

- `thread::scope` and child `JoinHandle` ownership in `subagent_execution.rs`;
- the inline synchronous `run_child_agent` path in `execute_subagent_tool`;
- duplicate single and batch schema/worktree/result formatting;
- lifecycle completion paths that can select more than one terminal status;
- async foreground `subagent.started` publication without a matching foreground
  completion;
- parent adoption that can overwrite a worker's newer persisted task state;
- source-string tests that enforce old subagent module placement, context field
  lists, or call shape without exercising behavior;
- any new detached synchronous worker, resettable cancellation token, unbounded
  bridge, JSON-only internal command protocol, duplicate task ledger, or TUI-local
  subagent cancellation state machine introduced during migration.
