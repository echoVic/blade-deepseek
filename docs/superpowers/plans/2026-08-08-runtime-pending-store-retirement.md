# Runtime Pending Store Retirement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove process-local pending-interaction state from runtime-host Goal admission while preserving a source-compatible compatibility builder.

**Architecture:** Keep the public store and builder for one migration window, but make the builder a documented no-op and remove the store from `HostedTurnRequest` state. Legacy Goal admission will no longer read the map or carry an unreachable pending flag; surface-owned typed state remains the runtime source of truth, while the public reject enum stays source-compatible. Add a Rust deprecation attribute only in a separately versioned API migration because it is a semver minor change.

**Tech Stack:** Rust workspace, `orca-runtime` integration tests, Markdown architecture docs, cargo fmt, cargo test, cargo-semver-checks, configured DeepSeek smoke harness.

---

### Task 1: Specify the ownership and compatibility gate

**Files:**
- Create: `docs/superpowers/specs/2026-08-08-runtime-pending-store-retirement.md`
- Create: `docs/superpowers/plans/2026-08-08-runtime-pending-store-retirement.md`

- [x] **Step 1: Record evidence, user value, boundaries, acceptance, and rollback.**

- [x] **Step 2: Check the plan against the spec.**

The spec's public compatibility, no-production-read, behavior, validation, and
deletion-gate requirements each map to Tasks 2-6 below.

### Task 2: Establish the behavior regression before implementation

**Files:**
- Modify: `crates/orca-runtime/tests/runtime_host.rs:4373-4438`

- [x] **Step 1: Change the existing legacy Goal pending-store test to use `token_budget: Some(10)` and assert `budget_limited`, not `pending_interaction`.**

The test still inserts a real `RuntimePendingInteractionRecord` and calls the
public builder. With the old implementation, the recorded admission reason is
`pending_interaction`, so this command must fail before production changes:

```bash
cargo test -p orca-runtime --test runtime_host legacy_goal_pending_store_does_not_block_continuation -- --exact --nocapture
```

Expected RED: the assertion looking for `budget_limited` fails because the old
runtime reads the injected map.

### Task 3: Remove runtime-host ownership while preserving source compatibility

**Files:**
- Modify: `crates/orca-runtime/src/runtime_host.rs:77,404,566,732-738,35097-35110`

- [x] **Step 1: Remove the `HostedTurnRequest.pending_interactions` field and its constructor initialization.**

- [x] **Step 2: Keep `with_pending_interactions` with the old public signature, document it as a compatibility no-op, and return `self` without storing the argument.**

```rust
/// Compatibility no-op. Pending interactions are runtime-surface owned.
pub fn with_pending_interactions(
    self,
    _pending_interactions: RuntimePendingInteractionStore,
) -> Self {
    self
}
```

- [x] **Step 3: Remove the runtime-host import, legacy Goal read, and private preflight pending field/branch.**

Leave public `GoalContinuationRejectCode::PendingInteraction` intact for
external Rust compatibility.

### Task 4: Update tests and current architecture documentation

**Files:**
- Modify: `crates/orca-runtime/tests/runtime_host.rs:45,4373-4438`
- Modify: `docs/production-roadmap.md:34-40,1160-1178,2160-2175`

- [x] **Step 1: Keep the store import only where the compatibility regression constructs it.**

- [x] **Step 2: State in the roadmap that the store remains a source-compatible projection, is not a runtime owner, and is blocked from deletion until legacy Goal/server/CLI migration and durable broker recovery gates pass.**

### Task 5: Verify the slice

**Files:**
- Test: `crates/orca-runtime` runtime-host and surface lifecycle suites
- Test: Rust API gate and workspace checks

- [x] **Step 1: Run focused tests, formatter, and diff checks.**

```bash
cargo test -p orca-runtime --test runtime_host legacy_goal_pending_store_does_not_block_continuation -- --exact --nocapture
cargo test -p orca-runtime --test runtime_host
cargo test -p orca-runtime runtime_surface --lib
cargo test -p orca-runtime runtime_pending_interaction --lib
cargo fmt --all -- --check
git diff --check
```

- [x] **Step 2: Run the workspace lifecycle/full gate required by a runtime ownership change.**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

The focused server/lifecycle contracts and real smoke pass. The default
workspace gate was also attempted but is blocked by an existing FIFO supervisor
test under concurrent and serial execution; strict Clippy reports 17 existing
`orca-core` lints before reaching this slice's files.

- [x] **Step 3: Run the Rust API compatibility gate against the refreshed `origin/main` baseline and the configured real API lifecycle smoke where available.**

```bash
## The current branch predates the Node gate script; the equivalent raw gate uses
## cargo-semver-checks 0.49.0 over a temporary HEAD-plus-working-diff snapshot.
/Users/qingyun/Documents/GitHub/blade-deepseek/.worktrees/tui-legacy-interaction-rail/target/cargo-tools/bin/cargo-semver-checks check-release \
  --manifest-path /tmp/orca-rust-api-gate-current.kF7Ruv/current/Cargo.toml \
  -p orca-core --baseline-root /tmp/orca-rust-api-gate-current.kF7Ruv/baseline \
  --release-type patch --color never
/Users/qingyun/Documents/GitHub/blade-deepseek/.worktrees/tui-legacy-interaction-rail/target/cargo-tools/bin/cargo-semver-checks check-release \
  --manifest-path /tmp/orca-rust-api-gate-current.kF7Ruv/current/Cargo.toml \
  -p orca-runtime --baseline-root /tmp/orca-rust-api-gate-current.kF7Ruv/baseline \
  --release-type patch --color never
/Users/qingyun/Documents/GitHub/blade-deepseek/.worktrees/tui-legacy-interaction-rail/target/cargo-tools/bin/cargo-semver-checks check-release \
  --manifest-path /tmp/orca-rust-api-gate-current.kF7Ruv/current/Cargo.toml \
  -p orca-tui --baseline-root /tmp/orca-rust-api-gate-current.kF7Ruv/baseline \
  --release-type patch --color never

node scripts/release/test-real-api-tui-approval-recovery.mjs
node scripts/release/real-api-tui-approval-recovery.mjs \
  --bin /absolute/path/to/target/debug/orca --timeout-ms 240000
```

### Task 6: Review, rebase, and commit

**Files:**
- Commit the files changed by Tasks 2-5 only.

- [x] **Step 1: Review the diff for two pending-interaction sources, accidental API deletion, and source-shape-only assertions.**

- [x] **Step 2: Fetch and rebase onto the newest `origin/main`; rerun focused tests and API checks if the baseline moved.**

`origin/main` remained at `445baf596`; rebase was a no-op with the worktree
changes restored by autostash. The affected runtime-host suite was rerun 66/66
after the rebase.

- [x] **Step 3: Commit one independently revertible semantic slice.**

```bash
git add crates/orca-runtime/src/runtime_host.rs \
  crates/orca-runtime/tests/runtime_host.rs \
  docs/production-roadmap.md \
  docs/superpowers/specs/2026-08-08-runtime-pending-store-retirement.md \
  docs/superpowers/plans/2026-08-08-runtime-pending-store-retirement.md
git commit -m "refactor(runtime): retire pending store from goal admission"
```

After the commit, leave the worktree and branch intact and pause the current
goal as requested; do not merge, push, publish, or delete any worktree/backup.

Commit created: `506f67768 refactor(runtime): retire pending store from goal admission`.
