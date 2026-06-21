# Orca Production Roadmap

> Goal: evolve Orca into a production-grade DeepSeek-native agent runtime.
> Reference implementations: Codex CLI, Claude Code, and the current Orca codebase.

Last updated: 2026-06-22
Current baseline: v0.1.11 planning baseline

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
| Persistent goals | `/goal` with persisted state and `update_goal` | Codex goal extension | Implemented |
| Workflows | JavaScript workflow runner with task state | Codex automations/tasks concepts | Implemented; packaging/docs can improve |
| TUI | Markdown-ish rendering, themes, Vim mode, diff preview, slash commands | Codex/Claude richer terminal UX | Partial |
| History | JSONL transcripts, resume/fork/search/archive/compress | Codex thread store with queryable metadata | Partial |
| Release | GitHub release + npm alias distribution scripts | Codex npm/native release model | Implemented |
| Skills | No first-class skill loader yet | Codex skills and plugin-provided skill bundles | Missing |

---

## Patch Release Plan

The next work should land as independent patch releases. Each release must be
verified before the next phase starts.

### P0: Roadmap and State Baseline

**Release target:** v0.1.11

**Goal:** make docs and release metadata reflect the actual current product so
future work is not planned from stale assumptions.

**Deliverables:**

- Refresh this roadmap with current-state evidence.
- Keep README, website, tool comparison docs, and release notes aligned with the current patch version.
- Record the next implementation priorities: P1 context reliability, P2 user/workflow ergonomics, and skills.

**Verification:**

- `cargo test`
- `npm --prefix site run build`
- `node scripts/release/test-stage-npm.mjs`
- `git diff --check`

### P1: Context Reliability Foundations

**Release target:** v0.1.12

**Goal:** reduce context pollution and make model/runtime limits configurable
before adding larger ecosystem features.

**Scope:**

1. Add a truncation policy abstraction inspired by Codex `utils/output-truncation`.
   - Support byte and token budgets.
   - Preserve explicit warnings with original line/token counts.
   - Keep existing default behavior compatible at 8 KiB unless configured.
2. Add model metadata/config overrides.
   - Context window.
   - Auto-compact token limit.
   - Tool output token limit.
   - Reasoning/summary support flags where useful for DeepSeek models.
3. Wire tool output truncation through built-in, MCP, and external tool result paths.

**Out of scope for P1:**

- Remote LLM summary compaction.
- New providers.
- Full shell session/PTY support.

### P2: User and Workflow Ergonomics

**Release target:** v0.1.13

**Goal:** close the highest-value daily-use gaps visible from Codex and Claude
without destabilizing core runtime behavior.

**Scope:**

1. Fuzzy file search for TUI `@mention` and file discovery.
   - Use `.gitignore`-aware traversal.
   - Prefer a small crate boundary so provider/runtime do not own fuzzy matching.
2. Sandbox/config summary.
   - Show current approval mode, filesystem scope, network posture, and key limits in `/config show`.
   - Reuse the summary in startup/session events where appropriate.
3. Structured user question tool.
   - Provide a small, approval-safe mechanism for the model to request user input in TUI.
   - Keep headless JSONL behavior deterministic.

**Out of scope for P2:**

- Full background shell sessions.
- Image/PDF/Notebook readers.
- Worktree automation.

### Skills System

**Release target:** v0.1.14

**Goal:** add a first-class skill system that can load human-readable procedures
from user and project directories and inject only relevant skill instructions.

**Scope:**

1. Skill discovery.
   - User skills: `$ORCA_HOME/skills/*/SKILL.md` or `~/.orca/skills/*/SKILL.md`.
   - Project skills: `.orca/skills/*/SKILL.md`.
   - Manifest-free minimum viable format: directory name is the skill id.
2. Skill metadata parsing.
   - Frontmatter fields: `name`, `description`.
   - Body remains Markdown instructions.
3. Skill selection.
   - Include explicitly named skills in the prompt.
   - Add a small `list_skills`/`read_skill` tool pair or equivalent registry output if prompt size becomes a concern.
4. Safety.
   - Skills are instructions, not executable code.
   - Project skills cannot read outside the workspace during discovery.
   - Invalid skills are skipped with diagnostics.

**Out of scope for first skills release:**

- Plugin installation marketplace.
- Skill-provided MCP servers.
- Executable skill scripts.

---

## Priority Matrix

| Priority | Item | Why Now | Risk |
|----------|------|---------|------|
| P0 | Roadmap/state baseline | Prevents stale planning and mixed release notes | Low |
| P1 | Tool output truncation policies | Protects context window and makes long tasks more reliable | Low/Medium |
| P1 | Model metadata overrides | Enables per-model context and truncation decisions | Medium |
| P2 | Fuzzy file search | Improves everyday TUI ergonomics | Low/Medium |
| P2 | Sandbox/config summary | Makes safety posture visible to users | Low |
| P2 | Structured user question tool | Reduces brittle ad-hoc clarification patterns | Medium |
| Skills | Skill discovery and loading | Unlocks reusable domain procedures without full plugins | Medium |
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
| Default truncation | Byte budget, 8 KiB compatibility | P1 should add token-aware policy without breaking existing callers |
| MCP transport | stdio and SSE | Keep routing namespaced as `mcp__server__tool` |
| Sandbox | macOS Seatbelt first, graceful fallback elsewhere | Add summaries before adding more platform sandboxes |
| History | JSONL transcript files | Introduce ThreadStore trait before considering SQLite metadata |
| Skills | Markdown `SKILL.md` files | Start with instruction loading, not executable plugins |

---

## Completion Gates

Every patch phase must satisfy:

1. Version references are aligned across `Cargo.toml`, `Cargo.lock`, README, website metadata, and release notes.
2. Tests relevant to the touched surface pass fresh.
3. Release staging still validates with the current version.
4. `git diff --check` is clean.
5. The release note describes user-visible changes and follow-up scope.
