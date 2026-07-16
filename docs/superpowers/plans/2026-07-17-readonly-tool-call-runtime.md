# P1.2a Runtime-Owned Read-Only Tool Calls Plan

- Status: planned; release target `v0.2.34`
- Base: `30e8e92f68342e63c19523847f6dad51652305ed`
- Branch: `codex/readonly-tool-call-runtime-p12a`

## User Value, Architecture Value, And Slice Acceptance

P1.2a makes interrupting a TUI or server turn stop an in-flight parallel
read-only tool batch instead of waiting for every already-started call to
finish. This matters most for MCP resource discovery and other remote reads:
after `Esc`, `Ctrl+C`, or `turn/interrupt`, Orca must cancel the admitted calls,
wait for their cleanup, publish one truthful terminal row per call, and return
the thread to an idle state that accepts the next prompt.

The architecture value is the first production client of a final
`RuntimeToolCallRuntime` ownership boundary. Each admitted invocation owns its
task, concurrency permit, cancellation observation, started state, join result,
and exactly-once terminal selection. The read-only batch owns request ordering,
pre/post hooks, conversation persistence, and projection only after the
invocation owner has returned a terminal result. Tool implementations remain
inside `orca-tools`; scheduling and lifetime ownership move to `orca-runtime`.

This slice is accepted only when behavior tests prove all of the following:

- cancellation before permit acquisition produces `cancelled` with
  `started = no` and never executes the tool;
- cancellation after dispatch reaches each in-flight MCP/read-only executor,
  waits for cleanup, and produces `cancelled` with `started = yes`;
- a natural completion racing cancellation wins as one completed result rather
  than being overwritten by a synthetic cancellation;
- a task panic after dispatch produces one `indeterminate` terminal and never a
  retry-safe failure;
- every accepted request produces exactly one conversation result and one
  public terminal item in provider order, even when tasks finish out of order;
- turn interruption joins every invocation task before RuntimeHost reports the
  operation terminal or admits the next turn;
- TUI and server behavior remain identical because both execute through the
  canonical RuntimeHost turn kernel;
- ordinary sequential tools, mutable tools, shell sessions, workflows, and
  subagents retain their existing behavior until their explicit P1.2 follow-up
  slices migrate them.

## Structural Problem And Evidence

The current dispatch scheduler identifies adjacent read-only concurrent calls,
but lifetime ownership falls out of `orca-runtime` immediately afterward.
`runtime_readonly_tool_turn::execute_readonly_batch` performs hooks and events,
then calls `orca_tools::run_readonly_batch_parallel_with_policy_or_cancel`.
That helper creates scoped OS threads, checks cancellation only before each
spawn, executes through the non-cancellable tool entrypoint, and joins in
handle order.

The existing behavior test names the limitation directly:
`readonly_batch_cancels_only_requests_not_spawned_yet`. Once a request has been
spawned, turn cancellation cannot reach that invocation. A slow MCP resource
read therefore keeps the RuntimeHost generation alive, delays TUI recovery,
and makes the parent turn function the accidental owner of worker cleanup.

Terminal ownership is also split:

- `orca-tools` maps a worker panic to a generic failed result;
- `runtime_readonly_tool_turn` emits tool lifecycle events and post hooks;
- `tool_turn` decides sibling closure and turn termination;
- session helpers append the model-facing and durable tool result;
- the RuntimeHost can only cancel and join the outer generation, not the
  individual tool tasks keeping it alive.

This is an ownership defect, not a missing cancellation check. Passing another
closure into the current scoped-thread helper would still leave task admission,
join order, panic classification, and terminal selection outside the runtime.

## Reference Findings

Codex's useful pattern is its async `ToolCallRuntime`: it retains the step that
advertised the tool, admits calls through a parallel/exclusive execution gate,
spawns an owned task, races that task with cancellation, distinguishes handlers
that must finish their own teardown, joins before returning, and protects the
terminal outcome against cancellation races.

Orca should not copy Codex types mechanically. Orca already has typed
`ToolRequest` / `ToolResult`, RuntimeHost-owned turn cancellation, truthful
`cancelled` / `indeterminate` terminal metadata, and synchronous blocking tool
implementations. The appropriate Rust boundary is an async runtime owner that
uses owned invocation data and `spawn_blocking` for current adapters while the
canonical turn kernel waits through a narrow synchronous facade.

Claude Code's broad renderer-coupled `ToolUseContext` is not the target. TUI
state must not become the tool scheduler, cancellation owner, or result ledger.

## Target Ownership And Module Boundaries

Add `runtime_tool_call.rs` with these responsibilities:

- `RuntimeToolCallRuntime` owns the Tokio runtime handle used by the current
  RuntimeHost generation and a bounded parallel-admission gate;
- an owned read-only invocation value carries the provider call id, effective
  request, cwd, MCP registry, output policy, and one-shot cancellation view;
- one invocation task acquires its permit, marks execution started once, calls
  the cancel-aware tool executor, and returns one typed terminal candidate;
- the runtime races parent cancellation with task completion, signals admitted
  tasks, awaits their joins, and resolves one terminal through an atomic/locked
  once cell;
- a join panic before execution is a failed-before-start result; a panic after
  execution begins is `indeterminate_after_start`;
- batch collection restores provider request order without serializing task
  execution or terminal observation.

`runtime_readonly_tool_turn` remains the policy and publication boundary for the
read-only dispatch window. It owns pre-tool hooks before admission and post-tool
hooks, events, conversation writes, and history writes after all invocation
handles settle. It no longer creates threads or calls a batch executor in
`orca-tools`.

`orca-tools` remains a library of tool implementations. Its existing
cancel-aware single-invocation entrypoint is the only execution API the new
runtime needs. It must no longer export read-only batch scheduling or worker
ownership.

The provider call id remains the public invocation identity. P1.2a does not
mint a second task id or change thread/item identity. The RuntimeHost operation
scope remains the parent cancellation owner; per-invocation state is a child
lifetime, not another resettable cancellation controller.

## External Compatibility

- Keep CLI arguments, TUI keys and flows, server methods, JSONL names, and
  response wrappers unchanged.
- Keep `ToolRequest`, `ToolResult`, conversation records, semantic events, and
  public item ids unchanged.
- Cancellation becomes more responsive for already-started parallel read-only
  calls. Their terminal metadata becomes more accurate when a worker panics.
- Preserve provider request order in model replay and public history even when
  invocation tasks complete out of order.
- Keep the configured `max_read_parallel` meaning and upper bound unchanged.

## Migration Sequence And Temporary State

1. Add RED runtime behavior tests with an injected blocking read-only executor.
   Prove that current cancellation cannot reach an already-started invocation
   and that the outer turn cannot finish until the fixture releases it.
2. Add server/RuntimeHost coverage for interrupt, join-before-terminal, next
   turn admission, exact terminal count, and provider-order persistence.
3. Introduce the runtime-owned invocation and terminal types with an injected
   executor for deterministic cancellation, completion-race, and panic tests.
4. Route `runtime_readonly_tool_turn` through the new owner and the existing
   cancel-aware single-call tool entrypoint.
5. Delete the `orca-tools` read-only batch thread helpers and their test that
   codifies pre-spawn-only cancellation.
6. Delete source-string architecture assertions for read-only ownership and
   scheduler placement when equivalent compiler/behavior coverage exists.
7. Rebase latest main, run focused runtime/tool/server/TUI tests, the serial
   workspace gate, Clippy, site/release helpers, and real DeepSeek regression
   verification before release.

P1.2a is a final boundary for read-only batch invocations, not a compatibility
wrapper. Sequential normal tools remain on their existing path until P1.2b;
subagent calls retain their child-agent runtime until P1.2c. Neither path may
call back into the deleted read-only batch helper. P1.2 is not complete until
those follow-up slices share the same invocation ownership and the obsolete
per-domain task owners are removed.

## Failure And Recovery Rules

- A request rejected by policy or a pre-tool hook never acquires a permit and
  remains a failed/cancelled-before-start terminal owned by the parent window.
- After a permit is acquired, cancellation must be delivered to the tool and
  the task must be joined. Returning while a worker still runs is forbidden.
- If a tool returns a completed result concurrently with cancellation, retain
  that observed completion. Do not relabel known success as cancellation.
- If the executor panics after start, persist `indeterminate` and instruct the
  model/user to inspect external state before retrying.
- Event or history publication failure after a terminal result must not execute
  the tool again or create another terminal candidate.
- Dropping the batch owner cancels admitted invocation tasks and waits for them
  through the owning RuntimeHost generation; no detached reaper is introduced.

## Stage Validation

### Plan checkpoint

- plan records structural evidence, target ownership, TUI value,
  compatibility, migration, failure rules, acceptance, and deletion targets;
- branch is based on current `origin/main` after the verified `v0.2.33`
  release.

### RED checkpoint

- an already-started injected read-only invocation remains blocked after parent
  cancellation under the current implementation;
- a RuntimeHost/server interruption cannot complete before the fixture releases
  the old worker;
- assertions inspect task lifecycle, terminal values, ordering, and public
  events rather than source text.

### Runtime owner checkpoint

- focused runtime tests cover pre-admission cancel, in-flight cancel, natural
  completion race, panic after start, out-of-order completion, terminal once,
  and drop/shutdown cleanup;
- real MCP cancellation coverage proves the transport returns and its worker is
  joined;
- RuntimeHost and server tests prove interrupt then next-submit recovery;
- TUI projection renders the existing cancelled/indeterminate terminal labels
  without a new surface-specific state machine.

### Release checkpoint

- `cargo test --workspace --all-targets -- --test-threads=1` passes;
- `cargo clippy --workspace --all-targets` passes with no new warnings;
- site, SEO, npm staging, public verifier, and real-API helper tests pass;
- real DeepSeek provider/CLI/history/server regression verification passes;
- roadmap, plan status, changelog, and `v0.2.34` release notes describe the
  final read-only invocation owner and the remaining P1.2 boundary;
- main, tag, GitHub Release, npm package, executable smoke, release assets, and
  public changelog are verified before worktree cleanup.

The repository's existing three-file rustfmt baseline remains unchanged;
P1.2a must not add another formatting difference.

## Final Deletion Targets

P1.2a is incomplete until it deletes:

- `orca_tools::run_readonly_batch_parallel`;
- `orca_tools::run_readonly_batch_parallel_with_policy`;
- `orca_tools::run_readonly_batch_parallel_with_policy_or_cancel`;
- the scoped-thread result-slot owner inside `orca-tools`;
- the test asserting that read-only cancellation affects only requests not yet
  spawned;
- source-string tests that claim module ownership without exercising behavior;
- any new detached invocation worker, resettable per-call token, duplicate
  terminal ledger, or TUI-local cancellation branch added during development.
