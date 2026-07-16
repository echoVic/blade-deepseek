# P1.1d Durable Event Sequence Reservation Plan

- Status: in progress
- Base: `d7d613f2cdf397197664ea457c14d01b6a3f951b`
- Branch: `codex/durable-event-sequence-p11d`

## Structural Problem And Evidence

P1.1c gives each live thread one ordered publication boundary, but
`ThreadActor::new` still constructs `EventFactory::new(thread_id)`. Resuming a
recorded thread therefore reuses sequence zero even though the thread id is
unchanged. A client or future semantic journal cannot distinguish an event
from the resumed process from an earlier event with the same `(run_id, seq)`.

The session JSONL already persists conversation messages, usage, compaction,
plan state, and terminal status, but it records no event-sequence high water
mark. The server also derives `turn-N` and `item-N` from message indexes. Stable
item ids cannot safely use the P1.1 event identity until the event sequence is
non-repeating across process recovery.

Persisting every assigned sequence would synchronously write every DeepSeek
reasoning, text, tool-progress, and tool-output delta. That would make storage
latency part of token streaming and contradict the P1.1 requirement to journal
semantic state without journaling every token delta.

Codex persists typed rollout items and uses explicit flush barriers at durable
lifecycle boundaries. Orca should adopt the durability principle without
copying the async recorder: reserve bounded ranges synchronously before use,
then let later slices persist only semantic events inside the already ordered
publication boundary.

## Target Ownership And Module Boundary

`EventPublication` remains the only owner of live sequence assignment. It gains
an optional typed `EventSequenceStore` and owns both `next_seq` and the current
exclusive reservation limit.

Before publishing the first event in a block, the publication boundary writes
an exclusive high-water reservation to the store while holding the same lock
that assigns the event sequence. Only after that write succeeds may observer,
JSONL/text writer, and flush side effects see the event. Forked factories share
the same state and store.

`SessionWriter` implements the store by appending a typed
`event.sequence.reserved` session record. `SessionTranscript` reduces all such
records to the maximum reserved exclusive sequence. A resumed `RuntimeThread`
supplies that value and a clone of its writer when the `ThreadActor` creates
the thread event factory.

The reservation block is fixed and bounded. A crash may leave unused sequence
values, so a resumed stream can contain a gap, but it can never reuse a value.
Within one live process the P1.1c unique contiguous arrival-order guarantee is
unchanged.

## TUI User Value

After Orca or the terminal process restarts, the same recorded thread cannot
emit an event identity that an earlier TUI session already observed. This
removes a prerequisite collision from reconnect, durable task/approval state,
and stable transcript item ids. It also keeps DeepSeek token streaming free of
one disk flush per delta.

This slice is primarily a reliability prerequisite. It directly eliminates
cross-process identity reuse and unlocks the next semantic-journal and stable
item-id slices used by TUI history and recovery.

## External Compatibility

- Keep event schema version, envelope fields, event names, payloads, CLI
  arguments, TUI flows, server methods, and JSONL output unchanged.
- Add one typed session JSONL record. Existing Orca versions already ignore
  unknown non-terminal session lines; new Orca continues to read histories
  without the record.
- Histories without a reservation start from zero and reserve a block before
  their first newly published event.
- History-disabled runs keep the current in-memory sequence beginning at zero
  and perform no reservation writes.
- A resumed recorded stream is monotonic but may have an intentional gap at
  the process boundary. No consumer may require cross-process contiguity.

## Migration Sequence And Temporary State

1. Add RED core tests proving reservation happens before delivery, one block
   covers multiple events, failure prevents delivery, and resume starts after
   the previously reserved range.
2. Replace the publication counter with explicit next/reserved state and add
   the typed store boundary.
3. Add the session reservation record, transcript reduction, writer support,
   redaction/rewrite handling, and old-history compatibility tests.
4. Initialize the RuntimeHost thread publisher from the session writer and
   recovered reservation rather than calling `EventFactory::new` directly.
5. Add recorded-thread behavior tests covering first publication, resume,
   monotonic sequence identity, and absence of per-delta reservation writes.
6. Re-run focused event, history, RuntimeHost, server, and TUI tests before the
   full workspace and real DeepSeek gates.

Conversation messages, usage, plan state, task files, and completion records
remain their current persistence sources during P1.1d. This slice does not
claim that the semantic journal is complete. P1.1e will persist selected typed
lifecycle and terminal envelopes through this publication boundary; P1.1f
will replace index-derived turn/item ids using that durable identity.

Multi-process writer leases and stale-owner fencing remain P1.4. P1.1d assumes
the existing single `ThreadActor` process owner and does not hide concurrent
process ownership behind file-lock conflict repair.

## Acceptance Criteria

- A recorded thread persists a reservation before sequence zero is observable.
- Multiple events inside one block require one reservation write, including
  high-volume transient deltas.
- Crossing a block boundary persists the next reservation before publishing
  the first event in that block.
- Reservation failure returns an I/O error before observer or writer delivery
  and does not consume an unreserved sequence.
- Observer or writer failure after successful reservation still consumes its
  assigned sequence, preserving P1.1c cleanup behavior.
- Resuming a recorded thread begins at or above the prior exclusive
  reservation and never reuses an earlier `(run_id, seq)` identity.
- Fresh, history-disabled, and legacy-history behavior remains compatible.
- Compression, rewrite, redaction, load, resume, and fork paths preserve or
  deliberately reset the reservation according to thread identity.
- Focused core, history/thread-store, RuntimeHost, server, and TUI tests pass.
- The serial workspace all-targets gate, workspace Clippy, and targeted real
  DeepSeek CLI/history smoke pass before integration.

## Final Deletion Targets

- direct `EventFactory::new` construction in `ThreadActor::new`;
- any recovery path that infers the next event sequence from live memory or
  message indexes;
- any proposed per-delta high-water write path;
- tests that assume a resumed recorded thread restarts at `seq=0`.
