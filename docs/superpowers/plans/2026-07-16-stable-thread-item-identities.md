# P1.1f Stable Thread And Item Identity Plan

- Status: complete; release target `v0.2.31`
- Base: `a9aec6ab3ca5aeaa733cc8ebb587ab185058f22c`
- Branch: `codex/stable-thread-item-ids-p11f`

## Structural Problem And Evidence

P1.1c through P1.1e establish one ordered event publication boundary, reserve
non-repeating sequence ranges, and append selected semantic envelopes before
observer or output visibility. Public thread history still discards that
identity. `thread_store/projection.rs` groups the current message vector again
on every read and formats `turn-N` and `item-N` from the resulting positions.
Compaction, compatibility repair, filtering, or a later projection change can
therefore rename an already observed turn or item.

The same index identity currently controls live server work. `ServerThread`
predicts the next `turn-N` from a snapshot before starting an operation, passes
that value through `HostedTurnRequest::with_task_id`, and the process-level
`ServerActiveTurnRegistry` keys all threads by that value. Two threads can both
predict `turn-1`; the second insert can replace the first route, so interrupt,
resume, steer, permission, and user-input control may address the wrong
operation. This is both an identity bug and an ownership bug: persisted turn,
runtime task, and active-operation routing are separate domains.

`SessionRecord::Message` currently contains only the model-facing message.
There is no durable item id or owning turn id for ordinary user and assistant
items. Tool and workflow projections already have domain identities such as a
DeepSeek tool-call id or workflow run id, but the outer `StoredThreadItem`
wrapper ignores them and assigns a new global item index. `turn.started` is
journaled, but its payload contains only a numeric lifecycle counter, optional
prompt, and task snapshot; it does not carry an opaque stable turn id.

Codex supplies two applicable principles, not a schema to copy: create opaque
item ids at the durable history boundary, persist the owning turn id with each
response item, and use explicit turn-start records for reconstruction. Orca
must keep its Rust message model, synchronous JSONL append boundary, and
DeepSeek tool-call identities. It must not infer identity by matching prompt
text, hashing mutable content, or replaying transient token deltas.

## Target Ownership And Module Boundary

`orca-core` owns transparent typed `TurnId` and `ConversationItemId` values.
Fresh values use UUIDv7-backed opaque strings with type prefixes. Callers may
serialize or compare them but may not construct new durable identity from a
message index.

`HostedTurnRequest` owns one `TurnId` for the logical turn. The id is allocated
once before actor admission, survives interrupt/resume generations, and is
copied into `ThreadTurnRequest`. It is separate from `OperationId`, the runtime
task lifecycle id, and the `TaskRegistry` main-session task id. The runtime
turn-start event includes this id in its typed payload; P1.1e makes that payload
durable before provider or tool work becomes visible.

`SessionWriter` owns durable identity assignment for ordinary conversation
records. Each newly appended `conversation.message` receives one
`ConversationItemId` and the writer's current scoped `TurnId`. Writer clones
share the ordered in-memory record ledger but retain their own turn scope, so a
background provider keeps the turn that created it while the foreground writer
may later enter another turn. The record append succeeds before the ledger is
updated. `ThreadTurnContext` sets the writer scope before it appends accepted
user input.

`SessionTranscript` exposes identified conversation records as the projection
source and continues to expose `messages` as a derived compatibility view for
model replay. There is one message append path and one set of bytes; these are
not independent facts. Conversation content, compaction, tool-boundary repair,
and provider replay remain owned by their current records and reducers.

`ThreadStore` projects identified records through one internal item model:

- a record's `turn_id` defines its turn boundary;
- ordinary user or assistant items use the record's `item_id`;
- tool items keep the persisted DeepSeek tool-call id, with the existing
  `:file-change` suffix for a second file-change projection;
- workflow items keep their workflow run id;
- tool result records update the matching request item and do not mint a second
  public item id.

The live server receives the same `TurnId` from the admitted request. Active
turn routing keys by that id, and `turn.started` exposes it additively as
`turnId`. Runtime task lifecycle and TaskRegistry ownership no longer change in
order to satisfy a server projection id.

## TUI User Value

A saved conversation can be reopened, compressed, repaired, paginated, and
resumed without changing the identity of surviving turns and items. A TUI or
server client can reconcile a cold history snapshot with later live events
instead of discarding and rebuilding rows because `item-7` became `item-5`.

Concurrent server threads also receive globally unique turn routes. Interrupt,
resume, steer, and interactive responses can no longer collide merely because
two conversations are both on their first turn. This directly improves TUI
control reliability even though the visible labels and interaction flow remain
unchanged.

## External Compatibility

- Keep CLI arguments, TUI keys and flows, server methods, response field names,
  event names, model-visible conversation content, and existing tool/workflow
  item shapes.
- Treat turn and item id strings as opaque protocol values. New turns no longer
  use the `turn-N` value pattern; clients must not parse that undocumented
  shape.
- Add `turnId` to `turn_started` output and add optional `id` / `turn_id`
  identity fields to `conversation.message` JSONL records. Older Orca versions
  ignore the extra record fields; new Orca accepts records without them.
- Resume preserves existing identified records and appends new turns with fresh
  ids. Interrupt/resume generations retain the same logical `TurnId`.
- Fork mints a new thread and event stream. Inherited message-only history is
  projected through the explicit legacy path; every newly executed fork turn
  uses fresh typed identity.
- History-disabled threads use fresh in-memory turn ids. Their message/item
  projection may use the legacy fallback because no restart contract exists,
  but process-level active-turn routing must still be collision-free.

## Legacy Fallback And Migration Limit

Legacy records without typed identity remain readable without rewriting user
files. One named legacy projector may derive `turn-N` / `item-N` from original
record order. The fallback is selected per legacy record span, so appending an
identified turn to an old session does not renumber the old span and new
records never enter the index path.

The fallback must not spread back into RuntimeHost, server admission, current
record writes, or identified projections. It may not inspect content to guess a
journal match. A malformed non-empty id or a turn id that changes within one
identified record group fails closed rather than silently becoming legacy.

P1.1f does not turn the semantic event journal into the model-conversation
content source. It also does not yet replace the live delta projector with a
full journal reducer. Completed model-item events and exact live/cold text-item
shape equivalence remain a later gate; this slice removes index-derived public
history identity and the active-turn routing collision without inventing a
second response-content store.

## Migration Sequence And Temporary State

1. Add RED behavior tests proving two threads cannot share an active turn id,
   new recorded turns/items retain identity across cold reload, and existing
   position-derived tests fail for identified records.
2. Introduce typed ids and carry one `TurnId` through hosted/direct turn
   requests, runtime lifecycle publication, server interaction handlers, and
   active-turn routing without reusing runtime task ids.
3. Add optional typed identity to conversation records, writer-scoped turn
   ownership, transcript reconstruction, clone ordering, redaction/rewrite, and
   zstd round-trip coverage.
4. Replace identified message projection with record-based grouping and stable
   item ids. Isolate legacy grouping and update pagination/filtering to treat
   ids as opaque values while cursors remain position-based.
5. Cover resume, interrupt/resume generation identity, fork, legacy/new hybrid
   history, compaction, compatibility repair, archive/rename, and malformed
   identity behavior.
6. Rebase latest `main`, run focused core/runtime/thread-store/server/TUI tests,
   then run the serial workspace gate, Clippy, and a real DeepSeek two-process
   recorded resume smoke that compares turn/item ids before and after restart.

## Acceptance Criteria

- Every admitted logical turn has one typed opaque id that differs from
  operation, runtime task, and TaskRegistry ids and survives all resumed
  generations.
- Two concurrently active threads cannot collide in `ServerActiveTurnRegistry`;
  control and interactive responses remain fenced to the correct thread and
  turn.
- `turn.started` journals and projects the admitted `turnId` before provider or
  tool visibility.
- Every new durable conversation record has a non-empty typed item id and turn
  id. Append failure cannot expose the record through the writer ledger.
- Cold history and live recorded-thread reads return the same stable turn/item
  ids. Reload, resume, pagination, rename, archive, zstd compression/restore,
  redaction, and compatible repair do not rename surviving identified items.
- Tool results retain the request item's id and produce exactly one public
  wrapper. File-change and workflow secondary projections retain their existing
  domain suffix/id rules.
- Legacy histories and hybrid legacy-plus-new histories remain readable. Only
  legacy spans emit `turn-N` / `item-N`; all new writes use typed identity.
- Forked new turns have ids independent from the parent, while inherited legacy
  content remains readable without a synthetic journal rewrite.
- Model conversation reconstruction, DeepSeek requests, tool boundary repair,
  compaction content, usage, plan, task registry, and completion recovery keep
  their pre-P1.1f fact ownership.
- Focused identity, journal, RuntimeHost, ThreadStore, server, and TUI tests
  pass, followed by the serial workspace gate, workspace Clippy, and targeted
  real DeepSeek resume/history verification.

## Final Deletion Targets And Gates

P1.1f deletes:

- `ServerThread::next_persisted_turn_id` and message-count turn prediction;
- server use of `HostedTurnRequest::with_task_id` for persisted turn identity;
- index-derived ids from all identified/new ThreadStore projection paths;
- tests that require new turns or items to equal `turn-N` / `item-N`;
- any global active-turn route that is not keyed by typed opaque `TurnId`.

`turn-N` and `item-N` may remain only in the named legacy fallback and tests
that construct records without identity. Their final deletion gate is an
explicit history migration tool or a compatibility sunset, not another read
path layered onto current records.

The static live text-item ids and combined persisted assistant-message shape
are not allowed to become a second durable identity source. A later canonical
completed-model-item reducer must either make live and cold text item shapes
identical or delete the remaining live-only ids; P1.1f documents that gate and
does not claim replay equivalence before it exists.

## Completion Evidence

The implementation landed as four independently reviewable slices:

- `9857c4c05` records the structural problem, target ownership, migration, and
  deletion gates before implementation.
- `9c99f5e7c` introduces typed UUIDv7 turn/item ids and separates logical turn
  identity from operation, runtime task, and TaskRegistry identity.
- `849c15a16` persists writer-scoped conversation identity and replaces current
  ThreadStore projection with record-owned ids while isolating legacy fallback.
- `b201ff496` fixes the release harness to control turns by logical `turnId` and
  adds the two-process stable-identity DeepSeek gate.

Deletion audit confirms `next_persisted_turn_id`, the old
`stored_messages_to_thread_*` projectors, and server `with_task_id` identity
reuse are gone. The only numeric fallbacks are the two named legacy helpers in
`thread_store/projection.rs`.

Focused validation covers core identity parsing, identified transcript IO,
RuntimeHost generation ownership, live and cold server projections,
ThreadStore pagination and mutations, resume, fork, compaction, compatibility
repair, redaction, zstd restore, malformed identity, tool result merging, file
change ids, and workflow run ids. The final serial
`cargo test --workspace --all-targets -- --test-threads=1` gate and
`cargo clippy --workspace --all-targets` pass; Clippy reports only the existing
non-deny warning baseline.

`node scripts/release/real-api-e2e.mjs --skip-build --max-budget 0.02
--timeout-ms 300000` passes against the configured DeepSeek API. Its isolated
identity case records one turn, reads typed ids from a cold server process,
resumes the same thread from a second `orca exec` process, recalls the unique
first-process sentinel, and verifies the old turn/item id prefix remains exact
while the new turn and items receive fresh typed ids. The complete provider,
CLI, compatibility-repair, server memory, active interrupt/resume, metadata,
search, pagination, and cold-read gates pass in the same run.
