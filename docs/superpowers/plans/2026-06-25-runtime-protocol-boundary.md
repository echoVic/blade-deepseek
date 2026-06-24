# Runtime Protocol Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move server-mode request decoding and event encoding into a typed runtime protocol module while preserving the current JSON wire format.

**Architecture:** Add `orca_runtime::protocol` with typed submissions and events. Keep `orca_runtime::server` as the stdin/stdout loop that dispatches protocol operations and streams controller events through the typed adapter.

**Tech Stack:** Rust 2024, serde, serde_json, existing Orca event schema and release scripts.

## Global Constraints

- Preserve current server request shape: `{"id":1,"op":"submit","prompt":"..."}`.
- Preserve current server response event names and flat JSON shape.
- Do not add new public commands in this release.
- Update docs and release notes before release.
- Release `v0.1.32` only after local and public verification pass.

---

### Task 1: Typed Protocol Module

**Files:**
- Create: `crates/orca-runtime/src/protocol.rs`
- Modify: `crates/orca-runtime/src/lib.rs`

**Interfaces:**
- Produces: `Submission::decode(line: &str) -> Result<Submission, DecodeError>`
- Produces: `ClientOp::Submit { prompt }`
- Produces: `ServerEvent`
- Produces: `map_runtime_event_line(line: &str) -> Option<ServerEvent>`
- Produces: `write_server_event(writer, id, event)`

- [x] Add typed protocol structures.
- [x] Add tests for submit decoding and unsupported op errors.
- [x] Add typed event serialization that preserves the legacy flat JSON shape.
- [x] Add runtime event mapping tests.

### Task 2: Server Integration

**Files:**
- Modify: `crates/orca-runtime/src/server.rs`
- Test: `crates/orca-runtime/src/server.rs`
- Test: `tests/session_server_contract.rs`

**Interfaces:**
- Consumes: `protocol::Submission`
- Consumes: `protocol::ServerEvent`
- Preserves: server mode JSONL behavior.

- [x] Replace ad hoc request parsing in `server.rs` with `Submission::decode`.
- [x] Replace server-local event JSON construction with typed protocol events.
- [x] Keep existing server tests passing against the same wire shape.
- [x] Run focused runtime tests.

### Task 3: Docs And Release Prep

**Files:**
- Create: `docs/superpowers/specs/2026-06-25-runtime-protocol-boundary-design.md`
- Create: `docs/releases/v0.1.32.md`
- Modify: `docs/production-roadmap.md`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `README.md`
- Modify: `site/src/shared.ts`
- Modify: `site/src/changelog/Changelog.tsx`
- Modify: `site/src/App.tsx`
- Modify: `site/index.html`

**Interfaces:**
- Produces: release notes and version alignment for `v0.1.32`.

- [x] Document the typed runtime protocol boundary and P2 follow-up.
- [x] Bump root package version to `0.1.32`.
- [x] Update `Cargo.lock`.
- [x] Update README pinned install version.
- [x] Add `docs/releases/v0.1.32.md`.

### Task 4: Verification And Release

**Files:**
- No source changes expected after this task unless verification fails.

**Interfaces:**
- Produces: pushed commit and tag `v0.1.32`.
- Produces: verified GitHub Release and npm package.

- [x] Run `cargo fmt -- --check`.
- [x] Run `cargo test --workspace --all-targets`.
- [x] Run `npm --prefix site run build`.
- [x] Run `npm --prefix site run check:seo`.
- [x] Run `node scripts/release/test-stage-npm.mjs`.
- [x] Run `git diff --check`.
- [ ] Commit P1 implementation and docs.
- [ ] Push `main`.
- [ ] Create and push tag `v0.1.32`.
- [ ] Wait for GitHub Actions release workflow to complete.
- [ ] Run `node scripts/release/verify-published.mjs --version 0.1.32 --repo echoVic/blade-deepseek --package @blade-ai/orca --bin orca`.
