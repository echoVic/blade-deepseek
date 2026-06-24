# Orca Production Roadmap

> Goal: evolve Orca into a production-grade DeepSeek-native agent runtime.
> Reference implementations: Codex CLI, Claude Code, and the current Orca codebase.

Last updated: 2026-06-25
Current baseline: v0.1.32 typed runtime protocol boundary for server mode

---

## Current State

Orca has moved beyond the original MVP roadmap. The table below is the current
working baseline used to prioritize the next patch releases.

| Area | Current Orca State | Codex/Claude Reference | Status |
|------|--------------------|------------------------|--------|
| Tool registry | Built-ins, MCP tools, and TOML external tools share `ToolSpec` metadata | Codex-style spec/capability registry | Implemented |
| Tool approval | Action kind is derived from tool capabilities, with TOML allow/deny rules | Capability/policy driven approvals | Implemented |
| File discovery | `glob` is model-facing; `list_files` remains a compatibility alias | Claude `Glob`, Codex file search | Implemented; fuzzy search still missing |
| Shell execution | Synchronous `bash` via `sh -c` with approval and macOS Seatbelt path | Codex `exec_command` sessions, PTY, stdin, timeout | Partial |
| Context management | BPE token counting, local compaction, persisted collapse/summary records | Multi-level local/remote compaction | Partial |
| Tool output control | Fixed byte truncation helper on tool output | Codex truncation policies by bytes/tokens with explicit warnings | Partial |
| Model metadata | `ModelSelection` plus DeepSeek defaults | Codex `models-manager` with model capability metadata | Partial |
| MCP | stdio/SSE config surface and tool routing | Codex MCP client/server ecosystem | Partial |
| Hooks | Lifecycle hooks with JSON stdout actions | Codex hooks runtime and schema validation | Implemented; schema docs/validation can improve |
| Project instructions | User/project/rules files with includes | `AGENTS.md` style layered instructions | Implemented |
| Memory | Manual `/remember` plus optional project extraction | Codex memories extension | Partial |
| Persistent goals | `/goal` with persisted state plus goal-scoped `get_goal`, `create_goal`, and narrow `update_goal` | Codex goal extension | Implemented |
| Workflows | JavaScript workflow DSL, multi-stage runner, task state, notifications, and runtime status events | Codex automations/tasks concepts | Implemented; packaging/docs can improve |
| TUI | Markdown-ish rendering, themes, Vim mode, diff preview, slash commands, workflow panel, elapsed timers, and clearer approval dialogs | Codex/Claude richer terminal UX | Partial |
| History | JSONL transcripts, resume/fork/search/archive/compress | Codex thread store with queryable metadata | Partial |
| Release | GitHub release + npm alias distribution scripts plus retrying post-publish GitHub/npm/npm-exec verification | Codex npm/native release model | Implemented |
| Skills | Markdown skill discovery, `list_skills`/`read_skill`, and explicit `$skill` prompt injection | Codex skills and plugin-provided skill bundles | Partial |

---

## Patch Release Plan

The next work should land as independent patch releases. Each release must be
verified before the next phase starts.

### P0: Session Runtime Unification

**Release target:** v0.1.31

**Current status:** done in v0.1.31.

**Goal:** move long-lived interactive session state from the TUI bridge into
`orca-runtime`, creating the runtime boundary needed for a Codex-style protocol
layer.

**Deliverables:**

- Add `orca_runtime::session::InteractiveSession`.
- Centralize conversation, history writer, session id, project instructions,
  memory, hooks, MCP registry, cost tracking, and workflow task registry in
  runtime.
- Keep `TuiConversationSession` as a compatibility wrapper that delegates to the
  runtime session.
- Preserve current TUI event names, JSONL behavior, workflows, goals, backtrack,
  compaction, and request-user-input continuation.
- Document the boundary in
  `docs/superpowers/specs/2026-06-25-session-runtime-unification-design.md`.

**Verification:**

- `cargo fmt -- --check`
- `cargo test --workspace --all-targets`
- `npm --prefix site run build`
- `npm --prefix site run check:seo`
- `node scripts/release/test-stage-npm.mjs`
- `git diff --check`
- Post-publish `scripts/release/verify-published.mjs` for GitHub Release, npm,
  and `npm exec` smoke verification.

### P1: Protocol And Event Boundary

**Release target:** v0.1.32

**Current status:** server-mode submissions and server-facing events now flow
through `orca_runtime::protocol` with typed `Submission`, `ClientOp`, and
`ServerEvent` values while preserving the legacy flat JSON wire format.

**Goal:** introduce a runtime protocol boundary so TUI/headless clients can send
commands and consume versioned events without owning turn execution details.

**Scope:**

1. Define an `orca-runtime` protocol module inspired by Codex protocol types. Done in v0.1.32 for server mode.
   - User input, approval responses, cancel/backtrack, goal operations, and
     workflow controls should be commands.
   - Session lifecycle, assistant deltas, reasoning, tool calls, workflow/task
     updates, approvals, errors, and completion should be events.
2. Add a runtime event adapter. Server-mode adapter done in v0.1.32; TUI adapter remains P2/P3 follow-up.
   - Preserve existing display behavior while sourcing events from runtime where practical.
   - Keep JSONL output names stable for this release unless explicitly versioned.
3. Move more turn-loop orchestration behind runtime-owned APIs. Still open after v0.1.32.
   - The TUI may still render and request approvals.
   - Runtime should own command handling and event emission.

**Out of scope for P1:**

- Full app-server transport.
- Remote UI clients.
- Tool-system rewrite.
- Background shell/PTTY sessions.

### P2: Tool System Convergence

**Release target:** v0.1.33+

**Goal:** reduce the remaining divergence between built-in tools, MCP tools,
external tools, approvals, and future plugin-provided tools.

**Scope:**

1. Normalize tool invocation records across all tool sources.
2. Move approval classification and result shaping into a shared runtime path.
3. Prepare for long-running shell sessions, worktree automation, and async
   subagents without adding them in the same patch.

### Skills And Plugins

**Release target:** after P2 unless P1 uncovers a smaller safe slice.

**Goal:** evolve the existing Markdown skill loading into a plugin-compatible
instruction and capability system.

**Scope:**

- Keep current `list_skills`, `read_skill`, and explicit `$skill` injection
  stable.
- Add richer skill manifests only after protocol/tool boundaries can carry
  plugin-provided capabilities cleanly.

---

## Priority Matrix

| Priority | Item | Why Now | Risk |
|----------|------|---------|------|
| P0 | Runtime-owned interactive session | Removes duplicated TUI/runtime state before deeper refactors | Medium |
| P0 | Published release verification | Prevents local tags from being mistaken for GitHub/npm releases | Low |
| P1 | Runtime protocol commands/events | Gives TUI/headless surfaces a shared contract | Medium |
| P1 | TUI event adapter | Lets UI behavior stay stable while ownership moves runtime-side | Medium |
| P2 | Unified tool invocation records | Reduces drift across built-in, MCP, and external tools | Medium |
| P2 | Shared approval/result shaping | Keeps policy decisions consistent across tool sources | Medium |
| Skills | Plugin-compatible skill manifests | Unlocks reusable instruction bundles after runtime contracts stabilize | Medium |
| Later | Shell sessions / PTY | High value, larger runtime surface | High |
| Later | Remote compaction | High value, model-dependent behavior | Medium/High |
| Later | Worktree automation | High value, more filesystem/git risk | High |
| Later | Multi-format reading | Useful, but dependency and rendering heavy | Medium |

---

## Technical Decisions

| Decision | Current Choice | Notes |
|----------|----------------|-------|
| Tokenizer | `tiktoken-rs` BPE | Good enough for DeepSeek-compatible accounting until a DeepSeek-specific tokenizer is required |
| Config format | TOML | Keep user-facing config stable |
| Tool registry | `ToolSpec` capability registry | All built-ins, MCP, and external tools should flow through this path |
| Default truncation | Byte/token policy with compatibility defaults | Keep result budgets consistent as tool execution centralizes |
| MCP transport | stdio and SSE | Keep routing namespaced as `mcp__server__tool` |
| Sandbox | macOS Seatbelt first, graceful fallback elsewhere | Add summaries before adding more platform sandboxes |
| History | JSONL transcript files | Runtime now owns interactive writer setup; introduce ThreadStore trait before considering SQLite metadata |
| Interactive session | `orca_runtime::session::InteractiveSession` | TUI wrapper is temporary while protocol/events are extracted |
| Skills | Markdown `SKILL.md` files | Keep instruction loading stable before adding plugin-provided capabilities |

---

## Completion Gates

Every patch phase must satisfy:

1. Version references are aligned across `Cargo.toml`, `Cargo.lock`, README, website metadata, and release notes.
2. Tests relevant to the touched surface pass fresh.
3. Release staging still validates with the current version.
4. `git diff --check` is clean.
5. The release note describes user-visible changes and follow-up scope.
