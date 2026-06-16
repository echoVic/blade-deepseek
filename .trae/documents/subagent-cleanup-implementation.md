# Subagent 系统清理实施计划

## 摘要
完成 subagent 系统的精简：删除异步分支和死代码，统一使用 `agent_common` 共享模块，接入 SubagentType 工具过滤。

## 当前状态
- `agent_common.rs` ✅ 已创建
- `subagent.rs` ✅ 已精简（仅保留同步请求结构）
- `tool_schema.rs` ✅ 已新增 `deepseek_tools_schema_for_type()`
- `provider/mod.rs` ✅ `ProviderConfig` 已新增 `tools_override` 字段
- `deepseek_http.rs` ✅ 已使用 `tools_override`
- `controller.rs` ❌ 仍引用 `SubagentMode`、含异步分支、本地重复函数
- `tui/bridge.rs` ❌ 仍含本地重复函数、未接入 SubagentType
- `runtime/mod.rs` ❌ 仍引用 `subagent_pool`
- 死文件存在：`subagent_async.rs`, `subagent_pool.rs`, `tools/subagent_status.rs`, `tests/subagent_async_contract.rs`

## 具体变更

### Step 1: 更新 `src/runtime/controller.rs`
- 删除 `use crate::runtime::subagent::SubagentMode;` import
- 删除本地函数 `subagent_field()`, `build_agent_system_prompt()`, `format_tool_result_for_model()`, `requires_approval()`
- 替换为 `agent_common::` 调用
- `run_agent_loop` 签名新增 `subagent_type: &SubagentType` 参数
- ProviderConfig 构造时 depth>0 传入 `tools_override: Some(deepseek_tools_schema_for_type(subagent_type))`，depth==0 传 `None`
- `execute_subagent_tool` 删除异步分支，从 SubagentRequest 获取 SubagentType
- 删除 `use crate::provider::system_prompt::build_system_prompt;` (已在 agent_common)

### Step 2: 更新 `src/tui/bridge.rs`
- 删除本地函数 `subagent_field()`, `build_agent_system_prompt()`, `format_tool_result_for_model()`, `requires_approval()`
- 替换为 `agent_common::` 调用
- ProviderConfig 构造新增 `tools_override: None`（顶层）和 `tools_override: Some(...)` (子代理)
- `execute_subagent_for_tui` 使用 `subagent::create_subagent_request` 提取 SubagentType
- 删除 `use crate::provider::system_prompt::build_system_prompt;`

### Step 3: 更新 `src/runtime/mod.rs`
- 添加 `pub mod agent_common;`
- 删除 `pub mod subagent_pool;`

### Step 4: 清理 `src/tools/mod.rs`
- 删除 `pub mod subagent_status;`
- 删除 `SubagentStatus` 变体及其 match 分支

### Step 5: 清理 `src/event/schema.rs`
- 删除 `SubagentLaunched` 变体
- 删除 `subagent_launched()` 方法

### Step 6: 清理 `src/event/sink.rs`
- 删除 `SubagentLaunched` 格式化分支

### Step 7: 删除死文件
- `src/runtime/subagent_async.rs`
- `src/runtime/subagent_pool.rs`
- `src/tools/subagent_status.rs`
- `tests/subagent_async_contract.rs`

### Step 8: 编译验证
- `cargo build` 无错误
- `cargo test` 全部通过

## 验证标准
- 零编译警告（22 个 warning 全部消除）
- 所有现有测试通过
- subagent 工具过滤按 SubagentType 正确运作
