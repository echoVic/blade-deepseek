# Goal Runtime System Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the split TUI/JSON/prompt Goal implementation with a runtime-owned, recoverable Goal control plane that correctly counts outer turns, audits terminal claims, and admits continuation only from one owner.

**Architecture:** `orca-core` owns pure Goal domain types and the deterministic tracker. `orca-runtime` owns the Goal actor, SQLite persistence, verifier boundary, composite GoalRun, and event emission. `orca-tools` remains a pure model-facing protocol adapter; `orca-tui` and ACP issue typed commands and render projections. The provider receives bounded internal context fragments in a dedicated system message, never by mutating a tool result.

**Tech Stack:** Rust workspace, `rusqlite` with bundled SQLite, existing RuntimeHost/ThreadActor mailboxes, DeepSeek provider, JSONL semantic event journal, mock and real DeepSeek harnesses.

---

## File Map

- Create `crates/orca-core/src/goal_runtime.rs`: ids, states, reasons, intents,
  acks, ledger records, and pure transition value objects.
- Modify `crates/orca-core/src/goal_types.rs`: compatibility projection from
  the new runtime state to existing TUI-facing `ThreadGoal` values.
- Create `crates/orca-runtime/src/goal_tracker.rs`: pure state machine and
  outer-turn progress policy; no filesystem or provider calls.
- Create `crates/orca-runtime/src/goal_store.rs`: SQLite schema, migration,
  transactional transitions, usage idempotency, and recovery.
- Create `crates/orca-runtime/src/goal_actor.rs`: mailbox, command handlers,
  GoalRuntimeHandle, and GoalVerifier integration.
- Create `crates/orca-runtime/src/goal_verifier.rs`: deterministic preflight,
  verifier port, DeepSeek structured verifier, and bounded verifier usage.
- Modify `crates/orca-runtime/src/runtime_host.rs`: composite GoalRun,
  generation admission, cancellation, recovery, and runtime events.
- Modify `crates/orca-runtime/src/runtime_special.rs`, `tool_router.rs`,
  `tool_turn.rs`, and `controller.rs`: explicit GoalTurnContext and typed
  update acknowledgements.
- Modify `crates/orca-runtime/src/runtime_event_projector.rs` and
  `orca-core/src/event_schema.rs`: Goal semantic events.
- Modify `crates/orca-tools/src/update_goal.rs`: evidence/blocker schema and
  pure result formatting.
- Modify `crates/orca-core/src/conversation.rs`,
  `crates/orca-provider/src/context.rs`, and
  `crates/orca-provider/src/deepseek_http.rs`: role-safe fragments.
- Modify `crates/orca-tui/src/app.rs`, `types.rs`, and `ui.rs`: runtime command
  migration, status projection, and removal of the TUI continuation loop.
- Modify `docs/goal-mode.md`, `docs/harness-contract.md`, and release harness
  files after runtime behavior is complete.

## Task 1: Pure Goal Domain And Tracker

**Files:**

- Modify: `crates/orca-core/src/goal_types.rs`
- Create: `crates/orca-core/src/goal_runtime.rs`
- Create: `crates/orca-runtime/src/goal_tracker.rs`
- Modify: `crates/orca-core/src/lib.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Test: module tests in the three files above

- [x] **Step 1: Add RED state-transition tests**

Add tests for:

```rust
assert!(tracker.begin_outer_turn(GoalTurnOrigin::Continuation).is_ok());
assert!(tracker.inner_turn_completed(128).is_err());
assert_eq!(tracker.outer_turn_count(), 1);

let result = tracker.classify_gap("same-fingerprint", true);
assert!(matches!(result, GoalProgressDecision::Continue));
let result = tracker.classify_gap_after_successful_turn("same-fingerprint", true);
assert!(matches!(result, GoalProgressDecision::Pause(GoalPauseReason::NoProgress)));

let result = tracker.apply_verification(GoalVerificationResult::Blocked { blocker });
assert!(matches!(result.state(), GoalState::Blocked { .. }));
```

Run:

```bash
cargo test -p orca-runtime goal_tracker --lib
```

Expected: the new module/types are missing and the RED tests fail to compile.

- [x] **Step 2: Implement typed domain values**

Add serde-compatible types for `GoalId`, `GoalRunId`, `GoalOuterTurnId`,
`IntentId`, `GoalTurnOrigin`, `GoalState`, `GoalPauseReason`, `EvidenceItem`,
`GoalGap`, `BlockerSummary`, `GoalUsage`, `GoalUpdateIntent`,
`GoalVerificationResult`, and `GoalNextAction`. Keep `ThreadGoalStatus` as a
compatibility enum and map `Stalled` to `Paused { reason: NoProgress }` in the
new projection.

- [x] **Step 3: Implement the pure tracker**

Implement `GoalTracker` with these invariants:

```rust
pub fn begin_outer_turn(&mut self, origin: GoalTurnOrigin) -> Result<GoalOuterTurnId, GoalTrackerError>;
pub fn record_tool_attempt(&mut self, tool_name: &str);
pub fn record_model_response(&mut self);
pub fn finish_outer_turn(&mut self, result: GoalTurnResult) -> Result<GoalNextAction, GoalTrackerError>;
pub fn submit_terminal_intent(&mut self, intent: GoalUpdateIntent) -> GoalUpdateAck;
pub fn apply_verification(&mut self, result: GoalVerificationResult) -> GoalNextAction;
pub fn pause(&mut self, reason: GoalPauseReason, message: String) -> GoalNextAction;
pub fn resume(&mut self, origin: GoalTurnOrigin) -> GoalNextAction;
```

Only a verifier result can create `Complete` or `Blocked`. Same-gap counting
uses closed outer turns and resets on a new fingerprint, progress evidence, or
resume.

- [x] **Step 4: Run GREEN tracker tests and serialization tests**

Run:

```bash
cargo fmt --all
cargo test -p orca-core goal --lib
cargo test -p orca-runtime goal_tracker --lib
```

Expected: all new tracker tests pass and existing `goal_types` compatibility
tests remain green.

- [x] **Step 5: Commit the domain slice**

```bash
git add crates/orca-core/src/goal_types.rs crates/orca-core/src/goal_runtime.rs crates/orca-core/src/lib.rs crates/orca-runtime/src/goal_tracker.rs crates/orca-runtime/src/lib.rs
git commit -m "feat(goal): add typed tracker and outer turn domain"
```

## Task 2: Pure Model Protocol And Ack Formatting

**Files:**

- Modify: `crates/orca-tools/src/update_goal.rs`
- Modify: `crates/orca-tools/src/registry.rs`
- Test: existing update-goal module

- [x] **Step 1: Add RED protocol tests**

Add parser tests for evidence and blocker fields, missing evidence rejection at
the runtime boundary, deferred ack formatting, duplicate intent formatting, and
the existing compatibility inputs `{"status":"complete"}` and
`{"status":"blocked","reason":"..."}`.

Run:

```bash
cargo test -p orca-tools update_goal --lib
```

Expected: the new fields and typed result variants are absent.

- [x] **Step 2: Keep `orca-tools` pure**

Make `UpdateGoalArgs` deserialize `status`, `reason`, bounded `evidence`, and
optional typed `blocker`. Convert it to `GoalUpdateIntent`. Do not add a store,
session lookup, callback, or thread-local value to `orca-tools`.

- [x] **Step 3: Format typed acknowledgements without runtime ownership**

Add a pure formatter that maps every `GoalUpdateAck` variant into an explicit
model-facing `ToolResult`. Deferred intent output must say that terminal audit
will run at outer-turn end. Rejected and inactive variants remain failed tool
results; no formatter opens a store or claims the Goal already transitioned.

- [x] **Step 4: Preserve compatibility normalization**

```rust
pub fn parse_update_intent(request: &ToolRequest) -> Result<GoalUpdateIntent, String>;
pub fn acknowledgement_result(request: &ToolRequest, ack: &GoalUpdateAck) -> ToolResult;
```

Continue accepting `completed: true`, `complete: true`, and `status:
"completed"` as normalization aliases while the advertised schema exposes only
`status: complete|blocked` plus reason/evidence/blocker.

- [x] **Step 5: Run protocol and runtime tests**

```bash
cargo test -p orca-tools update_goal --lib
```

- [x] **Step 6: Commit the protocol slice**

```bash
git add crates/orca-tools/src/update_goal.rs crates/orca-tools/src/registry.rs
git commit -m "feat(goal): make terminal updates typed runtime intents"
```

## Task 3: SQLite Goal Store, Migration, And Recovery

**Files:**

- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/orca-runtime/Cargo.toml`
- Create: `crates/orca-runtime/src/goal_store.rs`
- Modify: `crates/orca-runtime/src/goals.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Test: `crates/orca-runtime/src/goal_store.rs`

- [x] **Step 1: Add RED store tests**

Add tests for schema creation, transactional transition rollback, usage-event
idempotency, in-flight recovery, concurrent writers, JSON migration, malformed
JSON preservation, and projection of old `ThreadGoal` values.

Run:

```bash
cargo test -p orca-runtime goal_store --lib
```

Expected: the SQLite store module and dependency are absent.

- [x] **Step 2: Add SQLite dependency and schema bootstrap**

Add `rusqlite = { version = "0.32", features = ["bundled"] }` to workspace
dependencies and `orca-runtime`. Open `$ORCA_HOME/goals.sqlite3` with WAL,
foreign keys, a busy timeout, and a schema-version table. Implement migrations
for `goals`, `goal_runs`, `goal_turns`, `goal_intents`, `goal_usage_events`, and
`goal_transitions` exactly as defined in the approved spec.

- [x] **Step 3: Implement transactional APIs**

Expose:

```rust
pub fn create_goal(&self, input: CreateGoalInput) -> Result<GoalRecord, GoalStoreError>;
pub fn begin_outer_turn(&self, input: BeginOuterTurn) -> Result<GoalTurnSnapshot, GoalStoreError>;
pub fn record_intent(&self, input: GoalIntentRecord) -> Result<GoalUpdateAck, GoalStoreError>;
pub fn finish_outer_turn(&self, input: FinishOuterTurn) -> Result<GoalNextAction, GoalStoreError>;
pub fn record_usage_once(&self, event: GoalUsageEvent) -> Result<GoalUsage, GoalStoreError>;
pub fn recover_in_flight_runs(&self) -> Result<Vec<RecoveryRecord>, GoalStoreError>;
pub fn project_thread_goal(&self, session_id: &str) -> Result<Option<ThreadGoal>, GoalStoreError>;
```

Every transition and usage insert uses one transaction. `usage_event_id` is a
unique generation/source key; a duplicate returns the original usage without
adding tokens.

- [x] **Step 4: Implement one-time legacy JSON migration**

When the SQLite database has no migration marker and `goals_1.json` exists,
validate every record, insert it, commit, then rename the JSON to a timestamped
backup. On parse, validation, collision, or rename failure leave the source
untouched and return a recovery error.

- [x] **Step 5: Implement recovery semantics**

On open, any `goal_runs.in_flight = 1` becomes `Paused(Recovery)` in one
transaction, its stale continuation is rejected, and a `goal.recovered`
transition is recorded. Recovery never starts a provider call.

- [x] **Step 6: Run store tests and commit**

```bash
cargo fmt --all
cargo test -p orca-runtime goal_store --lib
git diff --check
git add Cargo.toml Cargo.lock crates/orca-runtime/Cargo.toml crates/orca-runtime/src/goal_store.rs crates/orca-runtime/src/goals.rs crates/orca-runtime/src/lib.rs
git commit -m "feat(goal): persist lifecycle in transactional sqlite store"
```

## Task 4: GoalActor And RuntimeThread Ownership

**Files:**

- Create: `crates/orca-runtime/src/goal_actor.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Modify: `crates/orca-runtime/src/thread.rs`
- Modify: `crates/orca-runtime/src/runtime_special.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Test: `crates/orca-runtime/src/goal_actor.rs`, `runtime_host.rs`

- [ ] **Step 1: Add RED mailbox tests**

Test create, read, pause, resume, clear, begin-turn, submit-intent, finish-turn,
duplicate intent, stale run, and closed mailbox behavior. Test that a GoalActor
never invokes the provider and that every command receives one reply.

- [ ] **Step 2: Implement `GoalRuntimeHandle` and actor commands**

Implement a bounded channel with commands:

```rust
pub enum GoalCommand {
    Get { session_id: String, reply: SyncSender<GoalReply> },
    Create(CreateGoalCommand),
    Edit(EditGoalCommand),
    Pause(PauseGoalCommand),
    Resume(ResumeGoalCommand),
    Clear(ClearGoalCommand),
    BeginOuterTurn(BeginOuterTurnCommand),
    SubmitIntent(SubmitIntentCommand),
    FinishOuterTurn(FinishOuterTurnCommand),
    Recover { reply: SyncSender<GoalReply> },
    Shutdown,
}
```

The actor owns `GoalTracker`, `GoalStore`, transition event publication, and
the verifier port. The handle is cloneable and is the only capability passed
into a Goal tool.

- [ ] **Step 3: Attach actor handles to RuntimeThread**

Start one GoalActor per RuntimeThread, expose `goal_runtime()` on
`RuntimeThreadHandle`, and shut it down after the thread actor joins. TUI
commands must use this handle instead of loading `GoalStore` directly.

- [ ] **Step 4: Carry explicit Goal context through dispatch**

Extend the runtime invocation snapshot with a required `Option<GoalTurnContext>`
that is `Some` only for Goal mode. The context carries `goal_id`, `goal_run_id`,
`outer_turn_id`, `session_id`, `origin`, and `GoalRuntimeHandle`; no function
reconstructs these from a thread id or current store row.

- [ ] **Step 5: Route terminal tools through the actor**

Change `RuntimeToolActorContext::execute_goal_tool` so it only parses the pure
operation, validates `GoalTurnContext`, sends `SubmitIntent`, waits for the ack,
and formats the result. Remove direct `GoalStore::update` and
`validate_goal_terminal_update_against_extensions` from production dispatch.

- [ ] **Step 6: Add typed tool disposition**

Return `ContinueModel` for malformed model arguments and typed actor rejections.
Return `StopTurn` for missing visible capability, closed GoalActor, stale
generation identity, or persistence failure. Record exactly one tool terminal
before stopping the outer turn.

- [ ] **Step 7: Run actor tests and commit**

```bash
cargo test -p orca-runtime goal_actor --lib
cargo test -p orca-runtime hosted_goal_tool --lib
cargo test -p orca-runtime --test runtime_host goal_tools -- --nocapture
git add crates/orca-runtime/src/goal_actor.rs crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/src/thread.rs crates/orca-runtime/src/runtime_special.rs crates/orca-runtime/src/tool_router.rs crates/orca-runtime/src/tool_turn.rs crates/orca-runtime/src/controller.rs crates/orca-runtime/src/lib.rs
git commit -m "feat(goal): add runtime goal actor ownership"
```

## Task 5: Composite GoalRun And Continuation Admission

**Files:**

- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Modify: `crates/orca-runtime/src/controller.rs`
- Modify: `crates/orca-runtime/src/background_turn.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Test: `crates/orca-runtime/src/runtime_host.rs`, `crates/orca-tui/src/app.rs`

- [ ] **Step 1: Add RED composite-operation tests**

Add a mock-provider test where one Goal command runs two successful outer turns,
then a third turn containing a deferred complete intent. Assert one operation
handle, monotonic outer-turn ids, and no second TUI submission. Add admission
tests for queued user input, cancellation, duplicate continuation, active
workflow, plan mode, and exhausted budget.

- [ ] **Step 2: Add `HostedOperationKind::GoalRun`**

Represent a GoalRun as one active RuntimeHost operation containing the current
generation, GoalRuntimeHandle, writer, origin, and next outer-turn state. The
operation remains active while the actor admits another outer turn; terminal
completion is published only after GoalNextAction is terminal.

- [ ] **Step 3: Implement one admission gate**

Add a host method returning:

```rust
pub enum GoalContinuationAdmission {
    Admit { outer_turn_id: GoalOuterTurnId, reason: GoalContinuationReason },
    Reject { code: GoalContinuationRejectCode, message: String },
}
```

The gate checks Goal state, pending input, cancellation, pending interaction,
workflow ownership, duplicate in-flight continuation, plan mode, and all
budgets. Emit one semantic event for every decision.

- [ ] **Step 4: Move cancellation/pause into the owner**

`pause`, `cancel`, and shutdown first send the GoalActor transition, then cancel
and join the current generation. Resume creates a fresh GoalRun and outer turn;
it never reuses an in-flight generation fence.

- [ ] **Step 5: Remove TUI continuation ownership**

Replace `run_hosted_goal_turns` with one `HostedOperationKind::GoalRun` request.
Delete `continuation`, `stall_streak`, `tokens_before`, and the TUI loop's direct
GoalStore reads. Keep TUI event handling and notices only as projections of
runtime events.

- [ ] **Step 6: Run composite tests and commit**

```bash
cargo test -p orca-runtime runtime_host goal --lib -- --test-threads=1
cargo test -p orca-tui goal --lib -- --test-threads=1
git diff --check
git add crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/src/controller.rs crates/orca-runtime/src/background_turn.rs crates/orca-tui/src/app.rs
git commit -m "feat(goal): own continuation in composite runtime operation"
```

## Task 6: Verifier, Blocked/No-Progress, Budget And Failure Semantics

**Files:**

- Create: `crates/orca-runtime/src/goal_verifier.rs`
- Modify: `crates/orca-runtime/src/goal_actor.rs`
- Modify: `crates/orca-runtime/src/goal_tracker.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Modify: `crates/orca-runtime/src/runtime_special.rs`
- Test: verifier/tracker/runtime integration modules

- [ ] **Step 1: Add RED verifier tests**

Cover achieved, not achieved with gaps, genuine external blocked, false blocked,
indeterminate, classifier cap, active workflow preflight, missing tool terminal,
and token budget boundary. The false-blocked fixture must contain a roadmap with
viable model-fixable alternatives and assert `Paused(NoProgress)` rather than
`Blocked`.

- [ ] **Step 2: Implement deterministic preflight**

Reject stale ids, active workflows for completion, empty terminal evidence,
missing terminal tool results, in-flight state, exhausted budget, and invalid
blocker kinds before any verifier provider request.

- [ ] **Step 3: Implement the verifier port and DeepSeek adapter**

Add a closed structured JSON schema, no tools, bounded input/evidence size,
bounded output tokens, cancellation propagation, and usage accounting with a
unique `verifier:<outer_turn_id>:<attempt>` usage-event id. Provider errors map
to `Indeterminate`, then `Paused(Infrastructure)`.

- [ ] **Step 4: Implement progress and budget decisions**

Use outer-turn gap fingerprints and evidence as the primary signal. Three
successful identical model-fixable gaps become `Paused(NoProgress)`. Charge
input plus output once; cache tokens are diagnostic only. Budget exhaustion
becomes `BudgetLimited` and rejects continuation.

- [ ] **Step 5: Test cancellation and failure paths**

Verify provider error, tool control-plane error, approval wait, user input wait,
cancellation, and verifier cancellation all close the outer ledger and cannot
auto-continue an active Goal.

- [ ] **Step 6: Run tests and commit**

```bash
cargo test -p orca-runtime goal_verifier --lib
cargo test -p orca-runtime goal_tracker --lib
cargo test -p orca-runtime goal --lib -- --test-threads=1
git add crates/orca-runtime/src/goal_verifier.rs crates/orca-runtime/src/goal_actor.rs crates/orca-runtime/src/goal_tracker.rs crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/src/runtime_special.rs
git commit -m "feat(goal): verify terminal claims and classify progress"
```

## Task 7: Role-Safe Context, Events, TUI And ACP Projections

**Files:**

- Modify: `crates/orca-core/src/conversation.rs`
- Modify: `crates/orca-provider/src/context.rs`
- Modify: `crates/orca-provider/src/deepseek_http.rs`
- Modify: `crates/orca-core/src/event_schema.rs`
- Modify: `crates/orca-runtime/src/runtime_event_projector.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Test: conversation/provider/event/TUI modules

- [ ] **Step 1: Add RED role-safety tests**

Build a conversation ending with an assistant tool call and tool result, install
a Goal fragment, and assert the output contains a separate system message, the
tool result bytes are unchanged, and repeated updates do not add transcript
messages. Add event payload tests for all Goal semantic events.

- [ ] **Step 2: Implement `InternalContextFragment`**

Store fragments outside `Conversation.messages`; bound each fragment by token
count and replace by id. Render Goal state after canonical system instructions
and before user/tool history. Keep plan/runtime/skill fragments on the same
structured path.

- [ ] **Step 3: Add semantic Goal events**

Add event factory methods and JSON payloads for created, run/turn started,
intent requested/acknowledged, turn finished, verification, transition,
continuation admission, pause, recovery, and complete. Preserve sequence and
observer failure semantics.

- [ ] **Step 4: Migrate TUI commands**

Route `/goal`, `/goal edit`, `/goal pause`, `/goal resume`, and `/goal clear`
through `RuntimeThreadHandle` commands. The TUI may display a compatibility
`ThreadGoal` projection but must not open or mutate the GoalStore directly.
Render pause reason, current outer turn, last gap/blocker, and charged usage.

- [ ] **Step 5: Migrate ACP projection**

Map Goal semantic events to existing ACP item/event types without creating a
second Goal state machine. Add read/control methods only through the same typed
runtime handle.

- [ ] **Step 6: Run tests and commit**

```bash
cargo test -p orca-core conversation --lib
cargo test -p orca-provider volatile --lib
cargo test -p orca-runtime event --lib
cargo test -p orca-tui goal --lib -- --test-threads=1
git diff --check
git add crates/orca-core/src/conversation.rs crates/orca-provider/src/context.rs crates/orca-provider/src/deepseek_http.rs crates/orca-core/src/event_schema.rs crates/orca-runtime/src/runtime_event_projector.rs crates/orca-tui/src/app.rs crates/orca-tui/src/types.rs crates/orca-tui/src/ui.rs
git commit -m "feat(goal): project role-safe runtime state to frontends"
```

## Task 8: Documentation, Real Harness, Full Verification And Deletion

**Files:**

- Modify: `docs/goal-mode.md`
- Modify: `docs/harness-contract.md`
- Modify: `scripts/release/real-api-e2e.mjs`
- Modify: `scripts/release/test-real-api-e2e.mjs`
- Modify: `crates/orca-runtime/examples/goal_mode_realapi.rs`
- Delete production paths: old TUI loop, direct JSON mutation, stale progress
  extension, and last-message Goal overlay logic

- [ ] **Step 1: Add RED real-harness assertions**

Extend the isolated real-API case to record outer turns, continuation admission,
update request/ack counts, verifier result, usage, final state, and zero stale
continuations. Add cases for completion, rejected completion, genuine blocked,
cancellation, and resume.

- [ ] **Step 2: Update user-facing contracts**

Document SQLite location/migration, typed statuses/reasons, turn-end audit,
pause/recovery behavior, evidence schema, and the fact that `orca exec` does
not expose Goal tools. Mark old JSON/TUI-loop documents as historical when the
new path is live.

- [ ] **Step 3: Delete obsolete production paths**

Run searches and remove every production reference to:

```bash
rg -n "run_hosted_goal_turns|GoalToolProgressState|stall_if_active|GOAL_HANDLER|with_goal_handler|volatile.*last|goals_1\.json" crates
```

Only migration tests and historical documents may mention legacy names.

- [ ] **Step 4: Run complete gates**

```bash
cargo fmt --all -- --check
cargo test -p orca-core -p orca-tools -p orca-runtime -p orca-tui --lib -- --test-threads=1
cargo test --workspace --all-targets -- --test-threads=1
cargo clippy --workspace --all-targets
node scripts/release/test-real-api-e2e.mjs
git diff --check
```

The known baseline workflow failure must be fixed as a separate commit or
explicitly resolved before this Goal branch can claim a clean full gate.

- [ ] **Step 5: Run real DeepSeek validation**

With a fresh isolated `ORCA_HOME` and a real API key, run the bounded Goal
harness. Inspect the JSONL event journal and SQLite rows, not only the final
text. Verify one outer-turn row per admission, one usage event per generation,
typed transition reasons, role-safe request messages, and no continuation after
completion/blocked/pause/recovery.

- [ ] **Step 6: Final branch verification and commit**

```bash
git status --short --branch
git log --oneline --decorate -12
git diff --check origin/main...HEAD
```

If all gates pass, use `superpowers:finishing-a-development-branch` to present
merge/rebase options. Do not mark the Goal complete until the branch, tests,
real harness, deletion search, and public docs all match the approved spec.
