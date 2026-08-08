# Goal Interaction Settlement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task with review checkpoints.

**Goal:** Preserve typed approval semantics while settling live Goal operations exactly once.

**Architecture:** Keep the operation terminal as the source of approval truth. Translate only the
`LegacyApprovalRequired` terminal classification at the RuntimeHost-to-GoalActor boundary, let the
existing GoalActor/GoalStore lifecycle own usage and resume fencing, and project its per-turn usage
deltas cumulatively through one shared cost conversion.

**Tech Stack:** Rust, `orca-runtime` typed runtime surface, SQLite-backed GoalStore, Cargo tests.

---

### Task 1: Add the failing live Goal approval behavior test

**Files:**
- Modify: `crates/orca-runtime/src/runtime_host.rs` in the `#[cfg(test)] mod tests` fixtures and
  surface Goal tests.

- [x] **Step 1: Add a `SurfaceGoalApprovalExecutor` fixture.**

The fixture feeds a provider continuation containing a real `write_file` call and `update_goal` call
through the normal runtime turn path. This preserves provider tool identity, publishes the real typed
approval interaction, and proves side effects with a marker file.

- [x] **Step 2: Add `surface_goal_tool_approval_settles_allow_once`.**

Create a recorded typed Goal with `GoalMutationAction::SetAndRun`, claim the Goal subscription,
respond Allow to the exact `ToolApproval` interaction, wait for the operation terminal, then assert
one terminal operation, one outer turn, exact usage, one tool side effect, and no in-flight run.

- [x] **Step 3: Add `surface_goal_approval_denial_pauses_and_resumes_fresh`.**

Drive the same fixture through Deny. Assert the interaction resolves, the operation terminal is a
`FailureClass::LegacyApprovalRequired`, the Goal outer-turn event/status is typed
`ApprovalRequired`, usage is charged exactly once, the marker is absent, and `in_flight_runs == 0`.
Resume explicitly and assert a new Goal run, operation, and interaction fence; Allow then executes
the tool once and surface/SQLite cumulative usage agrees.

- [x] **Step 4: Run the focused tests to confirm RED.**

Run:

```bash
cargo test -p orca-runtime --lib runtime_host::tests::surface_goal_tool_approval -- --nocapture
cargo test -p orca-runtime --lib runtime_host::tests::surface_goal_approval_denial -- --nocapture
```

Expected: the denial test fails because the current finalization maps
`LegacyApprovalRequired` to `GoalOuterTurnStatus::Failed`.

Observed additional RED evidence: the Allow path found fractional cost micros truncation in durable
Goal usage, and the Deny/resume path found that surface Goal usage retained only the resumed turn
while SQLite accumulated both turns.

### Task 2: Correct the typed approval projection

**Files:**
- Modify: `crates/orca-runtime/src/runtime_host.rs` in `finish_surface_goal` terminal-to-Goal status
  mapping.

- [x] **Step 1: Map only `FailureClass::LegacyApprovalRequired` to
  `GoalOuterTurnStatus::ApprovalRequired`.**

Keep all other failed terminals mapped to `Failed`. Do not change `OperationTerminal`, public enums,
or persistence fields.

- [x] **Step 2: Align usage conversion and cumulative projection.**

Use `cost::usd_to_micros` for both operation and durable Goal usage. Accumulate
`GoalPatch::OuterTurnFinished` deltas into the current surface Goal usage.

- [x] **Step 3: Run the two focused tests and inspect event order.**

Confirm Allow and Deny each have one outer-turn finish and one terminal operation, and Deny has no
continuation admission.

- [x] **Step 4: Run the focused runtime surface/Goal suite.**

```bash
cargo test -p orca-runtime --lib runtime_host::tests::surface_goal
cargo test -p orca-runtime goal_actor::tests --lib
cargo test -p orca-runtime --test runtime_host
```

Observed: Goal surface `10/10`, GoalActor `16/16`, reducer units `7/7`, and runtime-host contract
`66/66` passed.

### Task 3: Document, review, and validate the slice

**Files:**
- Modify: `docs/production-roadmap.md` in the current typed Goal/runtime-surface section.
- Modify: `docs/superpowers/specs/2026-08-08-goal-interaction-settlement.md` with final evidence.
- Modify: `docs/superpowers/plans/2026-08-08-goal-interaction-settlement.md` checkboxes and command
  results.

- [x] **Step 1: Document that approval-required Goal terminalization is typed and durable.**
- [x] **Step 2: Run formatting, diff, lifecycle, and server contract checks.**

```bash
cargo fmt --all -- --check
git diff --check
cargo test --test runtime_lifecycle_contract -- --test-threads=1
cargo test --test session_server_contract -- --test-threads=1
```

Observed before commit: formatting and diff checks passed, runtime lifecycle `54/54` passed, and
server contract `137/137` passed. Existing dead-code warnings remain unchanged.

- [x] **Step 3: Perform code review, stage, and create one semantic commit.**

Review the full diff for lifecycle ownership, cumulative replay safety, real typed interaction use,
and debug residue. Do not merge, push, or release.

- [x] **Step 4: Rebase latest `origin/main` and rerun affected focused/full gates.**

Final evidence:

- `git fetch origin main` kept `origin/main` and the merge base at `445baf596`; rebase was a no-op.
- Both live Goal approval tests and the shared cost-conversion tests passed after rebase.
- Goal surface `10/10`, GoalActor `16/16`, reducer units `7/7`, runtime-host contract `66/66`,
  runtime lifecycle `54/54`, server contract `137/137`, formatting, and diff checks passed.
- `cargo clippy --workspace --all-targets` exited zero with existing repository warnings.
- The unfiltered workspace all-target gate is not green on `origin/main`: session picker commit
  `445baf596` made `list_threads()` create `sessions-index.sqlite3`, while two unchanged stateless
  server tests still assert that listing leaves ORCA_HOME with zero files. The unchanged
  `session_listing_does_not_block_host_supervisor` FIFO test also times out under the new index path
  and fails to reap after panic. All other failures first seen under parallel load passed when rerun
  serially. These baseline session-index test defects are outside this Goal settlement slice and are
  not hidden by a speculative compatibility change.
