# P1.1c Ordered Event Publication Plan

- Status: complete
- Base: `028aae78a3b8b90dbe2bf70f63d2b0b11a9f59bf`
- Branch: `codex/ordered-event-publication-p11c`
- Validated after rebase onto: `87dd307d1`

## Structural Problem And Evidence

P1.1a and P1.1b give foreground turns, provider continuations, and workflow
workers one shared atomic sequence allocator. Allocation and delivery are still
separate operations: `EventFactory::make` calls `fetch_add`, returns an
`EventEnvelope`, and `EventSink::emit` or `observe_runtime_event` publishes it
later. Two producers can therefore allocate sequence zero and one, then deliver
one before zero. The current RuntimeHost assertion sorts observed sequence
numbers before comparison, so it proves uniqueness and contiguity but not the
order that the TUI actually receives.

This is a publication-boundary defect. A renderer-side reorder buffer would
leave JSONL writers, server projection, and future journal persistence with a
different ordering authority. Keeping allocation on workers and adding a lock
only around observer calls would also preserve the race between allocation and
publication.

Codex routes semantic events through a session-owned
`send_event_raw_with_persistence` boundary before client delivery. Orca should
adopt the ownership idea without copying the async implementation: the event
stream, not an individual worker or renderer, assigns the final sequence at the
publication side effect.

## Target Ownership And Module Boundary

`EventFactory` constructs typed `EventDraft` values. A draft contains the
thread run id, event type, payload, and a handle to the thread publication
sequencer, but no externally meaningful sequence or timestamp.

`EventSink::emit` consumes one draft. While holding the shared publication
sequencer, it creates the final `EventEnvelope`, invokes the observer, writes
the JSONL/text representation, flushes the writer, and advances the sequence.
All side effects for sequence N therefore finish before sequence N+1 begins.

Observer-only RuntimeHost background paths use the same consuming publication
primitive. They no longer call `EventObserver::observe` directly. A draft that
is never published consumes no sequence number, so optional observers cannot
create invisible gaps.

`EventEnvelope` remains the immutable wire value seen by TUI, server, JSONL,
and tests. `EventDraft` is the internal pre-publication value. This distinction
is target architecture for the later durable journal, not a compatibility
wrapper.

## TUI User Value

Foreground output, a concurrently completing workflow, and a foregrounded
provider continuation arrive in the same order as their `(run_id, seq)` values.
The TUI can reduce events directly without transient rollback, duplicate
suppression heuristics, or an unbounded reorder buffer. This also makes the
existing bounded TUI event channel honest backpressure for the whole thread
stream.

## External Compatibility

- Keep event schema version, envelope fields, event names, payloads, and JSONL
  text shapes unchanged.
- Keep CLI arguments, TUI flows, server methods, persistence records, workflow
  payload ids, cancellation, and shutdown behavior unchanged.
- Keep observer failures typed as operation failures where `EventSink` already
  propagates them.
- Internal Rust event construction changes from an allocated envelope to an
  unpublished draft; no npm or server/JSONL consumer sees that type.

## Migration Sequence And Temporary State

1. Add a deterministic RED core test that publishes a later draft from another
   thread before the earlier draft and proves the observer cannot receive it
   out of sequence.
2. Add `EventDraft` and move final sequence/timestamp assignment into one
   publication critical section shared by `EventFactory::fork` values.
3. Make `EventSink::emit` consume drafts and publish observer plus writer side
   effects under that boundary.
4. Replace RuntimeHost direct observer calls with the observer-only form of the
   same publication primitive.
5. Change RuntimeHost behavior assertions to compare arrival order directly,
   then cover foreground plus workflow/provider concurrency.
6. Delete the old allocation-time atomic counter and every production direct
   `EventObserver::observe` bypass.

The slice is complete only after every event created by `EventFactory` reaches
an observer or writer through the consuming publisher. There is no planned
long-term dual path.

## Acceptance Criteria

- Concurrent producers cannot deliver sequence N+1 before N.
- Observed and serialized envelopes are unique and contiguous in arrival order
  without sorting in tests.
- Dropping or suppressing an unpublished draft does not create a visible gap.
- Observer and writer see the same final envelope.
- Observer failure still returns through `EventSink::emit`, and the stream can
  advance to cleanup events without reusing a sequence.
- Provider background handoff, workflow completion/failure, capacity cleanup,
  cancellation, and host shutdown keep their existing lifecycle behavior.
- Focused `orca-core` event tests and all RuntimeHost tests pass.
- The serial workspace all-targets gate and workspace Clippy pass before
  integration because the publication boundary is shared by every surface.

This slice does not add durable replay, process-loss recovery, or stable
turn/item ids. Those remain later P1.1 slices after live publication has one
provable order.

## Completion Evidence

- `EventFactory` now creates typed `EventDraft` values, and the old
  allocation-time `AtomicU64` path is deleted.
- `EventSink::emit` and observer-only `observe_event` calls assign the final
  sequence and timestamp while holding the shared publication boundary.
- RuntimeHost has no production direct `EventObserver::observe` bypass, and
  its sequence assertions compare observer arrival order without sorting.
- Core tests cover publication order, unpublished drafts, shared observer and
  writer envelopes, and sequence consumption after observer failure.
- After rebasing onto the latest local `main`, 152 core tests, 43 RuntimeHost
  tests, and 390 TUI tests passed. The serial workspace all-targets gate and
  workspace Clippy also passed with the repository's existing warning
  baseline.

## Final Deletion Targets

- allocation-time `AtomicU64` sequencing in `EventFactory`;
- `EventFactory` methods returning already-published `EventEnvelope` values;
- RuntimeHost helpers that call `EventObserver::observe` directly;
- tests that sort observed sequence numbers before asserting continuity.
