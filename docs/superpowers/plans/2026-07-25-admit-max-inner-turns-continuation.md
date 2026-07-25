# Admit MaxInnerTurns Continuation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let goal mode auto-continue after an outer turn that ends solely because the per-run inner-turn ceiling was hit (`TurnEndReason::MaxInnerTurns`), while still pausing on cost budget exhaustion and keeping the no-progress watchdog + a consecutive budget-continuation cap as dual safety nets.

**Architecture:** Plumb the already-classified `TurnEndReason` from `RuntimeTurnStartError` through the agent-loop / thread / host outcomes so both the goal tracker and the continuation preflight can see it. The tracker is the first gate (keep goal `Active` only for resumable budget ends); the preflight consumes a typed `GoalTurnDisposition::{Advanced, Interrupted, Blocked}` and rejects only `Blocked`.

**Tech Stack:** Rust workspace, Cargo tests, Clippy.

---

## Why this is safe now

Prior plans landed the prerequisites:

1. `TurnEndReason::{MaxInnerTurns, CostBudgetExhausted, ...}` distinguishes resumable vs non-resumable `BudgetExhausted` (`2026-07-25-turn-end-reason-classification.md`).
2. The progress watchdog separates observable activity from substantive progress and rebuilds the same-gap streak, including `NULL` progress barriers, across restarts (`2026-07-25-goal-progress-watchdog.md`).

Without (1), admitting any `BudgetExhausted` would re-trip cost walls forever. Without (2), admitting `MaxInnerTurns` would have no durable no-progress stop for spins that produce no structured evidence.

## Explicitly out of scope

- Budget pre-warning / soft landing (implemented by the follow-up soft-landing plan).
- Structured continuation envelope (implemented by the follow-up envelope plan).
- `ProviderTokenLimit` / `VerificationRejected` end reasons (still not flowing through `RuntimeTurnStartError`).
- Auto-continuing `VerificationFailed`, `Failed`, or `Cancelled`.

## File structure

| File | Role |
|---|---|
| `crates/orca-runtime/src/lifecycle.rs` | `AgentLoopResult` carries `reason: TurnEndReason` |
| `crates/orca-runtime/src/runtime_turn_start.rs` | Preserve reason when folding start errors |
| `crates/orca-runtime/src/provider_turn.rs` | Preserve reason on provider error → result |
| `crates/orca-runtime/src/controller.rs` | `ThreadTurnCompletion` / `ThreadTurnOutcome` carry reason |
| `crates/orca-runtime/src/runtime_host.rs` | `ThreadOperationOutcome` + preflight admit MaxInnerTurns |
| `crates/orca-runtime/src/thread.rs` | `finish_goal_turn` forwards reason |
| `crates/orca-runtime/src/goal_tracker.rs` | Resumable budget ends enter progress/gap logic; consecutive MaxInnerTurns cap |
| `crates/orca-runtime/src/goal_actor.rs` | Pass reason into `GoalTurnResult` |

---

### Task 1: Carry `TurnEndReason` on `AgentLoopResult`

**Files:**
- Modify: `crates/orca-runtime/src/lifecycle.rs`
- Modify: `crates/orca-runtime/src/runtime_turn_start.rs`
- Modify: `crates/orca-runtime/src/provider_turn.rs` (fold path that drops reason today)

- [x] **Step 1: Failing test** in `lifecycle.rs` tests:

```rust
#[test]
fn agent_loop_result_preserves_turn_end_reason() {
    let from_start = AgentLoopResult::from(RuntimeTurnStartError {
        status: RunStatus::BudgetExhausted,
        reason: TurnEndReason::MaxInnerTurns,
        message: "max turns exhausted".to_string(),
    });
    assert_eq!(from_start.status, RunStatus::BudgetExhausted);
    assert_eq!(from_start.reason, TurnEndReason::MaxInnerTurns);

    let success = AgentLoopResult::success(Some("ok".into()));
    assert_eq!(success.reason, TurnEndReason::Unclassified);

    let generic = AgentLoopResult::failure(RunStatus::Failed, "x");
    assert_eq!(generic.reason, TurnEndReason::Unclassified);
}
```

- [x] **Step 2: Implement**

```rust
pub(crate) struct AgentLoopResult {
    pub(crate) status: RunStatus,
    pub(crate) reason: TurnEndReason,
    pub(crate) final_message: Option<String>,
    pub(crate) error: Option<String>,
}

impl AgentLoopResult {
    pub(crate) fn success(final_message: Option<String>) -> Self {
        Self {
            status: RunStatus::Success,
            reason: TurnEndReason::Unclassified,
            final_message,
            error: None,
        }
    }

    pub(crate) fn failure(status: RunStatus, error: impl Into<String>) -> Self {
        Self::terminal(status, TurnEndReason::Unclassified, Some(error.into()))
    }

    pub(crate) fn terminal(
        status: RunStatus,
        reason: TurnEndReason,
        error: Option<String>,
    ) -> Self {
        Self {
            status,
            reason,
            final_message: None,
            error,
        }
    }
}

impl From<RuntimeTurnStartError> for AgentLoopResult {
    fn from(error: RuntimeTurnStartError) -> Self {
        Self::terminal(error.status, error.reason, Some(error.message))
    }
}
```

Update call sites:

- `runtime_turn_start.rs` fold: `AgentLoopResult::from(error)` (or `error.into()`).
- `provider_turn.rs` paths that currently do `AgentLoopResult::failure(error.status, error.message)` for `RuntimeTurnStartError`: use `AgentLoopResult::from(error)`.
- Any `AgentLoopResult::terminal(status, error)` that only passed two args: add `TurnEndReason::Unclassified` or a known reason (`Cancelled` for cancel paths).

- [x] **Step 3: Test + commit unit**

```bash
cargo test -p orca-runtime --lib agent_loop_result_preserves_turn_end_reason -- --exact
```

---

### Task 2: Plumb reason through thread and host outcomes

**Files:**
- Modify: `crates/orca-runtime/src/controller.rs` (`ThreadTurnCompletion`, `ThreadTurnOutcome::Completed`)
- Modify: `crates/orca-runtime/src/runtime_host.rs` (`ThreadOperationOutcome::Completed`, `From<RunStatus>`, executor mapping)
- Modify: any test / mock `run_turn` implementations that construct `Completed { status, .. }`

- [ ] **Step 1: Shape**

```rust
// controller
pub enum ThreadTurnOutcome {
    Completed {
        status: RunStatus,
        end_reason: TurnEndReason,
        background_workflows: RuntimeBackgroundWorkflows,
    },
    ...
}

// runtime_host
pub enum ThreadOperationOutcome {
    Completed {
        status: RunStatus,
        end_reason: TurnEndReason,
        background_workflows: RuntimeBackgroundWorkflows,
    },
    ...
}
```

Default when reason is unknown: `TurnEndReason::Unclassified`.

`From<RunStatus>`:

```rust
Self::Completed {
    status,
    end_reason: TurnEndReason::Unclassified,
    background_workflows: ...,
}
```

Thread completion must copy `result.reason` from `AgentLoopResult` after the agent loop returns (before verifier runs). If the verifier changes status to `VerificationFailed`, keep reason as `Unclassified` (or leave the prior reason; gate still rejects non-Success/non-MaxInnerTurns).

- [ ] **Step 2: Compile-fix mocks** — every `ThreadOperationOutcome::Completed { status, background_workflows }` in tests needs `end_reason`.

```bash
cargo test -p orca-runtime --lib --no-run
```

---

### Task 3: Tracker — resumable budget ends + consecutive MaxInnerTurns cap

**Files:**
- Modify: `crates/orca-runtime/src/goal_tracker.rs`
- Modify: `crates/orca-runtime/src/goal_actor.rs` (`GoalTurnResult` / `build_turn_result` / `finish_outer_turn` API)
- Modify: `crates/orca-runtime/src/thread.rs` (`finish_goal_turn` accepts and forwards reason)

- [ ] **Step 1: Extend `GoalTurnResult`**

```rust
pub struct GoalTurnResult {
    pub status: GoalTurnStatus,
    pub end_reason: TurnEndReason,
    pub usage: GoalUsage,
    pub gaps: Vec<GoalGap>,
    pub evidence_count: usize,
}
```

Helpers set `end_reason: TurnEndReason::Unclassified`.

- [ ] **Step 2: Tracker behavior**

```rust
const MAX_CONSECUTIVE_INNER_TURN_CONTINUATIONS: u32 = 8;

// on GoalTracker:
consecutive_inner_turn_budget_exhaustions: u32,

// in finish_outer_turn, replace the blanket non-Success early return with:
match result.status {
    GoalTurnStatus::Success => {
        self.consecutive_inner_turn_budget_exhaustions = 0;
        // existing Success path
    }
    GoalTurnStatus::BudgetExhausted
        if result.end_reason == TurnEndReason::MaxInnerTurns =>
    {
        self.consecutive_inner_turn_budget_exhaustions =
            self.consecutive_inner_turn_budget_exhaustions.saturating_add(1);
        if self.consecutive_inner_turn_budget_exhaustions
            >= MAX_CONSECUTIVE_INNER_TURN_CONTINUATIONS
        {
            self.pending_intent = None;
            return Ok(self.pause(
                GoalPauseReason::NoProgress,
                format!(
                    "inner-turn budget exhausted for {} consecutive outer turns without completion",
                    self.consecutive_inner_turn_budget_exhaustions
                ),
            ));
        }
        // fall through into the same post-Success path (token budget,
        // terminal intent, evidence, gaps). Do NOT clear pending_intent
        // solely because of MaxInnerTurns.
    }
    GoalTurnStatus::BudgetExhausted
        if result.end_reason == TurnEndReason::CostBudgetExhausted =>
    {
        self.pending_intent = None;
        return Ok(self.pause(
            GoalPauseReason::UsageLimit,
            "goal outer turn ended because the cost budget was exhausted".to_string(),
        ));
    }
    other => {
        self.pending_intent = None;
        return Ok(self.pause(
            GoalPauseReason::Infrastructure,
            format!("goal outer turn ended with {other:?}"),
        ));
    }
}
```

- [ ] **Step 3: Tests**

```rust
#[test]
fn max_inner_turns_budget_exhaustion_can_continue() { ... }

#[test]
fn cost_budget_exhaustion_pauses_as_usage_limit() { ... }

#[test]
fn eight_consecutive_max_inner_turns_pauses_as_no_progress() { ... }
```

Also update actor/thread to pass `end_reason` from outcome into `finish_outer_turn`.

Store write can keep storing `GoalTurnStatus::BudgetExhausted` without a new column; the consecutive cap is in-memory for this plan (gap streak remains the durable watchdog). Document that restart resets the consecutive MaxInnerTurns counter — acceptable because gap streak still survives.

---

### Task 4: Preflight admits MaxInnerTurns only

**Files:**
- Modify: `crates/orca-runtime/src/runtime_host.rs`

- [x] **Step 1: Replace `successful_turn: bool` with a typed disposition**

```rust
enum GoalTurnDisposition {
    Advanced,
    Interrupted { reason: TurnEndReason },
    Blocked { status: RunStatus, reason: TurnEndReason },
}

struct GoalContinuationPreflight {
    cancelled: bool,
    disposition: GoalTurnDisposition,
    ...
}

fn goal_continuation_preflight(...) {
    ...
    if matches!(input.disposition, GoalTurnDisposition::Blocked { .. }) {
        return Some(reject(
            GoalContinuationRejectCode::NonSuccessfulTurn,
            "goal continuation rejected because the outer turn is blocked",
        ));
    }
    ...
}
```

Admission computation:

```rust
let disposition = match outcome {
    GenerationTaskOutcome::Executed(ThreadOperationOutcome::Completed {
        status: RunStatus::Success,
        ..
    }) => GoalTurnDisposition::Advanced,
    GenerationTaskOutcome::Executed(ThreadOperationOutcome::Completed {
        status: RunStatus::BudgetExhausted,
        end_reason: TurnEndReason::MaxInnerTurns,
        ..
    }) => GoalTurnDisposition::Interrupted {
        reason: TurnEndReason::MaxInnerTurns,
    },
    GenerationTaskOutcome::Executed(ThreadOperationOutcome::Completed {
        status,
        end_reason,
        ..
    }) => GoalTurnDisposition::Blocked { status, reason: end_reason },
};
```

- [x] **Step 2: Update the locked-in test**

Replace the “reason is observability-only” assertion with:

- Success → admit (None)
- BudgetExhausted + MaxInnerTurns → admit (None)
- BudgetExhausted + CostBudgetExhausted → reject NonSuccessfulTurn
- Failed / Cancelled → reject NonSuccessfulTurn

```rust
#[test]
fn goal_continuation_preflight_admits_only_resumable_budget_ends() { ... }
```

- [ ] **Step 3: run_hosted_operation** must pass `end_reason` into `finish_goal_turn` so tracker and preflight agree.

---

### Task 5: Verification

```bash
cargo test -p orca-runtime --lib \
  agent_loop_result_preserves_turn_end_reason \
  max_inner_turns_budget_exhaustion_can_continue \
  cost_budget_exhaustion_pauses_as_usage_limit \
  eight_consecutive_max_inner_turns_pauses_as_no_progress \
  goal_continuation_preflight \
  streak_survives \
  outer_turn_result_reflects \
  budget_exhausted_carries \
  -- --test-threads=1

cargo test -p orca-runtime --lib -- --test-threads=1
```

Confirm:

- Cost budget still pauses (UsageLimit / rejected preflight).
- MaxInnerTurns can Continue from tracker and is admissible in preflight.
- 8 consecutive MaxInnerTurns pause NoProgress.
- Gap streak still pauses after 3 identical model-fixable gaps on Success turns.

## What this unblocks next

1. Soft landing: warn before inner-turn / cost ceilings.
2. Structured continuation envelope: carry durable findings into the next outer turn prompt.
3. Optionally persist consecutive MaxInnerTurns count (or recompute from recent `budget_exhausted` turn rows) for full restart durability of that second net.
