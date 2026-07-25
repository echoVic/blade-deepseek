# Turn End Reason Classification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Attach an explicit, typed end reason to every `RuntimeTurnStartError` so that later work can distinguish a resumable inner-turn ceiling from a cost ceiling that would re-trip immediately.

**Architecture:** Add a `TurnEndReason` enum next to `RuntimeTurnStartError` in `lifecycle.rs` and populate it at every construction site. This plan is deliberately **behavior-preserving**: `goal_continuation_preflight` keeps rejecting every non-`Success` outer turn, so no goal that pauses today will start auto-continuing as a result of this change. The reason field is recorded but not yet consumed by any gate.

**Tech Stack:** Rust workspace, Cargo tests, Clippy.

---

## Why this scope, and what is explicitly NOT here

`RunStatus::BudgetExhausted` is currently reachable from at least two causes that differ in whether resuming can make progress:

- `crates/orca-runtime/src/lifecycle.rs:290` — the inner-turn ceiling (`turns_started >= max_turns`). Resuming makes progress.
- `crates/orca-runtime/src/lifecycle.rs:699` — the `max_budget_usd` cost ceiling. Resuming under unchanged config re-trips immediately.

Because one status covers both, no gate can safely act on `BudgetExhausted` today. This plan only restores the lost information.

**Deliberately out of scope** (each needs its own plan):

- Changing `goal_continuation_preflight` to admit any non-`Success` turn. Doing that before the progress watchdog actually reads evidence risks an auto-retry loop, because `goal_tracker.rs:234` routes non-`Success` straight to `paused(infrastructure)` without computing a gap streak.
- Making the watchdog read/persist real progress evidence. Today `thread.rs:178` passes zero tool/model counts and `None` gap, and `goal_actor.rs:995` then synthesizes a fixed `outer_turn:no_structured_progress` fingerprint; `goal_tracker.rs:89` also resets `last_gap_fingerprint` and streak on recovery, so the three-turn rule is not fully durable across restarts.
- Budget pre-warning / soft landing, and the structured continuation envelope (`runtime_host.rs:13736` currently sends only the objective plus a generic instruction).

**Variants deliberately not introduced:** `ProviderTokenLimit` and `VerificationRejected`. Verified during investigation that neither flows through `RuntimeTurnStartError`: `controller.rs:1292` returns `RunStatus::VerificationFailed` directly, and prompt-too-long is retried via compaction (`provider_turn.rs:236,257`) before degrading into a generic `Failed`. Adding those variants now would create unreachable code. They should be added when the gate work actually needs them.

## File Structure

- `crates/orca-runtime/src/lifecycle.rs` — owns `RuntimeTurnStartError`; gains the `TurnEndReason` enum plus classification at 4 construction sites (2 of which are the `BudgetExhausted` sites this plan exists for).
- `crates/orca-runtime/src/provider_turn.rs` — 3 production construction sites + 4 test sites; provider stream/error paths.
- `crates/orca-runtime/src/runtime_host.rs` — no production change. One new assertion in the existing preflight test to lock in that gate behavior did NOT change.

`TurnEndReason` lives beside `RuntimeTurnStartError` rather than in `orca-core`'s `event_schema.rs` because it is a runtime-internal classification, not part of the persisted event schema. Nothing serializes it yet.

---

### Task 1: Add the `TurnEndReason` type and classify the two `BudgetExhausted` sites

**Files:**
- Modify: `crates/orca-runtime/src/lifecycle.rs:214-218` (add enum, add field)
- Modify: `crates/orca-runtime/src/lifecycle.rs:290-295` (classify `MaxInnerTurns`)
- Modify: `crates/orca-runtime/src/lifecycle.rs:699-706` (classify `CostBudgetExhausted`)
- Test: `crates/orca-runtime/src/lifecycle.rs` (in the existing `mod tests`)

- [ ] **Step 1: Write the failing test**

Add this test to the existing `mod tests` block at the end of `crates/orca-runtime/src/lifecycle.rs`. It asserts the distinction that motivates the whole plan: same `RunStatus`, different reason.

```rust
    #[test]
    fn budget_exhausted_carries_distinct_reasons() {
        let max_turns = RuntimeTurnStartError {
            status: RunStatus::BudgetExhausted,
            reason: TurnEndReason::MaxInnerTurns,
            message: "max turns exhausted".to_string(),
        };
        let cost = RuntimeTurnStartError {
            status: RunStatus::BudgetExhausted,
            reason: TurnEndReason::CostBudgetExhausted,
            message: "budget exhausted".to_string(),
        };

        assert_eq!(max_turns.status, cost.status);
        assert_ne!(max_turns.reason, cost.reason);
        assert_eq!(TurnEndReason::default(), TurnEndReason::Unclassified);
    }
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p orca-runtime lifecycle::tests::budget_exhausted_carries_distinct_reasons --lib -- --exact
```

Expected: FAIL to compile, with `cannot find type TurnEndReason in this scope` and `struct RuntimeTurnStartError has no field named reason`.

- [ ] **Step 3: Add the enum and the field**

In `crates/orca-runtime/src/lifecycle.rs`, replace the struct at lines 214-218:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeTurnStartError {
    pub status: RunStatus,
    pub message: String,
}
```

with:

```rust
/// Why an outer turn ended, kept separate from [`RunStatus`] because one status
/// is reachable for reasons that differ in whether resuming can make progress:
/// an inner-turn ceiling resumes cleanly, while a cost ceiling would re-trip
/// immediately under unchanged config.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TurnEndReason {
    /// The per-run inner turn ceiling was reached.
    MaxInnerTurns,
    /// The configured `max_budget_usd` ceiling was exceeded.
    CostBudgetExhausted,
    /// The turn was explicitly cancelled.
    Cancelled,
    /// Not yet classified; treated as not automatically resumable.
    #[default]
    Unclassified,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeTurnStartError {
    pub status: RunStatus,
    pub reason: TurnEndReason,
    pub message: String,
}
```

- [ ] **Step 4: Classify the inner-turn ceiling site**

In the same file, in `start_turn` at line ~290, replace:

```rust
        if self.turns_started >= self.max_turns {
            return Err(RuntimeTurnStartError {
                status: RunStatus::BudgetExhausted,
                message: "max turns exhausted".to_string(),
            });
        }
```

with:

```rust
        if self.turns_started >= self.max_turns {
            return Err(RuntimeTurnStartError {
                status: RunStatus::BudgetExhausted,
                reason: TurnEndReason::MaxInnerTurns,
                message: "max turns exhausted".to_string(),
            });
        }
```

- [ ] **Step 5: Classify the cost ceiling site**

In the same file, in `record_usage` at line ~699, replace:

```rust
            return Err(RuntimeTurnStartError {
                status: RunStatus::BudgetExhausted,
                message: format!(
                    "budget exhausted: estimated cost ${:.6} exceeded limit ${:.6}",
                    totals.estimated_cost_usd, max_budget
                ),
            });
```

with:

```rust
            return Err(RuntimeTurnStartError {
                status: RunStatus::BudgetExhausted,
                reason: TurnEndReason::CostBudgetExhausted,
                message: format!(
                    "budget exhausted: estimated cost ${:.6} exceeded limit ${:.6}",
                    totals.estimated_cost_usd, max_budget
                ),
            });
```

- [ ] **Step 6: Run the test to verify it passes**

```bash
cargo test -p orca-runtime lifecycle::tests::budget_exhausted_carries_distinct_reasons --lib -- --exact
```

Expected: PASS. Note the crate as a whole will still fail to build until Task 2, because the remaining construction sites lack the new field. That is expected at this checkpoint.

- [ ] **Step 7: Commit**

```bash
git add crates/orca-runtime/src/lifecycle.rs
git commit -m "feat(runtime): add TurnEndReason to distinguish budget exhaustion causes"
```

---

### Task 2: Classify the remaining construction sites

**Files:**
- Modify: `crates/orca-runtime/src/lifecycle.rs:371-380` (cancel + hook-failure branches)
- Modify: `crates/orca-runtime/src/lifecycle.rs:~1584` (test site)
- Modify: `crates/orca-runtime/src/provider_turn.rs:242,383,737` (production sites)
- Modify: `crates/orca-runtime/src/provider_turn.rs:1200,1524,1540,1591` (test sites)

Classification rationale: only paths whose cause is positively known get a specific reason. Everything else gets `Unclassified`, which is the conservative default and keeps behavior identical to today.

- [ ] **Step 1: Compile to enumerate every remaining site**

```bash
cargo build -p orca-runtime 2>&1 | grep -A2 "missing field \`reason\`" | head -40
```

Expected: errors listing the sites in `lifecycle.rs` and `provider_turn.rs` enumerated below. Use this output as the worklist; if it names a file not listed in this task, classify it `Unclassified` and note it in the commit body.

- [ ] **Step 2: Classify the hook `map_err` branches in `lifecycle.rs`**

At line ~369, replace:

```rust
        result.map_err(|error| {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                RuntimeTurnStartError {
                    status: RunStatus::Cancelled,
                    message: "turn cancelled".to_string(),
                }
            } else {
                RuntimeTurnStartError {
                    status: RunStatus::Failed,
                    message: format!("pre_model_call hook failed: {error}"),
                }
            }
        })
```

with:

```rust
        result.map_err(|error| {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                RuntimeTurnStartError {
                    status: RunStatus::Cancelled,
                    reason: TurnEndReason::Cancelled,
                    message: "turn cancelled".to_string(),
                }
            } else {
                RuntimeTurnStartError {
                    status: RunStatus::Failed,
                    reason: TurnEndReason::Unclassified,
                    message: format!("pre_model_call hook failed: {error}"),
                }
            }
        })
```

- [ ] **Step 3: Classify the `provider_turn.rs` cancellation site**

At line ~737 in `cancelled_provider_turn`, replace:

```rust
    Ok(RuntimeProviderTurnOutput::terminal(RuntimeTurnStartError {
        status: RunStatus::Cancelled,
        message: "turn cancelled".to_string(),
    }))
```

with:

```rust
    Ok(RuntimeProviderTurnOutput::terminal(RuntimeTurnStartError {
        status: RunStatus::Cancelled,
        reason: TurnEndReason::Cancelled,
        message: "turn cancelled".to_string(),
    }))
```

- [ ] **Step 4: Classify the two `provider_turn.rs` failure sites**

At line ~242 (provider error fold), add the field:

```rust
                Ok(RuntimeProviderErrorStepOutcome::Failed(
                    RuntimeTurnStartError {
                        status: RunStatus::Failed,
                        reason: TurnEndReason::Unclassified,
                        message,
                    },
                ))
```

At line ~383 (stream disconnected), add the field:

```rust
                    return Ok(RuntimeProviderTurnOutput::terminal(RuntimeTurnStartError {
                        status: RunStatus::Failed,
                        reason: TurnEndReason::Unclassified,
                        message: "provider stream disconnected before completion".to_string(),
                    }));
```

Ensure `TurnEndReason` is in scope in `provider_turn.rs` by extending the existing `lifecycle` import at line ~33 to include it:

```rust
    AgentLoopResult, RuntimeTaskActor, RuntimeTurnContext, RuntimeTurnStartError, TurnEndReason,
```

- [ ] **Step 5: Fix the five test construction sites**

Each of these is inside a `#[test]` and only needs the field added, matching the status already asserted.

`lifecycle.rs` line ~1584 — status is `Failed`, message says "max turns exceeded"; this is a fold-behavior test, not the real ceiling path, so use `Unclassified`:

```rust
            error: Some(RuntimeTurnStartError {
                status: RunStatus::Failed,
                reason: TurnEndReason::Unclassified,
                message: "max turns exceeded".to_string(),
            }),
```

`provider_turn.rs` line ~1200 and line ~1591 — both `Failed` with "provider failed":

```rust
            RuntimeTurnStartError {
                status: RunStatus::Failed,
                reason: TurnEndReason::Unclassified,
                message: "provider failed".to_string(),
            }
```

`provider_turn.rs` line ~1524 and line ~1540 — both `Cancelled` with "turn cancelled":

```rust
        RuntimeTurnStartError {
            status: RunStatus::Cancelled,
            reason: TurnEndReason::Cancelled,
            message: "turn cancelled".to_string(),
        }
```

- [ ] **Step 6: Verify the crate builds clean**

```bash
cargo build -p orca-runtime 2>&1 | tail -5
cargo clippy -p orca-runtime --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: build succeeds; clippy reports no warnings. If clippy flags `TurnEndReason::MaxInnerTurns` or `CostBudgetExhausted` as never-read, that is expected at this stage — no gate consumes them yet. Silence it by confirming the variants are constructed (they are, in Task 1 Steps 4-5); do NOT add `#[allow(dead_code)]` to the whole enum.

- [ ] **Step 7: Run the full runtime test suite**

```bash
cargo test -p orca-runtime --lib -- --test-threads=1 2>&1 | tail -15
```

Expected: PASS, same count as before this plan. Any new failure means a construction site was misclassified — recheck against Step 5.

- [ ] **Step 8: Commit**

```bash
git add crates/orca-runtime/src/lifecycle.rs crates/orca-runtime/src/provider_turn.rs
git commit -m "feat(runtime): classify remaining turn end reasons as Unclassified"
```

---

### Task 3: Lock in that continuation gate behavior did not change

**Files:**
- Modify: `crates/orca-runtime/src/runtime_host.rs:19518-19566` (extend existing test)

This task adds no production code. It exists so a future reader cannot mistake this plan for the gate change, and so the gate change is a deliberate, test-visible decision later.

- [ ] **Step 1: Extend the existing preflight test with a regression assertion**

In `crates/orca-runtime/src/runtime_host.rs`, the test `goal_continuation_preflight_has_no_outer_turn_limit` ends with:

```rust
        assert_eq!(goal_continuation_preflight(baseline), None);
    }
```

Replace that with:

```rust
        assert_eq!(goal_continuation_preflight(baseline), None);

        // TurnEndReason classification is observability-only for now: a
        // non-successful outer turn is still rejected regardless of why it
        // ended. Admitting resumable reasons requires the progress watchdog to
        // read real evidence first, otherwise a cost-exhausted goal would
        // retry into the same wall.
        assert!(matches!(
            goal_continuation_preflight(GoalContinuationPreflight {
                successful_turn: false,
                ..baseline
            }),
            Some(GoalContinuationAdmission::Reject { code, .. })
                if code == GoalContinuationRejectCode::NonSuccessfulTurn
        ));
    }
```

- [ ] **Step 2: Run the test to verify it passes**

```bash
cargo test -p orca-runtime runtime_host::tests::goal_continuation_preflight_has_no_outer_turn_limit --lib -- --exact
```

Expected: PASS. It passes immediately because behavior genuinely is unchanged — that is the point of the assertion.

- [ ] **Step 3: Run the goal-focused suites**

```bash
cargo test -p orca-runtime goal --lib -- --test-threads=1 2>&1 | tail -10
```

Expected: PASS, including the existing three-turn no-progress test.

- [ ] **Step 4: Commit**

```bash
git add crates/orca-runtime/src/runtime_host.rs
git commit -m "test(runtime): assert turn end reason does not yet affect continuation gate"
```

---

## Verification

Run from the repo root:

```bash
cargo clippy -p orca-runtime --all-targets -- -D warnings
cargo test -p orca-runtime --lib -- --test-threads=1
```

Both must pass with no new failures versus the pre-plan baseline.

Manual check that the change is real and inert:

```bash
grep -n "reason: TurnEndReason::" crates/orca-runtime/src/lifecycle.rs crates/orca-runtime/src/provider_turn.rs
```

Expected: `MaxInnerTurns` appears exactly once (`lifecycle.rs` `start_turn`), `CostBudgetExhausted` exactly once (`lifecycle.rs` `record_usage`), and every other site is `Cancelled` or `Unclassified`.

## Follow-on work this unblocks

1. Make the watchdog read and durably persist progress evidence — runtime-observed side effects (file writes, command execution) plus plan item transitions as the semantic anchor; pure reads must not count as progress. Requires fixing `thread.rs:178`, `goal_actor.rs:995`, and the recovery reset at `goal_tracker.rs:89`.
2. Only then, admit `TurnEndReason::MaxInnerTurns` in `goal_continuation_preflight`, keeping `CostBudgetExhausted` paused. Pair it with an independent cap on consecutive budget continuations per goal as a second safety net beside the watchdog.
3. Budget pre-warning and soft landing, so a turn stops at a clean boundary instead of mid-exploration.
4. Structured continuation envelope carrying durable findings, completed side effects, and an explicit next action.
