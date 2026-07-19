# Goal Remove Fixed Outer-Turn Limit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the fixed 64-outer-turn GoalRun stop so continuation count is observability data only.

**Architecture:** Keep the existing runtime-owned continuation admission gate, but remove outer-turn count from its input and rejection vocabulary. Preserve persisted continuation counters and events for diagnostics while leaving all semantic state, ownership, budget, and structured no-progress checks unchanged.

**Tech Stack:** Rust workspace, Cargo tests and Clippy, Markdown runtime contracts.

---

### Task 1: Remove Turn Count From Continuation Admission

**Files:**
- Modify: `crates/orca-runtime/src/runtime_host.rs:54,270-352,3007-3047,3159-3193,4033-4047,4137-4187`
- Test: `crates/orca-runtime/src/runtime_host.rs:4137-4187`

- [ ] **Step 1: Write the failing structural regression test**

Rename the existing preflight matrix test and remove `continuation_count` from
its baseline. The desired input type has no turn-count field:

```rust
#[test]
fn goal_continuation_preflight_has_no_outer_turn_limit() {
    let baseline = GoalContinuationPreflight {
        cancelled: false,
        successful_turn: true,
        queued_user_input: false,
        pending_interaction: false,
        active_workflow: false,
        plan_mode: false,
        duplicate_admission: false,
    };
    let cases = [
        (
            GoalContinuationPreflight {
                queued_user_input: true,
                ..baseline
            },
            GoalContinuationRejectCode::QueuedUserInput,
        ),
        (
            GoalContinuationPreflight {
                pending_interaction: true,
                ..baseline
            },
            GoalContinuationRejectCode::PendingInteraction,
        ),
        (
            GoalContinuationPreflight {
                plan_mode: true,
                ..baseline
            },
            GoalContinuationRejectCode::PlanMode,
        ),
        (
            GoalContinuationPreflight {
                duplicate_admission: true,
                ..baseline
            },
            GoalContinuationRejectCode::DuplicateAdmission,
        ),
    ];

    for (input, expected) in cases {
        assert!(matches!(
            goal_continuation_preflight(input),
            Some(GoalContinuationAdmission::Reject { code, .. }) if code == expected
        ));
    }
    assert_eq!(goal_continuation_preflight(baseline), None);
}
```

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
cargo test -p orca-runtime runtime_host::tests::goal_continuation_preflight_has_no_outer_turn_limit --lib -- --exact
```

Expected: compilation fails because current `GoalContinuationPreflight`
requires `continuation_count`.

- [ ] **Step 3: Remove the fixed-limit production path**

Delete:

```rust
const MAX_GOAL_OUTER_TURNS_PER_RUN: u64 = 64;
```

Remove `OuterTurnLimit` from `GoalContinuationRejectCode`, remove
`continuation_count` from `GoalContinuationPreflight`, and delete this branch:

```rust
if input.continuation_count >= MAX_GOAL_OUTER_TURNS_PER_RUN {
    return Some(reject(
        GoalContinuationRejectCode::OuterTurnLimit,
        "goal continuation reached the outer-turn safety limit",
    ));
}
```

At the call site, remove only the admission-local count calculation and field:

```rust
let continuation_count = active
    .generation
    .context
    .fence()
    .generation_id()
    .as_u64()
    .saturating_add(1);
```

Do not remove `GoalRunSnapshot::continuation_count` or the count passed to
`goal.continuation.admitted/rejected` events.

Remove the obsolete pause mapping:

```rust
GoalContinuationRejectCode::OuterTurnLimit => {
    orca_core::goal_runtime::GoalPauseReason::NoProgress
}
```

Remove the obsolete event-name mapping:

```rust
GoalContinuationRejectCode::OuterTurnLimit => "outer_turn_limit",
```

- [ ] **Step 4: Run the focused test and verify GREEN**

Run:

```bash
cargo test -p orca-runtime runtime_host::tests::goal_continuation_preflight_has_no_outer_turn_limit --lib -- --exact
```

Expected: one test passes.

- [ ] **Step 5: Run the runtime Goal regression set**

Run:

```bash
cargo test -p orca-runtime goal --lib -- --test-threads=1
cargo test -p orca-runtime --test runtime_host goal -- --test-threads=1
```

Expected: all matching tests pass and no admission rejection matrix regresses.

- [ ] **Step 6: Commit the runtime change**

```bash
git add crates/orca-runtime/src/runtime_host.rs
git commit -m "fix(goal): remove fixed outer-turn limit"
```

### Task 2: Align Goal Contracts

**Files:**
- Modify: `docs/goal-mode.md:154-178`
- Modify: `docs/harness-contract.md:224-237`

- [ ] **Step 1: Remove the fixed limit from the Goal contract**

Delete the 64-turn admission condition from `docs/goal-mode.md`. Replace the
final paragraph with:

```markdown
The primary no-progress rule is three closed, successful outer turns with the
same normalized model-fixable gap. Token deltas and continuation counts are
accounting and observability data, not proof of progress and not stopping
conditions.
```

- [ ] **Step 2: Remove the fixed limit from the harness contract**

Change the continuation rejection sentence to:

```markdown
- Continuation is rejected for queued user input, cancellation, pending interaction, active workflow ownership, plan mode, duplicate generation fences, inactive state, or exhausted budget.
```

- [ ] **Step 3: Verify no live contract or code retains the limit**

Run:

```bash
rg -n "MAX_GOAL_OUTER_TURNS_PER_RUN|OuterTurnLimit|64-outer-turn|64-turn limit|outer-turn safety limit" crates docs/goal-mode.md docs/harness-contract.md README.md
```

Expected: no matches. Historical design, plan, and incident documents are not
part of this live-contract search and are not rewritten.

- [ ] **Step 4: Commit the contract update**

```bash
git add docs/goal-mode.md docs/harness-contract.md
git commit -m "docs(goal): drop fixed continuation ceiling"
```

### Task 3: Verify The Workspace

**Files:**
- Verify only; no planned source changes.

- [ ] **Step 1: Check formatting and whitespace**

```bash
cargo fmt --all --check
git diff --check
```

Expected: both commands exit successfully.

- [ ] **Step 2: Run Goal-facing TUI tests**

```bash
cargo test -p orca-tui goal --lib -- --test-threads=1
```

Expected: all matching tests pass.

- [ ] **Step 3: Run the full workspace suite**

```bash
cargo test --workspace --all-targets -- --test-threads=1
```

Expected: all workspace tests pass.

- [ ] **Step 4: Run Clippy**

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: no warnings or errors.

- [ ] **Step 5: Audit the final diff and history**

```bash
git status --short --branch
git log -4 --oneline
git show --stat --oneline HEAD~2..HEAD
```

Expected: only the approved runtime, contract, spec, and plan changes are
present; the branch is ahead of `origin/main` by the intentional commits.
