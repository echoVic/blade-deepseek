# Orca

Orca is a DeepSeek-native coding agent.

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

### curl

```bash
curl -fsSL https://orcaagent.dev/install.sh | sh
```

The installer downloads the native binary for your platform from GitHub Releases.
Set `INSTALL_DIR` to choose a destination and `ORCA_VERSION` to pin a version:

```bash
curl -fsSL https://orcaagent.dev/install.sh | \
  INSTALL_DIR=/usr/local/bin ORCA_VERSION=0.2.1 sh
```

### GitHub Releases

Download the archive for your platform from the latest GitHub Release, extract it, and place `orca` on your `PATH`.

## Community

- QQ group: `472309526`
- Telegram: https://t.me/+11No1w5ZbTMyZTQ1

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
- `DEEPSEEK_REASONING_EFFORT` — Reasoning effort override (`high` or `max`; default `max`)
- `ORCA_NODE_PATH` — Node.js executable used by workflow scripts when `node` is not on `PATH` (npm installs set this automatically)

### Config File

`~/.orca/config.toml`:

```toml
model = "auto"
reasoning_effort = "max"
api_key = "sk-..."
base_url = "https://api.deepseek.com"
```

### Updates

When `update_check` is enabled, Orca checks for a newer release before opening the interactive TUI. If a newer release is available, Orca shows a startup prompt with `Update now`, `Skip`, and `Skip until next version`. Choosing `Update now` updates the currently running install: npm-managed launches run the npm upgrade command, while direct binary launches rerun the curl installer into the current executable's directory. Choosing either skip option continues into the TUI.

If you installed with curl and later switch to npm, make sure the npm global bin directory appears before `~/.local/bin` on `PATH`, or remove the older curl-installed `~/.local/bin/orca`. Otherwise your shell may keep running the curl-installed binary.

Disable the startup check with:

```toml
update_check = false
```

Hooks may return structured JSON on stdout. `{"action":"deny","reason":"..."}` blocks, `{"action":"modify","modified_target":"..."}` rewrites a tool target, and `{"action":"inject","context":"..."}` adds model context. When JSON declares an `action`, Orca validates the action and required string fields so typoed or malformed structured outputs fail visibly. Plain non-JSON stdout and JSON without `action` are treated as injected context for compatibility. Supported events are `session_start`, `session_end`, `pre_tool_use`, `post_tool_use`, `pre_model_call`, `post_model_call`, `on_budget_warning`, `pre_compact`, and `post_compact`.

Custom tools can be added with TOML descriptors under `~/.orca/tools/`:

```toml
name = "deploy"
description = "Deploy the current branch"
action_kind = "write"
command = "./scripts/deploy.sh"
schema = { target = { type = "string", description = "environment" } }
```

External tool commands run from the workspace directory. The raw JSON arguments are provided on stdin and in `ORCA_TOOL_ARGS`.

Tool output truncation can be configured under `[tools]`. Byte mode preserves the historical 8 KiB default; token mode adds an explicit warning with original token and line counts before compacting large outputs:

```toml
[tools]
output_truncation = { mode = "tokens", limit = 2000 }
```

### Defaults

- Model: `auto` (main loop uses `deepseek-v4-pro`, auxiliary tasks use `deepseek-v4-flash`)
- Reasoning effort: `max`
- Base URL: `https://api.deepseek.com`
- Approval mode: `suggest`
- Output format: `text`
- Max turns: 128

## Command

```sh
orca exec [options] <prompt>
orca --mode=server
```

For headless harnesses, `orca exec` also accepts prompt input from stdin:

```sh
printf 'fix the failing test\n' | orca exec --output-format jsonl
printf 'review this diff\n' | orca exec --output-format jsonl -
printf 'compiler output\n' | orca exec --output-format jsonl 'summarize this failure'
```

When no prompt argument is provided, stdin becomes the prompt. A lone `-` also
forces reading the prompt from stdin. When a prompt argument and piped stdin are
both provided, stdin is appended to the prompt inside a `<stdin>...</stdin>`
context block.

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

`orca workflow run <script-or-name>` runs an Orca dynamic workflow.
Named workflows resolve from the nearest `.orca/workflows/` directory first,
then `~/.orca/workflows/`. Project workflows win over user workflows.
Workflow tool calls launch background tasks and return task metadata immediately;
the final workflow report is delivered back into the active TUI session as a
task notification.
If a backgrounded TUI turn pauses for tool approval, approving it from the
workflow/tasks panel continues the stored provider response. The originally
approved tool call is not prompted again; later approval-requiring tools still
follow the active approval policy.

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

## Persistent Goal Mode

TUI sessions support persistent goals with `/goal`. A goal is stored by session id in `~/.orca/goals_1.json` or `$ORCA_HOME/goals_1.json`, so it survives process restarts when the session is saved.

```text
/goal                         # show the current goal
/goal ship the refactor       # create or replace the active goal and start it
/goal edit finish the parser  # update the objective and reactivate it
/goal pause                   # stop automatic continuation
/goal resume                  # reactivate and continue when idle
/goal clear                   # delete the goal for this session
```

While a goal is active, Orca automatically starts another turn after a successful turn and injects goal-mode instructions as pinned context. The loop stops when the goal is paused, cleared, blocked, completed, budget-limited, interrupted, or reaches the continuation cap. Goal turns expose `get_goal`, `create_goal`, and `update_goal`; the model can only use `update_goal` to mark the active goal `complete` or `blocked`, while `/goal` commands own pause, resume, edit, and clear.

Persistent goals require recorded history. If history is disabled with `--no-history`, `/goal` reports an error instead of creating ephemeral goal state.

## Tools

Built-in tools:

| Tool | Description |
|------|-------------|
| `read_file` | Read file contents (UTF-8, truncated at 8KB) |
| `glob` | Find files and directories by glob pattern or fuzzy path query; preferred for file discovery |
| `list_files` | Compatibility alias for directory listing |
| `grep` | Search with ripgrep (regex, line numbers) |
| `bash` | Execute shell commands via `sh -c` |
| `edit` | Exact text replacement in files |
| `write_file` | Create or overwrite a file |
| `git_status` | Show git working tree status |
| `web_search` | Search the web for current information |
| `subagent` | Run a synchronous child agent for a delegated task |
| `Workflow` | Launch a background dynamic workflow |
| `update_plan` | Update the visible task plan |
| `get_goal` | Read active persistent goal state during goal mode |
| `create_goal` | Create a persistent goal during goal mode when no unfinished goal exists |
| `update_goal` | Mark active persistent goal complete or blocked from goal mode |
| `request_user_input` | Ask a structured clarification question; TUI answers continue the same turn |
| `list_skills` | List Markdown skills from user and project skill directories |
| `read_skill` | Read a skill's Markdown instructions by id |
| `list_mcp_resources` | List read-only resources exposed by configured MCP servers |
| `read_mcp_resource` | Read a specific MCP resource by server name and URI |

Tools are registered through a canonical tool registry with capability metadata. Approval behavior is derived from those capabilities: read-only tools run directly, write tools follow write approval policy, shell tools follow shell approval policy, network tools follow network policy, and agent/workflow tools follow agent policy. Tool arguments are validated before execution, including common JSON Schema object keywords plus `oneOf` and `anyOf`, so provider-advertised schemas and runtime rejection behavior stay aligned. `glob` is the model-facing file discovery tool and supports both normal glob patterns and `mode: "fuzzy"` path queries; `list_files` remains accepted for older prompts and saved sessions. `request_user_input` stays deterministic in headless runs and becomes interactive in TUI sessions.

Markdown skills live under `$ORCA_HOME/skills/*/SKILL.md`, `~/.orca/skills/*/SKILL.md`, or project `.orca/skills/*/SKILL.md`. The model can inspect them with `list_skills` and `read_skill`; when a prompt explicitly mentions a skill id such as `$debugging`, Orca injects that skill's instructions into the model context for the turn.

MCP tools and custom external tools can be added at startup. External tools live under `~/.orca/tools/*.toml` or `$ORCA_HOME/tools/*.toml`, and configured MCP server tools are exposed with namespaced tool names. MCP resources can also be listed and read through the read-only resource tools.

## Architecture

- **Agent Loop**: prompt → model → tool_call → execute → feed result → next turn (up to 128 turns)
- **Subagents**: Synchronous child agent loops share the parent workspace, provider/model config, and approval policy, then return a concise result to the parent
- **Persistent Goal Mode**: TUI sessions can persist a long-running objective, auto-continue successful turns, and stop through `/goal` controls or goal-mode tools
- **SSE Streaming**: Real-time reasoning and content deltas via Server-Sent Events
- **Context Window**: DeepSeek V4 1M-token context, 80% threshold compaction with response reserve (preserves system + recent messages)
- **Conversation History**: Local JSONL transcripts support listing, inspection, resume/fork, full-text search, archive/delete/rename, and zstd compression
- **HTTP Client**: Singleton with 30s connect / 120s request / 300s streaming timeouts, exponential backoff retry (3 attempts, handles 429/5xx)
- **Approval Policy**: Tool capabilities drive approval; read operations are allowed, `suggest` asks for write/network/agent/shell, `auto-edit` allows writes but asks for network/agent/shell, and `full-auto` allows all
- **Verification**: Optional post-completion verifier command with pass/fail status
- **Release Gate**: `scripts/release/verify-published.mjs` checks the GitHub Release, npm registry, and `npm exec` smoke path after publishing

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

Production-ready agent loop with DeepSeek streaming provider. Core tools, multi-turn conversation, persistent goals, context management, subagents, workflows, approval policies, and verification support are implemented.
