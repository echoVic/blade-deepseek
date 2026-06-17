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

All 7 tools are fully implemented:

| Tool | Action | Description |
|------|--------|-------------|
| `read_file` | read | Reads UTF-8 file content, truncated at 8KB |
| `list_files` | read | Lists one directory, sorted names |
| `grep` | read | Regex search via `rg` with line numbers, `(no matches)` for empty results |
| `git_status` | read | Runs `git status --short` |
| `bash` | shell | Executes via `sh -c`, requires approval unless `full-auto` |
| `edit` | write | Exact text replacement, requires approval unless `full-auto` |
| `subagent` | read | Runs a synchronous child agent with `description` and `prompt`, returning the child summary |

Tool events:
- `tool.call.requested` — emitted before execution, contains `name`, `action`, `target`
- `tool.call.completed` — emitted after execution, contains `name`, `status` (completed/failed/denied), `output`, `truncated`

Subagent events:
- `subagent.started` — emitted when the child agent starts, contains `id`, `description`
- `subagent.completed` — emitted when the child agent finishes, contains `id`, `description`, `status`, `output`, `error`

## Approval Policy

Three modes control which tool actions require user confirmation:

| Mode | read | write | shell |
|------|------|-------|-------|
| `suggest` (default) | allow | ask | ask |
| `auto-edit` | allow | allow | ask |
| `full-auto` | allow | allow | allow |

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
- Window size: 128K tokens (estimated as chars/4)
- Compaction threshold: 80% utilization
- Strategy: preserve system message + most recent messages, truncate older history with a marker

### Replay State

`provider.replay.updated` preserves provider-specific context for multi-turn DeepSeek thinking/tool-use flows (reasoning_content + tool call IDs). This is part of the trace for maintaining DeepSeek replay semantics.

## Configuration

Priority: Environment variables > CLI arguments > Config file (`~/.config/orca/config.toml`) > Defaults.

Config file fields: `model`, `api_key`, `base_url`.
