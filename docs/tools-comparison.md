# 工具系统对比分析

**日期**: 2026-06-22
**Orca 版本**: v0.1.20
**对比对象**:
- Claude Code (`@anthropic-ai/claude-code`)
- Codex CLI
- Orca (`blade-deepseek`)

---

## 执行摘要

v0.1.20 基线下，Orca 的工具系统已经从早期的硬编码分发，升级为“规格驱动”的注册表模型。每个工具都有统一的 `ToolSpec`，包含名称、别名、JSON Schema、能力集合、展示方式、可见性和并发安全信息。运行时审批、TUI 展示、provider tool schema、MCP 工具、external tools、skills 工具和结构化用户输入都围绕同一套规格工作。

这次变化的核心目标是接近 Codex CLI 的工具系统思路：工具不是散落在 prompt、审批和执行器里的字符串列表，而是一个可以解析、过滤、授权、渲染和扩展的统一能力表。v0.1.17-v0.1.19 进一步补齐了 Markdown skills 和 TUI `request_user_input` answer loop，让模型既能显式加载可复用流程，也能在交互式会话中提出结构化澄清问题。

---

## 当前 Orca 工具清单

| 工具 | 类型 | 当前状态 | 说明 |
|------|------|----------|------|
| `read_file` | read | 已实现 | 读取 UTF-8 文件内容，输出按 8KB 截断 |
| `glob` | read | 已实现 | 首选文件发现工具，支持 glob pattern，返回 workspace-relative 路径 |
| `list_files` | read | 兼容 alias | 保留给旧 prompt 和历史会话，模型 prompt 不再优先推荐 |
| `grep` | read | 已实现 | 使用 ripgrep 搜索，空结果返回 `(no matches)` |
| `git_status` | read | 已实现 | `git status --short` |
| `web_search` | network | 已实现 | 网络搜索，按 network 能力走审批策略 |
| `bash` | shell | 已实现 | 通过 `sh -c` 执行命令，shell 能力需要对应审批 |
| `edit` | write | 已实现 | 精确文本替换 |
| `write_file` | write | 已实现 | 创建或覆盖文件 |
| `subagent` | agent | 已实现 | 同步子智能体，支持类型/模型参数和深度限制 |
| `Workflow` | agent | 已实现 | 启动 JavaScript 动态 workflow |
| `update_plan` | read/state | 已实现 | 更新可见计划状态 |
| `update_goal` | read/state | 已实现 | 仅 goal 上下文中可用，用于更新持久目标状态 |
| `request_user_input` | read/state | 已实现 | headless 模式确定性失败，TUI 模式可等待用户回答并继续同一轮 |
| `list_skills` | read | 已实现 | 列出用户和项目 Markdown skills |
| `read_skill` | read | 已实现 | 读取指定 skill 的 Markdown 指令内容 |
| MCP tools | dynamic | 已实现基础路由 | 配置的 MCP server 工具以 namespaced tool 暴露 |
| external tools | dynamic | 已实现 | `~/.orca/tools/*.toml` 或 `$ORCA_HOME/tools/*.toml` 描述符注册命令工具 |

---

## 与 Claude Code / Codex CLI 的设计对比

| 维度 | Claude Code | Codex CLI | Orca v0.1.20 |
|------|-------------|-----------|--------------|
| 工具定义 | 类型化 schema | 规格/能力驱动 | `ToolSpec` 规格驱动 |
| 文件发现 | `Glob` | 文件搜索工具优先 | `glob` 优先，`list_files` 兼容 |
| Shell | `Bash`，支持后台任务 | `exec_command`/shell session | `bash` 同步执行，后台任务待增强 |
| 文件写入 | `FileWrite`/`FileEdit` | patch/edit 类工具 | `write_file`/`edit` |
| 子代理 | 同步/异步能力 | 多代理/任务能力 | 同步 `subagent`，深度受配置限制 |
| 工作流 | workflow/task 能力 | 自动化/任务工具 | `Workflow` JS 动态 workflow |
| MCP | 支持 | 支持 | MCP 客户端工具路由已接入 |
| 自定义工具 | 插件/扩展 | MCP/插件 | TOML external tools + MCP |
| Skills | skills / slash 工作流 | Codex skills / plugins | Markdown `SKILL.md` discovery + `$skill` 显式注入 |
| 用户输入 | 交互式问题/审批 | `request_user_input` | TUI `request_user_input` answer loop |
| 审批 | 工具能力/策略 | 工具能力/策略 | 从 `ToolSpec.capabilities` 推导 |
| 上下文工具 | 按模式暴露 | 按模式暴露 | `update_goal` 等按 runtime context 过滤 |

---

## v0.1.20 基线的关键改进

### 1. Canonical Tool Registry

工具注册集中在 `crates/orca-tools/src/registry.rs`。注册表负责：

- 注册 built-in tools。
- 解析 canonical name 与 alias。
- 暴露 model-visible tool schema。
- 路由 built-in、MCP、external tool 调用。
- 为审批和 TUI 提供统一的工具规格。

`list_files` 现在是 `glob` 的兼容 alias：模型优先看到 `glob`，旧会话仍可请求 `list_files`。

### 2. Capability-based Approval

审批不再依赖手写工具名列表，而是通过工具能力推导：

| Capability | ActionKind |
|------------|------------|
| `FsRead`, `FsList`, `FsSearch`, `GitInspect`, `PlanUpdate`, `GoalUpdate` | read |
| `FsWrite` | write |
| `ShellExecute` | shell |
| `NetworkSearch` | network |
| `AgentDelegate`, `WorkflowRun` | agent |

这让内置工具、MCP 工具和 external tools 能共用同一审批语义。

### 3. Context-scoped Tools

不是所有工具在所有上下文都应该暴露。例如：

- `update_goal` 只应在 TUI goal turn 中给模型使用。
- 超过 subagent 深度限制时，`subagent` 不应先进入审批，而应由执行路径返回明确失败。
- 工作流、MCP、external tools 需要根据当前 runtime 配置决定是否可用。

v0.1.11 基线确认了这些上下文过滤路径。

### 4. Skills and Structured User Input

v0.1.17-v0.1.19 补齐了两个常用交互面：

- `list_skills` / `read_skill` 可发现和读取 `$ORCA_HOME/skills`、`~/.orca/skills` 和项目 `.orca/skills` 下的 Markdown skills。
- 当 prompt 明确提到 `$skill_id` 时，Orca 会把对应 `SKILL.md` 注入当轮模型上下文。
- `request_user_input` 是模型可见的结构化澄清工具；headless/jsonl 路径保持确定性，TUI 路径会展示问题和 choices，并把用户下一次 composer 提交作为工具结果继续同一轮。

### 5. Empty-result Semantics

读类工具的“没有结果”不再等同于失败：

- `glob` 没有匹配时返回 `(no matches)`。
- `list_files` 缺失或空目录返回 `(empty)`。
- `grep` 没有匹配时返回 `(no matches)`。

这避免子代理或主循环把正常探索过程误标为失败。

---

## 仍然存在的差距

| 能力 | 当前差距 | 建议优先级 |
|------|----------|------------|
| Shell 后台任务 | `bash` 仍是同步阻塞执行 | P1 |
| Bash 超时/PTY/session | 还没有 Codex CLI 风格的 `exec_command` + `write_stdin` 会话模型 | P1 |
| 图片/PDF/Notebook 读取 | `read_file` 仍以 UTF-8 文本为主 | P2 |
| Worktree 隔离 | 还没有自动 enter/exit worktree 工具 | P1 |
| 异步 subagent | 当前子代理是同步模式 | P1 |
| apply_patch freeform | 仍以 JSON `edit` / `write_file` 为主 | P2 |

---

## 后续路线建议

### 短期

1. 引入 Codex CLI 风格的 shell session 工具：`exec_command`、`write_stdin`、可选 PTY、超时和后台 session id。
2. 保留 `bash` 作为兼容 alias，逐步让 prompt 推荐新 shell 工具。
3. 为 `request_user_input` 增加更接近 Codex 的多问题结构和自动超时选项。

### 中期

1. 支持图片/PDF/Notebook 读取。
2. 支持 worktree 隔离工具。
3. 支持异步 subagent 和结果查询。
4. 将 `apply_patch` 做成 grammar-backed freeform tool。

### 长期

1. 延展 deferred tool discovery，让 MCP、插件、workflow 只在需要时展开。
2. 建立统一的工具结果类型和错误码。
3. 让工具系统、TUI、JSONL harness 和文档都从同一份 tool registry 派生。

---

## 相关实现文件

- `crates/orca-core/src/tool_types.rs` — `ToolName`、`ToolSpec`、capabilities、renderer、exposure。
- `crates/orca-tools/src/registry.rs` — built-in/MCP/external tool registry。
- `crates/orca-tools/src/glob.rs` — 首选文件发现工具。
- `crates/orca-tools/src/list_files.rs` — 兼容目录列表工具。
- `crates/orca-runtime/src/controller.rs` — headless runtime 工具执行和审批。
- `crates/orca-tui/src/bridge.rs` — TUI 工具执行、审批和 goal context。
- `crates/orca-tools/src/update_goal.rs` — 持久化 goal 状态更新工具。
- `crates/orca-tools/src/skills.rs` — Markdown skill discovery、读取和 prompt 注入格式化。
- `crates/orca-tui/src/types.rs` — TUI user input request 状态和事件。

---

## 结论

Orca v0.1.20 的工具系统已经完成从“硬编码工具枚举”到“规格驱动工具注册表”的关键迁移，并补上了 Markdown skills 与 TUI 结构化用户输入。接下来最值得投入的是 shell session/PTY、worktree 隔离、多格式读取和更完整的 app/server SDK 面。
