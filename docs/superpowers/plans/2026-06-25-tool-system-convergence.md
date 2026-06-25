# Tool System Convergence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unify runtime tool invocation preparation, validation, approval request construction, and hook-modified request validation while preserving current tool behavior.

**Architecture:** Add `orca_runtime::tool_invocation` as a narrow policy boundary over the existing `orca-tools` registry. Keep controller-owned side effects and event ordering in `controller.rs`, but move reusable derivation and validation logic into the new module so TUI and future protocol adapters can share it.

**Tech Stack:** Rust 2024, existing `orca-core`, `orca-tools`, `orca-runtime`, DeepSeek real API smoke tests, GitHub/npm release scripts.

## Global Constraints

- Preserve public tool names, aliases, JSONL event names, and server-mode flat event mapping.
- Do not add shell session or PTY tools in this release.
- Do not change MCP transport behavior or external tool TOML schema.
- Release `v0.1.33` only after local, real API, GitHub Release, npm registry, and `npm exec` verification pass.

---

### Task 1: Runtime Tool Invocation Boundary

**Files:**
- Create: `crates/orca-runtime/src/tool_invocation.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Test: `crates/orca-runtime/src/tool_invocation.rs`

**Interfaces:**
- Produces: `ToolInvocation`
- Produces: `ToolExecutionFailure`
- Produces: `prepare_tool_invocation(tool_request, subagent_depth, mcp_registry, config) -> ToolInvocation`
- Produces: `validate_tool_invocation(invocation, mcp_registry, config) -> Result<(), ToolExecutionFailure>`
- Produces: `apply_pre_tool_outcome(invocation, outcome, mcp_registry, config) -> Result<ToolInvocation, ToolExecutionFailure>`
- Produces: `approval_request_for_invocation(invocation) -> Option<ApprovalRequest>`

- [x] Add tests proving canonical action ignores caller-supplied `ToolRequest.action`.
- [x] Add tests proving MCP and external tool action kinds resolve through the same helper.
- [x] Add tests proving subagent max-depth returns `action: None` and no approval request.
- [x] Add tests proving hook-modified request is revalidated and reports invalid input.
- [x] Implement the tool invocation module.
- [x] Export the module from `orca-runtime`.
- [x] Run `cargo test -p orca-runtime --lib tool_invocation -- --nocapture`.

### Task 2: Controller Integration

**Files:**
- Modify: `crates/orca-runtime/src/controller.rs`
- Test: `crates/orca-runtime/src/controller.rs`
- Test: `tests/approval_contract.rs`
- Test: `tests/tool_contract.rs`

**Interfaces:**
- Consumes: `tool_invocation::prepare_tool_invocation`
- Consumes: `tool_invocation::validate_tool_invocation`
- Consumes: `tool_invocation::apply_pre_tool_outcome`
- Consumes: `tool_invocation::approval_request_for_invocation`
- Preserves: existing event ordering and run statuses.

- [x] Replace controller-local approval action derivation with `prepare_tool_invocation`.
- [x] Replace controller-local validation errors with `ToolExecutionFailure::into_result`.
- [x] Use `approval_request_for_invocation` for approval event construction.
- [x] Use `apply_pre_tool_outcome` after `pre_tool_use` hooks.
- [x] Keep workflow and subagent execution branches in controller.
- [x] Run `cargo test -p orca-runtime --lib controller -- --nocapture`.
- [x] Run `cargo test --test approval_contract --test tool_contract`.

### Task 3: TUI Approval Helper Alignment

**Files:**
- Modify: `crates/orca-tui/src/bridge.rs`
- Test: `crates/orca-tui/src/bridge.rs`

**Interfaces:**
- Consumes: `tool_invocation::prepare_tool_invocation`
- Consumes: `tool_invocation::approval_request_for_invocation`
- Preserves: TUI approval dialog fields and allowlist behavior.

- [x] Replace TUI-local approval action helper with runtime invocation helper.
- [x] Keep TUI-specific approval preview rendering in `bridge.rs`.
- [x] Add or update tests proving caller-supplied read action cannot downgrade shell approval.
- [x] Run `cargo test -p orca-tui --lib bridge -- --nocapture`.

### Task 4: Verification And Real API Smoke

**Files:**
- No source changes expected unless verification fails.

**Interfaces:**
- Produces: local proof that P2 works before release.

- [x] Run `cargo fmt -- --check`.
- [x] Run `cargo test --workspace --all-targets`.
- [x] Run `npm --prefix site run build`.
- [x] Run `npm --prefix site run check:seo`.
- [x] Run `node scripts/release/test-stage-npm.mjs`.
- [x] Run `git diff --check`.
- [x] Run `cargo run -p orca-provider --example summary_render_realapi`.
- [x] Run real API CLI smoke with `ORCA_REAL_E2E_OK`.
- [x] Run real API server-mode smoke with `ORCA_SERVER_REAL_OK`.

### Task 5: Docs, Version, Release

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `README.md`
- Modify: `docs/production-roadmap.md`
- Create: `docs/releases/v0.1.33.md`
- Modify: `site/index.html`
- Modify: `site/src/App.tsx`
- Modify: `site/src/changelog/Changelog.tsx`
- Modify: `site/src/shared.ts`
- Modify: `docs/superpowers/plans/2026-06-25-tool-system-convergence.md`

**Interfaces:**
- Produces: release notes and version alignment for `v0.1.33`.
- Produces: pushed commit and tag `v0.1.33`.
- Produces: verified GitHub Release and npm package.

- [x] Bump root package version to `0.1.33`.
- [x] Update `Cargo.lock`.
- [x] Update README pinned install version.
- [x] Update roadmap P2 status and next phase.
- [x] Add `docs/releases/v0.1.33.md`.
- [x] Update site version and changelog.
- [x] Run release-prep verification again.
- [ ] Commit P2 implementation and docs.
- [ ] Push `main`.
- [ ] Create and push tag `v0.1.33`.
- [ ] Wait for GitHub Actions release workflow to complete.
- [ ] Run `node scripts/release/verify-published.mjs --version 0.1.33 --repo echoVic/blade-deepseek --package @blade-ai/orca --bin orca`.
