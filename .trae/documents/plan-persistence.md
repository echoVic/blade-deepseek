# Plan: 任务列表文件持久化

## Context

当前 `update_plan` 工具的 plan 状态仅存在于内存中（TUI 的 `current_plan` 字段和 event stream），session 结束即丢失。需要实现跨 session 持久化，使恢复对话时 plan 自动还原。

采用最轻量方案：在现有 session JSONL 中新增 `plan.state` 记录类型，无需额外目录结构或文件系统。

## 方案设计

**核心思路**：每次 `update_plan` 成功执行时，向 session JSONL 追加一条 `plan.state` 记录。恢复 session 时，取最后一条 `plan.state` 作为当前 plan 状态。

### 数据流

```
update_plan tool 执行成功
  → emit plan.updated event (已有)
  → append plan.state record to session JSONL (新增)

session resume
  → read_transcript 解析所有记录
  → 最后一条 plan.state → transcript.plan
  → TUI/controller 从 transcript.plan 还原 current_plan
```

## 修改文件

### 1. `src/runtime/history.rs`

- **SessionRecord enum** 新增 variant：
  ```rust
  #[serde(rename = "plan.state")]
  PlanState {
      explanation: Option<String>,
      plan: Vec<PlanItem>,
  },
  ```
- **SessionTranscript struct** 新增字段：
  ```rust
  pub plan: Option<(Option<String>, Vec<PlanItem>)>,
  ```
- **read_transcript()** 中累积最后一条 PlanState，plan 为空或全部 completed 时设为 None
- **SessionWriter** 新增方法 `append_plan_state()`
- **read_records()** 兼容性修复：将未知记录类型从硬错误改为 `continue`（跳过），保证旧版本二进制打开新 session 文件不崩溃
- 顶部 import `PlanItem` 和 `PlanStatus`

### 2. `src/tui/bridge.rs`

在 `run_agent_for_tui` 中 `execute_tool_for_tui` 返回后（约 line 489），检测如果 tool 是 UpdatePlan 且 completed，调用 `session.writer.append_plan_state()`：

```rust
if tool_request.name == tools::ToolName::UpdatePlan
    && result.status == tools::ToolStatus::Completed
{
    if let Ok(update) = tools::update_plan::parse_args(tool_request) {
        if let Some(writer) = &mut session.writer {
            let _ = writer.append_plan_state(update.explanation, update.plan);
        }
    }
}
```

同样处理并行 tool 执行的路径 (line ~1059)。

### 3. `src/runtime/controller.rs`

在 headless mode 的 plan.updated emit 之后，追加 `writer.append_plan_state()` 调用。

### 4. `src/tui/app.rs`

在 session resume 路径（两处：初始加载和 picker 选择），加载 transcript 后还原 plan：

```rust
if let Some((explanation, plan)) = transcript.plan {
    state.current_plan = Some((explanation, plan));
}
```

## 前向兼容性

修改 `read_records()` 中的错误处理：
```rust
Err(_) if i == lines.len() - 1 => break,
Err(_) => continue,  // 跳过未知 record type
```

这确保旧版本 Orca 打开含 `plan.state` 的新 session 文件时不会崩溃（仅跳过不认识的行）。

## 自动清理

- plan 为空：`transcript.plan = None`
- plan 全部 completed：`transcript.plan = None`（视为已完成，不再恢复）

无需删除文件，plan.state 记录留在 session JSONL 中作为历史记录。

## 验证方式

1. **单元测试** (`history.rs`)：append plan_state → read_transcript → 验证 transcript.plan 正确
2. **单元测试**：plan 全部 completed → transcript.plan 为 None
3. **单元测试**：旧 session 文件（无 plan.state）正常加载，transcript.plan 为 None
4. **集成测试**：`cargo run -- exec` 触发 update_plan → 验证 session 文件包含 plan.state 行
5. **手动验证**：TUI 中运行任务 → 退出 → `--resume latest` → 确认 plan 面板自动恢复
