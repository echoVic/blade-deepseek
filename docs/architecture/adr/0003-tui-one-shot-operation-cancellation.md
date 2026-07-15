# ADR 0003: One-Shot TUI Operation Cancellation

Status: proposed for v0.2.27

## Problem Evidence

The production TUI creates one `CancelToken` when the application starts and
shares it with the agent thread and keyboard handlers. A new submitted turn,
manual compaction, and saved-workflow path call `CancelToken::reset()` before
starting work. The same flag therefore represents multiple operations and can
be cleared after an older interrupt. The agent loop also receives a bare
`UserAction::Interrupt` after the UI has mutated that shared flag, so the
action has no operation identity of its own.

This is a lifecycle boundary defect, not a local reset bug: the UI owns a
long-lived mutable cancellation flag while the runtime executes separate
turns, compactions, and continuations with different lifetimes.

## Target Boundary

`orca-core::cancel` will expose `OperationCancellation`, an owned controller
for the current operation, and `OperationScope`, a one-shot scope with a
stable `OperationId` and immutable `CancelToken` view. Starting a scope
replaces the controller's current scope; cancelling a scope is permanent and
cannot affect a later scope. UI interrupt handlers cancel the controller's
current scope, while the agent loop creates and passes a fresh scope to each
submitted turn, compaction, goal continuation, and approved background
continuation.

The existing runtime functions continue to receive `&CancelToken` inside this
slice. That keeps provider, tool, and persistence behavior unchanged while the
TUI operation boundary moves to one-shot ownership. The server's
`turn/resume` path still reuses a resettable token and is an explicit follow-up
slice: it requires an actor-owned restart rather than silently reusing this
controller from the TUI.

## TUI Benefit

- Interrupting a DeepSeek stream cannot be undone by a later `reset()` from a
  different TUI path.
- The next user turn always starts with a non-cancelled scope.
- Manual compaction and approved background continuation are independently
  cancellable, so a prior turn's interrupt cannot make them inert.
- Operation identity is available for the later runtime host and event
  protocol without changing CLI, server JSONL, or persisted records now.

## Compatibility

CLI arguments, TUI key bindings, server methods/JSONL, provider behavior,
history records, and persisted task formats remain unchanged. The only
observable correction is that cancellation is scoped to the operation that was
active when the user interrupted it.

## Migration And Temporary State

1. Add typed `OperationCancellation`/`OperationScope` behavior tests.
2. Migrate TUI application, keyboard handlers, and agent-loop operation entry
   points; delete all TUI `CancelToken::reset()` calls.
3. Run focused TUI/core tests and the full workspace gate.
4. Update the roadmap with the remaining server `turn/resume` reset path.

The temporary compatibility path is the server's existing resettable
`CancelToken`. Its deletion gate is the runtime host/actor slice that can
restart an interrupted server turn with a new operation scope and stable turn
generation. No new TUI caller may depend on `reset()`.

## Acceptance

- `OperationScope` tests prove cancellation is one-shot, scope ids are stable,
  and a later scope remains live after an earlier scope is cancelled.
- TUI behavior tests prove interrupting one submitted turn leaves the next
  submitted turn and manual compaction uncancelled.
- `rg` finds no `CancelToken::reset()` call under `crates/orca-tui`.
- Focused core/TUI tests, full serial workspace tests, Clippy, and the existing
  release verification helpers pass before integration.
