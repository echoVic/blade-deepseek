# Runtime Thread Seed Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a runtime-owned `RuntimeThread` seed that keeps long-lived thread state behind one API before server, TUI, and headless execution converge on it.

**Architecture:** `RuntimeThread` will live in `crates/orca-runtime/src/thread.rs` and own `InteractiveSession` plus `RuntimeSessionLifecycle`. It will provide stable accessors and a `run_turn_to_writer` method that delegates to the existing `ThreadTurnExecutor` path, so the first slice creates the boundary without changing current user-facing behavior.

**Tech Stack:** Rust 2024, `orca-runtime`, existing `InteractiveSession`, `RuntimeSessionLifecycle`, `ThreadTurnExecutor`, cargo tests.

## Global Constraints

- Use TDD: write a failing test before production code.
- Keep this slice compatibility-first: no TUI/server/headless behavior changes.
- Do not duplicate session initialization logic outside `InteractiveSession`.
- Each completed feature gets its own commit.
- Verification must include focused Rust tests and broader workspace checks before commit.

---

### Task 1: RuntimeThread Boundary

**Files:**
- Create: `crates/orca-runtime/src/thread.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `docs/production-roadmap.md`
- Test: `crates/orca-runtime/src/thread.rs`

**Interfaces:**
- Consumes: `InteractiveSession::new_with_preloaded`, `ThreadTurnExecutor::run_request`, `RuntimeSessionLifecycle::new`, `RuntimeSessionLifecycle::start_task`.
- Produces: `RuntimeThread::start(config: &RunConfig, title: impl Into<String>) -> io::Result<Self>`, `RuntimeThread::run_turn_to_writer`, `RuntimeThread::thread_id`, `RuntimeThread::session`, `RuntimeThread::session_mut`, `RuntimeThread::lifecycle`.

- [x] **Step 1: Write the failing test**

Add unit tests in `crates/orca-runtime/src/thread.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::config::{HistoryMode, OutputFormat, ProviderKind, RunConfig};
    use orca_core::model::ModelSelection;
    use std::collections::HashMap;

    fn test_config(cwd: std::path::PathBuf) -> RunConfig {
        RunConfig {
            app_version: "test".to_string(),
            prompt: String::new(),
            cwd: Some(cwd),
            output_format: OutputFormat::Jsonl,
            approval_mode: orca_core::approval_types::ApprovalMode::Suggest,
            provider: ProviderKind::DeepSeek,
            verifier: None,
            model: ModelSelection::default(),
            model_runtime: Default::default(),
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: HashMap::new(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            subagents: Default::default(),
            tools: Default::default(),
            workflows: Default::default(),
            theme: Default::default(),
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    #[test]
    fn runtime_thread_starts_with_runtime_owned_session_and_lifecycle() {
        let cwd = tempfile::tempdir().unwrap();
        let config = test_config(cwd.path().to_path_buf());

        let thread = RuntimeThread::start(&config, "inspect repo").unwrap();

        assert!(thread.thread_id().starts_with("run-"));
        assert_eq!(thread.session().conversation().messages.len(), 1);
        assert_eq!(thread.lifecycle().run_id(), thread.thread_id());
    }
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p orca-runtime runtime_thread_starts_with_runtime_owned_session_and_lifecycle -- --nocapture`

Expected: FAIL because `crates/orca-runtime/src/thread.rs` / `RuntimeThread` does not exist or is not exported.

- [x] **Step 3: Write minimal implementation**

Create `crates/orca-runtime/src/thread.rs` with `RuntimeThread` owning `InteractiveSession` and `RuntimeSessionLifecycle`; export it from `crates/orca-runtime/src/lib.rs`.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p orca-runtime runtime_thread_starts_with_runtime_owned_session_and_lifecycle -- --nocapture`

Expected: PASS.

- [x] **Step 5: Add turn delegation test**

Add a test that constructs a `RuntimeThread`, calls `session_mut().replace_skill_context(Some("marker".to_string()))`, and verifies access goes through the thread boundary. This keeps the first slice provider-free while proving callers no longer need to own `InteractiveSession` directly.

- [x] **Step 6: Run focused tests**

Run: `cargo test -p orca-runtime runtime_thread -- --nocapture`

Expected: PASS.

- [x] **Step 7: Update roadmap**

Update `docs/production-roadmap.md` Current baseline and Runtime lifecycle notes to mention the `RuntimeThread` seed.

- [x] **Step 8: Run verification**

Run:

```bash
cargo fmt -- --check
cargo test -p orca-runtime runtime_thread -- --nocapture
cargo test --workspace --all-targets
npm --prefix site run build
npm --prefix site run check:seo
node scripts/release/test-stage-npm.mjs
git diff --check
```

- [ ] **Step 9: Commit**

```bash
git add crates/orca-runtime/src/thread.rs crates/orca-runtime/src/lib.rs docs/production-roadmap.md docs/superpowers/plans/2026-07-01-runtime-thread-seed.md
git commit -m "feat(runtime): seed runtime thread boundary"
```
