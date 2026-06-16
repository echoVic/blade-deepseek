# 5 个特性实施计划

## Context
subagent 系统清理完成后，项目进入功能增强阶段。本计划覆盖 5 个特性：streaming 中断、write_file 工具、并行子代理、上下文继承、TUI 子代理可视化。总预估 ~700 行新增代码。

## 实施顺序与依赖

```
Phase 1（可并行）: Feature 5 (Streaming 中断) + Feature 4 (write_file)
Phase 2:          Feature 1 (并行子代理) — 依赖 Feature 5 的 CancelToken
Phase 3（可并行）: Feature 2 (TUI 可视化) + Feature 3 (上下文继承)
```

---

## Feature 5: Streaming 中断

**设计**：`Arc<AtomicBool>` 作为 CancelToken，Esc 键触发，在 SSE stream 的每行读取间检查。

### 改动

1. **新增 `src/runtime/cancel.rs`**
   ```rust
   pub struct CancelToken(Arc<AtomicBool>);
   // new(), cancel(), is_cancelled(), reset()
   ```

2. **`src/provider/streaming.rs`** — `parse_sse_stream` 签名增加 `cancel: &CancelToken`，循环头部检查 `is_cancelled()`，返回 `Err("cancelled")`

3. **`src/provider/deepseek_http.rs`** — `request_chat_streaming` 透传 cancel token

4. **`src/provider/mod.rs`** — `call_streaming` 函数签名增加 `cancel: &CancelToken`

5. **`src/tui/types.rs`** — `UserAction` 新增 `Interrupt` variant

6. **`src/tui/app.rs`**:
   - CancelToken 传入 agent 线程
   - Running 状态 Esc → `cancel_token.cancel()` + send `UserAction::Interrupt`
   - agent_loop_thread 收到 Interrupt → reset token，继续 recv 循环
   - Ctrl+C 保持退出行为不变

7. **`src/tui/bridge.rs`** — `run_agent_for_tui` 接收 CancelToken，provider 返回 cancelled 时发 `SessionCompleted { status: "interrupted" }`

8. **`src/runtime/controller.rs`** — headless 模式传入 `CancelToken::new()`（不可中断，保持行为不变）

### 关键决策
- `Ordering::Relaxed` 足够（单 writer 单 reader）
- Response drop 时 reqwest 自动关闭 TCP 连接
- Headless 模式不支持中断（CancelToken 永远为 false）

---

## Feature 4: write_file 工具

**设计**：独立的 write_file 工具，ActionKind::Write 需审批。

### 改动

1. **新增 `src/tools/write_file.rs`**
   - 解析 `path` + `content` 参数
   - 校验路径在 cwd 内（安全检查）
   - `fs::create_dir_all` 父目录 + `fs::write`
   - 返回写入字节数

2. **`src/tools/mod.rs`** — ToolName 增加 `WriteFile`，as_str `"write_file"`，execute 分支

3. **`src/provider/tool_schema.rs`** — 新增 write_file schema（path + content 两个 required 参数）

4. **`src/provider/deepseek_http.rs`** — parse_tool_call 增加 `"write_file"` arm → `ToolName::WriteFile, ActionKind::Write`

5. **`src/provider/system_prompt.rs`** — Available Tools 段增加 write_file 描述

6. **`src/runtime/subagent_types.rs`** — 在 `General`/`TestWriter`/`Debugger` 的 allowed_tools 中加入 `"write_file"`

---

## Feature 1: 并行子代理

**设计**：同一轮 LLM 返回多个 subagent tool_call 时，用 `std::thread::scope` 并行执行。子代理内部降级为 full-auto（不请求审批）。

### 改动

1. **`src/runtime/controller.rs`**:
   - 工具执行循环拆分为：non-subagent 串行 + subagent 并行
   - 多 subagent 时用 `std::thread::scope` spawn
   - 每个线程独立运行 `run_agent_loop`（emit_deltas=false）
   - 收集结果后按顺序 add_tool_result

2. **`src/tui/bridge.rs`**:
   - 同理分组执行
   - 并行子代理各 clone `event_tx`（Sender 可 clone）
   - 不共享 action_rx（子代理 full-auto 不需要审批通道）

3. **`src/config/mod.rs`** 或 `RunConfig`:
   - 新增 `subagent_auto_approve: bool` 字段（并行子代理时 force true）

4. **子代理 full-auto 实现**：
   - `run_agent_loop` 中 `subagent_depth > 0` 时跳过 `requires_approval` 检查
   - 或在 `execute_tool_with_approval` 中增加 `force_approve` 参数

### 关键决策
- 使用 `std::thread::scope`（不引入 tokio）
- 子代理 full-auto 降级（不阻塞等待审批）
- 单个 subagent 仍串行（只有多个时才并行）
- cancel token 传入每个并行线程（支持中断）

---

## Feature 3: 子代理上下文继承

**设计**：子代理启动前调用一次 LLM 生成父对话摘要，作为 system prompt 附加上下文。默认开启。

### 改动

1. **新增 `src/runtime/context_summary.rs`**:
   ```rust
   pub fn summarize_for_subagent(
       conversation: &Conversation,
       config: &ProviderConfig,
   ) -> Option<String>
   ```
   - 取最近 N 条消息（跳过 system），序列化为文本
   - 构建临时对话：system="Summarize this conversation in 2-3 sentences for a sub-agent that needs context", user=序列化内容
   - 调用 `provider::call()`（非 streaming）
   - 返回 assistant_content

2. **`src/runtime/agent_common.rs`** — `build_agent_system_prompt` 增加 `context_summary: Option<&str>` 参数，存在时追加 `\n\n## Parent Context\n{summary}`

3. **`src/runtime/controller.rs`** — `execute_subagent_tool` 中调用 `summarize_for_subagent`，传给 `run_agent_loop`

4. **`src/tui/bridge.rs`** — `execute_subagent_for_tui` 同理

5. **`src/runtime/mod.rs`** — 添加 `pub mod context_summary;`

### 关键决策
- 默认开启，额外消耗 1 次 API 调用（使用同模型）
- 摘要 token 预算：最多取最近 20 条消息或 4K chars 作为输入
- 如果 conversation 只有 1-2 条消息（刚开始），跳过摘要

---

## Feature 2: TUI 子代理可视化

**设计**：spinner 动画 + streaming delta 转发 + 折叠/展开。

### 改动

1. **`src/tui/types.rs`**:
   - 新增 `TuiEvent::SubagentDelta { id: String, kind: String, text: String }`
   - `ChatMessage::Subagent` 增加 `deltas: Vec<String>`, `collapsed: bool`

2. **`src/tui/bridge.rs`**:
   - 子代理 streaming callback 从 `|_| {}` 改为发送 SubagentDelta 事件
   - 需要 clone event_tx 和 subagent_id 到闭包

3. **`src/tui/app.rs`**:
   - AppState 增加 `tick: u64` 每 50ms 递增
   - 处理 SubagentDelta 事件：追加到对应 Subagent message
   - Tab 键切换选中的 subagent 消息折叠状态
   - Running 状态下 Tab 键绑定折叠

4. **`src/tui/ui.rs`**:
   - Subagent running 时显示 spinner（`['⠋','⠙','⠹','⠸','⠼','⠴','⠦','⠧','⠇','⠏'][tick % 10]`）
   - collapsed=false 时渲染 deltas（最多显示末尾 5 行）
   - 并行多个 running subagent 时纵向排列

---

## 验证方案

| Feature | 验证方式 |
|---------|----------|
| Feature 5 | TUI 中发送 prompt → 等 streaming 开始 → 按 Esc → 确认回到 Idle 可继续输入 |
| Feature 4 | `cargo test` + headless 模式让模型创建一个测试文件 |
| Feature 1 | 构造 prompt 让模型同时发起 2+ subagent → 观察并行执行（时间应 < 串行总和） |
| Feature 3 | 对话几轮后触发 subagent → 检查子代理 system prompt 包含摘要 |
| Feature 2 | TUI 中触发子代理 → 观察 spinner 动画和 delta 显示 |

每个 Feature 完成后运行 `cargo test` + `cargo clippy` 确保无回归。
