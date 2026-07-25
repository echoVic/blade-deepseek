# Soft Landing + Structured Continuation Envelope Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Before hard budget walls, warn the model (Codex-style threshold reminders for remaining inner turns, cost, and Goal tokens), and when auto-continuing a goal, hand the next outer turn a structured envelope instead of a bare objective + generic instruction.

**Architecture:** Keep soft-landing policy pure and unit-tested (`budget_soft_landing.rs`). Deliver inner-turn and cost reminders as ephemeral/pinned system context right after a successful inner-turn start; include Goal-token reminders in refreshed Goal context. Build continuation prompts from a typed envelope assembled at admission time from the last outer-turn outcome, goal usage, recent gap history, and current plan snapshot.

**Tech Stack:** Rust workspace, Cargo tests.

---

## Explicitly out of scope

- Full Codex `RolloutBudget` weighted token accounting.
- Persisting soft-landing delivery state across process restarts.
- Auto-expanding `max_turns` or `max_budget_usd`.
- Full durable “findings journal” table (use already-persisted gap fingerprints + usage for now).

## Task 1: Pure soft-landing policy

New file: `crates/orca-runtime/src/budget_soft_landing.rs`

Thresholds (remaining inner turns): `[16, 8, 4, 2]` — mirrors Codex’s multi-threshold `reminder_at_remaining_tokens` pattern.

```rust
pub(crate) struct SoftLandingReminder {
    pub remaining: u64,
    pub reminder_index: u32, // count of crossed thresholds
    pub kind: SoftLandingKind,
}

pub(crate) enum SoftLandingKind {
    InnerTurns { max_turns: u32 },
    CostBudgetUsd { max_budget_usd: f64, spent_usd: f64 },
    GoalTokens { budget: i64, used: i64 },
}

pub(crate) fn pending_inner_turn_reminder(
    max_turns: u32,
    turns_started: u32,
    delivered_index: u32,
) -> Option<SoftLandingReminder>;

pub(crate) fn format_soft_landing_message(reminder: &SoftLandingReminder) -> String;
```

Semantics (Codex-aligned):
- `remaining = max_turns.saturating_sub(turns_started)`
- `reminder_index = thresholds.iter().filter(|t| remaining <= t).count()`
- Deliver only when `reminder_index > delivered_index` and `reminder_index > 0`
- Message tells the model the wall is near: prioritize finishing / verifying; do not thrash; do not mark complete merely because budget is low.

## Task 2: Deliver inner-turn soft landing after start_turn

- `RuntimeTaskActor` gains `inner_turn_reminder_index: u32`
- After successful `start_turn`, expose `take_pending_inner_turn_soft_landing() -> Option<String>`
- `runtime_turn_opening.rs` injects the message with `conversation.add_system_pinned(...)` so the model sees it on the next provider call without inventing a user turn
- Cost usage updates advance an independent delivery index; the next opening injects the pending cost reminder once.
- Goal-token reminders are included when Goal context is refreshed near its configured token ceiling.

## Task 3: Structured continuation envelope

Replace bare `goal_continuation_prompt(objective, n)` with:

```rust
struct GoalContinuationEnvelope {
    objective: String,
    continuation: usize,
    trigger: GoalContinuationTrigger,
    tokens_used: i64,
    token_budget: Option<i64>,
    last_gap_fingerprint: Option<String>,
    last_outer_status: Option<&'static str>,
    last_end_reason: Option<&'static str>,
    plan_snapshot: Option<String>,
    previous_checkpoint: Option<String>,
}

enum GoalContinuationTrigger {
    Progress,
    MaxInnerTurns,
    GapFeedback,
}
```

Envelope sections:
1. Header + objective
2. Why this outer turn started (trigger)
3. Budget snapshot (tokens used / remaining)
4. Prior outer-turn terminal (status + end reason when known)
5. Open gap fingerprint if any
6. Explicit next-action guidance (stronger wording for MaxInnerTurns / near budget)
7. Current structured task-plan snapshot with an explicit resume instruction
8. Bounded previous-assistant checkpoint, explicitly treated as a hint to
   verify against current state

Admission path passes `TurnEndReason` + goal record fields into the envelope.

## Verification

```bash
cargo test -p orca-runtime --lib budget_soft_landing goal_continuation_envelope soft_landing -- --test-threads=1
cargo test -p orca-runtime --lib -- --test-threads=1
```
