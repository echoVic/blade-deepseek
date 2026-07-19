# Goal Remove Fixed Outer-Turn Limit Design

Date: 2026-07-19

Status: approved

## Context

Goal mode currently rejects continuation after 64 outer turns in one
`GoalRun`. The limit was reintroduced as a final runtime safety boundary after
the Goal control-plane incident, but an outer-turn count is neither a progress
signal nor a resource budget. It can stop a healthy long-running task while a
broken task can still consume excessive tokens before reaching the limit.

Codex, Claude Code, and Grok Build do not use a fixed 64-turn ceiling as their
default Goal stopping policy. Orca should follow the same principle.

## Decision

Remove the fixed outer-turn ceiling completely. Do not replace it with another
fixed or configurable turn count.

Automatic continuation remains eligible regardless of the number of completed
outer turns when all existing semantic and ownership checks pass:

- the Goal remains active;
- the previous outer turn succeeded;
- no cancellation, user input, interaction, workflow, or plan-mode owner must
  take control;
- no duplicate generation fence is admitted;
- Goal and verifier budgets remain available;
- the structured no-progress policy has not paused the Goal.

The existing `continuation_count` remains in the GoalRun ledger and semantic
events for observability only. Runtime behavior must not branch on that value.

## Code Changes

- Delete `MAX_GOAL_OUTER_TURNS_PER_RUN`.
- Remove `OuterTurnLimit` from `GoalContinuationRejectCode` and its event-name
  and pause-reason mappings.
- Remove `continuation_count` from continuation preflight input. Continue
  recording the persisted count in observability events.
- Remove the 64-turn rule from the Goal and harness contracts.

No Goal state or migration is required. Historical `Paused(NoProgress)` rows
remain readable, although old rows created by the former 64-turn rule cannot be
distinguished from genuine repeated-gap pauses.

## Safety Model

Removing the turn ceiling does not remove bounded execution controls. GoalRun
still stops or yields on typed state transitions, user control, pending
interaction, workflow ownership, cancellation, failed outer turns, budget
exhaustion, verifier failure, crash recovery, and three identical
model-fixable gaps across closed outer turns.

If Orca later needs an emergency resource circuit breaker, it must use explicit
token, cost, elapsed-time, or control-plane failure budgets with its own typed
reason. It must not infer `NoProgress` from an arbitrary turn count.

## Verification

- Add a regression test proving a successful continuation remains admissible
  after 64 completed generations when no other rejection condition applies.
- Keep the admission rejection matrix for all remaining conditions.
- Run focused `orca-runtime` Goal tests, TUI Goal tests, the full workspace test
  suite, Clippy, and formatting/diff checks.

## Alternatives Rejected

- Keep 64 as a hidden emergency limit: still stops healthy long-running work
  and misclassifies the result as no progress.
- Make the turn count configurable or disabled by default: adds configuration
  for a signal that has no valid stopping semantics.
- Replace 64 with a larger number: changes incident timing, not the design flaw.
