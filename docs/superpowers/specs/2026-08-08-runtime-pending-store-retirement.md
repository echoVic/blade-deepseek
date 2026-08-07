# Runtime Pending Store Retirement

## Status

Proposed for the `codex/runtime-pending-store-retirement` slice, rebased onto
`origin/main` at `445baf596ef4cf58a422d685417d10f54c15d994`.

## Problem and evidence

The TUI single-surface slice removed the TUI-local interaction rail, but
`HostedTurnRequest` still accepts a process-local
`RuntimePendingInteractionStore`. The legacy Goal continuation path reads that
map during admission (`crates/orca-runtime/src/runtime_host.rs`), while the
runtime surface already owns typed pending-interaction state in its persisted
surface snapshot. The map has no production writer after the TUI rail removal;
the remaining behavior is an injectable second fact source that can block a
Goal without a durable interaction record.

This slice retires that ownership from runtime-host Goal admission. It does not
claim that P1.3 durable interaction recovery is complete; it removes a false
source of truth so the next broker work can establish one owner.

## User value

TUI Goal continuation can no longer be stranded by stale process-local state
left behind by a detached or crashed interaction path. Users see admission
decisions derived from the runtime-owned typed state, and a compatibility caller
that still supplies the old store cannot create an invisible pending prompt.

## Target ownership and boundaries

- Runtime surface/session state is the only production owner of pending
  interaction facts for surface-owned operations.
- Legacy `HostedTurnRequest` remains source-compatible during this migration,
  but `with_pending_interactions` is a documented no-op compatibility method.
- `RuntimePendingInteractionStore`, its records, and their conversion helpers
  remain publicly available for one migration window so downstream crates can
  compile; they are no longer read or stored by `HostedTurnRequest`.
- The public `GoalContinuationRejectCode::PendingInteraction` variant remains
  for Rust API compatibility, but the private legacy preflight no longer carries
  an unreachable pending-state branch.

## Compatibility and migration

No CLI, TUI, server/JSONL, persistence, or public Rust symbol is removed in
this slice. The old builder keeps its signature and does nothing. The public
store types remain unchanged so downstream consumers have time to migrate to
typed surface interactions without a patch-release warning churn.

The deletion gate for the compatibility module and builder is a later major
runtime API migration after all legacy `HostedTurnRequest` Goal paths are gone,
server/CLI callers no longer compile against the store, and lifecycle/recovery
tests demonstrate durable broker ownership.

## Acceptance criteria

1. A legacy Goal request that supplies a non-empty pending store is not rejected
   as `pending_interaction`; its outcome is determined by durable Goal state
   (the focused regression uses a deliberately exhausted token budget).
2. Production `RuntimeHost` contains no pending-store field or read.
3. The compatibility builder compiles and has no runtime side effect.
4. Existing pure preflight tests still cover every reachable legacy admission
   priority; the public `PendingInteraction` reject code remains constructible.
5. Focused runtime-host and surface lifecycle tests pass; formatting and diff
   checks pass; the Rust API gate reports no unexpected break.
6. The roadmap and this migration gate describe the current architecture, not a
   second long-term interaction owner.

## Rollback

Revert the single semantic commit. No persisted format or external protocol
change is introduced.
