# Server Shell Manager Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move server-mode shell session storage and lazy `RuntimeShellSessionManager` access out of `server.rs` into a focused `server/shell_manager.rs` module.

**Architecture:** `server.rs` keeps JSON protocol handling, response event shaping, and command/exec orchestration. `ServerShellManager` owns the optional runtime shell session manager, creates it with the server task registry on first use, and exposes narrow methods for shell CRUD/read/kill/reap/wait plus a borrowed manager hook for command/exec drain/terminate compatibility.

**Tech Stack:** Rust, `orca-runtime`, existing server JSONL contract tests, existing `RuntimeShellSessionManager`.

---

### Task 1: Add Ownership Test

**Files:**
- Modify: `crates/orca-runtime/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add `server_shell_manager_is_owned_by_shell_manager_module` near the existing server manager ownership tests. The test reads `src/server.rs` and `src/server/shell_manager.rs`, then verifies:
- `server.rs` declares `mod shell_manager;`
- `server.rs` no longer stores `shell_sessions: Option<RuntimeShellSessionManager>`
- `server/shell_manager.rs` owns `struct ServerShellManager`
- `server/shell_manager.rs` owns lazy manager creation via `TaskRegistry::new_for_cwd`
- `server/shell_manager.rs` exposes `sessions_mut` so command/exec drain and termination can still borrow the runtime shell manager

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p orca-runtime server_shell_manager_is_owned_by_shell_manager_module -- --nocapture
```

Expected: FAIL because `src/server/shell_manager.rs` does not exist yet.

### Task 2: Extract ServerShellManager

**Files:**
- Create: `crates/orca-runtime/src/server/shell_manager.rs`
- Modify: `crates/orca-runtime/src/server.rs`

- [ ] **Step 1: Create module**

Create `ServerShellManager` with:
- `sessions: Option<RuntimeShellSessionManager>`
- `manager_for_cwd(&mut self, cwd: &Path) -> &mut RuntimeShellSessionManager`
- `sessions_mut(&mut self) -> Option<&mut RuntimeShellSessionManager>`
- pass-through methods for `spawn`, `write_stdin`, `close_stdin`, `update_description`, `resize`, `list`, `reap_requested_stops`, `read`, `wait`, and `kill`

- [ ] **Step 2: Wire server state**

Update `ServerState` to hold `shells: ServerShellManager` and replace raw `shell_sessions` access with `state.shells`.

- [ ] **Step 3: Run targeted tests**

Run:

```bash
cargo test -p orca-runtime server_shell_manager_is_owned_by_shell_manager_module -- --nocapture
cargo test --test session_server_contract shell -- --nocapture
cargo test --test session_server_contract command_exec -- --nocapture
cargo test --test session_server_contract turn -- --test-threads=1 --nocapture
```

Expected: all pass.

### Task 3: Release Prep And Verification

**Files:**
- Modify: `docs/production-roadmap.md`
- Add release notes for the next patch version after implementation commit
- Update `Cargo.toml`, `Cargo.lock`, `npm/orca/package.json`, `README.md`, `site/src/shared.ts`, and `site/src/changelog/Changelog.tsx`

- [ ] **Step 1: Run formatting and focused verification**

Run:

```bash
cargo fmt -- --check
git diff --check
cargo test -p orca-runtime --all-targets -- --test-threads=1
cargo clippy -p orca-runtime --all-targets
```

- [ ] **Step 2: Commit feature**

Run:

```bash
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/server.rs crates/orca-runtime/src/server/shell_manager.rs docs/superpowers/plans/2026-07-05-server-shell-manager-boundary.md docs/production-roadmap.md
git commit -m "refactor(server): extract shell manager"
```

- [ ] **Step 3: Prepare and publish patch release**

Follow the existing tag-driven release workflow:

```bash
npm --prefix site run build
npm --prefix site run check:seo
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo clippy --workspace --all-targets
node scripts/release/real-api-e2e.mjs --timeout-ms 180000
git push origin main
git tag v0.1.129
git push origin v0.1.129
gh run watch <release-run-id> --repo echoVic/blade-deepseek --exit-status
node scripts/release/verify-published.mjs --version 0.1.129 --repo echoVic/blade-deepseek --package @blade-ai/orca --bin orca
```

Expected: release, npm package, npm exec smoke, and site/changelog public checks all verify `0.1.129`.
