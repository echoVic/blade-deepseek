# P1.1h Canonical User Turn Input Plan

- Status: planned; release target `v0.2.33`
- Base: `095c164b15547ec51b0323b7d2744c02528ac287`
- Branch: `codex/canonical-user-turn-input-p11h`

## User Value, Architecture Value, And Slice Acceptance

P1.1h makes one submitted TUI or server prompt remain one transcript row. A
user can complete, paginate, restart, and resume a thread without seeing every
prompt duplicated in `thread/read`, `thread/turns/list`, or
`thread/items/list`. Item counts, search results, and client-side row identity
therefore describe the conversation that the model actually received.

The architecture value is one owner for user-turn admission. A hosted
`ThreadTurnContext` owns adding the prompt to an existing session conversation
and appending its identified `conversation.message` record. Agent-loop
bootstrap owns only creation of an independent conversation when no runtime
session was supplied. Borrowing a session conversation must not imply a second
history bootstrap.

This slice is accepted only when behavior tests prove all of the following:

- one hosted turn persists exactly one user conversation record with the
  request's `TurnId` and one opaque `ConversationItemId`;
- two turns persist exactly two user records, one in each logical turn;
- live `thread/read`, `thread/turns/list`, and `thread/items/list` expose each
  submitted prompt once, with the same turn and item identities;
- cold reads after server exit expose the same user item prefix and counts;
- resuming the thread in another process does not replay or append an earlier
  user prompt again;
- independent child-agent bootstrap still creates one model-facing user
  message and does not acquire a session persistence responsibility;
- tool results, canonical completed model items, approval continuation, active
  steering, compaction, and legacy history remain behaviorally unchanged.

## Structural Problem And Evidence

`ThreadTurnContext::prepare` currently enters the request's logical turn,
adds the prompt to the runtime-owned conversation, and immediately calls
`SessionWriter::append_message`. That is the correct admission boundary: the
conversation mutation and durable user record either both happen before model
execution or the turn fails to start.

The controller then passes the same mutable conversation and writer into
`run_agent_loop` through three independent optional fields on
`AgentConversationContext`. `RuntimeConversationBootstrapStep::prepare`
cannot tell whether that conversation was just created by the agent loop or
was already admitted by the runtime session. It therefore calls
`record_initial_history_for_agent` unconditionally. For a borrowed hosted
conversation, that function appends the already-persisted final user message a
second time.

A current mock server reproduction proves the mismatch:

- the model-facing conversation contains exactly two user messages after two
  turns;
- the shared durable conversation ledger contains four user records;
- each duplicate pair shares a `TurnId` but has two different generated
  `ConversationItemId` values;
- both live turns and item-list APIs expose the duplicate rows.

This is a persistence-ownership defect, not a projection defect. Read-time
deduplication would preserve two durable facts, hide conflicting identities,
make pagination unstable, and leave search and future reducers vulnerable to
the same corruption.

## Target Ownership And Module Boundaries

`AgentConversationContext` becomes a typed provenance boundary instead of a
bag of optional references:

- `Owned` means the agent loop must construct an independent conversation from
  the prompt. It has no session writer and therefore cannot append hosted
  history.
- `Borrowed` contains a runtime-owned mutable conversation and optional writer.
  The caller has already admitted the user input. The agent loop may append
  later tool results and semantic model completions through that writer, but it
  must not bootstrap the prompt again.

`ThreadTurnContext::prepare` remains the only hosted user-input writer. It
enters the logical turn, mutates the conversation, and appends the exact new
message before provider execution. `RuntimeConversationBootstrapStep` only
selects owned versus borrowed storage, binds the writer to the current turn,
and returns the prepared parts used by later loop stages.

The existing `SessionWriter` ledger remains the single live and cold
projection source. No projection-specific duplicate filter is added.

## External Compatibility

- Keep CLI arguments, TUI key flows, server methods, JSONL event names, and
  response wrappers unchanged.
- Keep persisted `conversation.message` and semantic-event formats unchanged.
- Newly recorded hosted turns contain one user record instead of two. This is
  a correctness fix to public item counts and pagination.
- Existing histories with duplicate user records remain readable and are not
  rewritten during ordinary read, resume, compaction, or archive operations.
- Model-facing conversation content remains unchanged; the duplicate existed
  only in the durable projection ledger.
- Active steer messages remain independently admitted user inputs inside the
  owning logical turn and keep their current identifiers.

## Migration Sequence And Temporary State

1. Add RED server/runtime behavior tests that assert exact user counts and
   identities across messages, turns, items, and the persisted transcript.
2. Add RED real-harness assertions that each submitted sentinel prompt appears
   exactly once in live and paginated public projections.
3. Replace `AgentConversationContext` optional-field construction with explicit
   owned and borrowed variants.
4. Remove initial-history replay from the agent-loop bootstrap path. Keep
   `ThreadTurnContext` as the hosted admission owner and retain the writer for
   later tool/model records.
5. Delete the obsolete initial-history helper and source-shape assertions that
   preserve its old ownership. Replace them with provenance and behavior
   tests.
6. Rebase latest `main`, rerun affected focused tests, then run the serial
   workspace gate, Clippy, release helpers, and real DeepSeek cross-process
   server history verification.

Steps 3 through 5 are one vertical implementation slice. A state that adds a
read-time deduplicator while retaining both writes is not eligible to merge or
release.

## Failure And Recovery Rules

- If the canonical user-record append fails, turn preparation fails before
  provider execution; the in-memory prompt must not proceed as an
  unpersisted hosted turn.
- A later tool or completed-model append failure keeps the existing durable
  publication failure semantics and does not cause user input re-admission.
- Existing duplicate histories are preserved verbatim. The reader does not
  guess which opaque item id was intended or silently discard records.
- A borrowed conversation without a writer remains valid for history-disabled
  sessions. It still has one in-memory prompt and no durable record.
- An owned agent conversation never receives a hosted writer. Adding such a
  use case later requires a new explicit ownership variant and tests rather
  than another optional flag.

## Stage Validation

### Plan checkpoint

- plan includes structural evidence, target ownership, TUI/server user value,
  compatibility, migration, recovery, acceptance, and deletion targets;
- branch is based on current `origin/main`.

### RED checkpoint

- focused server/runtime tests fail because one turn currently produces two
  user item ids;
- the real-harness self-test fails if its fake server repeats a user item;
- tests inspect persisted records and API values, not source text shape.

### Canonical admission checkpoint

- focused controller, runtime bootstrap, RuntimeHost, server-runtime,
  session-server, ThreadStore, JSONL, TUI, and real-harness helper tests pass;
- hosted and history-disabled turns each have the correct single in-memory and
  durable behavior;
- child agents and provider continuation retain their existing conversation
  setup behavior.

### Release checkpoint

- rebase latest `main` before full validation;
- `cargo test --workspace --all-targets -- --test-threads=1` passes;
- `cargo clippy --workspace --all-targets` passes with no new warnings;
- site, SEO, workflow, npm staging, and public verifier helper tests pass;
- the real DeepSeek gate proves exact-once user rows across live read,
  pagination, server exit, cold read, and cross-process resume;
- docs, roadmap, and `v0.2.33` release notes describe the final ownership;
- `main`, tag, GitHub Release, npm packages, executable smoke, release assets,
  and public changelog are verified before worktree cleanup.

The repository's existing three-file rustfmt baseline remains unchanged;
P1.1h must not add another formatting difference.

## Final Deletion Targets

P1.1h is incomplete until it deletes:

- the three-optionals `AgentConversationContext` state that cannot express
  conversation provenance;
- `record_initial_history_for_agent` from the borrowed hosted path and, if no
  owned persisted caller exists, the helper entirely;
- source-shape tests that require the obsolete helper or its string markers;
- any new read-time duplicate filter or special-case item suppression added
  during development.

Historical duplicate records may remain only as immutable compatibility data.
Their optional future cleanup requires an explicit migration with identity and
pagination semantics; it is not part of normal read or resume.
