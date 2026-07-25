# Goal Progress Watchdog Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the goal no-progress watchdog operate on real progress evidence instead of hardcoded zeros, and make its streak survive process restarts — so that a later change can safely auto-continue budget-exhausted goals without risking an infinite retry loop.

**Architecture:** Thread real per-turn counters from the runtime down into `GoalTurnResult`, stop synthesizing a fake "no structured progress" gap when evidence exists, and rebuild the same-gap streak from the already-persisted `goal_turns` rows on recovery. No gate behavior changes and no SQLite schema migration.

**Tech Stack:** Rust workspace, Cargo tests, Clippy, rusqlite.

> **Implementation correction:** Observable activity is not sufficient progress.
> The final implementation preserves tool/model counters for diagnostics, but
> only completed side-effecting tool calls or a changed structured task plan
> clear the watchdog. Read-only exploration emits
> `outer_turn:no_substantive_progress`. Persisted `NULL` fingerprints are kept
> in recent history as progress barriers so equal gaps cannot join across a
> productive turn after restart.

---

## Why this exists

`RunStatus::BudgetExhausted` was split by cause in a prior change (`TurnEndReason::MaxInnerTurns` vs `CostBudgetExhausted`), but the continuation gate still rejects every non-`Success` turn. Before the gate can admit `MaxInnerTurns`, the safety net that would catch a goal spinning without progress has to actually work. Today it does not, for three independently verified reasons:

1. **Evidence is hardcoded.** `crates/orca-runtime/src/thread.rs` in `finish_goal_turn` calls `binding.handle.finish_outer_turn(..., 0, 0, None, ...)` — tool count, model response count, and gap fingerprint are literals, not measurements.

2. **A fake gap is always synthesized.** `crates/orca-runtime/src/goal_actor.rs` in `finish_outer_turn` defaults the fingerprint to `"outer_turn:no_structured_progress"` and unconditionally builds `GoalTurnResult { gaps: vec![one model_fixable gap], evidence_count: 0 }`. So every turn looks identical to the tracker, and `evidence_count > 0` in `goal_tracker.rs` is unreachable.

3. **The streak does not survive restart.** `same_gap_streak` and `last_gap_fingerprint` live only in `GoalTracker` memory. `GoalTracker::from_record` resets both to `0` / `None`. The three-turn rule therefore restarts its count on every process restart.

### The key discovery that shapes this plan

**No schema migration is required.** The `goal_turns` table in `crates/orca-runtime/src/goal_store.rs` already declares `tool_count`, `model_response_count`, and `gap_fingerprint` columns, and `finish_outer_turn` already writes them. The data has been getting persisted all along — as zeros and a constant string, because of problems 1 and 2. Once real values flow in, the streak can be **recomputed** by reading recent `goal_turns` rows back, rather than by adding new columns.

This is why the plan reads history instead of migrating: recomputation is derivable from data we already store, and avoids touching a schema that a live `~/.orca/goals.sqlite3` depends on.

## Explicitly out of scope

- **Changing `goal_continuation_preflight`.** The gate must keep rejecting non-`Success` turns until this watchdog is proven. Admitting `MaxInnerTurns` is the *next* plan, and it should also add an independent cap on consecutive budget continuations as a second net beside the streak.
- **The early return in `goal_tracker.rs` `finish_outer_turn`** that sends non-`Success` straight to `pause(Infrastructure)` before any streak math. It stays. Removing it is part of the gate change, not this one; doing it here would alter pause behavior with no gate benefit.
- Budget pre-warning / soft landing, and the structured continuation envelope.

## File Structure

- `crates/orca-runtime/src/thread.rs` — owns `finish_goal_turn`; gains a small `TurnProgressEvidence` value passed by callers and forwarded to the goal handle.
- `crates/orca-runtime/src/goal_actor.rs` — stops fabricating a gap when evidence is present; still synthesizes one when a turn genuinely produced nothing.
- `crates/orca-runtime/src/goal_store.rs` — gains one read-only query that returns recent turn fingerprints for a goal.
- `crates/orca-runtime/src/goal_tracker.rs` — `from_record` seeds the streak from that history instead of zeroing it.

---

### Task 1: Carry real progress evidence to the goal actor

**Files:**
- Modify: `crates/orca-runtime/src/thread.rs` (add struct; `finish_goal_turn` signature and its `finish_outer_turn` call; 4 internal call sites near lines 415, 447, 498, 546)
- Modify: `crates/orca-runtime/src/runtime_host.rs` (the `finish_goal_turn` call near line 13414)
- Test: `crates/orca-runtime/src/thread.rs` (in its `mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/orca-runtime/src/thread.rs`:

```rust
    #[test]
    fn turn_progress_evidence_reports_activity() {
        let empty = TurnProgressEvidence::default();
        assert_eq!(empty.tool_count, 0);
        assert_eq!(empty.model_response_count, 0);
        assert!(!empty.has_activity());

        let active = TurnProgressEvidence {
            tool_count: 3,
            model_response_count: 1,
        };
        assert!(active.has_activity());

        let responses_only = TurnProgressEvidence {
            tool_count: 0,
            model_response_count: 2,
        };
        assert!(responses_only.has_activity());
    }
```

- [ ] **Step 2: Run it and confirm it FAILS**

```bash
cargo test -p orca-runtime thread::tests::turn_progress_evidence_reports_activity --lib -- --exact
```

Expected: compile error `cannot find type TurnProgressEvidence in this scope`.

- [ ] **Step 3: Define the type**

In `crates/orca-runtime/src/thread.rs`, above `finish_goal_turn`:

```rust
/// Per-outer-turn activity counters used by the goal no-progress watchdog.
/// Counts observable runtime activity, not semantic success: a turn that ran
/// many tools but achieved nothing still reports activity here, and is
/// distinguished later by gap fingerprint repetition rather than by volume.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TurnProgressEvidence {
    pub(crate) tool_count: u32,
    pub(crate) model_response_count: u32,
}

impl TurnProgressEvidence {
    pub(crate) fn has_activity(&self) -> bool {
        self.tool_count > 0 || self.model_response_count > 0
    }
}
```

- [ ] **Step 4: Run the test to confirm it PASSES**

```bash
cargo test -p orca-runtime thread::tests::turn_progress_evidence_reports_activity --lib -- --exact
```

Expected: PASS.

- [ ] **Step 5: Thread it through `finish_goal_turn`**

Add a parameter to `finish_goal_turn` in `crates/orca-runtime/src/thread.rs`. The signature currently ends with `config: &RunConfig, cancel: CancelToken`. Add `evidence: TurnProgressEvidence` immediately before `config`:

```rust
    pub(crate) fn finish_goal_turn(
        &mut self,
        binding: Option<&GoalRuntimeBinding>,
        status: RunStatus,
        usage: orca_core::goal_runtime::GoalUsage,
        mut events: Option<&mut EventFactory>,
        observer: Option<&dyn orca_core::event_sink::EventObserver>,
        evidence: TurnProgressEvidence,
        config: &RunConfig,
        cancel: CancelToken,
    ) {
```

Then replace the hardcoded arguments in the `finish_outer_turn` call inside that function. It currently reads:

```rust
        let action = binding.handle.finish_outer_turn(
            &turn.session_id,
            goal_status,
            usage.clone(),
            0,
            0,
            None,
            now_timestamp(),
        );
```

Change it to:

```rust
        let action = binding.handle.finish_outer_turn(
            &turn.session_id,
            goal_status,
            usage.clone(),
            evidence.tool_count,
            evidence.model_response_count,
            None,
            now_timestamp(),
        );
```

Leave the `None` fingerprint argument alone — Task 2 handles fingerprints.

- [ ] **Step 6: Update all five call sites to pass real counts**

There are four calls inside `thread.rs` (near lines 415, 447, 498, 546) and one in `runtime_host.rs` (near line 13414).

For each, determine the counts from the session the same way usage is already derived. Immediately before each `finish_goal_turn(...)` call there is already a usage delta computed from `aggregate_usage_totals()` snapshots taken before and after the turn. Capture message-count snapshots the same way, using the session's conversation length before and after the turn:

```rust
        let messages_before = self.session.conversation_message_count();
        // ... existing turn execution ...
        let evidence = TurnProgressEvidence {
            tool_count: self
                .session
                .tool_result_count_since(messages_before)
                .try_into()
                .unwrap_or(u32::MAX),
            model_response_count: self
                .session
                .assistant_message_count_since(messages_before)
                .try_into()
                .unwrap_or(u32::MAX),
        };
```

If `Session` does not already expose `conversation_message_count`, `tool_result_count_since`, or `assistant_message_count_since`, add them to `crates/orca-runtime/src/session.rs` as thin read-only helpers over the existing conversation storage — do not restructure the session. Match whatever the conversation is actually stored as; inspect `aggregate_usage_totals` around line 432 of that file for the established access pattern.

In `runtime_host.rs` the receiver is `thread` rather than `self`, so use `thread.session()`.

**If the needed counts genuinely cannot be derived** from the session at a call site, stop and report rather than inventing a number. Passing `TurnProgressEvidence::default()` is acceptable ONLY for a call site that provably cannot run a model turn; note which one and why in your report.

- [ ] **Step 7: Verify build and full suite**

```bash
cargo build -p orca-runtime --all-targets 2>&1 | tail -5
cargo test -p orca-runtime --lib -- --test-threads=1 2>&1 | tail -5
```

Expected: clean build; 927 passing (926 pre-existing plus the new one), 0 failed.

- [ ] **Step 8: Commit**

```bash
git add crates/orca-runtime/src/thread.rs crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/src/session.rs
git commit -m "feat(runtime): carry real per-turn progress evidence to the goal actor

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

---

### Task 2: Stop fabricating a no-progress gap when the turn did work

**Files:**
- Modify: `crates/orca-runtime/src/goal_actor.rs` (`finish_outer_turn`, near line 985)
- Test: `crates/orca-runtime/src/goal_actor.rs` (in its `mod tests`)

Currently every turn hands the tracker one `model_fixable` gap and `evidence_count: 0`, which makes `goal_tracker.rs`'s progress branch unreachable. After this task, a turn with activity and no explicit gap reports evidence and no gap; a turn with no activity still reports the synthesized gap.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/orca-runtime/src/goal_actor.rs`:

```rust
    #[test]
    fn outer_turn_result_reflects_evidence_instead_of_constant_gap() {
        let active = build_turn_result(GoalTurnStatus::Success, 4, 2, None);
        assert_eq!(active.evidence_count, 6);
        assert!(
            active.gaps.is_empty(),
            "a turn with activity and no explicit gap must not synthesize one"
        );

        let idle = build_turn_result(GoalTurnStatus::Success, 0, 0, None);
        assert_eq!(idle.evidence_count, 0);
        assert_eq!(idle.gaps.len(), 1);
        assert_eq!(
            idle.gaps[0].fingerprint,
            "outer_turn:no_structured_progress"
        );

        let explicit = build_turn_result(
            GoalTurnStatus::Success,
            5,
            1,
            Some("roadmap:next-slice".to_string()),
        );
        assert_eq!(explicit.gaps.len(), 1);
        assert_eq!(explicit.gaps[0].fingerprint, "roadmap:next-slice");
    }
```

- [ ] **Step 2: Run it and confirm it FAILS**

```bash
cargo test -p orca-runtime goal_actor::tests::outer_turn_result_reflects_evidence_instead_of_constant_gap --lib -- --exact
```

Expected: compile error, `cannot find function build_turn_result`.

- [ ] **Step 3: Extract the decision into a testable free function**

In `crates/orca-runtime/src/goal_actor.rs`, add near `finish_outer_turn`:

```rust
/// Builds the tracker input for a finished outer turn.
///
/// An explicit gap fingerprint always wins. Otherwise a gap is synthesized only
/// when the turn produced no observable activity at all — previously this was
/// unconditional, which made the tracker's progress branch unreachable and hid
/// genuine no-progress turns among normal ones.
fn build_turn_result(
    status: GoalTurnStatus,
    tool_count: u32,
    model_response_count: u32,
    gap_fingerprint: Option<String>,
) -> GoalTurnResult {
    // GoalTurnResult::evidence_count is usize (verified at
    // goal_tracker.rs:17), so widen from the u32 wire counters here.
    let evidence_count = tool_count as usize + model_response_count as usize;
    let gaps = match gap_fingerprint {
        Some(fingerprint) => vec![GoalGap {
            summary: "outer turn reported a structured gap".to_string(),
            fingerprint,
            model_fixable: true,
        }],
        None if evidence_count == 0 => vec![GoalGap {
            summary: "outer turn ended without structured progress evidence".to_string(),
            fingerprint: "outer_turn:no_structured_progress".to_string(),
            model_fixable: true,
        }],
        None => Vec::new(),
    };
    GoalTurnResult {
        status,
        usage: GoalUsage::default(),
        gaps,
        evidence_count,
    }
}
```

Note `evidence_count`'s type must match `GoalTurnResult`'s field; if that field is not `u32`, convert with `try_into().unwrap_or(...)` rather than changing the field type.

- [ ] **Step 4: Run the test to confirm it PASSES**

```bash
cargo test -p orca-runtime goal_actor::tests::outer_turn_result_reflects_evidence_instead_of_constant_gap --lib -- --exact
```

Expected: PASS.

- [ ] **Step 5: Use it from `finish_outer_turn`**

In `finish_outer_turn`, delete the `gap_fingerprint.unwrap_or_else(...)` defaulting line and replace the inline `GoalTurnResult { ... }` literal with a call to the new function, carrying the real usage through:

```rust
        let mut turn_result = build_turn_result(
            status,
            tool_count,
            model_response_count,
            gap_fingerprint.clone(),
        );
        turn_result.usage = usage.clone();
        let tracker_action = active
            .tracker
            .finish_outer_turn(turn_result)
            .map_err(|error| GoalActorError::Invalid(error.to_string()))?;
```

`gap_fingerprint` is still needed afterwards for the store write. If the store call previously relied on the defaulted string, preserve that exact persisted behavior by defaulting only at the store boundary:

```rust
            gap_fingerprint: Some(
                gap_fingerprint.unwrap_or_else(|| "outer_turn:no_structured_progress".to_string()),
            ),
```

Check how `FinishOuterTurnInput` is populated below and keep the persisted value byte-identical to today, so Task 3's history read sees a consistent column.

- [ ] **Step 6: Verify build and full suite**

```bash
cargo build -p orca-runtime --all-targets 2>&1 | tail -5
cargo test -p orca-runtime --lib -- --test-threads=1 2>&1 | tail -5
```

Expected: clean build, 0 failed. If a pre-existing goal test now fails, it is asserting the old always-a-gap behavior — read it carefully and report before changing it; that assertion may be encoding the bug.

- [ ] **Step 7: Commit**

```bash
git add crates/orca-runtime/src/goal_actor.rs
git commit -m "fix(runtime): only synthesize a no-progress gap when the turn did nothing

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

---

### Task 3: Rebuild the gap streak on recovery

**Files:**
- Modify: `crates/orca-runtime/src/goal_store.rs` (add a read-only history query)
- Modify: `crates/orca-runtime/src/goal_tracker.rs` (`from_record`, near line 89)
- Test: `crates/orca-runtime/src/goal_tracker.rs` (in its `mod tests`)

`GoalTracker::from_record` currently sets `last_gap_fingerprint: None, same_gap_streak: 0`, so a restart forgets how many consecutive turns hit the same gap. The `goal_turns` table already stores `gap_fingerprint` per turn, so the streak is derivable.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/orca-runtime/src/goal_tracker.rs`:

```rust
    #[test]
    fn streak_is_rebuilt_from_recent_fingerprints() {
        // Ordered most-recent-first, as returned by the history query.
        let repeated = vec![
            "gap:alpha".to_string(),
            "gap:alpha".to_string(),
            "gap:beta".to_string(),
        ];
        assert_eq!(
            streak_from_history(&repeated),
            (Some("gap:alpha".to_string()), 2)
        );

        let fresh = vec!["gap:beta".to_string(), "gap:alpha".to_string()];
        assert_eq!(
            streak_from_history(&fresh),
            (Some("gap:beta".to_string()), 1)
        );

        assert_eq!(streak_from_history(&[]), (None, 0));
    }
```

- [ ] **Step 2: Run it and confirm it FAILS**

```bash
cargo test -p orca-runtime goal_tracker::tests::streak_is_rebuilt_from_recent_fingerprints --lib -- --exact
```

Expected: compile error, `cannot find function streak_from_history`.

- [ ] **Step 3: Implement the pure helper**

In `crates/orca-runtime/src/goal_tracker.rs`:

```rust
/// Recomputes the same-gap streak from recent turn fingerprints, most recent
/// first. Kept pure and separate from storage so the streak survives restarts
/// without persisting derived state that could drift from the turn history.
fn streak_from_history(fingerprints: &[String]) -> (Option<String>, u32) {
    let Some(most_recent) = fingerprints.first() else {
        return (None, 0);
    };
    let streak = fingerprints
        .iter()
        .take_while(|fingerprint| *fingerprint == most_recent)
        .count()
        .try_into()
        .unwrap_or(u32::MAX);
    (Some(most_recent.clone()), streak)
}
```

- [ ] **Step 4: Run the test to confirm it PASSES**

```bash
cargo test -p orca-runtime goal_tracker::tests::streak_is_rebuilt_from_recent_fingerprints --lib -- --exact
```

Expected: PASS.

- [ ] **Step 5: Add the history query to the store**

In `crates/orca-runtime/src/goal_store.rs`, add a read-only method on the store. Follow the existing style of `load_current_run` (around line 1385) for error handling and connection access:

```rust
    /// Recent non-null gap fingerprints for a goal's current run, most recent
    /// first. Bounded because only the streak limit's worth of history matters.
    pub fn recent_gap_fingerprints(
        &self,
        goal_id: &GoalId,
        limit: u32,
    ) -> Result<Vec<String>, GoalStoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT t.gap_fingerprint
             FROM goal_turns t
             JOIN goal_runs r ON t.goal_run_id = r.goal_run_id
             WHERE r.goal_id = ?1
               AND t.gap_fingerprint IS NOT NULL
               AND t.finished_at IS NOT NULL
             ORDER BY t.finished_at DESC
             LIMIT ?2",
        )?;
        let rows = statement.query_map(
            rusqlite::params![goal_id.as_str(), limit],
            |row| row.get::<_, String>(0),
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
```

Adapt `self.connection()` to whatever accessor this store actually uses — check `load_stored_goal` near line 1328.

- [ ] **Step 6: Seed the streak in `from_record`**

`from_record` is a pure constructor taking only a `&GoalRecord`, so it cannot query. Rather than giving it store access, add a companion that the caller uses when a store is available:

```rust
    pub fn from_record_with_history(record: &GoalRecord, history: &[String]) -> Self {
        let mut tracker = Self::from_record(record);
        let (fingerprint, streak) = streak_from_history(history);
        tracker.last_gap_fingerprint = fingerprint;
        tracker.same_gap_streak = streak;
        tracker
    }
```

Then find `from_record`'s callers and, where a store is in scope, switch to `from_record_with_history` passing `store.recent_gap_fingerprints(goal_id, SAME_GAP_STREAK_LIMIT)?`. Locate them with:

```bash
grep -rn "from_record(" crates/orca-runtime/src/
```

If a caller has no store access, leave it on `from_record` — behavior there is unchanged from today. Report which callers you changed and which you left.

- [ ] **Step 7: Verify build and full suite**

```bash
cargo build -p orca-runtime --all-targets 2>&1 | tail -5
cargo clippy -p orca-runtime --all-targets -- -D warnings 2>&1 | grep -E "orca-runtime/src" | head -5
cargo test -p orca-runtime --lib -- --test-threads=1 2>&1 | tail -5
```

Expected: clean build; **no clippy findings in `crates/orca-runtime/src`** (the workspace has ~18 pre-existing findings in `orca-core` — those are not yours and must be ignored); all tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/orca-runtime/src/goal_store.rs crates/orca-runtime/src/goal_tracker.rs
git commit -m "fix(runtime): rebuild goal gap streak from persisted turn history

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

---

## Verification

```bash
cargo clippy -p orca-runtime --all-targets -- -D warnings 2>&1 | grep -E "orca-runtime/src" || echo "no findings in orca-runtime"
cargo test -p orca-runtime --lib -- --test-threads=1
```

Baseline for comparison: before this plan, `orca-runtime` had **926 passing tests, 0 failing**, and **zero** clippy findings in `crates/orca-runtime/src` (all ~18 workspace findings are in `orca-core`).

Confirm the hardcoded zeros are gone:

```bash
grep -n "finish_outer_turn(" -A6 crates/orca-runtime/src/thread.rs | grep -E "^\s+[0-9]+,$" && echo "STILL HARDCODED" || echo "evidence is threaded"
```

Confirm the gate was not touched:

```bash
git diff HEAD~3 HEAD -- crates/orca-runtime/src/runtime_host.rs | grep -c "goal_continuation_preflight" || echo "gate untouched"
```

## What this unblocks

With real evidence flowing and the streak durable, the gate change becomes safe to attempt: admit `TurnEndReason::MaxInnerTurns` in `goal_continuation_preflight` while keeping `CostBudgetExhausted` paused, and remove the non-`Success` early return in `goal_tracker.rs` `finish_outer_turn` so budget-exhausted turns reach the streak logic instead of pausing immediately. Pair that with an independent cap on consecutive budget continuations per goal, so the streak is not the only thing standing between a stuck goal and an infinite retry loop.
