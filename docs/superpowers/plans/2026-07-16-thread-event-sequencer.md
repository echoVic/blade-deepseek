# P1.1 Thread Event Sequencer Plan

- Status: P1.1a in progress
- Base: `3c6eb902c2c4ad7a4f4cf12a506f5811716d541b`
- Branch: `codex/thread-event-sequencer-p11a`

## Structural Problem And Evidence

`RuntimeHost` now owns foreground turns and suspended provider work, but it does
not yet own one event sequence across that full lifetime. `ThreadActorState`
keeps the foreground `EventFactory`, while `run_provider_background_task`
creates a second factory with the same `run_id`. The provider terminal and task
status therefore restart at `seq = 0` after handoff. A client cannot use the
pair `(run_id, seq)` to deduplicate, order, or resume the event stream.

The current host behavior test proves a contiguous sequence only across two
foreground operations. Background-provider ownership tests verify admission,
usage, cancellation, and join, but do not verify event identity across the
handoff.

This is an ownership defect, not a renderer defect. Filtering duplicate
sequence numbers in the TUI or server would preserve two sequence authorities
and make reconnect behavior surface-specific.

## Target Ownership And Module Boundary

The thread event stream owns one shareable sequence allocator. An
`EventFactory` fork keeps the same `run_id` and allocator, so a host-adopted
provider worker can create events without creating a second sequence source.
The provider worker owns its factory handle only for its joined lifetime; the
`ThreadActor` remains the owner that creates the stream and transfers the
handle during background admission.

P1.1a establishes allocation ownership only. It does not claim to provide the
future durable semantic journal, ordered multi-producer publication, or stable
conversation item ids.

## TUI User Value

Background completion, foreground recovery, and the next submit no longer
reuse an earlier event identity. This makes task status and terminal updates
safe to deduplicate and provides the ordering foundation needed for reliable
reattach, visible lifecycle state, and eventual process-loss recovery.

## External Compatibility

- Keep the event schema version and fields unchanged.
- Keep `run_id`, JSONL event names and payloads, server methods, TUI events, and
  persistence records unchanged.
- Keep foreground operation, provider suspension, cancellation, usage, and
  shutdown behavior unchanged.
- The only observable change is that provider-background events continue the
  existing sequence instead of restarting at zero.

## Migration Sequence And Temporary State

1. Add a RED runtime-host behavior test covering one foreground-to-background
   provider handoff and assert one unique contiguous sequence for the thread.
2. Give `EventFactory` an explicit fork operation backed by one shared atomic
   allocator; retain exclusive mutable factory handles at call sites.
3. Transfer a fork from `ThreadActorState` into the admitted provider worker
   and delete the worker-local `EventFactory::new` path.
4. Verify provider handoff, event sequencing, usage, cancellation, and shutdown
   behavior before committing P1.1a.
5. In later P1.1 slices, move workflow/background semantic events behind the
   thread sequencer, introduce the durable semantic journal, and replace
   index-derived turn/item ids with stable ids.

The shared allocator is target architecture, not a compatibility wrapper. The
temporary limitation is explicit: allocation is shared before event delivery
and persistence are unified.

## Acceptance Criteria

- A host-adopted provider emits no duplicate `(run_id, seq)` values.
- Sorting all observed events for the thread by `seq` produces every integer
  from zero through the last observed sequence without gaps.
- Existing foreground operations still share one contiguous sequence.
- Provider background completion settles task state and usage exactly once.
- Provider background cancellation and host shutdown still cancel and join.
- `orca-core` event-schema tests and `orca-runtime` runtime-host tests pass.
- The full serial workspace gate and Clippy pass before integration because the
  shared event type affects every runtime surface.

## Final Deletion Targets

P1.1 is not complete until these paths are removed or replaced:

- background event producers that construct an independent sequence for a
  thread-owned semantic event stream;
- index-derived `turn-N` and `item-N` identities in live/stored projections;
- direct semantic history/task/goal mutations that bypass the eventual journal
  sequencer;
- source-shape tests used in place of event replay and projection behavior
  tests.
