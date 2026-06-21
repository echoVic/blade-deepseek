# Orca 生产级路线图

> 目标：从 MVP 升级为生产级 DeepSeek Native Agent Runtime
> 参考实现：[Codex CLI](https://github.com/openai/codex) (Rust) + Claude Code (TypeScript)

---

## 三方能力对比

| 维度 | Orca (当前) | Codex CLI | Claude Code |
|------|-------------|-----------|-------------|
| 工具数 | 7 | 数十 (含沙箱执行) | 30+ |
| 沙箱隔离 | ✅ macOS Seatbelt | ✅ Seatbelt/Landlock/bwrap/Windows | ✅ sandbox-runtime |
| Token 计数 | ✅ tiktoken BPE | 真正 tokenizer | tiktoken 估算 |
| Context 管理 | 单阶压缩 | 本地+远端双阶压缩 | 自动/手动/micro 三级 |
| MCP 协议 | ❌ 无 | ✅ 完整客户端+服务器 | ✅ 多传输协议 (stdio/SSE/WS) |
| Hooks/扩展 | ❌ 无 | ✅ hooks runtime | ✅ 28种 hook 事件 |
| 多 Provider | Mock + DeepSeek | OpenAI + Ollama + LMStudio + AWS | Anthropic + Bedrock + Vertex |
| 并行 Agent | ❌ 深度=1 同步 | ✅ Multi-agent graph | ✅ Swarm/Coordinator |
| 项目指令 | ✅ AGENTS.md 多层加载 | ✅ AGENTS.md | ✅ CLAUDE.md 多层加载 |
| 费用追踪 | ✅ usage + 预算上限 | ✅ token usage | ✅ 完整费用+限额+预算 |
| 配置 Schema | 简单 TOML | JSON Schema (154KB) | 多层 settings |
| Slash 命令 | ❌ 无 | ✅ 丰富 | ✅ 60+ 命令 |
| Markdown TUI | ✅ 基础渲染 | ✅ 103KB 高级渲染 | ✅ Ink React |
| Diff 展示 | ❌ 无 | ✅ 95KB diff render | ✅ structured diff |
| 权限规则 | ✅ TOML allow/deny 规则 | TOML 细粒度规则 | pattern-match 规则系统 |
| 会话管理 | ✅ JSONL+resume+fork | ✅ SQLite thread store | ✅ JSONL+branch+fork |
| 子代理类型 | ✅ 5种专用类型 | ✅ 多 agent 图 | ✅ 内置代理+自定义 |
| 验证系统 | ✅ --verifier 命令 | ❌ 无独立验证 | ❌ 无独立验证 |
| 事件系统 | ✅ 14种结构化事件 | ✅ 类似 | ✅ hook 事件流 |

---

## Phase 1: 安全与信任基础

优先级最高。用户信任 = 安全边界 + 透明度。

### 1.1 Token 计数器

**目标**: 替换 `chars/4` 启发式，提供精确 token 计量。

**当前状态**: 已引入 `TokenCounter` 抽象并使用 `tiktoken-rs` BPE 后端替换 `chars/4` 启发式。

**设计**:
- 集成 `tiktoken-rs` 或实现 DeepSeek BPE tokenizer
- `TokenCounter` trait 抽象，支持不同模型的 tokenizer
- 更新 `provider/context.rs` 使用真实计数
- 每轮记录 input_tokens / output_tokens / cache_tokens

**影响文件**:
- `src/provider/context.rs` — 替换 `estimate_tokens()`
- `src/provider/mod.rs` — 解析 API 响应中的 usage 字段
- `Cargo.toml` — 新增 tiktoken-rs 依赖

### 1.2 项目指令文件 (AGENTS.md)

**目标**: 支持项目级和用户级自定义指令，注入到 system prompt。

**当前状态**: 已实现用户级、项目级、项目规则文件加载和 `@include` 展开，并注入 CLI/TUI/subagent system prompt。

**加载层级** (优先级递增):
1. `~/.orca/AGENTS.md` — 用户全局指令
2. `<project_root>/AGENTS.md` — 项目指令
3. `<project_root>/.orca/rules/*.md` — 项目细粒度规则

**设计**:
- 启动时递归向上查找项目根 (`.git`, `Cargo.toml`, `package.json` 等标记)
- 合并所有层级的指令内容
- 追加到 system prompt 末尾，以 `<project-instructions>` 标记包裹
- 支持 `@include ./path` 语法引用其他文件

**影响文件**:
- 新增 `src/runtime/instructions.rs`
- `src/runtime/agent_common.rs` — `build_agent_system_prompt()` 接受 instructions 参数
- `src/tui/bridge.rs` + `src/runtime/controller.rs` — 传递 instructions

### 1.3 费用追踪

**目标**: 实时显示 token 消耗和估算费用。

**当前状态**: 已实现 provider usage 解析、`usage.updated` 事件、TUI 状态栏展示、history `session.usage` 持久化，并支持 `--max-budget` 预算上限。

**设计**:
- `CostTracker` struct: 累计 input/output/cache tokens + USD 估算
- 从 API 响应的 `usage` 字段提取真实数据
- TUI 状态栏显示当前会话累计费用
- 会话结束时写入 history record (`session.usage`)
- 支持 `--max-budget` 参数设置费用上限

**影响文件**:
- 新增 `src/runtime/cost.rs`
- `src/provider/streaming.rs` — 解析 usage 字段
- `src/tui/ui.rs` — 状态栏展示
- `src/event/schema.rs` — 新增 usage 事件

### 1.4 细粒度权限规则

**目标**: 支持基于工具名+路径 pattern 的 allow/deny 规则配置。

**当前状态**: 已实现 `config.toml` 权限规则解析，并在 CLI/TUI 审批路径中按 `deny > allow > mode default` 执行。

**设计**:
```toml
# ~/.orca/config.toml
[[permissions.allow]]
tool = "bash"
pattern = "cargo *"

[[permissions.allow]]
tool = "edit"
pattern = "src/**"

[[permissions.deny]]
tool = "bash"
pattern = "rm -rf *"

[[permissions.deny]]
tool = "write_file"
pattern = "/etc/**"
```

- 规则优先级: deny > allow > mode default
- 支持 glob pattern 匹配
- 运行时缓存编译后的 pattern

**影响文件**:
- `src/approval/policy.rs` — 扩展为规则引擎
- `src/config/mod.rs` — 解析 permissions 配置
- 新增 `src/approval/rules.rs`

### 1.5 macOS Seatbelt 沙箱

**目标**: bash 工具执行时通过 `sandbox-exec` 限制文件系统和网络访问。

**当前状态**: 已实现 macOS `sandbox-exec` 执行路径，限制工作目录外写入；非 macOS 平台保留普通 shell 执行降级。

**设计**:
- 生成动态 Seatbelt profile (`.sb` 格式)
- 允许: 工作目录读写、`/tmp` 读写、系统库读取、网络出站
- 拒绝: 工作目录外写入、`~/.ssh`/`~/.orca` 等敏感路径
- `bash` 工具通过 `sandbox-exec -f <profile> sh -c <cmd>` 执行
- 非 macOS 平台优雅降级 (跳过沙箱)

**影响文件**:
- 新增 `src/sandbox/mod.rs`, `src/sandbox/seatbelt.rs`
- `src/tools/bash.rs` — 集成沙箱执行路径

---

## Phase 2: 协议与生态扩展

打开生态边界，对接标准协议。

### 2.1 MCP 客户端

**目标**: 作为 MCP 客户端连接外部 MCP Server，自动注册其工具。

**设计**:
- 支持 stdio 和 SSE 两种传输协议
- 配置来源: `~/.orca/config.toml` 的 `[[mcp_servers]]` 段
- 启动时连接所有配置的 MCP Server
- 将 MCP 工具合并到 tool schema（前缀 `mcp__<server>__<tool>`）
- 工具调用时路由到对应 MCP Server

**影响文件**:
- 新增 `src/mcp/` 模块 (client.rs, transport.rs, types.rs)
- `src/provider/tool_schema.rs` — 合并 MCP 工具
- `src/tools/mod.rs` — MCP 工具路由
- `Cargo.toml` — 新增 tokio (async runtime for SSE)

### 2.2 多 Provider 抽象

**目标**: 支持 OpenAI-compatible API / Ollama / 其他 DeepSeek 兼容端点。

**设计**:
```rust
pub trait ModelProvider: Send + Sync {
    fn call_streaming(&self, conversation: &Conversation, cancel: &CancelToken, callback: StreamCallback) -> ProviderResponse;
    fn model_info(&self) -> ModelInfo;
}
```

- `DeepSeekProvider` — 现有实现迁移
- `OpenAICompatProvider` — 兼容 OpenAI chat/completions 格式
- `OllamaProvider` — 本地 Ollama 模型
- Provider 选择: 通过 `--provider` 参数或 config 中的 `provider = "ollama"` 指定

**影响文件**:
- `src/provider/mod.rs` — 提取 trait
- 新增 `src/provider/openai_compat.rs`, `src/provider/ollama.rs`
- `src/config/mod.rs` — provider 配置

### 2.3 Hooks 系统

**目标**: 在关键生命周期节点执行用户自定义 shell 命令。

**Hook 事件**:
- `pre_tool_use` — 工具执行前 (可阻止)
- `post_tool_use` — 工具执行后
- `session_start` — 会话开始
- `session_end` — 会话结束
- `pre_model_call` — 模型调用前，可注入 system prompt 片段
- `post_model_call` — 模型调用后，可观测 token 使用
- `on_budget_warning` — 上下文即将溢出
- `pre_compact` — 上下文压缩前
- `post_compact` — 上下文压缩后

**配置**:
```toml
# ~/.orca/config.toml
[[hooks]]
event = "post_tool_use"
tool = "bash"
command = "echo 'bash executed: $ORCA_TOOL_TARGET' >> ~/.orca/audit.log"

[[hooks]]
event = "session_end"
command = "notify-send 'Orca session completed'"
```

Hook 成功退出时可通过 stdout 返回 JSON：`{"action":"allow"}`、`{"action":"deny","reason":"..."}`、`{"action":"modify","modified_target":"..."}`、`{"action":"inject","context":"..."}`。非 JSON stdout 继续按上下文注入处理。

**影响文件**:
- 新增 `src/runtime/hooks.rs`
- `src/runtime/controller.rs` + `src/tui/bridge.rs` — 注入 hook 调用点

### 2.4 Slash 命令框架

**目标**: TUI 内通过 `/` 前缀命令触发内置操作。

**初始命令集**:
- `/help` — 显示可用命令
- `/model <name>` — 运行时切换模型
- `/compact` — 手动触发上下文压缩
- `/clear` — 清屏
- `/cost` — 显示当前会话费用
- `/history` — 列出最近会话
- `/mode <suggest|auto-edit|full-auto>` — 切换审批模式
- `/plan` / `/plan off` — 进入或退出只读计划模式
- `/goal [<objective>|edit|pause|resume|clear]` — 管理持久化长期目标
- `/workflows` — 查看后台 workflow 任务
- `/remember <note>` — 写入用户或项目记忆
- `/exit` — 退出

**设计**:
- 输入框检测 `/` 前缀
- `SlashCommand` trait: `name()`, `description()`, `execute(&mut AppState)`
- Tab 自动补全

**影响文件**:
- `crates/orca-tui/src/commands/mod.rs` — slash 命令解析和菜单
- `crates/orca-tui/src/app.rs` — Submit 前检测 slash 命令并分发 TUI action

### 2.5 Web Search 工具

**目标**: 内置网络搜索能力。

**设计**:
- 使用 SearXNG 或 Brave Search API
- 返回 top-N 结果的标题+摘要+URL
- 可选通过 MCP 集成而非内置

**影响文件**:
- 新增 `src/tools/web_search.rs`
- `src/provider/tool_schema.rs` — 注册 schema

---

## Phase 3: Agent 智能升级

提升 Agent 的自主能力和上下文利用效率。

### 3.1 并行子代理

**目标**: 支持多个子代理并发执行，结果汇聚后继续主对话。

**设计**:
- 移除 `MAX_SUBAGENT_DEPTH = 1` 硬限制，改为可配置 (默认 2)
- 主 agent 可一次 dispatch 多个 subagent tool call
- 使用 `tokio::spawn` + `JoinSet` 并发执行
- 结果按顺序收集后合并为 tool results
- 并发限制: 最大 4 个并行子代理

**影响文件**:
- `src/runtime/subagent.rs` — 并行执行逻辑
- `src/runtime/controller.rs` + `src/tui/bridge.rs` — 并行调度
- `Cargo.toml` — 确保 tokio multi-thread runtime

### 3.2 远端 Compaction (LLM 摘要压缩)

**目标**: 用 LLM 对旧上下文生成摘要，替代简单截断。

**设计**:
- 当 `needs_compaction()` 触发时:
  1. 提取需要压缩的消息段
  2. 调用 LLM (可用更轻量模型如 deepseek-v4-flash) 生成摘要
  3. 摘要作为 System 消息插入，替代原始消息
- 本地截断作为 fallback (LLM 调用失败时)
- 摘要持久化到 history (新 record type: `context.summary`)

**影响文件**:
- `src/provider/context.rs` — 新增 `compact_with_summary()`
- `src/runtime/history.rs` — 新增 summary record

### 3.3 Plan Mode

**目标**: 只读模式，Agent 只能使用读取类工具，用于方案设计。

**设计**:
- 新增 `ApprovalMode::Plan`
- Plan 模式下: Read 允许，Write/Shell 全部 Deny
- TUI 中通过 `/plan` 进入，`/plan off` 退出
- Plan 模式下 system prompt 追加 "你当前处于只读模式，只能分析和规划，不能修改文件"

**影响文件**:
- `src/approval/policy.rs` — 新增 Plan 模式
- `src/tui/commands/` — `/plan` 命令
- `src/runtime/agent_common.rs` — Plan mode system prompt

### 3.4 Agent Memory (跨会话记忆)

**目标**: 自动从对话中提取关键信息，跨会话持久化。

**设计**:
- 存储: `~/.orca/memory/` 目录
  - `user.md` — 用户偏好 (手动通过 `/remember` 写入)
  - `projects/<hash>/memory.md` — 项目级记忆
- 会话结束时可选调用 LLM 提取 "值得记住的内容"
- 下次启动时注入到 system prompt

**影响文件**:
- 新增 `src/runtime/memory.rs`
- `src/runtime/agent_common.rs` — 注入 memory 到 prompt

### 3.4b Persistent Goal Mode

**目标**: 支持 Codex 风格 `/goal` 长期目标，目标跨 TUI 进程持久化，并在 active 状态下自动续跑。

**当前状态**: 已实现。目标以 session id 为 key 写入 `$ORCA_HOME/goals_1.json` 或 `~/.orca/goals_1.json`；TUI 支持 `/goal`、`/goal <objective>`、`/goal edit <objective>`、`/goal pause`、`/goal resume`、`/goal clear`；模型可通过 `update_goal` 工具标记 `complete` 或 `blocked` 来停止续跑。

**设计**:
- `ThreadGoalStatus`: `active`, `paused`, `blocked`, `usage_limited`, `budget_limited`, `complete`
- active goal 每个成功 turn 后自动提交 continuation prompt
- 每轮注入单一 pinned goal state，避免重复堆叠上下文
- 持久化 goal 依赖 recorded history；`--no-history` 会禁用 `/goal`

**影响文件**:
- `crates/orca-core/src/goal_types.rs`
- `crates/orca-runtime/src/goals.rs`
- `crates/orca-tools/src/update_goal.rs`
- `crates/orca-tui/src/app.rs`
- `crates/orca-tui/src/bridge.rs`

### 3.5 Auto-compact 优化

**目标**: 更智能的自动压缩策略。

**设计**:
- 分级压缩: micro-compact (工具输出截断) → local-compact (消息截断) → remote-compact (LLM 摘要)
- 工具输出超过 N tokens 时自动 micro-compact (保留首尾)
- 基于 token 预算动态调整保留策略

**影响文件**:
- `src/provider/context.rs` — 多级策略
- `src/tools/mod.rs` — 工具输出 micro-compact

---

## Phase 4: 体验打磨

面向日常使用的体验提升。

### 4.1 Diff 渲染

**目标**: edit/write_file 结果以 unified diff 着色展示。

**设计**:
- `similar` crate 计算 diff
- TUI 中以红/绿着色展示 +/- 行
- 支持 inline diff (行内变更高亮)

**影响文件**:
- 新增 `src/tui/diff.rs`
- `src/tui/ui.rs` — 工具输出渲染切换

### 4.2 File @mention

**目标**: 输入框中 `@path/to/file` 自动将文件内容注入到用户消息。

**设计**:
- 输入解析: 检测 `@` 后接路径
- Tab 补全: 文件路径模糊匹配
- 展开: 将 `@file` 替换为 `<file path="...">content</file>` XML 块

**影响文件**:
- `src/tui/app.rs` — 输入预处理
- 新增 `src/tui/mention.rs`

### 4.3 TUI 主题

**目标**: 支持暗色/亮色主题，可自定义配色。

**设计**:
- 自动检测终端背景色 (OSC 11 查询)
- 预设主题: `dark` (默认), `light`, `solarized`, `catppuccin`
- 配置: `~/.orca/config.toml` 中 `theme = "dark"`

**影响文件**:
- 新增 `src/tui/theme.rs`
- `src/tui/ui.rs` — 所有颜色引用改为 theme 查询

### 4.4 Vim 模式

**目标**: 输入框支持 vi 键绑定 (normal/insert/visual mode)。

**设计**:
- 基于 `tui-textarea` 的 vim 模式特性 (已内置支持)
- 配置: `vim_mode = true`
- Mode 指示器显示在输入框左侧

**影响文件**:
- `src/tui/app.rs` — 启用 vim mode
- `src/config/mod.rs` — vim_mode 配置项

### 4.5 自动更新检测

**目标**: 启动时检查是否有新版本可用。

**设计**:
- 异步检查 GitHub releases API (或 crates.io)
- 有新版本时在 TUI 底部显示提示
- 不阻塞启动

**影响文件**:
- 新增 `src/runtime/update_check.rs`
- `src/tui/ui.rs` — 更新提示渲染

### 4.6 Desktop 通知

**目标**: 长任务完成时发送系统通知。

**设计**:
- macOS: `osascript -e 'display notification'`
- Linux: `notify-send`
- 仅在 TUI 不在前台焦点时发送

**影响文件**:
- 新增 `src/runtime/notify.rs`
- `src/tui/bridge.rs` — 会话完成时触发

---

## 实施优先级矩阵

```
影响力 ↑
        │  P1.2 AGENTS.md  P2.1 MCP
        │  P1.5 Seatbelt   P3.1 并行Agent
        │  P1.1 Token计数  P2.2 多Provider
        │  P1.3 费用追踪   P3.2 远端Compact
        │  P1.4 权限规则   P2.4 Slash命令
        │  P2.3 Hooks      P3.3 Plan Mode
        │  P4.1 Diff渲染   P3.4 Memory
        │  P4.2 @mention   P2.5 WebSearch
        │  P4.3 主题       P4.4 Vim
        └──────────────────────────────→ 复杂度
            低                        高
```

## Phase 1 推荐启动顺序

1. **Token 计数器** — 所有后续功能（费用追踪、精确 compaction、远端压缩）的基础依赖
2. **AGENTS.md 项目指令** — 低复杂度高收益，用户定制性大幅提升
3. **费用追踪** — 构建用户信任，透明消耗
4. **细粒度权限规则** — 为沙箱做铺垫，先软后硬
5. **macOS Seatbelt** — 安全硬边界，生产必备

---

## 技术决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| Tokenizer | tiktoken-rs | DeepSeek 使用类 BPE 编码，tiktoken 兼容性好 |
| MCP 传输 | stdio 优先 | 最广泛支持，SSE 作为第二阶段 |
| 沙箱 | Seatbelt (macOS) → Landlock (Linux) | 平台原生，零依赖 |
| 并行 Runtime | tokio multi-thread | 已有 reqwest 依赖 tokio，复用 |
| Diff 库 | similar | 纯 Rust，无外部依赖，API 友好 |
| 配置格式 | 保持 TOML | 已有基础，不引入新格式 |
| 指令文件 | Markdown (.md) | 人类可读可编辑，git-friendly |
