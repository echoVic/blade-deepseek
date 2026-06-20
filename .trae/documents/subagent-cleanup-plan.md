# Subagent 系统清理与完善实施计划

## Context

`feat/subagents-sync` 分支实现了子代理系统，但存在大量死代码、半成品异步路径、代码重复、未接入的类型系统。参考 Claude Code 和 Codex CLI 的设计，本次 PR 的目标是：**仅保留可用的同步子代理，清除所有死代码，接入 SubagentType（含工具过滤），消除 22 个编译警告**。

## 变更范围

### 删除的文件
| 文件 | 原因 |
|------|------|
| `src/runtime/subagent_async.rs` | 未被 mod.rs 引入，完全死代码 |
| `src/runtime/subagent_pool.rs` | 所有 API 从未被调用，内部线程是 sleep 模拟 |
| `src/tools/subagent_status.rs` | 仅服务异步模式，且有路径穿越漏洞 |
| `tests/subagent_async_contract.rs` | 测试已删除的异步功能 |

### 新建的文件
| 文件 | 职责 |
|------|------|
| `src/runtime/agent_common.rs` | 从 controller/bridge 提取的共享函数 |

### 修改的核心文件
- `src/runtime/controller.rs` — 删除异步分支，使用共享模块，传入 SubagentType
- `src/tui/bridge.rs` — 删除重复函数，使用共享模块
- `src/runtime/subagent.rs` — 精简为仅保留 SubagentRequest + 解析函数
- `src/runtime/mod.rs` — 移除 subagent_pool 声明，新增 agent_common
- `src/tools/mod.rs` — 移除 SubagentStatus 变体
- `src/provider/tool_schema.rs` — subagent schema 增加 subagent_type 参数，新增过滤函数
- `src/provider/mod.rs` — ProviderConfig 增加 tools_override，call_streaming 使用它
- `src/provider/deepseek_http.rs` — call_streaming 尊重 tools_override
- `src/event/schema.rs` — 移除 SubagentLaunched 事件
- `src/event/sink.rs` — 移除 SubagentLaunched 格式化
- `src/tui/types.rs` — 清理无用引用
- `Cargo.toml` — 移除 uuid、chrono 依赖

## 详细实施步骤

### Step 1: 创建 `src/runtime/agent_common.rs`

从 controller.rs 和 bridge.rs 提取 3 个重复函数：
- `build_agent_system_prompt(cwd, subagent_depth, subagent_type)` — 构建系统提示 + 类型后缀
- `format_tool_result_for_model(result)` — 工具结果格式化
- `requires_approval(action, approval_mode)` — 审批判断

参考现有实现：controller.rs L415-471, bridge.rs L398-430

### Step 2: 精简 `src/runtime/subagent.rs`

删除所有异步相关类型（SubagentMode, SubagentStatus, SubagentProgress, SubagentStatistics, SubagentOutput, SubagentResult, SubagentRuntime），保留：
- `SubagentRequest { description, prompt, subagent_type }`
- `extract_subagent_field(tool_request, field)` 
- `create_subagent_request(tool_request)` — 包含 subagent_type 解析

### Step 3: 更新 `src/runtime/mod.rs`

```rust
pub mod agent_common;
pub mod controller;
pub mod session;
pub mod subagent;
pub mod subagent_types;
// 删除: pub mod subagent_pool;
```

### Step 4: 更新 `src/provider/tool_schema.rs`

1. subagent 工具 schema 增加 `subagent_type` 参数（enum: general/code_reviewer/test_writer/debugger/documenter）
2. 新增 `pub fn deepseek_tools_schema_for_type(subagent_type: &SubagentType) -> Vec<Value>` — 根据 `subagent_type.allowed_tools()` 过滤工具列表，并移除 subagent 自身（防止嵌套）

### Step 5: 添加工具过滤到 Provider 层

在 `src/provider/mod.rs` 的 `ProviderConfig` 增加：
```rust
pub tools_override: Option<Vec<serde_json::Value>>,
```

`src/provider/deepseek_http.rs` 的 `call_streaming` 和 `call` 中：
```rust
let tools = config.tools_override.clone().unwrap_or_else(deepseek_tools_schema);
```

### Step 6: 更新 `src/runtime/controller.rs`

1. 删除 `use crate::runtime::subagent::SubagentMode;`
2. 删除 `subagent_field()` 函数（L409-413）
3. 删除 `build_agent_system_prompt()` 函数（L415+），替换为 `agent_common::build_agent_system_prompt`
4. `run_agent_loop` 新增参数 `subagent_type: &SubagentType`，用于：
   - 调用 `agent_common::build_agent_system_prompt(cwd, subagent_depth, subagent_type)`
   - 构建 ProviderConfig 时设置 `tools_override`（depth > 0 时使用过滤列表）
5. `execute_subagent_tool` 简化：删除异步分支，直接解析请求 → 检查深度 → 递归调用
6. 顶层 `run_inner` 调用传入 `&SubagentType::General`

### Step 7: 更新 `src/tui/bridge.rs`

1. 删除本地重复函数：`subagent_field`, `build_agent_system_prompt`, `format_tool_result_for_model`, `requires_approval`
2. 替换为 `use crate::runtime::agent_common::{...}`
3. `execute_subagent_for_tui` 使用 `subagent::create_subagent_request`
4. `run_child_agent_for_tui` 参照 controller 改造：构建带类型后缀的 system prompt + 过滤工具列表

### Step 8: 删除文件与清理引用

1. 删除 `src/runtime/subagent_async.rs`
2. 删除 `src/runtime/subagent_pool.rs`
3. 删除 `src/tools/subagent_status.rs`
4. `src/tools/mod.rs`: 移除 `pub mod subagent_status`、`SubagentStatus` 枚举变体及相关 match 分支
5. `src/event/schema.rs`: 移除 `SubagentLaunched` 变体和 `subagent_launched()` 方法
6. `src/event/sink.rs`: 移除 `SubagentLaunched` 格式化分支

### Step 9: 移除无用依赖

`Cargo.toml` 删除 `uuid` 和 `chrono`（仅被已删除代码使用）。

### Step 10: 删除异步测试

删除 `tests/subagent_async_contract.rs`。

### Step 11: 修复 TUI types 编译

`src/tui/types.rs` 中如有引用已删除类型，清理 import。确认 `SubagentStarted`/`SubagentCompleted` TUI 事件仍正确（它们服务同步模式）。

## 验证

1. `cargo build` — 0 error
2. `cargo clippy` — 0 warning
3. `cargo test` — 所有测试通过，包括 `tests/subagent_contract.rs` 的 3 个同步集成测试
4. 手动测试：`cargo run -- exec --provider mock "subagent inspect repo"` 输出 subagent.started + subagent.completed 事件

## 预期成果

- 编译警告：22 → 0
- 代码量：净减少 ~800 行
- 依赖：移除 uuid + chrono（减少编译时间）
- 功能完整：同步子代理工作 + SubagentType 工具过滤生效
- 安全修复：路径穿越漏洞消除（删除 subagent_status）
