# TUI Single-Surface Interaction Rail Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Remove the process-local legacy TUI interaction rail so all TUI ordinary turns and interaction tests use the runtime-owned typed surface.

**Architecture:** Keep `TuiSurfaceTaskControl` as the sole TUI presentation/control owner. Delete the local broker, generation waiters, four legacy adapters, and legacy turn runner; keep runtime-host's unrelated pending-store compatibility dependency explicitly outside this slice. Preserve typed surface admission, response routing, recovery, and supervised presentation joins.

**Tech Stack:** Rust workspace, Tokio/crossbeam channels, `cargo test`, `cargo clippy`, typed `RuntimeSurface` protocol, Markdown architecture docs.

---

### Task 1: Freeze the removal boundary with a failing behavior gate

**Files:**
- Modify: `crates/orca-tui/src/surface_boundary_tests.rs`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`

- [x] **Step 1: Add the expected single-rail manifest entries and a behavior-facing inventory assertion.**

Keep the existing action classification test. Remove the eight legacy entrypoint identifiers from `TUI_ENTRYPOINTS`; the remaining manifest entries already point at the typed surface command and projection routes. The exact inventory assertion must fail before the implementation because the current manifest still contains the legacy identifiers.

- [x] **Step 2: Run the focused boundary test and confirm the expected RED failure.**

Run:

```bash
cargo test -p orca-tui runtime_surface_contract -- --nocapture
```

Expected: failure naming at least one removed legacy identifier from the current manifest, not a compile or fixture parse error.

- [x] **Step 3: Keep the RED changes unstaged until the complete slice is ready.**

The project rule requires one semantic commit for this vertical slice, so do not create a micro-commit for the RED checkpoint. Record the failing command output in the implementation notes and continue with the same worktree.

### Task 2: Delete the local interaction implementation and simplify ownership

**Files:**
- Delete: `crates/orca-tui/src/interaction_broker.rs`
- Delete: `crates/orca-tui/src/runtime_interaction_adapter.rs`
- Delete: `crates/orca-tui/src/runtime_interaction_adapter_tests.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/operation_controller.rs`
- Modify: `crates/orca-tui/src/agent_runtime.rs`
- Modify: `crates/orca-tui/src/action_dispatcher.rs`
- Modify: `crates/orca-tui/src/surface_client.rs`
- Delete: `crates/orca-tui/src/runtime_event_projection.rs`
- Modify: `crates/orca-tui/src/hosted_runtime.rs`

- [x] **Step 1: Remove broker and adapter module declarations/imports.**

Delete the three TUI module declarations and all imports of `TuiInteractionBroker`, `TuiInteractionWaiter`, `TuiTurnControl`, and the four adapter handler types. Do not remove typed surface handler imports from runtime code.

- [x] **Step 2: Make `TuiSurfaceTaskControl` constructible and pass it directly.**

Move the existing `hosted` state, operation id allocator, and typed methods to `TuiSurfaceTaskControl`; add a single `pub(crate) fn new() -> Self` constructor that creates the existing default state and allocator. Change `TuiAgentRuntime::spawn_hosted` to accept `TuiSurfaceTaskControl`, store it, and stop converting/dropping a legacy controller. Update callers to construct the control directly.

- [x] **Step 3: Remove legacy active-operation, waiter, and broker fields/methods.**

Delete `TuiOperationController`, `TuiTurnControl`, `wait_for_hosted`, `install_hosted`, `complete_hosted`, `broker`, and the legacy `active`/`interrupt_requested` state. Retain typed operation activation, cancellation, background handoff, interaction response routing, watermarks, presentation task cancellation, and joining exactly once.

- [x] **Step 4: Run compilation to expose remaining old call sites.**

Run:

```bash
cargo test -p orca-tui --lib --no-run
```

Expected: only call-site errors for the removed legacy types/functions; no runtime surface type errors.

- [x] **Step 5: Keep the implementation changes in the same semantic slice.**

Do not commit until the migrated tests, manifest, docs, and full verification are complete.

### Task 3: Migrate tests and remove the legacy runner

**Files:**
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/operation_controller.rs`
- Modify: `crates/orca-tui/src/surface_client.rs`
- Modify: `crates/orca-tui/src/hosted_runtime.rs`

- [x] **Step 1: Delete `OrdinaryTurnRunner::Legacy`, `run_hosted_operation`, and legacy runtime helpers.**

Keep `OrdinaryTurnRunner::Typed` only, remove the `run_legacy_feature_*` helpers, and call `run_hosted_ordinary_turn` directly where the enum no longer adds behavior. Remove old operation-admission tests that only prove the deleted in-memory path.

- [x] **Step 2: Run workflow-notification tests through the normal typed helper.**

Change the two tests at the current legacy helper call sites to invoke `run_hosted_tui_controller_for_test`; their assertions on task identity, labels, and terminal status must remain unchanged.

- [x] **Step 3: Replace test-only legacy harness uses with isolated typed controls or direct cancellation tokens.**

Delete `HostedControlExecutor` and `HostedOperationHarness` from `test_support`. For manual compaction freshness, create two independent `CancelToken` values and assert the second is not cancelled after cancelling the first. For presentation shutdown tests, use `TuiSurfaceTaskControl::isolated_for_test()` and call its shutdown/join path directly.

- [x] **Step 4: Run the TUI unit suite and confirm GREEN.**

Run:

```bash
cargo test -p orca-tui --lib
```

Expected: zero failures and no references to deleted modules in compiler output.

- [x] **Step 5: Keep the migrated tests in the same semantic slice.**

Do not create a separate test-only commit; the deleted path and its replacement tests must be reviewed together.

### Task 4: Update boundary documentation and roadmap

**Files:**
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
- Modify: `crates/orca-tui/src/surface_boundary_tests.rs`
- Modify: `docs/production-roadmap.md`

- [x] **Step 1: Remove deleted legacy entrypoints from the reviewed manifest.**

Make the manifest's `tui_entrypoints` list match the current typed route and preserve all existing action classifications and compatibility statements.

- [x] **Step 2: Record the remaining pending-store deletion gate in the roadmap.**

State that TUI interaction ownership is now typed-surface-only, while the runtime-host compatibility `RuntimePendingInteractionStore` remains until legacy Goal continuation preflight is migrated and server/CLI recovery evidence is complete.

- [x] **Step 3: Run the boundary and repository shape audit.**

Run:

```bash
cargo test -p orca-tui runtime_surface_contract -- --nocapture
rg -n "TuiInteractionBroker|TuiTurnControl|runtime_interaction_adapter|run_hosted_operation|OrdinaryTurnRunner::Legacy" crates/orca-tui/src
```

Expected: boundary test passes and the `rg` command returns no matches in TUI source.

- [x] **Step 4: Keep docs and manifest changes in the same semantic slice.**

The final commit must include the implementation, behavior tests, manifest, roadmap, Spec, and plan together.

### Task 5: Full verification, review, rebase, and handoff

**Files:**
- Verify: all files changed by Tasks 1-4
- Update: this plan checkboxes and Spec verification notes

- [x] **Step 1: Run focused lifecycle and interaction suites.**

```bash
cargo test -p orca-tui --lib
cargo test -p orca-runtime --test runtime_surface_interaction
cargo test -p orca-runtime --test runtime_lifecycle_contract
```

- [x] **Step 2: Run the required workspace gate.**

The serial workspace gate on rebased `origin/main@445baf596` was not clean because
three upstream tests fail independently of this slice: the FIFO release test
`runtime_host::tests::session_listing_does_not_block_host_supervisor` does not
settle, and the two stateless server shutdown/failure tests observe
`sessions-index.sqlite3` under `ORCA_HOME`. The isolated FIFO reproduction
timed out after 45 seconds. `crates/orca-runtime` is byte-for-byte identical to
`origin/main`, so these are recorded as current-main baseline failures.

The workspace gate with those three tests explicitly filtered passed across all
other targets. Clippy remains a clean-base gate failure: the current toolchain
reports 17 `orca-core` warnings as errors under `-D warnings`; this slice does
not modify that crate. `cargo-semver-checks 0.49.0` passed 223/223 checks for
each of `orca-core`, `orca-runtime`, and `orca-tui` against
`origin/main@445baf596`.

```bash
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
git diff --check
```

- [x] **Step 3: Run real TUI interaction recovery validation.**

`node scripts/release/test-real-api-tui-approval-recovery.mjs` passed, and the
fresh binary run emitted `ORCA_TUI_APPROVAL_RECOVERY_REAL_OK` after approval,
latest-session recovery, and stream cancellation. The harness also gained a
cursor-span ANSI self-test because the real TUI writes popup words with cursor
addressing.

Use the configured DeepSeek API harness and record fresh run identifiers for normal interaction, cancel, and recovery/restart. Verify no stale continuation, no in-flight run after terminal shutdown, and exactly one user-visible terminal event.

- [x] **Step 4: Request code review and fix all Critical/Important findings.**

The full branch diff was reviewed against `origin/main` after rebase. The two
Important findings were fixed: the contract digest now matches the reviewed
manifest, and the validator anchors/mutation baselines no longer encode deleted
legacy files or entrypoints. No Critical or Important findings remain.

- [x] **Step 5: Rebase latest main and rerun affected gates.**

```bash
git fetch origin main
git rebase origin/main
cargo test -p orca-tui --lib
cargo test -p orca-runtime --test runtime_surface_interaction
cargo test --workspace
```

- [x] **Step 6: Commit the final semantic slice and pause the goal.**

The independently reviewable slice was committed as
`refactor(tui): remove legacy interaction rail`. The worktree, feature branch,
and pre-rebase backup stash remain preserved. Do not push, release, or delete
worktrees unless separately authorized.
