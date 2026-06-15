# Agent Loop 实现计划：DeepSeek tool_calls + 多轮循环

## Context

当前 Orca 的 DeepSeek HTTP provider 是一个"哑"的单轮文本生成器：只发送一条 user 消息，只解析 `content` 和 `reasoning_content`，不发送 tools schema，不解析 `tool_calls`，不进行多轮对话。

目标：让 `orca exec --provider deepseek` 具备完整的 Agent 能力 — 注入工具定义、解析模型的 tool_calls、执行工具、将结果反馈给模型、循环直到任务完成或达到 max_turns。

参考：OpenAI Codex CLI (codex-rs) 的 agent loop 模式。

## 设计决策

1. **循环在 controller 层**：provider 保持无状态（单次 API call），controller 维护对话历史并驱动循环
2. **Conversation 数据结构**：新引入，承载完整对话历史（system/user/assistant/tool 消息）
3. **provider 函数签名升级**：新增 `call(kind, &Conversation) -> ProviderResponse`，旧 `plan()` 保留为兼容层
4. **Edit 工具重构**：直接支持结构化参数（path/old_text/new_text），不再依赖容易出错的 DSL 解析
5. **所有 provider 统一走 agent loop**：Mock/Fixture 第一轮后不返回 tool_calls，循环自然结束

## 实现步骤

### Step 1: 引入 Conversation 类型 + 工具 Schema

**新增文件**:
- `src/provider/conversation.rs` — `Conversation`, `Message`, `RawToolCall` 类型
- `src/provider/tool_schema.rs` — 6 个工具的 OpenAI-compatible JSON Schema 定义
- `src/provider/system_prompt.rs` — 系统提示模板

**修改文件**:
- `src/provider/mod.rs` — 声明新模块

验证：`cargo build`

### Step 2: 扩展 ToolRequest + 重构 Edit 工具

**修改文件**:
- `src/tools/mod.rs` — `ToolRequest` 新增 `raw_arguments: Option<String>` 字段
- `src/tools/edit.rs` — 支持从 `raw_arguments` JSON 中直接读取 path/old_text/new_text（保留 DSL fallback）

验证：`cargo test`，所有现有测试通过

### Step 3: 新增 ProviderResponse + 升级 provider 接口

**修改文件**:
- `src/provider/mod.rs` — 新增 `ProviderResponse` 结构体 + `call()` 函数，旧 `plan()` 包装为 `call()` 的适配
- `src/provider/deepseek_fixture.rs` — 适配到新接口（可选，或通过 mod.rs 适配层处理）

验证：`cargo test`，所有集成测试通过

### Step 4: 重写 DeepSeek HTTP Provider

**修改文件**:
- `src/provider/deepseek_http.rs` — 完整重写:
  - 请求支持 `tools` 数组 + 多消息格式（Conversation → ApiMessage 列表）
  - 响应解析 `tool_calls` 数组 + `finish_reason`
  - 参数映射：tool_call JSON arguments → ToolRequest（name + action + target/raw_arguments）
  - 正确回传 `reasoning_content`（DeepSeek 要求）

验证：设置 `DEEPSEEK_API_KEY`，手动运行 `cargo run -- exec --provider deepseek "read README.md"`

### Step 5: Controller 多轮循环

**修改文件**:
- `src/runtime/controller.rs`:
  - 新增 `run_agent_loop()` 替代 `run_provider_plan()` 作为 DeepSeek provider 的执行路径
  - 循环逻辑：调用 provider → 发射事件 → 若有 tool_calls 则执行 → 将结果追加到 Conversation → 下一轮
  - max_turns 检查（默认 10），超限返回 `BudgetExhausted`
  - 重构 `run_tool_request` 使其同时返回 `(RunStatus, ToolResult)` 以便获取工具输出

验证：`cargo test` + 手动 E2E 测试 `orca exec --provider deepseek "What files are in this project?"`

### Step 6: 测试固化

**修改/新增文件**:
- `src/provider/deepseek_fixture.rs` — 新增多轮 fixture（第一轮返回 tool_call，第二轮收到 tool result 后返回 message）
- `tests/` — 新增集成测试:
  - 多轮循环正常完成
  - max_turns 超限返回正确退出码
  - approval deny 中断循环

验证：`cargo test`

## 工具 Schema 设计

```json
[
  {"type":"function","function":{"name":"read_file","description":"Read file contents","parameters":{"type":"object","properties":{"path":{"type":"string","description":"File path relative to workspace"}},"required":["path"]}}},
  {"type":"function","function":{"name":"list_files","description":"List files in a directory","parameters":{"type":"object","properties":{"path":{"type":"string","description":"Directory path, default '.'"}},"required":[]}}},
  {"type":"function","function":{"name":"grep","description":"Search for a regex pattern in files using ripgrep","parameters":{"type":"object","properties":{"pattern":{"type":"string","description":"Regex pattern"},"path":{"type":"string","description":"Directory to search, default '.'"}},"required":["pattern"]}}},
  {"type":"function","function":{"name":"bash","description":"Execute a shell command via sh -c","parameters":{"type":"object","properties":{"command":{"type":"string","description":"The command to run"}},"required":["command"]}}},
  {"type":"function","function":{"name":"edit","description":"Edit a file by replacing exact text. old_text must match exactly one location.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"File path"},"old_text":{"type":"string","description":"Exact text to find"},"new_text":{"type":"string","description":"Replacement text"}},"required":["path","old_text","new_text"]}}},
  {"type":"function","function":{"name":"git_status","description":"Show git working tree status (short format)","parameters":{"type":"object","properties":{},"required":[]}}}
]
```

## 关键注意事项

1. **reasoning_content 回传**：DeepSeek 要求如果 assistant 消息包含 reasoning_content + tool_calls，后续请求必须原样回传该字段，否则 400 错误
2. **Edit 参数映射**：从 `{"path":"x","old_text":"y","new_text":"z"}` 映射到现有 DSL 时，old_text/new_text 可能包含分隔符。解决方案：edit.rs 支持直接从 raw_arguments 解析
3. **max_turns 默认值**：10 轮，可通过 `--max-turns` 覆盖
4. **Mock/Fixture 兼容**：通过适配层包装，不改变其行为。Mock 返回的 ProviderResponse 中 tool_calls 为空（除非原有 steps 中包含 ToolCall），循环第一轮就结束

## 验证方式

1. `cargo test` — 全部现有测试通过（回归）
2. `cargo run -- exec --provider deepseek "What files are in this project?"` — 应触发 list_files 工具调用
3. `cargo run -- exec --provider deepseek --max-turns 2 "Refactor README.md"` — 应在 2 轮后停止
4. `cargo run -- exec --provider deepseek --approval-mode read-only "Run cargo test"` — bash 应被拒绝，退出码 3
