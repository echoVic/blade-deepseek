# Goal Runtime System Redesign

- Date: 2026-07-18
- Status: approved in chat, design review pending
- Scope: persistent Goal lifecycle, execution ownership, continuation scheduling,
  terminal verification, recovery, provider context injection, and observability
- Supersedes: the partial continuation-stop design and the v0.2.46 control-plane
  implementation plan

## Decision Summary

Goal mode will be rebuilt as a runtime-owned composite operation. The TUI will
submit commands and render state; it will not own a continuation loop, decide
whether a model claim is terminal, or write the persistent Goal record directly.

The runtime will own one Goal control plane per live thread. A Goal run has an
explicit outer-turn ledger. An outer turn contains the complete model/tool loop
for one continuation admission; inner provider/tool iterations are never used
as Goal progress or blocked evidence.

`update_goal` becomes an intent protocol. The tool sends a typed intent to the
Goal control plane and waits for an acknowledgement. `complete` and `blocked`
are pending claims until a turn-end audit accepts them. Automatic continuation
is admitted only by the same runtime owner that records the turn result.

Persistent state moves from an unversioned JSON map to a SQLite database with
transactional transitions, idempotent usage records, and recovery snapshots.
Existing `goals_1.json` records are migrated once and never silently discarded.

Volatile Goal state becomes a role-safe internal context fragment. The provider
adapter injects it as a dedicated system-context message before user/tool
conversation content; it is never appended to a tool result or persisted as a
conversation message.

## Why A Full Rewrite Is Required

The current v0.2.48 implementation already fixed the original thread-local
Goal handler incident, but it still has a split lifecycle:

| Boundary | Current behavior | Failure exposed by the latest task |
| --- | --- | --- |
| Goal owner | `GoalStore` plus TUI state | No single owner for transition, continuation, and recovery |
| Turn accounting | Runtime counts model/tool turns; TUI counts continuations | Inner loops are mistaken for outer Goal turns |
| Terminal update | Runtime validates only one non-Goal tool attempt, then writes the store | A model claim becomes persisted state without a turn-end audit |
| Continuation | `run_hosted_goal_turns` loops in the TUI | Runtime cannot reject duplicate, stale, or already-invalid continuations |
| Persistence | Atomic JSON replacement guarded by a process-local mutex | No cross-process transaction, in-flight marker, or idempotent usage event |
| Context | Provider appends volatile text to the last wire message | Goal state can be appended inside a `tool` result |
| Recovery | Resume reconstructs a session and starts the loop | A crash can leave an active-looking goal with no trustworthy run owner |

The latest task proves the remaining defect is not a missing threshold. The
recorded run ended with `blocked` after 53 successful outer sessions, 2,049
model responses, 2,227 tool calls, 33 compactions, about 5 hours 24 minutes of
wall time, 360,293,802 accounted tokens, and an estimated cost of
`$21.073655646`. The final reasoning listed viable next strategies and a
roadmap, so the runtime had no evidence of an external impasse. The model had
counted inner work as blocked turns because the host did not expose a
structured outer-turn ledger.

This is a lifecycle and ownership problem. Continuing to add prompt wording or
TUI counters would preserve the same ambiguity.

## Reference Findings

### Codex

The local Codex implementation under `/Users/qingyun/Documents/GitHub/codex`
keeps Goal accounting and lifecycle in a dedicated extension. Typed hooks cover
thread start/resume/idle/stop, turn start/stop/abort/error, tool completion, and
usage. Automatic continuation passes an admission gate that checks user input,
active tasks, plan mode, and thread idleness. Goal steering is an internal
context fragment rather than a mutation of the last tool result. SQLite stores
state and usage for resume, fork, and deferred continuation.

Codex still leaves some blocked validation to the prompt contract. Orca will
reuse its lifecycle/accounting boundaries but make terminal audit and blocker
classification runtime invariants.

### Claude Code

The local Claude Code package under `/Users/qingyun/Documents/GitHub/claude-code`
uses an explicit mutable query state, transition reasons, turn counters,
abort checks, and a bounded continuation budget. API errors do not enter Stop
hooks, which avoids error/hook retry loops. Its task ledger keeps owner,
dependencies, and `in_progress` state outside model prose.

Orca will apply the same separation: a Goal turn result is not a terminal Goal
state, and a blocker remains structured and owned by the host.

### Grok Build

The local Grok Build implementation under
`/Users/qingyun/Documents/GitHub/grok-build` is the closest reference. Its pure
`GoalTracker` distinguishes active, user-paused, backoff-paused,
no-progress-paused, infrastructure-paused, blocked, budget-limited, and
complete states. `update_goal` sends a command to the session actor and waits
for an acknowledgement. Terminal claims are deferred to turn end, where a
classifier can return achieved, not achieved, stalled, or blocked. Snapshots
persist gap fingerprints, classifier counters, token high-water marks,
in-flight phase, and bounded history; recovery never resumes active self-drive
automatically.

Orca will adopt this state and acknowledgement model while keeping the
existing DeepSeek provider and TUI surface.

## Goals And Non-Goals

### Goals

1. Count and reason about exactly one Goal outer turn per admitted continuation.
2. Make every model-visible Goal tool executable through an explicit runtime
   capability owned by the live thread.
3. Prevent a model claim from becoming `complete` or `blocked` without a
   turn-end audit.
4. Make cancellation, pause, crash recovery, and resume idempotent.
5. Prevent duplicate or stale continuation scheduling.
6. Preserve tool-result roles and transcript history when injecting Goal state.
7. Persist enough evidence to explain every Goal transition after restart.
8. Keep the existing `/goal` commands and normal non-Goal tool semantics.

### Non-goals

- Goal mode will remain a TUI feature in the public `orca exec` contract. The
  runtime API will be reusable by ACP/server adapters, but this design does not
  add a new headless Goal command.
- The design does not make the runtime understand arbitrary project semantics.
  Semantic completion is delegated to a bounded verifier with explicit
  evidence; deterministic runtime invariants still gate the result.
- Existing workflow, shell, and subagent implementations are not rewritten
  except where their lifecycle signals are required by continuation admission.

## Target Architecture

```text
TUI / ACP adapter
        |
        | typed GoalCommand / GoalView
        v
RuntimeThreadHandle
        |
        +--> ThreadActor --------------------------+
        |       owns one composite GoalRun         |
        |       owns generation cancellation       |
        |                                            |
        +--> GoalRuntimeHandle --> GoalActor -------+
                owns GoalTracker                   |
                owns terminal audit                |
                owns continuation admission       |
                owns SQLite transactions          |
                                                    |
                                                    v
                                             GoalStore SQLite

Provider turn
    -> GoalTurnContext { goal_id, outer_turn_id, origin }
    -> tool intent -> GoalActor ack
    -> turn result -> GoalActor finish_outer_turn
    -> GoalNextAction -> ThreadActor schedules or closes GoalRun

Conversation -> InternalContextFragment[] -> provider adapter
```

The ThreadActor and GoalActor have separate responsibilities:

- ThreadActor owns execution resources, generation cancellation, writer
  ownership, workflow handles, and the composite operation visible to callers.
- GoalActor owns the Goal state machine, evidence ledger, SQLite transaction,
  verifier result, and admission decision. It never starts a provider call.
- A GoalRun is the only bridge. It carries a unique `goal_run_id` and
  `outer_turn_id` into every generation and returns one `GoalNextAction` to the
  ThreadActor.

No component discovers Goal ownership from an OS thread, thread-local value,
global mutable callback, or the last conversation message.

## Domain Model

The public `ThreadGoal` becomes a projection of a richer runtime record. The
following names are normative; implementation may choose module placement but
must preserve the semantics.

```rust
pub struct GoalRecord {
    pub goal_id: GoalId,
    pub session_id: String,
    pub objective: String,
    pub objective_revision: u32,
    pub state: GoalState,
    pub token_budget: Option<i64>,
    pub usage: GoalUsage,
    pub current_run: Option<GoalRunSnapshot>,
    pub last_transition: GoalTransitionSummary,
}

pub enum GoalState {
    Active,
    Paused { reason: GoalPauseReason, message: String },
    Blocked { blocker: BlockerSummary },
    BudgetLimited,
    Complete { evidence: Vec<EvidenceItem> },
}

pub enum GoalPauseReason {
    User,
    NoProgress,
    Backoff,
    Infrastructure,
    WaitingForWorkflow,
    Recovery,
    UsageLimit,
}

pub struct GoalRunSnapshot {
    pub goal_run_id: GoalRunId,
    pub outer_turn_id: GoalOuterTurnId,
    pub origin: GoalTurnOrigin,
    pub continuation_count: u32,
    pub in_flight: bool,
}
```

`Complete` is the only irreversible state. `Blocked`, `BudgetLimited`, and
all `Paused` states are resumable. `/goal edit` starts a new objective revision
and reactivates the record; `/goal resume` starts a new run with a fresh blocked
and no-progress audit window.

### Outer-turn ledger

An outer turn is opened once, immediately before a GoalRun generation is
admitted. It is closed exactly once after the provider/tool loop returns. The
ledger records:

- `goal_run_id`, `goal_id`, `outer_turn_id`, and origin (`user`, `resume`,
  `continuation`, `workflow_notification`)
- provider `turn_id` and generation fence
- start/end timestamps and charged usage deltas
- model response count, tool request/completion counts, and tool names
- terminal intent ids and their acknowledgement outcomes
- progress evidence and optional gap fingerprint
- final generation status (`success`, `failed`, `cancelled`,
  `approval_required`, `budget_exhausted`)
- the resulting `GoalNextAction`

The `RuntimeTaskActor` inner turn number is retained for runtime diagnostics but
is not valid Goal evidence, a blocked count, a no-progress count, or a
continuation count.

### Blocker and gap semantics

`blocked` is reserved for a verifier result that says no model-fixable path is
available without a user or external dependency. A blocker must have:

- a typed kind (`user_decision`, `missing_authority`, `external_state`,
  `environment_contradiction`, or `unverifiable_requirement`)
- a normalized fingerprint
- a human-readable explanation
- at least one evidence item or an explicit verifier explanation

If a classifier says the current strategy is exhausted but another model-fixable
strategy exists, the result is `NotAchieved { gaps }`, not `Blocked`. Repeated
identical model-fixable gaps increment `same_gap_streak`; after the configured
outer-turn threshold they produce `Paused(NoProgress)`. This directly prevents
the latest task's “there is no clean extraction target” reasoning from becoming
a false user blocker.

## Goal Tool Protocol

`orca-tools` remains pure. It owns JSON schema, argument normalization, and
rendering. It does not open SQLite, read a session, or call a callback.

The model-facing schema is:

```json
{
  "status": "complete | blocked",
  "reason": "short explanation",
  "evidence": [
    {"kind": "test | file | command | observation | external", "summary": "...", "target": "..."}
  ],
  "blocker": {
    "kind": "user_decision | missing_authority | external_state | environment_contradiction | unverifiable_requirement",
    "summary": "..."
  }
}
```

`reason`, `evidence`, and `blocker` are optional for compatibility at the
parser boundary, but a terminal intent without the required evidence is
rejected by the GoalActor. The old `status` values remain accepted only for
normalization of historical prompts; the runtime still exposes only
`complete|blocked` to the model.

The runtime context passed to the special-tool dispatcher is explicit:

```rust
pub struct GoalTurnContext {
    pub goal_id: GoalId,
    pub goal_run_id: GoalRunId,
    pub outer_turn_id: GoalOuterTurnId,
    pub session_id: String,
    pub origin: GoalTurnOrigin,
    pub goal_actor: GoalRuntimeHandle,
}
```

The dispatcher sends `GoalCommand::SubmitIntent` and waits for `GoalUpdateAck`.
The acknowledgement is never a plain “success” string:

```rust
pub enum GoalUpdateAck {
    DeferredToTurnEnd { intent_id: IntentId, pending_depth: u32 },
    Rejected { code: GoalRejectCode, message: String },
    AlreadyPending { intent_id: IntentId },
    BlockedAgainstInactive { state: GoalState },
}
```

The tool result tells the model exactly what happened. For a deferred terminal
claim it says that the host will audit the claim after the current outer turn;
it never says that the Goal is already complete or blocked.

Malformed JSON, unsupported fields, and invalid model claims remain recoverable
tool results. Missing runtime context, a closed GoalActor, database failure, or
an impossible generation fence are control-plane failures: the current outer
turn terminates with a typed infrastructure error and cannot auto-continue.

## Terminal Audit And Progress Control

### Turn-end ordering

Every GoalRun generation follows this order:

1. `GoalActor::begin_outer_turn` persists the in-flight marker.
2. ThreadActor starts the provider/tool loop with `GoalTurnContext`.
3. Tool calls submit intents and receive acks; no persistent terminal state is
   applied mid-turn.
4. ThreadActor records the complete generation outcome and usage high-water
   mark.
5. `GoalActor::finish_outer_turn` closes the ledger row in one transaction.
6. If a terminal intent exists, the verifier runs with a bounded budget.
7. The tracker applies exactly one transition and returns `GoalNextAction`.
8. ThreadActor either schedules the next outer turn or publishes the composite
   GoalRun terminal event.

No observer can see a completed outer turn before steps 5-7 commit.

### Verifier contract

The verifier is a trait with a deterministic test implementation and a
DeepSeek implementation:

```rust
pub trait GoalVerifier: Send + Sync {
    fn verify(&self, input: GoalVerificationInput) -> GoalVerificationResult;
}

pub enum GoalVerificationResult {
    Achieved { evidence: Vec<EvidenceItem> },
    NotAchieved { gaps: Vec<GoalGap> },
    Blocked { blocker: BlockerSummary },
    Indeterminate { message: String },
}
```

The deterministic preflight runs first and rejects impossible states: active
workflows, missing required terminal tool results, empty evidence for a
terminal claim, stale goal/run ids, budget exhaustion, and an in-flight
generation. Only after preflight passes may the bounded DeepSeek verifier run.

The DeepSeek verifier receives the objective, recent ledger summary, declared
evidence, task/plan summaries, and the last model response. It has no tools,
cannot mutate the Goal, and must return a closed JSON schema. Its token and
cost usage is charged to the Goal with a unique usage-event id.

Verifier calls are capped per outer turn and per GoalRun. A cap or an
indeterminate result produces `Paused(NoProgress)` or
`Paused(Infrastructure)` with a visible reason; it never leaves the Goal
eligible for unbounded continuation.

### Progress rules

- Progress is measured between closed outer turns, not inner tool calls.
- The primary signal is structured evidence and verifier gaps.
- Token deltas remain a secondary accounting signal and are never sufficient
  to declare completion or blocked.
- The same normalized gap fingerprint repeated across three successful outer
  turns produces `Paused(NoProgress)` unless the verifier has classified it as
  a non-model-fixable blocker.
- A failed provider/control-plane turn produces `Paused(Infrastructure)` or
  bounded `Paused(Backoff)` according to the retry policy. It cannot be counted
  as a successful progress turn.

## Continuation Admission And Composite GoalRun

`run_hosted_goal_turns` is deleted. The TUI submits one `HostedOperationKind::GoalRun`
operation. RuntimeHost keeps the operation active while it schedules outer
turns and emits normal per-turn events.

Admission requires all of the following:

- Goal state is `Active`.
- No user input is queued for the thread.
- No cancellation or shutdown is requested.
- No provider suspension, approval wait, or pending interaction owns the next
  action.
- No active workflow is waiting for a notification unless the next origin is
  `workflow_notification`.
- The current thread is idle at the generation boundary.
- The operation has no already-admitted continuation.
- The Goal token/cost budget and verifier budget remain available.
- The request is not in plan mode.

Every rejection is a typed `goal.continuation.rejected` event with the exact
reason. A rejected continuation transitions to a resumable paused state when
appropriate; it is never silently retried by the TUI.

User controls are commands to the same owner:

- `pause`: transactionally persist `Paused(User)`, then cancel and join the
  current generation. Repeated pause is idempotent.
- `resume`: clear the previous run's in-flight marker, create a fresh run and
  outer-turn ledger, and explicitly admit one user-origin turn.
- `cancel`: persist `Paused(User)` before cancellation; do not leave `Active`
  merely because the provider returned `cancelled`.
- `clear`: close the GoalRun, remove the goal record, and remove the context
  fragment in one ordered command.

If a process exits while an outer turn is in flight, the next database open
converts that run to `Paused(Recovery)` and records `goal.recovered`. It never
starts provider work automatically after a crash.

## Persistence And Migration

SQLite is the authoritative store. The database uses WAL mode, foreign keys,
busy timeout, and a schema version table. The minimum tables are:

```sql
goals(
  goal_id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  objective TEXT NOT NULL,
  objective_revision INTEGER NOT NULL,
  state TEXT NOT NULL,
  state_reason TEXT,
  state_message TEXT,
  token_budget INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

goal_runs(
  goal_run_id TEXT PRIMARY KEY,
  goal_id TEXT NOT NULL REFERENCES goals(goal_id),
  status TEXT NOT NULL,
  origin TEXT NOT NULL,
  current_outer_turn_id TEXT,
  continuation_count INTEGER NOT NULL,
  in_flight INTEGER NOT NULL,
  started_at INTEGER NOT NULL,
  finished_at INTEGER
);

goal_turns(
  outer_turn_id TEXT PRIMARY KEY,
  goal_run_id TEXT NOT NULL REFERENCES goal_runs(goal_run_id),
  origin TEXT NOT NULL,
  provider_turn_id TEXT NOT NULL,
  status TEXT NOT NULL,
  tool_count INTEGER NOT NULL,
  model_response_count INTEGER NOT NULL,
  charged_input_tokens INTEGER NOT NULL,
  output_tokens INTEGER NOT NULL,
  verifier_tokens INTEGER NOT NULL,
  gap_fingerprint TEXT,
  started_at INTEGER NOT NULL,
  finished_at INTEGER
);

goal_intents(
  intent_id TEXT PRIMARY KEY,
  outer_turn_id TEXT NOT NULL REFERENCES goal_turns(outer_turn_id),
  requested_state TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  ack_code TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

goal_usage_events(
  usage_event_id TEXT PRIMARY KEY,
  goal_id TEXT NOT NULL REFERENCES goals(goal_id),
  source TEXT NOT NULL,
  charged_input_tokens INTEGER NOT NULL,
  output_tokens INTEGER NOT NULL,
  cache_tokens INTEGER NOT NULL,
  cost_micros INTEGER NOT NULL,
  elapsed_seconds INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);

goal_transitions(
  transition_id TEXT PRIMARY KEY,
  goal_id TEXT NOT NULL REFERENCES goals(goal_id),
  outer_turn_id TEXT,
  previous_state TEXT NOT NULL,
  next_state TEXT NOT NULL,
  reason_code TEXT NOT NULL,
  evidence_json TEXT,
  created_at INTEGER NOT NULL
);
```

All state changes, transition rows, and usage events are committed in one
transaction. `usage_event_id` is derived from the generation fence and source,
so a retry cannot charge the same provider response twice. Cache tokens are
stored for diagnostics but are not added a second time to charged input tokens.

On first open, the store migrates `$ORCA_HOME/goals_1.json` (or
`~/.orca/goals_1.json`) into SQLite. It validates every objective and status,
writes a migration marker, and renames the source to a timestamped backup only
after the transaction commits. Malformed JSON, duplicate session identities,
or a failed rename leave the source untouched and surface a recovery error.

## Role-Safe Internal Context

Replace the string-only `VolatileContext.goal` path with:

```rust
pub struct InternalContextFragment {
    pub id: String,
    pub kind: InternalContextKind,
    pub origin: InternalContextOrigin,
    pub content: String,
    pub max_tokens: usize,
}
```

The Conversation stores fragments separately from transcript messages. The
DeepSeek adapter renders them as a bounded synthetic `system` message after
the canonical system instructions and before the first user/tool conversation
message. The adapter must preserve assistant-tool-call/tool-result adjacency.

The Goal fragment contains the objective, current state/reason, current outer
turn, last verifier gap or blocker, budget summary, and the next admissible
action. It is replaced atomically before each provider request and removed when
the Goal is not active or the GoalRun ends.

Tests must assert that a conversation ending in a tool result produces a
separate system message, that the tool content is byte-for-byte unchanged, and
that repeated replacements do not grow the transcript.

## Observability

Every Goal transition is emitted through the existing semantic event journal.
The following event types and required payload fields are normative:

| Event | Required fields |
| --- | --- |
| `goal.created` | goal id, session id, objective revision |
| `goal.run.started` | goal id, run id, outer turn id, origin |
| `goal.turn.started` | goal id, run id, outer turn id, provider turn id |
| `goal.update.requested` | intent id, requested state, outer turn id |
| `goal.update.acknowledged` | intent id, ack code, reason |
| `goal.turn.finished` | status, tool counts, charged usage, progress summary |
| `goal.verification.completed` | outcome, gaps/blocker/evidence, verifier usage |
| `goal.transitioned` | previous/next state, reason code, transition id |
| `goal.continuation.admitted` | next outer turn id, reason, counters |
| `goal.continuation.rejected` | rejection code, current state, counters |
| `goal.paused` | pause reason, message, current run |
| `goal.recovered` | stale run id, recovered state, discarded continuation |
| `goal.completed` | evidence summary, total usage, elapsed time |

Each event carries `goal_id`, `goal_run_id` when present, `outer_turn_id` when
present, source (`user`, `model`, `system`, `continuation`), and the event
sequence from `EventFactory`. TUI and ACP projections consume these events;
they do not infer state from prose or from the JSON database directly.

## Migration Plan

Each slice must be independently testable and committed. The implementation
plan will be written after this spec is approved.

### Slice 1: Pure tracker and protocol

- Add Goal state, reason, blocker, gap, intent, acknowledgement, and
  outer-turn types in `orca-core`.
- Implement a pure `GoalTracker` with exhaustive transition tests.
- Extend `update_goal` schema and formatting without adding storage access.
- Add the latest incident as a deterministic false-blocked regression fixture.

Acceptance: tracker rejects inner-turn counts as outer-turn evidence; the same
model-fixable gap becomes `NoProgress`, not `Blocked`; terminal claims remain
pending.

### Slice 2: SQLite store and recovery

- Add the SQLite dependency and schema migrations.
- Implement transactional GoalStore, idempotent usage events, transition rows,
  in-flight recovery, and `goals_1.json` migration.
- Keep a read-only projection API for existing TUI rendering during migration.

Acceptance: kill/reopen tests recover an in-flight run to `Paused(Recovery)`,
concurrent writers serialize, and repeated usage delivery charges once.

### Slice 3: GoalActor and explicit tool acknowledgement

- Add `GoalActor`/`GoalRuntimeHandle` mailbox and command protocol.
- Carry `GoalTurnContext` through runtime tool dispatch.
- Remove remaining direct `GoalStore::update` calls and any ambient ownership.

Acceptance: a visible `update_goal` tool always receives an ack from the owned
GoalActor; missing context fails before provider sampling; duplicate intent ids
are idempotent.

### Slice 4: Runtime-owned composite GoalRun

- Extend RuntimeHost with one long-lived GoalRun operation.
- Move outer-turn scheduling, admission, generation cancellation, workflow
  waiting, and continuation limits into ThreadActor/GoalActor.
- Delete `run_hosted_goal_turns` and TUI-side continuation counters.

Acceptance: one `/goal` action produces one composite operation; no TUI loop can
schedule a second continuation; pending user input, cancellation, and active
workflow each produce an observable admission result.

### Slice 5: Turn-end verifier and progress policy

- Add deterministic preflight and bounded DeepSeek verifier.
- Apply complete/blocked only after verifier output.
- Add gap fingerprint, no-progress, backoff, budget, and classifier caps.

Acceptance: a fake verifier can produce each outcome; the latest task's roadmap
is `NotAchieved`/`NoProgress` when appropriate; a genuine external dependency
becomes `Blocked` with evidence; verifier failure never auto-continues.

### Slice 6: Role-safe fragments and frontend migration

- Replace last-message overlay logic with internal fragments.
- Route `/goal` show/edit/pause/resume/clear through RuntimeThreadHandle
  commands.
- Project Goal events into TUI and ACP status views.

Acceptance: tool results remain unmodified, inactive Goal fragments disappear,
and all frontend controls are idempotent while a generation is running.

### Slice 7: Real validation and deletion

- Add mock, crash/recovery, cancellation, concurrency, and provider wire tests.
- Extend the real DeepSeek harness with completion, rejected completion,
  genuine blocked, cancellation, and resumed Goal cases.
- Remove compatibility shims and obsolete docs once all consumers migrate.

Acceptance: focused tests, serial workspace tests, Clippy, release harness,
and real DeepSeek cases pass; the old TUI loop, JSON writes, stale counters,
and role-unsafe overlay have no production references.

## Compatibility And User Experience

The slash commands remain unchanged:

```text
/goal
/goal <objective>
/goal edit <objective>
/goal pause
/goal resume
/goal clear
```

The UI gains more precise status details: pause reason, current outer turn,
last verifier gap/blocker, and charged usage. Existing `ThreadGoal` consumers
receive a compatibility projection while migration is in progress. The
authoritative state is never inferred from that projection.

`orca exec` remains unchanged and continues to reject Goal tools outside a
Goal-capable TUI/runtime context. ACP can observe and control a Goal only after
its adapter uses the same typed RuntimeThreadHandle commands; it must not
reimplement the state machine.

## Testing And Verification Gates

### Unit and property tests

- exhaustive state-transition table, including duplicate and stale commands
- outer-turn ledger invariants: one open/one close, monotonic ids, no inner-turn
  contribution
- intent acknowledgement and idempotency
- blocker/gap normalization and fingerprint stability
- usage high-water mark and budget boundary accounting
- SQLite migration, transaction rollback, concurrent writers, and recovery
- provider fragment placement and unchanged tool result bytes

### Runtime integration tests

- composite GoalRun with two successful continuations and one completion
- completion rejected by verifier and resumed with gap feedback
- genuine external blocker accepted as Blocked
- false blocked claim routed to NoProgress
- cancellation during provider call, tool execution, approval, and verifier
- pause/resume during an active generation
- active workflow notification and duplicate continuation rejection
- crash after begin-turn, after tool intent, and before transition commit

### Required commands

```bash
cargo fmt --all -- --check
cargo test -p orca-core -p orca-tools -p orca-runtime -p orca-tui --lib -- --test-threads=1
cargo test --workspace --all-targets -- --test-threads=1
cargo clippy --workspace --all-targets
git diff --check
```

The existing baseline currently has one unrelated failure in
`tests/workflow_runtime_contract.rs::workflow_runner_cancels_unawaited_agent_after_terminal_event`.
It must be fixed or explicitly isolated before the Goal refactor is released;
the Goal slices cannot claim a clean workspace gate while that failure remains.

The real DeepSeek gate must run with an isolated `ORCA_HOME` and record:

- number of Goal outer turns and continuations
- `update_goal` request/ack counts
- verifier outcomes
- final state and transition reason
- charged usage and estimated cost
- absence of duplicate or stale continuations

## Deletion Targets

The redesign is incomplete while any of these production paths remain:

- `run_hosted_goal_turns` or another TUI-owned continuation loop
- direct `GoalStore` mutation from `orca-tui`
- `GoalToolProgressState` as the source of Goal progress or blocked evidence
- terminal-state persistence directly inside the model tool dispatcher
- prompt-only three-turn blocked rules
- token delta as the sole stall/terminal signal
- `thread_local!` or global callbacks for Goal ownership
- volatile Goal text appended to the last wire message
- automatic recovery of an in-flight Goal to active self-drive
- an untyped success response for a deferred or rejected terminal intent

## Final Acceptance Criteria

The work is complete only when all of the following are true:

1. One runtime owner controls Goal state, outer-turn ledger, verifier, usage,
   persistence, and continuation admission.
2. The model cannot directly persist `complete` or `blocked`; it can only submit
   an intent and receive a typed ack.
3. Inner provider/tool loops cannot affect blocked thresholds or continuation
   counts.
4. A model-fixable repeated gap becomes a bounded no-progress pause, while a
   verified external dependency becomes blocked with evidence.
5. Cancellation, pause, crash, restart, and resume leave no active orphan run
   or duplicate charge.
6. Goal context is role-safe and never changes tool-result content or transcript
   history.
7. TUI and future ACP adapters use the same runtime command/event contract.
8. Existing Goal commands still work, legacy JSON data migrates safely, and
   `orca exec` remains outside the Goal contract.
9. Focused tests and real DeepSeek verification demonstrate the behavior, and
   no obsolete Goal path remains in production.
