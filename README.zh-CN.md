# Orca

为终端打造的 DeepSeek 原生编程智能体。

给 Orca 一个任务，它会读取代码、编辑文件、运行命令、验证结果，并持续工作，
直到任务完成或需要你的决定。交互式工作使用 TUI，脚本和 CI 使用 `orca exec`。
Orca 使用 Rust 构建，在本地运行，并采用 MIT 许可证。

[English](README.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja-JP.md) · [Tiếng Việt](README.vi.md) · [한국어](README.ko-KR.md) · [Español](README.es-419.md) · [Português](README.pt-BR.md)

[官网](https://orcaagent.dev/) · [更新日志](https://orcaagent.dev/changelog/) · [版本发布](https://github.com/echoVic/blade-deepseek/releases/latest) · [npm](https://www.npmjs.com/package/@blade-ai/orca)

## 安装

```bash
npm install -g @blade-ai/orca
```

也可以直接安装原生二进制文件：

```bash
curl -fsSL https://orcaagent.dev/install.sh | sh
```

npm 包支持 macOS 和 Linux 的 ARM64 与 x64 平台。也可以从
[GitHub Releases](https://github.com/echoVic/blade-deepseek/releases/latest) 下载预编译文件。

## 使用

```bash
export DEEPSEEK_API_KEY=sk-...

orca                                      # 打开 TUI
orca exec "修复失败的测试"                 # 无界面运行
orca exec --verifier "cargo test" "修复它" # 完成前执行验证
orca --mode=acp                           # 连接 ACP 客户端
```

在 TUI 中，`@` 可以搜索文件、Skills、Plugins 和 MCP Resources。
使用 `/plan` 进行只读规划，使用 `/goal` 管理持久目标，使用 `/workflows`
查看后台任务，使用 `/trust` 管理当前目录的沙箱权限。

### 首次启动设置

当 TUI 启动时未检测到有效 API 密钥，首次启动设置固定经过七步：
欢迎 → 服务商 → API 密钥 → 模型 → 主题 → 确认 → 完成。
DeepSeek 是唯一的生产服务商，不会显示仅供开发使用的服务商。模型可选 `auto`、
`deepseek-v4-flash`、`deepseek-v4-pro`；主题可选 Auto、Dark、Light、Solarized、
Catppuccin。

进入确认步骤前，API 密钥仅保存在草稿中；在之前任一步骤按 Esc 退出不会产生任何写入。
设置期间不进行网络验证。确认后，服务商、模型和主题写入 `config.toml`，API 密钥
单独写入 `auth.json`。任一保存失败时，所选值仍应用于当前会话。完成步骤仅显示不含敏感信息的错误类型。

### Doctor 诊断

`/doctor` 会根据当前 TUI 会话已经捕获的事实，输出一份安全且便于复制的诊断报告。
报告包含 Orca 版本与平台、终端与多路复用器、实际生效的颜色/背景/主题、通知与输入
能力、视口/会话/快捷键状态，以及有界的帧指标。报告不会包含 secrets、原始环境变量、
对话 transcript、当前工作目录或绝对文件路径；命令不会运行 Shell、读取文件或
re-probe 终端。

可选的 FPS HUD 默认为关闭（default-off），并且仅对当前会话生效
（session-only）：

- `/doctor fps` 切换显示状态。
- `/doctor fps on` 开启显示。
- `/doctor fps off` 关闭显示。

FPS 表示成功输出到终端的帧率；`render-ms` 表示实际 `terminal.draw` 耗时，
不是调度器唤醒时间或帧间隔。

## 核心能力

- 直接适配 DeepSeek 的推理和工具调用语义，支持 SSE 流式输出、前缀缓存友好提示词、
  自动上下文管理和请求重试。
- 读取、搜索、编辑和写入代码，运行 Shell 命令，并使用指定命令验证结果。
- 通过 `suggest`、沙箱内 `auto-edit`、完全访问 `full-auto` 和只读 `plan`
  模式控制风险，同时提供目录信任机制。
- 在本地保存对话历史，支持恢复、分叉、搜索、重命名、归档和压缩。
- 运行没有固定轮次上限的持久目标，并通过子智能体和 JavaScript 工作流处理长任务。
- 在工作区受信任后加载项目指令、Skills、Plugins、自定义工具、MCP 工具和资源。
- 为编辑器、测试框架和 CI 提供稳定的 JSONL、app-server 与 Agent Client
  Protocol（ACP）协议。

配置优先级依次为环境变量、命令行参数、配置文件和默认值。运行 `orca --help`
或 `orca exec --help` 查看完整命令。用户配置位于 `~/.orca/config.toml`；
受信任的项目还可以提供 `.orca/config.toml`、`AGENTS.md`、规则、Skills 和工作流。

### 自定义快捷键

TUI 从 `~/.orca/keybindings.json` 加载个人快捷键；设置 `ORCA_HOME` 后，路径为
`$ORCA_HOME/keybindings.json`。Orca 不读取项目内的快捷键文件，避免打开仓库时
被项目改变终端控制方式。

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

文件中出现的 action 会替换其内置绑定；未出现的 action 保留默认值，空数组会禁用
该 action。一个序列包含一到四个以空格分隔的按键，例如
`ctrl+x ctrl+f`。Chord 的固定超时时间为 1 second。可用 context 为
`global`、`idle`、`running` 和 `approval`。

可配置的 action ID：

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

`global.cancel` 必须保留至少一个可立即触发的单键绑定。自定义 Global 绑定只能使用
功能键或带修饰键的字符键，避免覆盖 Esc、Enter、方向键、Tab 等固定模态控制。
审批直接键 `1/2/3/4/y/a/A/n/d` 固定且保留。

TUI 每 500 ms 检查一次文件，并在不阻塞输入和渲染的前提下 live reload。
有效修改会原子替换完整 keymap；无效修改只提示一次，并继续使用
last-known-good keymap。删除文件会恢复全部内置绑定。

更多文档：

- [持久 Goal 模式](docs/goal-mode.md)
- [Harness 与 app-server 协议](docs/harness-contract.md)
- [动态工作流设计](docs/claude-code-workflow-parity.md)
- [生产路线图](docs/production-roadmap.md)

## 社区

- QQ 群：`472309526`
- [Telegram](https://t.me/+11No1w5ZbTMyZTQ1)

## 参与贡献

贡献前请阅读 [CONTRIBUTING.md](CONTRIBUTING.md)。对于较大或涉及兼容性的改动，
请先提交 Issue。

- [报告问题](https://github.com/echoVic/blade-deepseek/issues/new?template=bug_report.yml)
- [提出功能建议](https://github.com/echoVic/blade-deepseek/issues/new?template=feature_request.yml)
- [获取帮助](SUPPORT.md)
- [报告安全漏洞](SECURITY.md)

## 许可证

[MIT](LICENSE)
