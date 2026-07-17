# Orca

A DeepSeek-native coding agent for your terminal.

Give Orca a task and it reads code, edits files, runs commands, verifies the
result, and keeps working until the task is done or it needs you. Use the TUI
for interactive work or `orca exec` for scripts and CI. Orca is built in Rust,
runs locally, and is MIT licensed.

[English](README.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja-JP.md) · [Tiếng Việt](README.vi.md) · [한국어](README.ko-KR.md) · [Español](README.es-419.md) · [Português](README.pt-BR.md)

[Website](https://orcaagent.dev/) · [Changelog](https://orcaagent.dev/changelog/) · [Releases](https://github.com/echoVic/blade-deepseek/releases/latest) · [npm](https://www.npmjs.com/package/@blade-ai/orca)

## Install

```bash
npm install -g @blade-ai/orca
```

Or install the native binary directly:

```bash
curl -fsSL https://orcaagent.dev/install.sh | sh
```

The npm package supports macOS and Linux on ARM64 and x64. Prebuilt archives
are also available from [GitHub Releases](https://github.com/echoVic/blade-deepseek/releases/latest).

## Use

```bash
export DEEPSEEK_API_KEY=sk-...

orca                                      # open the TUI
orca exec "fix the failing test"          # run headlessly
orca exec --verifier "cargo test" "fix it" # verify before finishing
orca --mode=acp                           # connect an ACP client
```

In the TUI, `@` searches files, skills, plugins, and MCP resources. Use
`/plan` for read-only planning, `/goal` for a persistent objective,
`/workflows` for background work, and `/trust` to manage the current folder's
sandbox permissions.

## What it does

- Uses DeepSeek's reasoning and tool-use semantics directly, with SSE streaming,
  prefix-cache-friendly prompts, automatic context management, and retry logic.
- Reads, searches, edits, and writes code; runs shell commands; and can verify
  the result with a command you choose.
- Gates risky actions with `suggest`, sandboxed `auto-edit`, full-access
  `full-auto`, and read-only `plan` modes, plus per-folder trust.
- Saves local conversation history with resume, fork, search, rename, archive,
  and compression support.
- Runs persistent goals without a fixed turn ceiling, plus subagents and
  JavaScript workflows for longer tasks that need continuation or parallel work.
- Loads project instructions, skills, plugins, custom tools, MCP tools, and MCP
  resources after the workspace is trusted.
- Exposes stable JSONL, app-server, and Agent Client Protocol (ACP) contracts
  for editors, harnesses, and CI.

Configuration priority is environment variables, CLI arguments, config files,
then defaults. Run `orca --help` or `orca exec --help` for the full command
surface. User configuration lives at `~/.orca/config.toml`; trusted projects
can also provide `.orca/config.toml`, `AGENTS.md`, rules, skills, and workflows.

More detail:

- [Persistent Goal Mode](docs/goal-mode.md)
- [Harness and app-server contract](docs/harness-contract.md)
- [Dynamic workflow design](docs/claude-code-workflow-parity.md)
- [Production roadmap](docs/production-roadmap.md)

## Community

- QQ group: `472309526`
- [Telegram](https://t.me/+11No1w5ZbTMyZTQ1)

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) before contributing. Open an issue first
for large or compatibility-sensitive changes.

- [Report a bug](https://github.com/echoVic/blade-deepseek/issues/new?template=bug_report.yml)
- [Request a feature](https://github.com/echoVic/blade-deepseek/issues/new?template=feature_request.yml)
- [Ask for help](SUPPORT.md)
- [Report a vulnerability](SECURITY.md)

## License

[MIT](LICENSE)
