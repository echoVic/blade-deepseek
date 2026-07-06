# Extension Contributor Kernel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Codex-inspired extension contributor kernel to Orca, then migrate goal progress hooks onto that boundary without changing user-facing behavior.

**Architecture:** Introduce a small runtime extension module that owns typed per-scope extension data and an ordered contributor registry. Keep the first slice in `orca-runtime` so existing goal, memory, task, and tool lifecycle code can migrate gradually without a new crate/version choreography. Migrate goal progress accounting through tool lifecycle contributors as the first real consumer.

**Tech Stack:** Rust 2024, `orca-runtime`, existing cargo tests, existing release scripts.

## Global Constraints

- Use TDD for every behavior change: write a failing test, verify the expected failure, implement, verify green.
- Preserve existing wire formats, TUI behavior, goal storage format, and public CLI commands.
- Keep each requirement independently committable.
- After the complete feature round, update docs/version metadata, push, create a GitHub release, publish an npm patch, and verify public GitHub/npm artifacts.

---

### Task 1: Runtime Extension Data And Registry Seed

**Files:**
- Create: `crates/orca-runtime/src/extension.rs`
- Modify: `crates/orca-runtime/src/lib.rs`

**Interfaces:**
- Produces: `ExtensionData::new(level_id)`, `ExtensionData::get<T>()`, `ExtensionData::get_or_init<T>()`, `ExtensionData::insert<T>()`, `ExtensionRegistryBuilder`, `ExtensionRegistry`, `ToolLifecycleContributor`, `ToolStartInput`, `ToolFinishInput`, and `empty_extension_registry()`.
- Consumes: no new dependencies.

- [ ] **Step 1: Write the failing test**

Add tests in `crates/orca-runtime/src/extension.rs` proving typed data round-trips and contributors run in registration order.

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR=/tmp/blade-deepseek-target-extension-red cargo test -p orca-runtime extension -- --nocapture`
Expected: FAIL because `crate::extension` does not exist yet.

- [ ] **Step 3: Write minimal implementation**

Implement the typed data map and registry in `extension.rs`; expose it with `pub mod extension;` in `lib.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_TARGET_DIR=/tmp/blade-deepseek-target-extension cargo test -p orca-runtime extension -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/extension.rs crates/orca-runtime/src/lib.rs docs/superpowers/plans/2026-07-06-extension-contributor-kernel.md
git commit -m "refactor(runtime): seed extension contributor kernel"
```

### Task 2: Goal Tool Lifecycle Contributor Seed

**Files:**
- Modify: `crates/orca-runtime/src/goals.rs`
- Modify: `crates/orca-runtime/src/extension.rs`
- Modify: `crates/orca-runtime/src/lib.rs`

**Interfaces:**
- Consumes: `ToolLifecycleContributor`, `ToolFinishInput`, and `ExtensionData`.
- Produces: a small goal lifecycle contributor API that can account goal progress through the extension registry without direct tool-turn coupling.

- [ ] **Step 1: Write the failing test**

Add a test that installs a goal lifecycle contributor into `ExtensionRegistryBuilder`, emits a completed non-`update_goal` tool finish, and proves the registered contributor observes the finish exactly once with the thread/turn scope data.

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR=/tmp/blade-deepseek-target-goal-ext-red cargo test -p orca-runtime goal_extension -- --nocapture`
Expected: FAIL because goal lifecycle installation is not implemented.

- [ ] **Step 3: Write minimal implementation**

Add the contributor adapter and registry invocation helper while keeping existing goal store behavior unchanged.

- [ ] **Step 4: Run focused tests to verify they pass**

Run: `CARGO_TARGET_DIR=/tmp/blade-deepseek-target-goal-ext cargo test -p orca-runtime extension -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/extension.rs crates/orca-runtime/src/goals.rs crates/orca-runtime/src/lib.rs
git commit -m "refactor(runtime): route goal tool lifecycle through extensions"
```

### Task 3: Feature Round Documentation, Verification, Release

**Files:**
- Modify: `docs/production-roadmap.md`
- Add: `docs/releases/v0.1.150.md`
- Modify: `Cargo.toml`
- Modify: `npm/orca/package.json`

**Interfaces:**
- Consumes: Task 1 and Task 2 behavior.
- Produces: v0.1.150 release metadata and public package/release verification.

- [ ] **Step 1: Update docs and versions**

Bump the root crate/package from `0.1.149` to `0.1.150`, add a release note, and update the roadmap baseline to mention the extension contributor kernel and goal lifecycle seed.

- [ ] **Step 2: Run full local verification**

Run the repository's release gate commands, including cargo formatting, cargo tests, site checks, npm staging checks, and `git diff --check`.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml npm/orca/package.json docs/production-roadmap.md docs/releases/v0.1.150.md
git commit -m "docs(release): prepare v0.1.150"
```

- [ ] **Step 4: Push, release, publish, verify**

Push `main`, create the GitHub release/tag for `v0.1.150`, publish the npm patch package, wait for GitHub Actions if applicable, and run `node scripts/release/verify-published.mjs --version 0.1.150 --repo echoVic/blade-deepseek --package @blade-ai/orca --bin orca` until GitHub Release, npm package, and `npm exec` smoke verification all pass.
