# PRD: Orca Codex-Style Harness

## 摘要
- **产品定位**: Orca 是 Blade 项目下的 DeepSeek-native 本地 coding agent runtime。
- **第一版目标**: 先交付 `orca exec` headless harness contract，让外部 CI、benchmark 和自动化系统可以稳定驱动 Orca。
- **核心用户**: agent 开发者、自动化评测用户、需要本地 coding agent 的工程师。
- **核心价值**: 把 DeepSeek reasoning/tool-use、权限审批、事件轨迹、预算、验证结果变成可审计、可回放、可评分的运行契约。
- **非目标**: 不复刻完整 Blade TS 功能，不先做 Web UI/VSCode/完整 MCP/skills/subagents。

---

## 1. 背景与问题
现有 Blade 已经证明了 CLI agent 的产品方向，具备 headless、jsonl、permission mode、tools、MCP、memory、skills、subagents、ACP 等经验。但如果目标是对标 Codex CLI，并做最适合 DeepSeek 的 agent，新项目不应是 Blade v1 的语言重写，而应是新的 Rust-first runtime。

社区对 harness 的理解也已经超过“测试脚本”或“JSONL 输出”。在 coding agent 场景里，harness 实际决定了模型如何接触计算机、如何被约束、如何恢复、如何被评测。工具接口、sandbox、审批、事件轨迹、预算、验证器都会改变 agent 表现。

Orca 第一版要先把这个运行契约打硬，后续 TUI、MCP、skills、ACP、eval suite 都基于它扩展。

## 2. 用户与场景
### 2.1 用户画像
- **Agent 开发者**: 需要可调试、可复现的事件流来观察 agent loop。
- **CI/Benchmark 用户**: 需要以脚本方式运行任务，收集退出码、事件、验证结果。
- **本地开发者**: 需要在仓库里安全执行修复、分析、验证任务。

### 2.2 核心场景
1. **非交互执行**
   用户运行 `orca exec "修复这个测试"`，Orca 在当前仓库执行 agent loop，并输出 JSONL。
2. **自动化评测**
   外部 harness 提供 workspace、prompt、budget、approval policy 和 verifier，Orca 输出可评分事件流。
3. **权限审批**
   当 agent 请求写文件或运行危险命令时，Orca 输出 approval request；外部系统决定 allow/deny/abort。
4. **可回放调试**
   用户根据事件日志还原一次失败任务，定位是模型、工具、审批、预算还是验证失败。

## 3. 功能需求
### FR-1: `orca exec` headless 入口
- 支持 `orca exec <prompt>`。
- 支持从 stdin 读取 prompt。
- 支持 `--output-format jsonl|text`，第一版默认 jsonl 或明确要求 jsonl。
- 支持 `--cwd <path>` 指定 workspace。
- 支持 `--max-turns <n>`、`--timeout <duration>` 这类预算参数。

### FR-2: JSONL 事件流
事件必须一行一个 JSON 对象，稳定、可解析、可版本化。第一版至少覆盖：
- `session.started`
- `turn.started`
- `assistant.reasoning.delta`
- `assistant.message.delta`
- `tool.call.requested`
- `tool.call.completed`
- `approval.requested`
- `approval.resolved`
- `verification.started`
- `verification.completed`
- `error`
- `session.completed`

### FR-3: Approval contract
- 支持权限模式: `read-only`、`workspace-write`、`full-auto`。
- 支持 allow/deny 决策结果进入事件流。
- 不允许危险操作绕过 approval contract。
- 非交互场景下，approval policy 必须可预测: deny、auto-allow-safe、abort-on-request 等策略要明确。

### FR-4: Agent-computer interface
第一版工具少而硬，优先支持：
- `read_file`
- `list_files`
- `grep`
- `edit`
- `bash`
- `git_status`

工具返回必须适合模型理解：
- 大输出截断并记录截断状态。
- 空输出要显式表达。
- 命令失败要包含 exit code、stdout、stderr 摘要。
- edit 失败要可恢复，不直接破坏文件。

### FR-5: DeepSeek reasoning/tool-use 状态
- reasoning 与普通 assistant text 分开记录。
- tool call 前后的 reasoning 状态必须保留协议位置。
- 事件模型要支持后续 thinking mode 的 reasoning 回灌，不需要重构。

### FR-6: Verification contract
- 支持传入 verifier 命令或让 agent 产出验证计划。
- verification 结果必须进入 JSONL 和最终状态。
- 若验证失败，最终状态为 `verification_failed`，不得伪装成 success。

### FR-7: 运行预算与指标
至少记录：
- turns
- tool calls
- approvals
- failures/retries
- wall time
- token/usage 占位
- final status

## 4. 非功能需求
- **可复现**: 同一次 run 的输入、policy、events、结果可被外部保存和比较。
- **可扩展**: 后续支持 TUI、MCP、ACP、eval harness 不推翻核心事件模型。
- **安全**: 文件写入和 shell 执行都有权限边界。
- **性能**: Rust-first，避免 TUI render 或文本输出阻塞 agent loop。
- **兼容**: 保留与 Codex-style headless/jsonl/approval 协议的互操作可能。

## 5. 验收标准
- `orca exec --output-format jsonl "..."` 可以输出合法 JSONL。
- 外部脚本能根据退出码和 `session.completed.status` 判断运行结果。
- approval request 能在 JSONL 中被捕获，并根据 policy 进入 resolved/abort。
- 工具调用事件包含 request、result、duration、status。
- verification 失败时最终状态不是 success。
- DeepSeek reasoning 与普通 message 分离为不同事件类型。

## 6. 参考与约束
- 参考 `/Users/bytedance/Documents/GitHub/blade` 的 headless/jsonl/permission/session 经验。
- 参考 Codex CLI/codex-rs 的本地 Rust agent、exec/headless、sandbox/approval 思路。
- 参考 Claude Code SDK/headless permissions 对非交互权限策略的处理。
- 参考 SWE-agent/ACI 对工具形态影响 agent 能力的强调。
- 参考 SWE-bench、Terminal-Bench、OpenHands 的 benchmark/runtime/evaluation harness 范式。
- 参考 DeepSeek-TUI 的 DeepSeek terminal agent 方向。
- 参考 Reasonix 对 DeepSeek prefix cache、成本和延迟优化的关注。

