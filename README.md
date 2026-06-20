# Orca

Orca is a DeepSeek-native coding agent runtime by Blade.

A local terminal coding agent built in Rust, focused on DeepSeek reasoning and tool-use semantics. It runs a multi-turn agent loop with SSE streaming, automatic context window management, and HTTP retry with exponential backoff.

## Installation

### npm

```bash
npm install -g @blade-ai/orca
orca --version
```

The npm package installs a small Node.js launcher and the native `orca` binary for supported platforms.

Supported npm platforms:

- macOS Apple Silicon (`darwin/arm64`)
- macOS Intel (`darwin/x64`)
- Linux x64 (`linux/x64`)
- Linux ARM64 (`linux/arm64`)

### GitHub Releases

Download the archive for your platform from the latest GitHub Release, extract it, and place `orca` on your `PATH`.

## Quick Start

```sh
# Set your API key
export DEEPSEEK_API_KEY=sk-...

# Run a task
orca exec "fix this test"

# With options
orca exec --approval-mode full-auto "refactor the auth module"
orca exec --model deepseek-v4-pro "explain this codebase"
orca exec --verifier "cargo test" "fix the failing test"
```

## Configuration

Priority chain (highest wins): Environment variables > CLI arguments > Config file > Defaults.

### Environment Variables

- `DEEPSEEK_API_KEY` — API key (required)
- `DEEPSEEK_MODEL` — Model override
- `DEEPSEEK_BASE_URL` — API base URL override

### Config File

`~/.orca/config.toml`:

```toml
model = "auto"
api_key = "sk-..."
base_url = "https://api.deepseek.com"
```

Hooks may return structured JSON on stdout. `{"action":"deny","reason":"..."}` blocks, `{"action":"modify","modified_target":"..."}` rewrites a tool target, and `{"action":"inject","context":"..."}` adds model context. Plain non-JSON stdout is treated as injected context for compatibility. Supported events are `session_start`, `session_end`, `pre_tool_use`, `post_tool_use`, `pre_model_call`, `post_model_call`, `on_budget_warning`, `pre_compact`, and `post_compact`.

Custom tools can be added with TOML descriptors under `~/.orca/tools/`:

```toml
name = "deploy"
description = "Deploy the current branch"
action_kind = "write"
command = "./scripts/deploy.sh"
schema = { target = { type = "string", description = "environment" } }
```

External tool commands run from the workspace directory. The raw JSON arguments are provided on stdin and in `ORCA_TOOL_ARGS`.

### Defaults

- Model: `auto` (main loop uses `deepseek-v4-pro`, auxiliary tasks use `deepseek-v4-flash`)
- Base URL: `https://api.deepseek.com`
- Approval mode: `suggest`
- Output format: `text`
- Max turns: 128

## Command

```sh
orca exec [options] <prompt>
orca --mode=server
```

Options:

- `--output-format text|jsonl` — Output format (default: text)
- `--cwd <path>` — Workspace directory
- `--approval-mode suggest|auto-edit|full-auto` — Approval policy
- `--model auto|deepseek-v4-flash|deepseek-v4-pro` — Model to use; `auto` defaults to Pro for the main loop and Flash for auxiliary tasks
- `--base-url <url>` — API base URL
- `--verifier <command>` — Post-run verification command
- `--resume <session|latest>` — Continue from a saved conversation transcript
- `--fork <session|latest>` — Continue from a saved transcript in a new child session with parent metadata
- `--continue` / `--last` — Continue from the latest saved conversation transcript
- `--no-history` — Disable local transcript persistence for this run
- `--save-history` — Persist transcript even with `--output-format jsonl`
- top-level `--continue` / `--last` — Open the latest saved conversation in TUI mode
- top-level `--resume <session|latest>` — Open a saved conversation in TUI mode
- top-level `--fork <session|latest>` — Fork a saved conversation in TUI mode
- top-level `--session-picker` — Choose a saved conversation before entering TUI mode
- top-level `--mode=server` — Read JSONL `submit` requests from stdin and emit protocol events to stdout

## Workflows

`orca workflow run <script-or-name>` runs a Claude Code-style dynamic workflow.
Named workflows resolve from the nearest `.claude/workflows/` directory first,
then `~/.claude/workflows/`. Project workflows win over user workflows.

Workflow scripts are JavaScript modules beginning with:

```js
export const meta = { name: "audit", description: "Audit code", phases: ["scan"] };
```

## Conversation History

Text-mode `orca exec` saves local JSONL transcripts under `~/.orca/sessions/YYYY/MM/DD/`.
JSONL mode is side-effect free by default for harness use; pass `--save-history` when a machine-readable run should also be resumable.

```sh
orca history list
orca history list --all
orca history show latest
orca history rename latest "short title"
orca history search "needle"
orca history compress latest
orca history archive latest
orca history delete <session>
orca exec --resume latest "continue the refactor"
orca exec --continue "continue the refactor"
orca exec --fork latest "try another approach"
orca --session-picker
```

`--resume` and `--fork` accept a full session ID, a filename/session prefix, or `latest`. Resumed runs create a new transcript that includes the loaded context plus the new turn. Forked runs also write `parent_id` and `forked: true` metadata. Context compaction is persisted as `context.collapsed` records, appends are guarded with file locks on Unix, `history search` uses local ripgrep when available, and `history compress` rewrites large transcripts as `.jsonl.zst` while keeping list/show/search support.

In the TUI, `Esc` during an idle composer backtracks to the previous user message and places that prompt back in the input box for editing and re-asking.

## Tools

Built-in tools:

| Tool | Description |
|------|-------------|
| `read_file` | Read file contents (UTF-8, truncated at 8KB) |
| `list_files` | List directory entries |
| `grep` | Search with ripgrep (regex, line numbers) |
| `bash` | Execute shell commands via `sh -c` |
| `edit` | Exact text replacement in files |
| `git_status` | Show git working tree status |
| `subagent` | Run a synchronous child agent for a delegated task |

## Architecture

- **Agent Loop**: prompt → model → tool_call → execute → feed result → next turn (up to 128 turns)
- **Subagents**: Synchronous child agent loops share the parent workspace, provider/model config, and approval policy, then return a concise result to the parent
- **SSE Streaming**: Real-time reasoning and content deltas via Server-Sent Events
- **Context Window**: 128K tokens, 80% threshold compaction (preserves system + recent messages)
- **Conversation History**: Local JSONL transcripts support listing, inspection, resume/fork, full-text search, archive/delete/rename, and zstd compression
- **HTTP Client**: Singleton with 30s connect / 120s request / 300s streaming timeouts, exponential backoff retry (3 attempts, handles 429/5xx)
- **Approval Policy**: Read operations always allowed; write/shell actions require interactive confirmation (suggest mode) or auto-allowed based on mode
- **Verification**: Optional post-completion verifier command with pass/fail status

## Event Stream (JSONL)

When `--output-format jsonl` is used, each line is a versioned event:

```json
{"version":"1","run_id":"run-...","seq":0,"timestamp_ms":1780647978857,"type":"session.started","payload":{}}
```

Event types: `session.started`, `turn.started`, `assistant.reasoning.delta`, `assistant.message.delta`, `provider.replay.updated`, `approval.requested`, `approval.resolved`, `tool.call.requested`, `tool.call.completed`, `subagent.started`, `subagent.completed`, `verification.started`, `verification.completed`, `error`, `session.completed`.

## Exit Codes

- `0`: success
- `1`: failed
- `2`: verification failed
- `3`: approval required or denied
- `4`: budget exhausted
- `130`: cancelled

## Tech Stack

- Rust 2024 edition
- clap (CLI), reqwest (blocking HTTP), serde (JSON), toml (config), dirs (XDG paths)
- Synchronous blocking I/O, suitable for CLI interaction

## Status

Production-ready agent loop with DeepSeek streaming provider. All 7 tools implemented, multi-turn conversation with context management, subagents, approval policies, and verification support.
