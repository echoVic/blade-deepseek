# P1.2b Runtime-Owned Sequential Normal Tool Calls Plan

- Status: planned; release target `v0.2.35`
- Base: `edefa9e86f78be4727118a3b993b83fcc704a835`
- Branch: `codex/normal-tool-call-runtime-p12b`

## User Value, Architecture Value, And Slice Acceptance

P1.2b makes a sequential normal tool call a child lifetime owned by the same
`RuntimeToolCallRuntime` boundary that owns parallel read-only calls. For a TUI
user, interrupting bash, an external tool, an MCP call, or another cooperatively
cancellable normal tool must signal that invocation, wait for its process or
transport cleanup, publish one truthful tool terminal, and return the thread to
an idle state that accepts the next prompt. A handler panic must become an
indeterminate tool row instead of escaping as a session-level runtime panic.

Tools whose registered interrupt policy is `WaitForTerminal` must retain their
observed result after a parent interrupt. Orca must stop admitting later sibling
calls, but it must not relabel a completed mutation as cancellation or detach a
worker whose effects are still unknown. Natural completion racing cancellation
continues to win.

The architecture value is one final owner for actual normal-tool execution:
the invocation task, started state, invocation-scoped cancel token, typed output
and interaction mailboxes, join, panic classification, permission delta, and
exactly-once terminal. Validation, approval, pre/post hooks, extension lifecycle,
conversation/history recording, and sibling closure remain outside the worker
at the canonical turn policy and publication boundary.

This slice is accepted only when behavior tests prove all of the following:

- cancellation before execution starts produces `cancelled` with
  `started = no` and never invokes the handler;
- `CooperativeCancel` signals an already-started handler and RuntimeHost waits
  for its cleanup and join before publishing the operation terminal or
  admitting the next turn;
- `WaitForTerminal` does not receive a synthetic invocation cancellation after
  dispatch and preserves the observed result;
- a natural completion racing parent cancellation wins exactly once;
- a worker panic after dispatch produces one `indeterminate` tool result and
  does not panic the RuntimeHost generation;
- output-event or interaction delivery failure cannot return while the worker,
  child process, proxy, or transport remains alive;
- permission grants obtained while bash executes return as a typed delta and
  are merged into the canonical turn overlay before the next sibling call;
- every admitted request produces one extension finish, one conversation
  result, one durable result, and one public terminal item;
- TUI, server, and headless execution retain the same behavior because they all
  use the canonical RuntimeHost turn kernel;
- workflow, runtime-special, and subagent execution keep their current owners
  until their explicit P1.2 follow-up slices.

## Structural Problem And Evidence

P1.2a added `RuntimeToolCallRuntime`, but production uses it only from
`runtime_readonly_tool_turn`. The sequential `RuntimeToolDispatch::Normal` path
still calls `run_normal_tool_turn`, creates a fresh `ToolExecutionActor`, and
executes `RuntimeToolRouter::dispatch` inline on the RuntimeHost generation.
The router's `RuntimeSpecialToolDispatch::Normal` branch then calls a borrowed
`RuntimeNormalToolInvocation` through `RuntimeNormalToolExecutor` and directly
into bash or `orca-tools`.

That path has four structural defects:

1. `ToolSpec::interrupt_semantics` is registered and tested in `orca-tools`, but
   no runtime production code reads it. `CooperativeCancel`, `WaitForTerminal`,
   and `DetachAndObserve` are metadata rather than enforced lifecycle policy.
2. The handler runs inline. A panic after a write or process spawn escapes the
   tool boundary and becomes an operation panic, so the TUI can lose the
   truthful per-call terminal even though RuntimeHost later reclaims the thread.
3. `execute_tool_with_approval` creates one `ToolExecutionActor` per tool call.
   Bash permission recovery mutates that actor's private
   `TurnPermissionOverlay`, while the canonical overlay lives in
   `RuntimeSamplingRequestState`. The normal branch does not merge the actor
   overlay back, so a turn-scoped grant can disappear before the next sibling.
4. Ownership is expressed through borrowed contexts and source-shape tests.
   The invocation carries borrowed config, request, registries, handlers,
   output callback, extension stores, cancellation, and mutable permission
   state. This prevents an owned task boundary and makes compiler shape, rather
   than behavior, the main proof that the layers remain arranged as intended.

Existing server tests prove that the current bash and MCP handlers observe a
cancel token in ordinary cases. They do not prove join-before-terminal, panic
isolation, control-semantics enforcement, permission-delta return, or exact
terminal behavior when output or interaction delivery fails.

This is an ownership and state-consistency defect, not a missing
`catch_unwind`. Catching one panic inline would leave cancellation policy,
cleanup joins, permission state, and terminal selection split across the same
temporary actor chain.

## Reference Findings

Codex's `ToolCallRuntime` owns every direct call, whether parallel or exclusive.
It retains the step that advertised the tool, admits the call through a shared
parallel/exclusive gate, spawns an abort-on-drop task, races task completion
with cancellation, distinguishes handlers that must finish runtime teardown,
waits for the join before returning, and protects the terminal outcome with an
atomic once boundary.

Orca should reuse that ownership idea, not copy the type graph. Orca already
has typed `ToolRequest` / `ToolResult`, explicit replay and interrupt semantics,
RuntimeHost generation cancellation, task/process registries, typed permission
responses, MCP elicitation, and truthful `started` metadata. The appropriate
boundary is an owned blocking invocation task plus bounded typed bridges back
to the synchronous turn policy owner.

Claude Code passes one abort controller through the tool-use context and keeps
bash process cleanup explicit: foreground execution consumes progress, handles
interrupt as a result, and invokes cleanup before returning; backgrounding
transfers cleanup ownership to a task object. Orca should preserve the useful
rule that cancellation and cleanup travel together, without moving renderer or
application state into the Rust tool scheduler.

## Target Ownership And Module Boundaries

Generalize `runtime_tool_call.rs` so `RuntimeToolCallRuntime` owns both current
read-only batches and one sequential normal invocation.

For a normal call, the runtime owns:

- an owned `RuntimeNormalToolCall` containing the effective request, cloned
  config snapshot, cwd, additional roots, MCP registry, external tool specs,
  truncation policy, timeout, task registry, extension data owners, and the
  invocation's initial permission overlay;
- the canonical `ToolControlSemantics` resolved from the same registry identity
  that will execute the tool;
- one child cancel token whose behavior is selected by `InterruptSemantics`;
- one blocking task, execution-start marker, join result, panic classification,
  and exactly-once terminal slot;
- bounded typed mailboxes for output chunks, permission requests, and MCP
  elicitation requests while the worker is active;
- a typed `RuntimeNormalToolCallOutput` containing the observed `ToolResult` and
  the permission-overlay delta to merge after the join.

The worker owns tool implementation execution only. A worker-side permission
bridge implements `RuntimePermissionRequestHandler` by sending a typed request
to the parent and waiting for its typed response. An MCP elicitation bridge does
the same for `McpElicitationHandler`. Output chunks cross a bounded channel and
are published by the parent. These bridges keep borrowed UI/server handlers and
event writers out of the worker without weakening the internal protocol to
JSON or untyped callbacks.

`tool_execution` remains the validation, approval, requested-event, pre-hook,
post-hook, extension start/finish, and terminal-event boundary.
`tool_turn` remains the conversation/history recording and sibling-closure
boundary. Neither module owns worker lifetime after this slice.

`runtime_normal_tool` may retain implementation selection between runtime bash
and the `orca-tools` registry, but it must become an owned handler adapter, not
a task owner. The borrowed `RuntimeNormalToolInvocation`, duplicate execution
context, fallback-owner trait, and direct inline entrypoint are deleted.

`RuntimeToolRouter` continues classifying workflow, runtime-special, and
subagent calls. Only its `RuntimeSpecialToolDispatch::Normal` branch enters the
new normal invocation owner. P1.2b must not wrap workflows or subagents in a
second owner; P1.2c will migrate subagent execution deliberately.

## Interrupt And Terminal Rules

- Before admission, parent cancellation creates a cancelled-before-start
  terminal and the handler is never called.
- For `CooperativeCancel`, parent cancellation signals the child token and the
  runtime awaits handler cleanup and join. An observed handler result wins a
  race with cancellation.
- For `WaitForTerminal`, parent cancellation closes later sibling admission but
  does not cancel the child token. The runtime waits for the observed terminal.
- `DetachAndObserve` is rejected until a tool has a separate durable observer
  owner. The current registry contains no such tool.
- A panic before the execution-start marker is failed-before-start. A panic
  after it is `indeterminate_after_start` and instructs the user to inspect
  external state before retrying.
- Output, permission, or elicitation channel failure cancels cooperative work,
  keeps draining/joining, and returns only after ownership is settled.
- Publication or persistence failure after an observed terminal never executes
  the tool again and never replaces that terminal with a retry-safe result.

## Permission State Rules

The canonical turn overlay remains in `RuntimeSamplingRequestState`. Admission
clones the effective overlay into the owned call. Permission responses mutate
the invocation-local overlay while the worker retries bash. After the worker
joins, the runtime returns that overlay as typed state and the parent merges it
into the canonical overlay before extension finish, post hooks, conversation
recording, or the next sibling dispatch.

The worker must not mutate the canonical turn state through a shared mutex.
Returning a typed state delta preserves one writer and makes failure ordering
testable. A denied or failed permission request returns no grant. Session-scope
permission persistence remains owned by the existing TUI/server handlers.

## External Compatibility

- Keep CLI arguments, TUI keys and flows, server methods, JSONL names and
  wrappers, tool names and schemas, provider call ids, and persisted record
  formats unchanged.
- Keep approval prompts, permission request payloads and scopes, MCP elicitation
  payloads, shell task ids, output-delta events, and `max_read_parallel`
  behavior unchanged.
- Preserve public `RuntimeSessionLifecycle` and `RuntimeToolActorContext`
  convenience methods as thin calls into the canonical owner; they must not
  retain an alternate inline execution implementation.
- User-visible differences are more accurate interruption/panic terminals,
  cleanup-before-next-submit, and turn permission grants that remain effective
  for later sibling calls.

## Migration Sequence And Temporary State

1. Add RED behavior tests with an injected normal handler and typed control
   semantics. Cover in-flight cooperative cancellation, wait-for-terminal,
   completion races, panic after start, join-before-terminal, event failure,
   next-turn admission, and permission-delta return.
2. Extend `RuntimeToolCallRuntime` with the owned normal-call task, terminal
   slot, control-semantics logic, and injected executor used by deterministic
   tests.
3. Add bounded output, permission, and MCP elicitation bridges. Move normal
   invocation inputs to owned values and return the overlay delta after join.
4. Route `RuntimeSpecialToolDispatch::Normal` through the new owner and merge
   returned permission state before post-tool publication and result recording.
5. Delete the borrowed normal executor owner and direct inline path. Keep only
   one implementation adapter for bash versus the `orca-tools` registry.
6. Replace source-string assertions for the old normal owner/context layout
   with compiler and behavior coverage.
7. Rebase latest main, run focused runtime/tool/server/TUI tests, the serial
   workspace gate, Clippy, site/release helpers, and real DeepSeek regression
   verification before release.

The implementation commit must not leave both inline and runtime-owned normal
execution in production. Read-only batches remain on the P1.2a owner.
Runtime-special calls, workflows, and subagents remain explicit later slices,
not compatibility fallbacks from the normal owner.

## Stage Validation

### Plan checkpoint

- plan records structural evidence, target ownership, TUI value,
  compatibility, interrupt rules, permission-state rules, migration,
  acceptance, and deletion targets;
- branch is based on the publicly verified `v0.2.34` release.

### RED checkpoint

- current inline normal execution lets an injected handler panic escape the
  per-tool terminal boundary;
- registered interrupt semantics do not change runtime behavior;
- a permission delta produced inside the ephemeral tool actor is not visible
  in the canonical sampling overlay;
- assertions inspect tasks, joins, terminal values, state deltas, conversation
  results, public events, and next-turn admission rather than source text.

### Runtime owner checkpoint

- focused runtime tests cover pre-admission cancel, cooperative in-flight
  cancel, wait-for-terminal, natural completion race, panic after start,
  output/interactions failure, terminal once, and drop/shutdown cleanup;
- bash tests prove child-process and managed-proxy cleanup before return;
- MCP tests prove transport cleanup and later reuse;
- RuntimeHost/server tests prove interrupt then next-submit recovery;
- a multi-call behavior test proves a bash turn grant affects the next sibling;
- TUI projection renders existing cancelled and state-unknown labels without a
  surface-specific normal-tool state machine.

### Release checkpoint

- `cargo test --workspace --all-targets -- --test-threads=1` passes;
- `cargo clippy --workspace --all-targets` passes with no new warnings;
- site, SEO, npm staging, public verifier, and real-API helper tests pass;
- real DeepSeek provider/CLI/history/server regression verification passes;
- roadmap, plan status, changelog, and `v0.2.35` release notes describe the
  final normal invocation owner and remaining P1.2 boundary;
- main, tag, GitHub Release, npm package, executable smoke, release assets, and
  public changelog are verified before worktree cleanup.

The established three-file rustfmt baseline must remain unchanged.

## Final Deletion Targets

P1.2b is incomplete until it deletes or makes non-owning:

- borrowed `RuntimeNormalToolInvocation` and
  `RuntimeNormalToolExecutionContext`;
- `RuntimeNormalToolExecutor` as a task/lifecycle owner;
- `RuntimeNormalToolFallbackExecutor` and its borrowed fallback context when a
  direct owned handler adapter provides the same test seam;
- the direct inline normal execution call from `RuntimeToolRouter`;
- any per-tool permission overlay that can diverge from the canonical sampling
  overlay after execution;
- source-string tests that protect the old normal executor, long context shape,
  or module placement without exercising behavior;
- any new detached worker, resettable invocation token, unbounded output lane,
  JSON interaction bridge, duplicate terminal ledger, or TUI-local cancellation
  branch introduced during migration.
