# Goal Cumulative Timer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the TUI Goal timer accumulate across automatic continuations and preserve all elapsed/token metadata when a Goal session is restored.

**Architecture:** Keep `ThreadGoal.time_used_seconds` as the persisted completed-turn source of truth and add the current turn's `running_started_at` delta only while rendering an active Goal. Add one atomic `GoalStore::resume_into` operation so same-session resume reactivates in place and different-session restoration migrates the full record without clearing usage.

**Tech Stack:** Rust 2024, ratatui TUI state/rendering, JSON-backed `GoalStore`, Cargo unit tests.

---

## File Map

- `crates/orca-tui/src/ui.rs`: compose persisted Goal time with the live turn delta and retain ordinary per-turn rendering.
- `crates/orca-runtime/src/goals.rs`: restore a Goal atomically while preserving usage and creation metadata.
- `crates/orca-tui/src/app.rs`: replace restored-Goal `replace` calls with `resume_into` and strengthen the existing end-to-end TUI resume test.
- `docs/superpowers/specs/2026-07-12-goal-cumulative-timer-design.md`: approved behavior and scope; no implementation edits required.

### Task 1: Render Cumulative Goal Time

**Files:**
- Modify: `crates/orca-tui/src/ui.rs:2002-2020`
- Test: `crates/orca-tui/src/ui.rs:2911-3345`

- [ ] **Step 1: Add a test helper and failing cumulative timer test**

In the `ui.rs` test module, add the Goal imports beside the existing config import:

```rust
use orca_core::goal_types::{ThreadGoal, ThreadGoalStatus};
```

Add this helper after `test_state`:

```rust
fn goal_with_elapsed(status: ThreadGoalStatus, time_used_seconds: i64) -> ThreadGoal {
    ThreadGoal {
        session_id: "goal-session".to_string(),
        objective: "finish the migration".to_string(),
        status,
        token_budget: None,
        tokens_used: 42,
        time_used_seconds,
        created_at: 1,
        updated_at: 2,
    }
}
```

Add this test next to `running_activity_line_shows_elapsed_time`:

```rust
#[test]
fn active_goal_activity_line_adds_persisted_and_live_elapsed_time() {
    let mut state = test_state();
    let theme = Theme::named(orca_core::config::ThemeName::Dark);
    state.status = AppStatus::Running;
    state.current_goal = Some(goal_with_elapsed(ThreadGoalStatus::Active, 13 * 60));
    state.running_started_at = Some(Instant::now() - Duration::from_secs(10));

    let (text, color) = activity_line(&state, &theme).expect("running shows an activity line");

    assert_eq!(text, "● running 13m 10s");
    assert_eq!(color, theme.warning);
}
```

- [ ] **Step 2: Run the new test and verify RED**

Run:

```bash
cargo test -p orca-tui active_goal_activity_line_adds_persisted_and_live_elapsed_time -- --nocapture
```

Expected: FAIL because the current renderer returns `● running 10s` and ignores `current_goal.time_used_seconds`.

- [ ] **Step 3: Add failing continuation and inactive-Goal coverage**

Add these tests beside the first timer test:

```rust
#[test]
fn active_goal_activity_line_never_decreases_across_continuations() {
    let mut state = test_state();
    let theme = Theme::named(orca_core::config::ThemeName::Dark);
    state.status = AppStatus::Running;
    state.current_goal = Some(goal_with_elapsed(ThreadGoalStatus::Active, 13 * 60));
    state.running_started_at = Some(Instant::now() - Duration::from_secs(10));
    let first = activity_line(&state, &theme).unwrap().0;

    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });
    state.update(TuiEvent::GoalStatus(Some(goal_with_elapsed(
        ThreadGoalStatus::Active,
        13 * 60 + 20,
    ))));
    state.update(TuiEvent::TurnStarted {
        turn: 2,
        task: None,
    });
    state.running_started_at = Some(Instant::now() - Duration::from_secs(5));
    let second = activity_line(&state, &theme).unwrap().0;

    assert_eq!(first, "● running 13m 10s");
    assert_eq!(second, "● running 13m 25s");
}

#[test]
fn inactive_goal_does_not_change_the_current_turn_timer() {
    let theme = Theme::named(orca_core::config::ThemeName::Dark);
    for status in [
        ThreadGoalStatus::Paused,
        ThreadGoalStatus::Blocked,
        ThreadGoalStatus::UsageLimited,
        ThreadGoalStatus::BudgetLimited,
        ThreadGoalStatus::Complete,
    ] {
        let mut state = test_state();
        state.status = AppStatus::Running;
        state.current_goal = Some(goal_with_elapsed(status, 13 * 60));
        state.running_started_at = Some(Instant::now() - Duration::from_secs(10));

        assert_eq!(activity_line(&state, &theme).unwrap().0, "● running 10s");
    }
}

#[test]
fn active_goal_activity_line_clamps_negative_persisted_time() {
    let mut state = test_state();
    let theme = Theme::named(orca_core::config::ThemeName::Dark);
    state.status = AppStatus::Running;
    state.current_goal = Some(goal_with_elapsed(ThreadGoalStatus::Active, -20));
    state.running_started_at = Some(Instant::now() - Duration::from_secs(10));

    assert_eq!(activity_line(&state, &theme).unwrap().0, "● running 10s");
}
```

- [ ] **Step 4: Run the timer tests and verify RED is specific**

Run:

```bash
cargo test -p orca-tui goal_activity_line -- --nocapture
```

Expected: the active-Goal tests FAIL because the persisted base is missing; the inactive-Goal test PASSes because existing per-turn behavior is unchanged.

- [ ] **Step 5: Implement cumulative Goal rendering**

Replace the `AppStatus::Running` branch of `activity_line` with:

```rust
AppStatus::Running => {
    let live_elapsed = state
        .running_started_at
        .map(|started| started.elapsed().as_secs())
        .unwrap_or_default();
    let persisted_goal_elapsed = state
        .current_goal
        .as_ref()
        .filter(|goal| goal.status.should_continue())
        .map(|goal| goal.time_used_seconds.max(0) as u64)
        .unwrap_or_default();
    let elapsed = format_elapsed_compact(
        persisted_goal_elapsed.saturating_add(live_elapsed),
    );
    Some((format!("● running {elapsed}"), theme.warning))
}
```

- [ ] **Step 6: Run focused and full TUI tests**

Run:

```bash
cargo test -p orca-tui goal_activity_line -- --nocapture
cargo test -p orca-tui running_activity_line_shows_elapsed_time -- --nocapture
cargo test -p orca-tui -- --test-threads=1
```

Expected: all commands PASS; the existing non-Goal test still reports `● running 1m 05s`.

- [ ] **Step 7: Commit the rendering slice**

```bash
git add crates/orca-tui/src/ui.rs
git commit -m "fix(tui): show cumulative goal elapsed time"
```

### Task 2: Preserve Goal Metadata During Store Resume

**Files:**
- Modify: `crates/orca-runtime/src/goals.rs:137-271`
- Test: `crates/orca-runtime/src/goals.rs:296-410`

- [ ] **Step 1: Add failing same-session restoration coverage**

Add this test after `store_updates_goal_and_accounts_usage`:

```rust
#[test]
fn resume_into_same_session_preserves_usage_and_creation_metadata() {
    let dir = tempdir().unwrap();
    let mut store = GoalStore::with_path(dir.path().join("goals_1.json"));
    let created = store
        .replace(
            "session-1",
            "resume safely",
            ThreadGoalStatus::Paused,
            Some(50_000),
        )
        .unwrap();
    let accounted = store
        .account_usage("session-1", 12_345, 13 * 60)
        .unwrap()
        .unwrap();

    let resumed = store
        .resume_into("session-1", "session-1")
        .unwrap()
        .expect("source goal exists");

    assert_eq!(resumed.session_id, "session-1");
    assert_eq!(resumed.status, ThreadGoalStatus::Active);
    assert_eq!(resumed.objective, "resume safely");
    assert_eq!(resumed.token_budget, Some(50_000));
    assert_eq!(resumed.tokens_used, 12_345);
    assert_eq!(resumed.time_used_seconds, 13 * 60);
    assert_eq!(resumed.created_at, created.created_at);
    assert_eq!(resumed.created_at, accounted.created_at);
}
```

- [ ] **Step 2: Run the test and verify RED**

Run:

```bash
cargo test -p orca-runtime resume_into_same_session_preserves_usage_and_creation_metadata -- --nocapture
```

Expected: compilation FAILs because `GoalStore::resume_into` does not exist.

- [ ] **Step 3: Add migration and missing-source coverage**

Add these tests beside the same-session test:

```rust
#[test]
fn resume_into_new_session_migrates_full_record_and_pauses_source() {
    let dir = tempdir().unwrap();
    let mut store = GoalStore::with_path(dir.path().join("goals_1.json"));
    let created = store
        .replace(
            "session-old",
            "migrate safely",
            ThreadGoalStatus::Active,
            Some(99_000),
        )
        .unwrap();
    store
        .account_usage("session-old", 45_678, 321)
        .unwrap()
        .unwrap();

    let resumed = store
        .resume_into("session-old", "session-new")
        .unwrap()
        .expect("source goal exists");

    assert_eq!(resumed.session_id, "session-new");
    assert_eq!(resumed.status, ThreadGoalStatus::Active);
    assert_eq!(resumed.objective, "migrate safely");
    assert_eq!(resumed.token_budget, Some(99_000));
    assert_eq!(resumed.tokens_used, 45_678);
    assert_eq!(resumed.time_used_seconds, 321);
    assert_eq!(resumed.created_at, created.created_at);
    assert_eq!(
        store.get("session-old").unwrap().unwrap().status,
        ThreadGoalStatus::Paused
    );
}

#[test]
fn resume_into_missing_source_does_not_create_target() {
    let dir = tempdir().unwrap();
    let mut store = GoalStore::with_path(dir.path().join("goals_1.json"));

    assert!(store.resume_into("missing", "target").unwrap().is_none());
    assert!(store.get("target").unwrap().is_none());
}

#[test]
fn replace_still_starts_a_new_goal_with_zero_usage() {
    let dir = tempdir().unwrap();
    let mut store = GoalStore::with_path(dir.path().join("goals_1.json"));
    store
        .replace("session-1", "first run", ThreadGoalStatus::Active, None)
        .unwrap();
    store
        .account_usage("session-1", 500, 60)
        .unwrap()
        .unwrap();

    let replacement = store
        .replace("session-1", "new run", ThreadGoalStatus::Active, None)
        .unwrap();

    assert_eq!(replacement.tokens_used, 0);
    assert_eq!(replacement.time_used_seconds, 0);
}
```

- [ ] **Step 4: Implement `GoalStore::resume_into`**

Add this method after `latest_active` and before `replace`:

```rust
pub fn resume_into(
    &mut self,
    source_session_id: &str,
    resumed_session_id: &str,
) -> io::Result<Option<ThreadGoal>> {
    let mut db = self.load()?;
    let Some(source) = db.goals.get(source_session_id).cloned() else {
        return Ok(None);
    };
    let now = now_timestamp();
    let mut resumed = source;
    resumed.session_id = resumed_session_id.to_string();
    resumed.status = ThreadGoalStatus::Active;
    resumed.updated_at = now;

    if source_session_id != resumed_session_id
        && let Some(source) = db.goals.get_mut(source_session_id)
    {
        source.status = ThreadGoalStatus::Paused;
        source.updated_at = now;
    }
    db.goals
        .insert(resumed_session_id.to_string(), resumed.clone());
    self.save(&db)?;
    Ok(Some(resumed))
}
```

- [ ] **Step 5: Run GoalStore tests**

Run:

```bash
cargo test -p orca-runtime resume_into -- --nocapture
cargo test -p orca-runtime goals::tests -- --test-threads=1
```

Expected: all `resume_into` and existing GoalStore tests PASS. Existing `replace` tests continue proving that a brand-new Goal starts with zero usage.

- [ ] **Step 6: Commit the store slice**

```bash
git add crates/orca-runtime/src/goals.rs
git commit -m "fix(runtime): preserve goal usage during resume"
```

### Task 3: Use Atomic Goal Restoration In The TUI

**Files:**
- Modify: `crates/orca-tui/src/app.rs:1155-1223`
- Modify: `crates/orca-tui/src/app.rs:2985-3104`

- [ ] **Step 1: Strengthen the existing TUI resume test**

In `empty_recorded_agent_loop_goal_resume_restores_latest_active_goal`, replace the initial `replace` call with:

```rust
let mut goal_store = orca_runtime::goals::GoalStore::load_default();
goal_store
    .replace(
        &old_session_id,
        "resume me",
        orca_core::goal_types::ThreadGoalStatus::Active,
        Some(80_000),
    )
    .unwrap();
let original = goal_store
    .account_usage(&old_session_id, 23_456, 13 * 60)
    .unwrap()
    .unwrap();
assert_eq!(original.token_budget, Some(80_000));
```

Inside the `TuiEvent::GoalUpdated(goal)` match arm, add these assertions before moving `goal.session_id`:

```rust
assert_eq!(goal.token_budget, Some(80_000));
assert_eq!(goal.tokens_used, 23_456);
assert_eq!(goal.time_used_seconds, 13 * 60);
assert_eq!(goal.created_at, original.created_at);
```

After loading the persisted goal at the end of the test, replace the status-only assertion with:

```rust
let persisted = store.get(&resumed_session_id).unwrap().unwrap();
assert_eq!(persisted.status, orca_core::goal_types::ThreadGoalStatus::Active);
assert_eq!(persisted.token_budget, Some(80_000));
assert_eq!(persisted.tokens_used, 23_456);
assert_eq!(persisted.time_used_seconds, 13 * 60);
assert_eq!(persisted.created_at, original.created_at);
```

- [ ] **Step 2: Run the TUI resume test and verify RED**

Run:

```bash
cargo test -p orca-tui empty_recorded_agent_loop_goal_resume_restores_latest_active_goal -- --nocapture
```

Expected: FAIL because the current `store.replace` call returns and persists zero tokens, zero elapsed seconds, and a replacement `created_at`.

- [ ] **Step 3: Replace the reset-prone restoration block**

Delete the `new_session_id != goal.session_id` pause block and the following `store.replace` call. Replace both with:

```rust
let active_goal = match store.resume_into(&goal.session_id, &new_session_id) {
    Ok(Some(goal)) => goal,
    Ok(None) => {
        let _ = event_tx.send(TuiEvent::Error(
            "goal disappeared while restoring its session".to_string(),
        ));
        return;
    }
    Err(error) => {
        let _ = event_tx.send(TuiEvent::Error(format!(
            "failed to resume goal in restored session: {error}"
        )));
        return;
    }
};
```

- [ ] **Step 4: Run focused TUI restoration and Goal tests**

Run:

```bash
cargo test -p orca-tui empty_recorded_agent_loop_goal_resume_restores_latest_active_goal -- --nocapture
cargo test -p orca-tui goal_resume -- --test-threads=1
cargo test -p orca-runtime resume_into -- --nocapture
```

Expected: all commands PASS and the existing same-session id assertion remains valid.

- [ ] **Step 5: Commit the TUI restoration slice**

```bash
git add crates/orca-tui/src/app.rs
git commit -m "fix(tui): restore goals without resetting usage"
```

### Task 4: Final Verification

**Files:**
- Verify: `crates/orca-tui/src/ui.rs`
- Verify: `crates/orca-runtime/src/goals.rs`
- Verify: `crates/orca-tui/src/app.rs`
- Verify: `docs/superpowers/specs/2026-07-12-goal-cumulative-timer-design.md`

- [ ] **Step 1: Format the Rust changes**

Run:

```bash
cargo fmt --all
cargo fmt --all -- --check
```

Expected: formatting completes and the check exits with status 0.

- [ ] **Step 2: Run affected crate suites**

Run:

```bash
cargo test -p orca-runtime -- --test-threads=1
cargo test -p orca-tui -- --test-threads=1
```

Expected: both crate suites PASS with zero failed tests.

- [ ] **Step 3: Run workspace lint and regression gates**

Run:

```bash
cargo clippy --workspace --all-targets
cargo test --workspace --all-targets -- --test-threads=1
git diff --check
```

Expected: every command exits with status 0. Existing non-denying Clippy warnings may remain, but no new error is accepted.

- [ ] **Step 4: Inspect the final history and worktree**

Run:

```bash
git status --short --branch
git log --oneline --decorate -8
```

Expected: only intentional plan-tracking edits, if any, remain; implementation files are committed in the three focused commits above.
