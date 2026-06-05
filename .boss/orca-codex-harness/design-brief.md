# 设计简报: Orca Codex-Style Harness

## 一句话描述
Orca 是 Blade 项目下专注 DeepSeek 的本地 coding agent runtime；第一版优先提供兼容 Codex 风格的 headless/jsonl/approval 运行契约，让它能被 CI、benchmark、外部调度器和未来 Blade harness 稳定驱动。

## 目标用户
- 主要用户: 需要在本地仓库中运行 AI coding agent 的开发者、agent 研发者和自动化评测/CI 使用者。
- 用户特点: 关心稳定可复现的 agent 执行、可审计事件流、权限边界、长任务恢复，以及 DeepSeek reasoning/tool-use 的正确处理。

## 核心场景
1. 用户在终端运行 `orca exec "..."`，Orca 以非交互方式执行完整 agent loop，并输出机器可消费的 JSONL 事件流。
2. 用户在 CI 或 benchmark harness 中调用 Orca，外部系统可以根据事件流判断工具调用、审批请求、失败恢复、最终结果和验证状态。
3. 用户在需要人工介入时，通过 approval 契约控制文件写入、命令执行等高风险操作，而不是让 agent 无约束运行。
4. 用户对比 Blade、Codex CLI、DeepSeek-TUI、Reasonix 等路线时，可以用统一 headless/harness contract 评估启动速度、长任务稳定性、工具成功率、验证质量和成本。

## 功能范围
### 第一版必须有
- 提供 `orca exec` headless 模式，支持单 prompt 任务执行入口。
- 输出稳定 JSONL 事件流，覆盖 session/task start、assistant reasoning/text、tool call、tool result、approval request、error、verification、final result。
- 支持 approval mode，用统一事件表达需要用户或上层 harness 决策的操作。
- 支持外部 harness 可判定的退出码和最终状态，包括 success、failed、cancelled、approval_required、verification_failed。
- 将 DeepSeek reasoning/tool-use 视为事件流中的一等状态，为后续 thinking mode、reasoning 回灌和 tool-call 约束留出协议位置。
- 明确 agent-computer interface: 工具暴露、文件查看、搜索、编辑、命令输出、空输出、失败恢复都必须经过 runtime 设计，而不是直接把普通 shell/UI 暴露给模型。
- 支持运行预算与可观测性: 至少记录 turns、tool calls、wall time、token/usage 占位、approval 次数、失败次数和最终验证状态。
- 为 sandbox/环境隔离预留接口: 第一版可以先本地执行，但协议要能表达 workspace-write、read-only、danger/full-auto 等运行策略，后续接 Docker/远程 sandbox 不推翻事件模型。
- 为 benchmark adapter 预留 contract: 外部可把 SWE-bench、Terminal-Bench、repo-local eval 任务映射为 prompt、workspace、budget、approval policy、verifier，再从 JSONL 和退出码收集结果。
- 基于 `/Users/bytedance/Documents/GitHub/blade` 的经验复用产品语义: headless、jsonl、permission mode、tools、memory、MCP、ACP、skills/subagents，但第一版只抽取契约和行为经验。
- 参考 Codex CLI 的本地终端 agent 形态和 exec/headless 契约，参考 DeepSeek-TUI 的 DeepSeek terminal agent 方向，参考 Reasonix 对 DeepSeek prefix cache 和成本/延迟优化的关注点。

### 明确不做（留给后续版本）
- 第一版不做完整 Blade TS 功能复刻。
- 第一版不优先做 Web UI、VSCode extension、完整 MCP marketplace、skills 生态或 subagent 编排。
- 第一版不把 harness 绑定成只能服务某一个评测平台；先提供通用 headless/jsonl/approval contract。
- 第一版不做多模型优先适配；DeepSeek-native 是主线，其他 provider 只能作为后续兼容层。

## 成功标准
- 外部脚本可以只通过 `orca exec`、退出码和 JSONL 事件流，可靠判断一次 agent 任务的过程与结果。
- JSONL 事件格式稳定、可回放、可用于 benchmark 统计，不依赖 TUI 渲染。
- approval 请求能被外部 harness 捕获并决策，危险操作不会绕过权限契约。
- DeepSeek reasoning/tool-call 的关键状态不会丢失，未来接入 thinking mode 时不需要推翻事件模型。
- 与 Blade 当前 headless/jsonl/permission 经验相比，Orca 的第一版目标更克制: 先把运行契约打硬，再扩展 UI 和生态功能。

## 用户原话
> [$brainstorming] 基于/Users/bytedance/Documents/GitHub/blade改造、参考开源的 codex cli、deepseek tui、和 Reasonix。天然集成 harness
>
> 先 B，另外你要去搜索大家在 harness 这里的探索和理解再决定

## 项目现状
- 技术栈: 当前 `blade-deepseek` 是 Rust 2024 edition 的最小 CLI 项目，crate/package 名为 `blade-deepseek`，bin 名为 `orca`。
- 项目结构: 单体 Rust CLI 骨架，当前只有 `Cargo.toml`、`README.md`、`src/main.rs`。
- 已有功能: `orca --help`、`orca --version` 占位 CLI；尚未实现 agent runtime。
- 参考母体: `/Users/bytedance/Documents/GitHub/blade` 是 TypeScript/Bun monorepo，已有 CLI、Ink UI、headless/jsonl、permission mode、MCP、memory、skills、subagents、ACP、工具系统和 Web server 经验。
- 新功能定位: 新增独立 Rust 主线，不是 Blade TS 代码的逐行重写；从 Blade 抽取产品经验和协议经验，构建 DeepSeek-native Codex-style runtime。

## 补充信息
- 参考产品: OpenAI Codex CLI/codex-rs、DeepSeek-TUI、Reasonix。
- 用户偏好: 专注 DeepSeek，不做通用 provider 聚合器；命名为 `blade-deepseek / Orca / orca`。
- 约束: 第一阶段选择兼容 Codex 风格 harness，即优先 headless/jsonl/approval/可审计事件流，而不是先做完整内置 harness 或旧 Blade 互通。
- 外部调研结论: 社区里的 harness 更接近“可自动化、可评测、可审计、可复现的 agent 运行系统”，不只是测试夹具或 JSONL 输出。对 Orca 来说，harness contract 应成为 runtime 对外边界: 标准输入、事件输出、权限审批、退出状态、可回放日志、sandbox 策略、运行预算、验证器和 benchmark 指标。

## 社区 harness 范式调研
- Codex CLI 范式: `exec` 非交互模式面向 CI/自动化，重点不是聊天 UI，而是 sandbox/approval、JSONL 事件、结构化输出、session resume、退出码和脚本集成。Orca 应学习其 headless contract，但事件 schema 要保留 DeepSeek reasoning/tool-use 的一等字段。
- Claude Code/Agent SDK 范式: permissions 是运行系统的一部分，包含 hooks、deny、permission mode、allow、runtime callback 等顺序；headless 场景尤其需要 `dontAsk`/显式 allow/deny 这类可预测策略。Orca 的 approval 不能只是提示用户点确认，而要能被外部 harness 程序化决策。
- SWE-agent/ACI 范式: harness 的核心是 agent-computer interface，工具形态会直接影响成功率。文件查看窗口、搜索摘要、编辑校验、空输出提示这些细节都是能力，不是 UI 小事。Orca 第一版工具要少，但每个工具都必须为模型可用性设计。
- Terminal-Bench/SWE-bench 范式: evaluation harness 通常包含任务描述、可复现环境、自动验证脚本、oracle/reference、运行预算、日志与结果聚合。Orca 不必第一版内置这些 benchmark，但 `orca exec` 必须容易被这类 harness 调用和评分。
- OpenHands 范式: production scaffold 把 agent、runtime/sandbox、controller、history、metrics、user simulation、evaluation output 组织成一条管线；社区已经把 “fake user response / automated continuation” 当成评测 harness 的必要组件。Orca 的 headless 模式需要明确遇到澄清/审批/卡住时的自动化策略。
- 新近 harness research 范式: Harness-Bench、Agentic Harness Engineering、Natural-Language Agent Harnesses 都指向同一个判断: harness 配置会显著改变 agent 表现，且 tools、middleware、memory、budget、validation gates 比单纯 prompt 更可迁移。Orca 应把 harness policy 模块化、可观测、可 ablate，而不是埋在 controller 代码里。
- 社区经验总结: 一个可用的 coding-agent harness 至少包含 7 个面: task contract、ACI/tool contract、environment/sandbox contract、permission contract、event/trace contract、budget/retry/resume contract、verification/artifact contract。

## 关键假设
- [假设] 第一版的主要使用者是 agent 开发者和自动化/CI/benchmark 用户，而不是普通终端聊天用户。
- [假设] 第一版以 `orca exec` 为主，交互式 TUI 可以并行设计但不作为 harness 成功标准的前置条件。
- [假设] “天然集成 harness”第一阶段等价于提供兼容 Codex-style 的外部运行契约，但设计时按完整 community harness 七面体预留边界，后续再考虑内置 eval harness 或连接 Blade ACP。
