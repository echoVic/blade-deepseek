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
