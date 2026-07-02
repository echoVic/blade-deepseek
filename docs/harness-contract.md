# Orca Harness Contract

This document defines the external contract for `orca exec`.

The contract covers: a headless command, a versioned JSONL event stream, approval events, tool events, verification events, and deterministic exit codes.

## Command

```sh
orca exec [options] <prompt>
```

Options:

- `--output-format text|jsonl` — Output format (default: text)
- `--cwd <path>` — Workspace directory
- `--approval-mode suggest|auto-edit|full-auto` — Approval policy (default: suggest)
- `--verifier <command>` — Post-completion verification command
- `--model <name>` — Model override
- `--base-url <url>` — API base URL override

## Embedded Server Protocol

```sh
orca --mode=server
```

Server mode reads one JSON object per line from stdin and writes one JSON object per line to stdout. The initial supported operation is `submit`:

```json
{"id":1,"op":"submit","prompt":"fix the bug in main.rs"}
```

The response stream preserves the request `id` and emits compact protocol events derived from the normal runtime event stream:

```jsonl
{"id":1,"event":"turn_started","turn":1}
{"id":1,"event":"reasoning_delta","text":"Let me look..."}
{"id":1,"event":"tool_requested","tool":"read_file","target":"src/main.rs"}
{"id":1,"event":"tool_completed","tool":"read_file","status":"completed"}
{"id":1,"event":"message_delta","text":"I found the issue..."}
{"id":1,"event":"turn_completed","status":"success"}
```

Unsupported operations and malformed requests emit an `error` event. Server mode exits when stdin closes.

Requests are processed serially — the next `submit` is not read until the current one completes. Events are streamed as they occur (not batched).

## Event Envelope

Every JSONL line is one event:

```json
{
  "version": "1",
  "run_id": "run-...",
  "seq": 0,
  "timestamp_ms": 1780647978857,
  "type": "session.started",
  "payload": {}
}
```

## Event Types

- `session.started`
- `turn.started`
- `assistant.reasoning.delta`
- `assistant.message.delta`
- `provider.replay.updated`
- `approval.requested`
- `approval.resolved`
- `tool.call.requested`
- `tool.call.completed`
- `subagent.started`
- `subagent.completed`
- `verification.started`
- `verification.completed`
- `error`
- `session.completed`

## Run Status

The final `session.completed` event contains one of:

- `success`
- `failed`
- `cancelled`
- `approval_required`
- `verification_failed`
- `budget_exhausted`

## Exit Codes

- `0`: success
- `1`: failed
- `2`: verification failed
- `3`: approval required or denied
- `4`: budget exhausted
- `130`: cancelled

## Tool Contract

Built-in tools:

| Tool | Action | Description |
|------|--------|-------------|
| `read_file` | read | Reads UTF-8 file content, truncated at 8KB |
| `glob` | read | Finds files and directories by glob pattern or `mode: "fuzzy"` path query, sorted as workspace-relative paths; returns `(no matches)` when the path is missing or no entries match |
| `list_files` | read | Compatibility alias for directory listing; returns sorted names and `(empty)` for missing directories |
| `grep` | read | Regex search via `rg` with line numbers, `(no matches)` for empty results |
| `git_status` | read | Runs `git status --short` |
| `web_search` | network | Searches the web for current information |
| `bash` | shell | Executes via `sh -c`, requires approval unless `full-auto` |
| `edit` | write | Exact text replacement, requires approval unless `full-auto` |
| `write_file` | write | Creates or overwrites a file, requires approval unless `full-auto` |
| `subagent` | agent | Runs a synchronous child agent with `description` and `prompt`, returning the child summary |
| `Workflow` | agent | Starts a background dynamic workflow |
| `update_plan` | read | Updates the visible plan state |
| `get_goal` | read | Reads active persistent goal state while goal mode is running |
| `create_goal` | read | Creates a persistent goal while goal mode is running and no unfinished goal exists |
| `update_goal` | read | Marks the active persistent goal complete or blocked while goal mode is running |

Tools are registered through a canonical registry. Each tool spec declares its capability set, renderer hint, exposure, aliases, and concurrent-safety flag. Runtime approval derives from the resolved tool spec instead of a separate hard-coded name list. Tool arguments are validated before execution with common JSON Schema object keywords, enums, arrays, and `oneOf` / `anyOf` composition.

Tool events:
- `tool.call.requested` — emitted before execution, contains `name`, `action`, `target`
- `tool.call.completed` — emitted after execution, contains `name`, `status` (completed/failed/denied), `output`, `truncated`

External tools:
- Orca loads `~/.orca/tools/*.toml` at startup.
- Each descriptor defines `name`, `description`, `action_kind`, `command`, and `schema`.
- Descriptors are advertised to the model as function tools.
- Commands run from the workspace directory with raw JSON arguments on stdin and in `ORCA_TOOL_ARGS`.

`glob` is the preferred file discovery tool. It accepts the existing `pattern` argument for glob searches and `{"mode":"fuzzy","query":"..."}` for fuzzy path discovery. `list_files` remains accepted for compatibility but is not recommended in the system prompt.

Hook stdout protocol:
- `{"action":"allow"}` allows the operation.
- `{"action":"deny","reason":"..."}` blocks the hook target.
- `{"action":"modify","modified_target":"..."}` rewrites a tool target.
- `{"action":"inject","context":"..."}` injects model context.
- When JSON declares an `action`, unsupported actions and malformed action
  payloads fail the hook instead of being silently injected or ignored.
- Non-JSON stdout and JSON without `action` are treated as injected context.

Subagent events:
- `subagent.started` — emitted when the child agent starts, contains `id`, `description`
- `subagent.completed` — emitted when the child agent finishes, contains `id`, `description`, `status`, `output`, `error`

Persistent goal mode:
- `/goal` is a TUI feature, not a headless `orca exec` contract.
- Goals are keyed by saved TUI session id and stored in `$ORCA_HOME/goals_1.json` or `~/.orca/goals_1.json`.
- `get_goal`, `create_goal`, and `update_goal` are advertised only while a TUI goal turn has installed goal context. Outside goal mode they return failed tool results instead of creating hidden state.
- `update_goal` only accepts `complete` or `blocked`; pause, resume, edit, clear, and budget/usage limiting are user or system controls.
- Active goals auto-continue after successful turns until the status becomes `paused`, `blocked`, `usage_limited`, `budget_limited`, or `complete`.

## Approval Policy

Four modes control which tool actions require user confirmation:

| Mode | read | write | network | agent | shell |
|------|------|-------|---------|-------|-------|
| `suggest` (default) | allow | ask | ask | ask | ask |
| `auto-edit` | allow | allow | ask | ask | ask |
| `full-auto` | allow | allow | allow | allow | allow |
| `plan` | allow | deny | deny | deny | deny |

Behavior of `ask`:
- In **text mode**: prompts the user interactively on stderr for y/n confirmation
- In **jsonl mode**: automatically denies (no interactive input available)

When an action is denied:
- `approval.requested` and `approval.resolved` (decision=deny) events are emitted
- The tool result status is `denied`
- The run terminates with status `approval_required` and exit code `3`

## Provider Contract

The default (and only production) provider is DeepSeek. Internal test providers (`mock`, `deepseek-fixture`) exist for harness testing but are not user-facing.

### DeepSeek Provider

- Default model: `auto` (main loop uses `deepseek-v4-pro`, auxiliary tasks use `deepseek-v4-flash`)
- Default base URL: `https://api.deepseek.com`
- Default reasoning effort: `max` (`reasoning_effort = "high"` or `DEEPSEEK_REASONING_EFFORT=high` lowers it)
- Streaming: SSE with real-time reasoning/content deltas
- Authentication: `DEEPSEEK_API_KEY` (required)
- HTTP retry: 3 attempts with exponential backoff for 429/5xx status codes
- Timeout: 30s connect, 120s request, 300s streaming
- `finish_reason=length` → error (response truncated)
- `finish_reason=content_filter` → error (content blocked)

Response mapping:
- `reasoning_content` → `assistant.reasoning.delta` + `provider.replay.updated`
- `content` → `assistant.message.delta`
- `tool_calls` → parsed into `tool.call.requested` events
- errors → `error` event + status `failed`

### Agent Loop

The runtime executes a multi-turn agent loop (max 128 turns):

1. Send conversation to DeepSeek (with system prompt + tool schemas)
2. If response contains tool calls → execute each tool → add results to conversation → next turn
3. If response is a final message → return success
4. If budget exhausted → return `budget_exhausted` (exit code 4)

Subagents run the same loop as a child conversation in the same workspace. They inherit provider/model config and approval mode. Nested subagents are rejected in this MVP.

Context window management:
- Window size: DeepSeek V4 1M-token context window, compacted at the configured threshold with response reserve
- Compaction threshold: 80% utilization
- Strategy: preserve system message + most recent messages, truncate older history with a marker

### Replay State

`provider.replay.updated` preserves provider-specific context for multi-turn DeepSeek thinking/tool-use flows (reasoning_content + tool call IDs). This is part of the trace for maintaining DeepSeek replay semantics.

## Configuration

Priority: Environment variables > CLI arguments > config files > defaults.

Config file path: `$ORCA_HOME/config.toml` or `~/.orca/config.toml`. Project overrides can also live at `.orca/config.toml` in the workspace.

Config file fields include `model`, `api_key`, `base_url`, `approval_mode`, permission rules, hooks, MCP servers, and related runtime settings.
