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
```

## Status

Early initialization. The current code is only a placeholder CLI while the runtime architecture is being built.

