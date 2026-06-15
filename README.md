# Orca

Orca is a DeepSeek-native coding agent runtime by Blade.

A local terminal coding agent built in Rust, focused on DeepSeek reasoning and tool-use semantics. It runs a multi-turn agent loop with SSE streaming, automatic context window management, and HTTP retry with exponential backoff.

## Quick Start

```sh
# Set your API key
export DEEPSEEK_API_KEY=sk-...

# Run a task
orca exec "fix this test"

# With options
orca exec --approval-mode full-auto "refactor the auth module"
orca exec --model deepseek-reasoner "explain this codebase"
orca exec --verifier "cargo test" "fix the failing test"
```

## Configuration

Priority chain (highest wins): Environment variables > CLI arguments > Config file > Defaults.

### Environment Variables

- `DEEPSEEK_API_KEY` — API key (required)
- `DEEPSEEK_MODEL` — Model override
- `DEEPSEEK_BASE_URL` — API base URL override

### Config File

`~/.config/orca/config.toml`:

```toml
model = "deepseek-v4-flash"
api_key = "sk-..."
base_url = "https://api.deepseek.com"
```

### Defaults

- Model: `deepseek-v4-flash`
- Base URL: `https://api.deepseek.com`
- Approval mode: `suggest`
- Output format: `text`
- Max turns: 128

## Command

```sh
orca exec [options] <prompt>
```

Options:

- `--output-format text|jsonl` — Output format (default: text)
- `--cwd <path>` — Workspace directory
- `--approval-mode suggest|auto-edit|full-auto` — Approval policy
- `--model <name>` — Model to use
- `--base-url <url>` — API base URL
- `--verifier <command>` — Post-run verification command

## Tools

All 7 tools are fully implemented:

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
