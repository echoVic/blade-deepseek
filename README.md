# Orca

Orca is a DeepSeek-native coding agent runtime by Blade.

This repository is the Rust-first foundation for `blade-deepseek`: a local terminal coding agent focused on DeepSeek thinking and tool-use semantics.

## Goals

- Build a fast local CLI/TUI runtime in Rust.
- Treat DeepSeek reasoning and tool-call state as first-class runtime data.
- Keep the first milestone small: interactive CLI, headless exec, core tools, approval, event log, and JSONL output.

## Command

```sh
orca
orca "fix this test"
orca --print "summarize this repository"
orca exec --output-format jsonl "run the full verification"
orca exec --approval-mode read-only "read README.md"
orca exec --verifier "cargo test" "run the full verification"
orca exec --provider deepseek-fixture "inspect repo"
```

## Harness Contract

`orca exec` is the first stable runtime boundary. It emits one JSON object per line when `--output-format jsonl` is selected.

The current mock runtime supports:

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
- `session.completed`

Exit codes:

- `0`: success
- `1`: failed
- `2`: verification failed
- `3`: approval required or denied
- `4`: budget exhausted
- `130`: cancelled

## Status

Early runtime contract implementation. `orca exec` currently supports a mock provider and a recorded `deepseek-fixture` provider while the real DeepSeek HTTP transport is being built.
