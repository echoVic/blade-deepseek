# P1.1g Canonical Completed Model Item Plan

- Status: planned; release target `v0.2.32`
- Base: `84062f9b1c73d6db974f99d7db1196dbde713d30`
- Branch: `codex/canonical-completed-model-items-p11g`

## User Value, Architecture Value, And Slice Acceptance

P1.1g makes a completed DeepSeek response look identical while it is live and
after the thread is reloaded. A TUI or server client can keep the same
assistant, reasoning, and proposed-plan rows across completion, restart,
pagination, and resume instead of replacing streamed rows with a differently
shaped persisted assistant-message row.

The architecture value is one durable completed-response fact. Runtime
streaming, server item lifecycle projection, model conversation replay, and
ThreadStore cold projection will share its typed item ids and completed item
shapes. This removes the remaining static live ids and the second assistant
shape source left after P1.1f.

This slice is accepted only when behavior tests prove all of the following:

- each provider response receives fresh item identities before its first
  visible delta, including later responses in a tool loop;
- `item_started`, item delta, `item_completed`, live ThreadStore, and cold
  ThreadStore use the same item id and item kind;
- assistant text, DeepSeek reasoning, and proposed-plan text replay as the same
  complete JSON objects after process restart;
- provider suspension and approval continuation retain the original response
  identities instead of allocating a second set;
- new recorded assistant responses have exactly one durable completion source;
- legacy `conversation.message` assistant records remain readable through one
  named compatibility reducer;
- model conversation reconstruction and DeepSeek tool-call/result ordering do
  not change;
- no current runtime path can emit the static `item-agent-message-1`,
  `item-reasoning-1`, or `item-plan-1` ids.

## Structural Problem And Evidence

`ProjectedTextItem` currently assigns one process-global static id per text
kind. `RuntimeEventProjector` creates those items from transient DeepSeek token
deltas, accumulates a second copy of their text, and fabricates completed items
when it sees `session.completed`. A second provider response in the same turn
therefore reuses the same ids, and the projector owns completion independently
from runtime persistence.

The durable path has a different model. `SessionWriter::append_message`
allocates one `ConversationItemId` only after the provider stream has finished
and stores the response as a combined assistant `conversation.message`.
`thread_store/projection.rs` later reconstructs a single object containing
`role`, `content`, `reasoningContent`, and `toolCalls`. It does not reconstruct
the live `agent_message`, `reasoning`, and `plan` objects.

The P1.1f live/cold regression does not cover this gap. Both sides of that test
query conversation-record projection, so they agree with each other while the
actual live `item_completed` objects remain outside the comparison. Current
session-server tests separately prove that reasoning and plan lifecycle events
exist, but do not restart the process and compare those objects with
`thread/items/list`.

This is a persistence-model boundary defect, not a local server formatting
bug. Patching only `RuntimeEventProjector` would preserve two completion
authorities. Patching only ThreadStore would require it to invent ids and split
plan text after the fact without knowing which ids the client already saw.

## Target Ownership And Module Boundaries

`orca-core` owns two new typed values:

- `ModelResponseItemIds` contains the conversation item id plus the ids reserved
  for agent-message, reasoning, and proposed-plan items for one provider
  response;
- `CompletedModelResponse` contains the owning `TurnId`, the model-facing
  assistant message, and the canonical completed text items derived from that
  message with those ids.

The agent-message item uses the response's conversation item id. Reasoning and
plan items receive their own opaque `ConversationItemId` values. Unused ids are
allowed; they are reservations, not persisted rows. Tool calls retain their
DeepSeek call ids and file-change suffix rules.

The runtime allocates `ModelResponseItemIds` before starting one provider
sampling request. The ids travel with the response across a suspended stream,
host-adopted background work, persisted approval state, and continuation. Delta
events carry the reserved id needed by the live projector. This is runtime
identity assignment, not a provider responsibility; DeepSeek remains unaware
of Orca ids.

After a complete provider response passes runtime validation, the runtime
builds one `CompletedModelResponse` and publishes a typed
`model.response.completed` event. That event is semantic. The existing
thread-owned publication boundary assigns its sequence and timestamp, appends
it before observer/output visibility, and remains the only publication owner.

`SessionWriter` is the only durable reducer for the new event. It stores the
exact typed envelope and derives the corresponding assistant conversation
record in its shared ordered ledger only after the append succeeds. New
assistant responses are no longer also appended as `conversation.message`.
User and tool messages keep their current record format.

`SessionTranscript` and `JsonlThreadStore` reduce the same semantic event into:

- the `Message::Assistant` value used for model replay;
- a record-owned canonical completed-item list used for public history;
- the existing tool-call projections, merged with later tool results.

`RuntimeEventProjector` becomes a live lifecycle adapter. It uses ids supplied
by delta events for `item_started` and item delta notifications, then forwards
the completed objects carried by `model.response.completed`. It no longer owns
completed text, static ids, or terminal completion synthesis.

## External Compatibility

- Keep CLI arguments, TUI keys and flows, server request methods, JSONL envelope
  fields, item event names, and ThreadStore response wrappers.
- Add `model.response.completed` to the runtime JSONL event schema. Existing
  consumers that ignore unknown event types remain compatible.
- Add item identity fields to assistant delta payloads. Existing `text` remains
  unchanged.
- Keep public item JSON shapes already used by live server events:
  `agent_message`, `reasoning`, and `plan`.
- Cold history for newly recorded responses changes from the combined
  persisted-assistant object to those existing live shapes. This is the
  intentional compatibility correction. Item ids remain opaque strings.
- Keep legacy assistant records readable in their historical combined shape;
  do not rewrite user history during normal read or resume.
- Keep the on-disk JSONL format append-compatible. The new semantic event is an
  additive record type already accepted by P1.1e readers.

## Migration Sequence And Temporary State

1. Add RED behavior tests that compare live completion objects with a cold
   process reload, cover multiple model responses in one turn, and prove
   reasoning/plan restart equivalence.
2. Add the core response identity and completed-response types, including one
   pure proposed-plan reducer and typed serialization tests.
3. Carry response identities through normal streaming, provider suspension,
   background approval persistence, and continuation. Add the identity fields
   to transient delta events.
4. Publish one semantic completed-response event after runtime acceptance and
   stop writing new assistant `conversation.message` records.
5. Teach SessionWriter, transcript loading, and JsonlThreadStore to reduce the
   event into model replay plus canonical public items. Keep the old assistant
   message reducer only for histories that lack a canonical completion fact.
6. Replace RuntimeEventProjector's text accumulator with an id-aware lifecycle
   adapter and delete terminal item synthesis and static ids.
7. Rebase latest `main`, rerun affected focused tests, then run the serial
   workspace gate, Clippy, release helpers, and the real DeepSeek two-process
   record/reload/resume smoke with full item-object comparison.

Steps 2 through 6 are one vertical implementation slice. No intermediate state
that writes both assistant record forms is eligible to merge or release.

## Failure And Recovery Rules

- A completed-response append failure prevents `model.response.completed` and
  public `item_completed` visibility. The failed sequence identity is consumed
  under the existing P1.1 publication rule.
- A truncated final semantic record remains recoverable under the existing
  JSONL tail rule; a complete malformed record fails closed.
- A response event with malformed turn, conversation, or item identity fails
  closed during ThreadStore projection. It is not silently converted to an
  index id.
- A legacy assistant record has no canonical item list and continues through
  the named legacy projector. A current completed-response event never falls
  back to that path.
- An interrupted provider stream has no completed model response. Turn terminal
  state owns interruption; the projector must not invent a durable completed
  item from transient partial deltas.

## Stage Validation

### Plan checkpoint

- plan includes structural evidence, ownership, user value, compatibility,
  migration, recovery, acceptance, and deletion targets;
- branch is based on current `origin/main`.

### RED checkpoint

- focused tests fail because live completion objects use static ids and cold
  projection uses the combined assistant shape;
- tests inspect behavior and serialized values, not source text.

### Canonical reducer checkpoint

- focused core event-schema, provider-turn, RuntimeHost, writer, ThreadStore,
  server-runtime, session-server, and TUI adapter tests pass;
- suspended/background approval continuation retains ids in round-trip tests;
- legacy, malformed, rewrite, redaction, archive, zstd, and compaction coverage
  passes.

### Release checkpoint

- rebase latest `main` before full validation;
- `cargo test --workspace --all-targets -- --test-threads=1` passes;
- `cargo clippy --workspace --all-targets` passes with no new warnings;
- site, SEO, workflow, npm staging, and public verifier helper tests pass;
- the real DeepSeek smoke records, cold-reads, resumes in a second process, and
  compares canonical item ids and full item objects;
- docs, roadmap, and `v0.2.32` release notes describe the final architecture;
- `main`, tag, GitHub Release, npm packages, npm executable, and release assets
  are verified publicly before worktree cleanup.

The repository's three pre-existing `cargo fmt --all -- --check` differences
remain the baseline; P1.1g must not add another formatting difference.

## Final Deletion Targets

P1.1g is incomplete until it deletes:

- `ProjectedTextItem` and `ProjectedTextItemKind` static-id ownership;
- `item-agent-message-1`, `item-reasoning-1`, and `item-plan-1` from production
  paths;
- `RuntimeEventProjector::project_terminal_items` and completed text assembled
  only from transient deltas;
- new assistant writes through `SessionWriter::append_message`;
- current-record cold projection through
  `ProjectedPersistedMessageThreadItem::Assistant`;
- tests that validate live and cold text items independently without comparing
  their complete objects.

The combined persisted assistant projection may remain only behind the named
legacy-record branch. Its final removal gate is an explicit history migration
or compatibility sunset. It must not receive new records or become a fallback
for malformed current events.
