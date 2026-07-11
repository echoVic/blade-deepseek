# Goal Cumulative Timer Design

## Summary

The TUI activity line must show cumulative elapsed time for an active persistent
Goal instead of restarting from zero after every automatic continuation. The
display uses the persisted Goal total as its base and adds the live elapsed time
for the current agent turn. Ordinary non-Goal turns keep their existing
per-turn timer.

Restoring a Goal must also preserve its accumulated time, token usage, creation
time, objective, and budget. Resuming the same history session changes only the
Goal status and update timestamp. If restoration produces a different session
id, the store migrates the complete Goal record atomically.

## Observed Failure

Goal timing currently has two independent sources:

- `ThreadGoal.time_used_seconds` is persisted in `goals_1.json` and incremented
  after each Goal turn.
- `AppState.running_started_at` is a TUI-only `Instant` used by the bottom
  `running` activity line.

Every successful turn emits `SessionCompleted`. The TUI handles that event by
entering `Idle`, which clears `running_started_at`. The next automatic Goal
continuation emits `TurnStarted`, which creates a new `Instant`. The persisted
Goal total remains correct, but the visible activity line restarts at zero.

The restored-Goal path has a second reset. It calls `GoalStore::replace` even
when the restored history keeps the same session id. `replace` intentionally
creates a new Goal record, so it zeros `tokens_used` and `time_used_seconds` and
assigns a new `created_at` timestamp.

## User-Facing Behavior

While an active Goal turn is running, the activity line displays:

```text
persisted Goal time + current turn elapsed time
```

For example, a Goal with 13 minutes already persisted shows `running 13m 10s`
ten seconds into its next continuation. Completing that turn and starting
another continuation must never make the displayed value decrease.

The timer counts the existing Goal accounting contract: wall-clock seconds
inside each agent turn. Time between turns, time while a Goal is paused, and
time while Orca is not running are not counted. Approval and structured-input
waits inside a running turn remain included in this patch because changing that
contract requires runtime-level interaction timing.

Ordinary sessions without an active Goal continue to show elapsed time for only
the current turn. A cleared, blocked, complete, budget-limited, usage-limited,
or paused Goal does not contribute a cumulative base to a later ordinary turn.

## Chosen Approach

`ThreadGoal.time_used_seconds` remains the persisted source of truth. No new
timestamp or persisted timer field is introduced.

When `AppStatus::Running` and `AppState.current_goal` contains an active Goal,
the activity renderer computes:

```text
max(goal.time_used_seconds, 0) + running_started_at.elapsed().as_secs()
```

The sum is saturating. The existing Goal loop accounts the completed turn and
sends `GoalStatus` before the next `TurnStarted`, so `current_goal` receives the
new persisted base before the next live delta starts. The intermediate
`SessionCompleted` may still finalize the transcript and clear the per-turn
`Instant`; continuity comes from the persisted base rather than from retaining
one process-local `Instant` across turns.

This approach is preferred over keeping `running_started_at` alive across Goal
continuations because it survives session restoration, matches `/goal` usage
summaries, and does not make transcript lifecycle events responsible for Goal
identity.

Using `created_at` as the timer origin is rejected because it would count
paused time and time while Orca is not running.

## TUI State And Rendering

The change stays inside the existing `AppState` contract:

- `current_goal` supplies the persisted base.
- `running_started_at` supplies the current-turn live delta.
- `activity_line` selects cumulative Goal rendering only for an active Goal.
- existing `GoalStatus` and `GoalUpdated` events refresh `current_goal`.
- existing `SessionCompleted` and `TurnStarted` behavior remains unchanged.

No new user-visible mode, status enum, event variant, or background timer is
needed. Frame ticks already redraw the activity line while a turn is running.

## Restoration And Persistence

Add a focused store operation:

```rust
GoalStore::resume_into(
    source_session_id: &str,
    resumed_session_id: &str,
) -> io::Result<Option<ThreadGoal>>
```

It restores an existing Goal into the session created or retained by history
restoration and performs one load/save transaction.

For the same session id it:

- preserves `objective`, `token_budget`, `tokens_used`, `time_used_seconds`, and
  `created_at`;
- sets `status` to `Active`;
- refreshes `updated_at`.

For a different resumed session id it:

- clones the complete source Goal record under the resumed session id;
- preserves all usage fields and `created_at`;
- sets the cloned record to `Active` and refreshes `updated_at`;
- marks the source record `Paused` in the same atomic save.

The TUI restoration path uses this operation instead of `replace`. Creating a
brand-new Goal continues to use `replace` and therefore still starts with zero
usage.

## Error Handling

- A missing source Goal returns `None`; the TUI reports that no resumable Goal
  exists and does not install a partially restored session.
- Store read, serialization, or atomic rename failures use the existing I/O
  error path and leave the prior database file authoritative.
- Negative persisted seconds from legacy or manually edited data render as
  zero before adding the live delta.
- Integer addition saturates rather than wrapping.
- A process crash can still lose the unfinished current-turn delta. This is the
  existing best-effort accounting contract and is outside this patch.

## Testing

TUI tests must cover:

1. an active Goal with a persisted base renders the base plus the live turn
   delta;
2. a `SessionCompleted` / updated `GoalStatus` / next `TurnStarted` sequence
   never decreases the displayed elapsed time;
3. a non-Goal turn retains the existing per-turn activity timer;
4. paused and terminal Goals do not supply a cumulative base to ordinary turns.

Goal store tests must cover:

1. same-session restoration preserves time, tokens, budget, objective, and
   `created_at` while reactivating the Goal;
2. different-session restoration migrates the full record and pauses the
   source record;
3. creating a new Goal still resets usage to zero;
4. a missing source does not create a target record.

Focused TUI and runtime tests run before the broader workspace verification.

## Compatibility And Scope

The persisted `ThreadGoal` JSON shape, TUI event protocol, slash commands,
provider behavior, and Goal continuation policy remain unchanged. Existing
`goals_1.json` files require no migration.

This patch does not:

- exclude approval or structured-input wait time from an in-progress turn;
- persist partial time while a turn is still running;
- add separate Goal and turn timers to the UI;
- change token accounting or budget enforcement;
- redesign Goal ownership around the future Runtime Operation Host.

## Acceptance Criteria

1. The activity line cannot return to `running 0s` merely because an active
   Goal entered its next automatic continuation.
2. Displayed Goal elapsed time equals persisted completed-turn time plus the
   current turn delta.
3. `/goal resume` preserves all prior Goal usage and creation metadata.
4. Non-Goal timer behavior remains unchanged.
5. Focused tests, the affected crate suites, formatting, and repository diff
   checks pass before completion.
