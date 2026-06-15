# Orca Harness Contract

This document defines the first external contract for `orca exec`.

The contract is intentionally small: a headless command, a versioned JSONL event stream, approval events, tool events, verification events, and deterministic exit codes.

## Command

```sh
orca exec [options] <prompt>
```

Options:

- `--output-format jsonl|text`
- `--cwd <path>`
- `--approval-mode read-only|workspace-write|full-auto`
- `--verifier <command>`
- `--model <name>`
- `--base-url <url>`

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

## Current Tool Contract

The mock runtime can currently trigger tools from simple prompts:

- `read README.md` -> `read_file`
- `list files` -> `list_files`
- `git status` -> `git_status`
- `grep ...` -> `grep` placeholder
- `bash ...` -> `bash` placeholder
- `edit ...` -> `edit` placeholder

Implemented tools emit `tool.call.requested` and `tool.call.completed`. Placeholder tools return `not_implemented`, still through the same event contract.

Current behavior:

- `read_file` reads UTF-8 text and truncates large output.
- `list_files` lists one directory and sorts names.
- `grep` uses `rg` with line numbers and returns `(no matches)` for empty results.
- `git_status` runs `git status --short`.
- `bash` runs through `sh -c` only when the approval policy permits shell actions. With the default `workspace-write` policy, shell actions are denied and the run exits with code `3`.
- `edit` supports exact replacement with `edit <path> :: <old> => <new>`. It fails without changing the file if the old text is missing, empty, or matches multiple locations.

## Provider Contract

Current providers:

- `mock`: default provider used for local harness contract tests.
- `deepseek-fixture`: recorded provider fixture that emits DeepSeek-style reasoning, replay state, a tool call, and a final assistant message.
- `deepseek`: minimal non-streaming HTTP provider. It reads `DEEPSEEK_API_KEY`, `DEEPSEEK_BASE_URL` (optional, default `https://api.deepseek.com`), and `DEEPSEEK_MODEL` (optional, default `deepseek-chat`).

`provider.replay.updated` exists to preserve provider-specific context that must be replayed in later model turns. For DeepSeek thinking/tool-use flows, this includes `reasoning_content` and tool call IDs. The event is part of the harness trace so future real HTTP transport code can keep DeepSeek replay semantics without changing the external JSONL contract.

The current `deepseek` provider maps non-streaming response fields into harness events:

- `reasoning_content` -> `assistant.reasoning.delta` and `provider.replay.updated`
- `content` -> `assistant.message.delta`
- provider/config/request errors -> `error` and final status `failed`
