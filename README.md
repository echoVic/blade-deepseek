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

### First-run onboarding

When the TUI starts without an effective API key, first-run onboarding follows exactly seven steps:
Welcome → Provider → API Key → Model → Theme → Review → Complete.
DeepSeek is the only production provider; development-only providers are not
shown. Model choices are `auto`,
`deepseek-v4-flash`, and `deepseek-v4-pro`. Theme choices are Auto, Dark,
Light, Solarized, and Catppuccin.

Before Review, the API key is draft-only. Pressing Esc before confirming Review
exits with zero writes, including when the Review page is open
but before pressing Enter. Setup performs no network validation. Confirming Review
writes the provider, model, and theme to `config.toml`, while the API key is
stored separately in `auth.json`. If either save fails, the selected values
remain applied to the current session. Complete reports only sanitized error categories.

### Doctor diagnostics

`/doctor` emits one safe, copyable diagnostics report from facts already
captured by the current TUI session. It includes the Orca version and platform,
terminal and multiplexer identity, effective color/background/theme,
notification and input posture, viewport/session/keybindings state, and bounded
frame metrics. It excludes secrets, raw environment values, conversation
transcripts, the current working directory, and absolute filesystem paths. The
command does not run shell commands, access files, or re-probe the terminal.

The optional FPS HUD is default-off and session-only:

- `/doctor fps` toggles it.
- `/doctor fps on` enables it.
- `/doctor fps off` disables it.

FPS is the rate of successful terminal output frames. `render-ms` is the actual
duration of `terminal.draw`, not scheduler wake-up time or the interval between
frames.

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

### Custom keybindings

The TUI loads personal keybindings from `~/.orca/keybindings.json`, or from
`$ORCA_HOME/keybindings.json` when `ORCA_HOME` is set. There is deliberately no
project-local keybindings file, so opening a repository cannot change your
terminal controls.

```json
{
  "version": 1,
  "bindings": {
    "global.open-transcript-search": ["ctrl+f", "ctrl+x ctrl+f"],
    "idle.submit": ["ctrl+s"],
    "running.interrupt": ["esc", "ctrl+g"],
    "approval.confirm": ["enter"]
  }
}
```

Each listed action replaces its built-in bindings; omitted actions keep their
defaults, and an empty array disables an action. A sequence contains one to
four space-separated strokes, such as `ctrl+x ctrl+f`. Chords have a fixed
1 second timeout. Binding contexts are `global`, `idle`, `running`, and
`approval`.

Available action IDs:

```text
global.cancel
global.open-transcript-search
global.toggle-shortcuts
global.scroll-bottom
global.scroll-top
global.clear-screen

idle.submit
idle.newline
idle.edit-latest-queued
idle.history-previous
idle.history-next
idle.scroll-up
idle.scroll-down
idle.page-up
idle.page-down
idle.half-page-up
idle.half-page-down
idle.backtrack
idle.expand-tool-output

running.background-current-turn
running.interrupt
running.submit-queued
running.newline
running.edit-latest-queued
running.scroll-up
running.scroll-down
running.page-up
running.page-down
running.half-page-up
running.half-page-down

approval.select-allow
approval.select-deny
approval.toggle-selection
approval.confirm
```

`global.cancel` must keep an immediate single-stroke binding. Configurable
global bindings use function keys or modified character keys, which prevents
them from shadowing fixed modal controls. Approval direct keys
`1/2/3/4/y/a/A/n/d` are fixed and reserved.

The TUI checks the file every 500 ms and performs a live reload without
blocking input or rendering. A valid edit swaps the complete keymap atomically.
An invalid edit reports one notice and keeps the last-known-good keymap.
Deleting the file restores all built-in bindings.

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
