# P1.1e Semantic Event Journal Plan

- Status: completed
- Base: `2b86633b63455df789a59fee668eed161cf7c56e`
- Branch: `codex/semantic-event-journal-p11e`

## Structural Problem And Evidence

P1.1c gives every live thread one ordered publication boundary, and P1.1d
reserves sequence ranges before publication so `(run_id, seq)` cannot repeat
after process recovery. The final `EventEnvelope` still exists only long enough
for the observer and CLI writer callbacks, however. Session JSONL separately
persists conversation messages, task responses, compaction records, usage,
plan state, completion, and the sequence reservation, but not the semantic
event identity that ordered those facts for the TUI or server.

That gap blocks stable `turn` and `item` identities. The server still derives
`turn-N` and `item-N` from message indexes, so insertion, compatibility repair,
or a later projection change can rename an item. A reconnecting TUI also has no
durable semantic boundary from which later approval, tool, workflow, or
verification recovery can be built.

Persisting every envelope is not acceptable. `assistant.reasoning.delta`,
`assistant.message.delta`, tool progress/output chunks, usage/context
projections, subagent progress, and task-list projections are high-volume or
replaceable state. Writing and flushing each DeepSeek delta would put history
I/O directly on token latency and turn transient rendering into permanent
history.

`EventDraft::publish` is the only boundary that owns the final sequence and
timestamp while holding the publication lock. It currently reserves a sequence
block, constructs the envelope, then invokes observer/writer/flush callbacks.
The journal must be inserted there, after identity assignment and before any
outward visibility. Adding it in the TUI, server, controller, or individual
event call sites would recreate multiple ordering authorities and miss
background producers.

Codex provides a useful principle rather than an implementation to copy: it
converts selected completed protocol events into durable rollout items and uses
explicit flush barriers, while streaming deltas remain delivery events. Orca
keeps its synchronous Rust publication boundary and typed session JSONL store;
it does not add Codex's asynchronous recorder or a second event loop.

## Target Ownership And Module Boundary

`EventPublication` remains the only owner of final event identity and order. Its
durability dependency becomes one typed `EventPublicationStore` that owns both:

1. exclusive sequence-range reservation; and
2. append-and-flush of selected final `EventEnvelope` values.

For a semantic draft, publication performs these operations under the same
lock: reserve if needed, construct the final envelope, append and flush the
journal record, then call the observer/output writer. A journal append failure
prevents observer and output delivery. Once a journal append has been
attempted, its sequence is consumed even on error because a partial append is
ambiguous and no later event may reuse that identity. A reservation failure
still consumes no unreserved sequence.

`SessionWriter` is the sole durable implementation. It writes a typed
`event.semantic` record containing the original envelope fields:
`version`, `run_id`, `seq`, `timestamp_ms`, `type`, and `payload`.
`SessionTranscript` exposes the ordered semantic envelopes for later projection
work but does not reduce them into conversation or task state in this slice.
The existing history redaction policy continues to apply recursively to
journal payload strings before bytes reach disk.

`observe_event` must treat a configured journal as a real publication sink. A
semantic event is journaled even when no external observer is attached;
transient events with no observer remain unpublished and do not consume a
sequence or reservation.

## Semantic Whitelist

The whitelist is closed and compiler-checked in `EventType`. Adding a new event
type requires an explicit journal decision.

| Category | Journaled event types | Reason |
| --- | --- | --- |
| Thread and turn lifecycle | `session.started`, `turn.started`, `model.routed` | Stable boundaries and routing decision for later turn projection |
| Context compaction | `context.compaction.started`, `context.compacted` | Durable start/terminal pair without token-level context projection |
| Interaction audit | `approval.requested`, `approval.resolved` | Stable request/result identities; P1.3 will later own resumable interaction state |
| Tool lifecycle | `tool.call.requested`, `tool.call.completed` | Stable tool item identity and exactly one observed terminal envelope |
| Plan lifecycle | `plan.updated` | Ordered plan transition; `plan.state` remains the current-state projection during migration |
| Subagent lifecycle | `subagent.started`, `subagent.completed` | Stable start/terminal pair without progress spam |
| Workflow lifecycle | `workflow.started`, `workflow.resumed`, `workflow.phase.started`, `workflow.phase.completed`, `workflow.agent.started`, `workflow.agent.cached`, `workflow.agent.completed`, `workflow.agent.failed`, `workflow.paused`, `workflow.stopped`, `workflow.completed`, `workflow.failed`, `workflow.result.available` | Ordered lifecycle and terminal audit while the task registry remains the operational state owner |
| Verification lifecycle | `verification.started`, `verification.completed` | Stable verification item and terminal result |
| Failure and completion | `error`, `session.completed` | Durable terminal/error boundaries |

## Explicit Non-Journal Events

| Event types | Ownership reason |
| --- | --- |
| `assistant.reasoning.delta`, `assistant.message.delta` | DeepSeek streaming delivery; completed conversation messages remain the durable content source |
| `tool.call.progress`, `tool.output.delta` | High-volume live tool rendering; `tool.call.completed` carries the semantic terminal |
| `provider.replay.updated` | Provider retry/replay transport state, not a stable user item |
| `usage.updated`, `context.updated` | Replaceable live projections; `session.usage` and compaction records remain durable facts |
| `subagent.progress` | Replaceable live progress between journaled start and terminal events |
| `workflow.tasks.updated`, `task.status.updated` | Task-registry projections; task files remain the operational truth until P1.4 defines journal-derived task recovery |

## Existing Fact Ownership And Migration Limit

P1.1e is an append-only audit and stable-identity source, not a second mutable
domain store. During this bounded migration:

- `conversation.message` remains the only source used to rebuild model-visible
  conversation content;
- task-session files remain the only source used to recover task registry
  state;
- `plan.state` remains the source for the current unfinished plan;
- `session.usage`, its baseline, and background provider response records
  remain the source for usage totals;
- context summary/collapse records remain the source for reconstructed context;
- `session.completed` remains the source for current completion status.

The journal may repeat an event payload for audit and identity, but no current
domain reducer reads both sources or chooses between them. P1.1f may consume
journal identity for stable server turn/item ids without changing conversation
recovery. Later slices may remove a legacy fact only after one typed journal
reducer owns that domain, old histories have an explicit fallback/migration,
and behavior tests prove there is no parallel write path.

## TUI User Value

This slice removes the crash window in which a semantic event could be visible
to the TUI/server but have no durable identity. It gives the next P1.1f slice a
reliable source for stable history item ids across restart, compaction, and
compatibility repair. It also establishes the durable request/terminal audit
needed for later reconnect and recovery work without making DeepSeek token
streaming wait on one disk flush per delta.

The immediate user-facing behavior remains compatible, so P1.1e is a
reliability prerequisite rather than a release by itself.

## External Compatibility

- Keep event schema version, JSON envelope fields, event names, payloads, CLI
  arguments, TUI flows, server methods, and JSONL output unchanged.
- Add only the typed `event.semantic` session record. Older Orca versions
  already ignore unknown non-terminal session lines; new Orca reads histories
  without journal records as legacy histories.
- Resume appends to the same thread journal and begins at the reserved sequence
  high-water mark. Fork copies conversation state but mints a new thread/event
  identity and starts a new empty journal.
- Rename rewrites, archive moves, redaction, zstd compression, and restoration
  must preserve semantic records.
- History-disabled execution keeps current output behavior and performs no
  journal writes.

## Migration Sequence And Temporary State

1. Add RED core behavior tests for whitelist classification, journal-before-
   observer ordering, exact envelope retention, transient exclusion, no-observer
   semantic publication, and persistence failure semantics.
2. Replace the sequence-only store boundary with the combined typed publication
   store and journal selected envelopes inside `EventDraft::publish`.
3. Add `event.semantic` session records, transcript loading, recursive payload
   redaction, writer support, and round-trip tests.
4. Add recorded RuntimeHost tests proving output visibility follows durable
   append, deltas do not enter the journal, resume appends with the restored
   identity, and fork/legacy behavior remains compatible.
5. Cover rewrite, compression/restore, metadata rename, and malformed trailing
   legacy record handling without introducing a journal-derived domain reducer.
6. Rebase latest `main`, rerun focused tests, then run the serial workspace
   all-targets gate, workspace Clippy, and the real DeepSeek CLI/history smoke.

## Acceptance Criteria

- Every whitelisted event is represented by one typed `event.semantic` record
  before an observer or output writer can see its envelope.
- Every non-journal event is explicitly classified and produces no semantic
  record, including high-volume reasoning/message/tool deltas.
- A journal record preserves the envelope's original version, run id, sequence,
  timestamp, event type, and payload subject only to the existing disk
  redaction policy.
- A journal append failure prevents observer/output delivery and consumes the
  ambiguous assigned sequence; a reservation failure prevents delivery without
  consuming an unreserved sequence.
- A semantic event with no external observer is still journaled; a transient
  no-observer draft remains unpublished.
- Recorded RuntimeHost turns journal semantic lifecycle/error/terminal events
  in the same order and with the same identities seen by output/observers.
- Resume continues the same journal above the prior reservation. Fork starts a
  journal for the new thread id at sequence zero. Legacy histories without
  semantic records remain readable and resumable.
- Rewrite, rename, redaction, archive, zstd compression, and restoration retain
  journal records and their order.
- Conversation, task registry, plan, usage, compaction, and completion recovery
  continue using exactly their pre-P1.1e fact sources.
- Focused core publication, thread-store/history, RuntimeHost, server, and TUI
  tests pass, followed by the serial workspace gate, Clippy, and targeted real
  DeepSeek CLI/history verification.

## Final Deletion Targets And Gates

P1.1e deletes the sequence-only durability abstraction; no parallel event
journal writer is permitted in TUI, server, controller, or background workers.

The following existing records are not deleted in this slice. Each has an
explicit later gate:

- index-derived `turn-N` / `item-N`: delete in P1.1f after all history/server
  projections use journal identities with a legacy fallback;
- `plan.state`: delete only after a journal reducer reconstructs current plan
  state and legacy migration is proven;
- background task response/task-session persistence: delete or narrow only
  after P1.4 provides leased, fenced, journal-derived task recovery;
- completion, usage, compaction, and conversation records: delete only after a
  typed reducer owns that domain and model-visible replay plus old-history
  compatibility pass full and real-API gates.

No compatibility layer introduced by P1.1e may become a second event
sequencer, a per-surface journal, or a per-delta persistence path.

## Completion Evidence

- Implementation commit: `c037cfc21 refactor(runtime): journal semantic events
  before publication`.
- The RED publication test first demonstrated that output could become visible
  before semantic durability. The green implementation now appends and flushes
  the complete envelope under the publication lock before observer or output
  delivery. Reservation failure does not consume an unreserved sequence;
  journal failure blocks delivery and consumes the ambiguous assigned sequence.
- The sequence-only `EventSequenceStore` is deleted. `EventPublicationStore`
  is the one typed reservation/journal boundary, `SessionWriter` is its only
  durable implementation, and no TUI, server, controller, or background-worker
  journal path was added.
- Focused validation passes: 36 core event tests, all 161 `orca-core` tests,
  775 runtime unit tests, 48 RuntimeHost integration tests, 390 TUI tests, 14
  exec JSONL contracts, 11 history contracts, 21 server-runtime tests, 132
  session-server tests, and 14 thread-store writer tests. Rewrite, zstd
  compression/restore, redaction, malformed-record, resume, fork, and legacy
  history behavior also pass.
- `cargo test --workspace --all-targets -- --test-threads=1` passes. `cargo
  clippy --workspace --all-targets` passes with the repository's existing
  warning baseline. `cargo fmt --all -- --check` still reports only the known
  pre-existing drift in `runtime_host.rs`, unrelated RuntimeHost test blocks,
  and `orca-tui/src/app.rs`; this slice does not reformat those unrelated
  regions.
- The existing real DeepSeek CLI and malformed-history replay gate passes,
  including compatibility repair of `legacy-missing-tool-call` as
  `indeterminate` without re-executing it.
- A dedicated isolated two-process DeepSeek smoke persisted four exact semantic
  envelopes from the first process at `seq=0..56` after reserving through
  `256`; 51 visible reasoning/message delta events produced no journal record.
  The second process resumed the same thread id, persisted four exact envelopes
  at `seq=256..300`, and reserved through `512`; its 39 visible deltas also
  produced no journal record. In both processes the journaled envelopes matched
  the externally visible envelopes field-for-field.
- P1.1e remains an unreleased reliability prerequisite. P1.1f is the next
  vertical slice and may consume journal identity for stable server/history
  turn and item ids without changing conversation replay ownership.
