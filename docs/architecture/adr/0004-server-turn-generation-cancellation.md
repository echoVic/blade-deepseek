# ADR 0004: Generation-Owned Server Turn Cancellation

Status: proposed; implementation branch `codex/server-turn-generation-cancellation-v0.2.28`

## Problem Evidence

The server currently stores an active turn's `CancelToken` in
`ActiveTurnControl`, starts one detached worker, and implements
`turn/resume` as `control.cancel.reset()`. The token therefore outlives the
execution that observed it, and resume races the old worker's cancellation
checkpoint instead of starting a new execution. The control map and join-handle
list are also separate facts that are merged only after a worker returns.

The three server interaction managers expose another lifecycle gap. Permission
and user-input handlers block on an uninterruptible `recv()`, while MCP
elicitation polls cancellation. Pending request ids are scoped to a public turn
id but not to the execution that created them. A late response can consequently
wake an old waiter or collide with a request from a resumed execution.

## Target Ownership And Boundary

`ActiveTurnManager` owns one actor-like worker per logical server turn. The
worker owns the `ServerThread` and runs sequential execution generations. Each
generation owns an immutable, one-shot `CancelToken`; interrupt cancels the
current generation and never resets it. Resume is a command that is applied
only after the cancelled generation has returned and been joined. The resumed
generation reuses the stable public `turnId`, but receives a fresh cancellation
scope and a fresh internal generation number.

The manager owns the current generation control, steer handle, command sender,
join handle, and session-permission metadata as one lifecycle record. The
server loop remains responsible for JSONL dispatch and event writes. Runtime
execution keeps receiving `&CancelToken`; the new boundary is at the server
turn host rather than inside provider or tool implementations.

Permission, user-input, and MCP handlers receive the generation scope. Their
pending ids are generation-scoped for resumed generations, and cancellation
removes the waiter before the worker can be joined. A response for an old id is
therefore rejected instead of reaching a later generation.

Generation output may stream normally, but its raw terminal `session.completed`
record is held until the generation outcome is known. If the generation is
replaced by resume, its terminal record is discarded; the logical turn emits
one final terminal event from the current generation. A generation that is not
resumed commits its cancelled or successful terminal event as before.

## TUI And Runtime Benefit

- Interrupting a DeepSeek stream is permanent for that execution and cannot be
  undone by clearing a flag.
- `turn/resume` means restart with a new scope, not a race against an old
  worker. The same logical turn id remains usable by TUI clients.
- Approval, user-input, elicitation, steer, completion, and continuation paths
  have a generation fence; stale responses cannot affect resumed work.
- Every worker is cancellable, joined, and either reclaimed by the server or
  handed to the shutdown reaper. Pending waiters do not prevent bounded
  shutdown.

## Compatibility

CLI arguments, TUI flow, server methods, public `turnId` values, persisted
message and turn records, provider selection, and DeepSeek request behavior
remain unchanged. The first generation preserves existing request-id shapes;
resumed generations add an internal generation component to interaction
request ids so clients can safely reject stale responses. `turn_controlled`
actions and statuses remain unchanged. A resumed logical turn emits one final
`turn_completed` event instead of exposing a stale cancelled completion.

## Migration Order And Temporary State

1. Add manager and runtime behavior tests that fail against resettable resume,
   unjoinable interaction waiters, duplicate generation ids, and stale terminal
   output.
2. Make permission and user-input waiters cancellation-aware; add generation
   scoped request ids and stale response rejection.
3. Replace the active-turn control with the sequential generation worker and
   terminal-event fence. Remove `CancelToken::reset()` from the server path.
4. Re-run focused server/runtime contracts, then the serial workspace gate,
   Clippy, release helpers, and real DeepSeek smoke coverage.
5. Update roadmap and release notes only after the old path is deleted and the
   full validation ladder passes.

The only temporary state is the compatibility of the public first-generation
request-id format. No second cancellation controller or reset branch is
allowed. The old path is deleted in this slice; its deletion gate is a focused
behavior test proving a cancelled generation remains cancelled while a resumed
generation runs with a different scope and the same public turn id.

## Acceptance

- No production `CancelToken::reset()` call remains under `crates/orca-runtime`.
- A server interrupt followed by resume starts a new generation with a fresh
  cancellation scope, emits one successful terminal turn, and preserves the
  public turn id.
- Cancelling a generation releases permission, user-input, and MCP waiters;
  stale responses are rejected and cannot resolve a later generation.
- Steer and terminal events from a replaced generation are not accepted as
  current-generation completion.
- Focused server/runtime behavior tests, full serial workspace tests, Clippy,
  site/release helpers, and real DeepSeek validation pass before integration.
