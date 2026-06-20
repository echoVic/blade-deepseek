# P0: 并发工具执行 + 交互体验优化

## Context

blade-deepseek (Orca) 当前的工具执行是严格串行的——LLM 返回多个 tool_calls 时逐个执行。对于读操作（read_file、list_files、grep、git_status），完全不存在竞态风险，串行执行白白浪费延迟。此外，长时间运行的 bash 命令缺少中间输出反馈，用户只能看到 `⟳ running` 等待。

参考 Claude Code 的 `toolOrchestration.ts`（读工具最多 10 并发，写工具串行）和 Codex 的子代理并行模式，本方案为 Orca 引入：

1. **读工具并发执行** — 同一轮多个只读工具并行跑在 OS 线程上
2. **Bash 流式输出** — 长命令实时回传 stdout 片段到 TUI
3. **动画 Spinner** — 替代静态 `⟳`，给用户视觉节奏感
4. **工具输出可展开** — 当前截断 2 行无法查看全量，加折叠/展开

---

## 1. 读工具并发执行

### 设计

给每个工具标注 `is_read_only` 属性（参考 Claude Code `Tool.isReadOnly`）：

| 工具 | is_read_only |
|------|-------------|
| read_file | ✓ |
| list_files | ✓ |
| grep | ✓ |
| git_status | ✓ |
| web_search | ✓ |
| bash | ✗ (可能有副作用) |
| edit | ✗ |
| write_file | ✗ |
| update_plan | ✗ (虽然无磁盘写入，但影响状态) |
| subagent | ✗ (已有自己的并行逻辑) |
| mcp | ✗ (无法确定副作用) |

### 执行策略

在 tool dispatch 循环中，将连续的 read-only 工具收集为一个 batch，用 `std::thread::scope` 并行执行（无需 tokio），与现有 subagent batch 模式对称：

```rust
// 伪代码
while index < tool_requests.len() {
    if should_run_subagent_batch(...) { /* 已有逻辑 */ }
    
    // NEW: 收集连续只读工具 batch
    let batch_end = collect_readonly_batch(&tool_requests, index);
    if batch_end > index + 1 {
        let results = execute_readonly_batch(&tool_requests[index..batch_end], cwd, mcp);
        // 按顺序添加 results 到 conversation
        for result in results { ... }
        index = batch_end;
        continue;
    }
    
    // 单个工具串行执行（已有逻辑）
    ...
}
```

### 改动文件

- `src/tools/mod.rs` — 添加 `ToolName::is_read_only(&self) -> bool` 方法
- `src/runtime/controller.rs` — 添加 `collect_readonly_batch()` + `execute_readonly_batch()` 函数
- `src/tui/bridge.rs` — 同步添加 TUI 版本的 readonly batch 执行

### 约束

- batch 最大并发数 = 8（可通过 config 调整）
- 遇到第一个非 read-only 工具时立即切断 batch
- batch 内的 TuiEvent 仍按原始顺序发送（先全部 `ToolRequested`，再逐个 `ToolCompleted`）

---

## 2. Bash 流式输出

### 现状问题

当前 `bash::execute()` 调用 `.output()` 一次性等待命令完成，对于 `cargo build`、`npm install` 等长命令，用户需盲等数十秒。

### 设计

新增 `bash::execute_streaming()` 变体，使用 `Command::spawn()` + 逐行读取 stdout/stderr，通过回调实时推送：

```rust
pub fn execute_streaming(
    request: &ToolRequest,
    cwd: &Path,
    max_bytes: usize,
    on_output: &mut dyn FnMut(&str),  // 每行回调
) -> ToolResult { ... }
```

### TUI 集成

新增 `TuiEvent::ToolOutputDelta { id: String, chunk: String }`：
- bridge.rs 中 bash 工具执行时使用 streaming 变体
- `AppState::update()` 将 chunk 追加到对应 ToolCall 的 output 缓冲
- ui.rs 的 tool output 区域实时更新（仍截断显示最后 N 行）

### 改动文件

- `src/tools/bash.rs` — 添加 `execute_streaming()` 函数
- `src/tui/types.rs` — 新增 `TuiEvent::ToolOutputDelta` 变体
- `src/tui/bridge.rs` — bash 工具调用改用 streaming 变体
- `src/tui/types.rs` (AppState::update) — 处理 ToolOutputDelta

注意：CLI/headless 模式无需此功能，保持原 `.output()` 调用即可。

---

## 3. 动画 Spinner

### 现状

`⟳` 是静态 Unicode 字符，没有视觉动效。

### 设计

在 `AppState` 中添加一个 `tick: u64` 计数器（每帧 +1，50ms 一帧），在渲染时根据 tick 选择 braille spinner frame：

```rust
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
```

仅在 `AppStatus::Running` 时递增 tick。工具行的 `⟳` 替换为 `SPINNER_FRAMES[tick / 2 % 10]`（每 100ms 转一帧）。

### 改动文件

- `src/tui/types.rs` — `AppState` 添加 `tick: u64` 字段
- `src/tui/app.rs` — 主循环每帧 `state.tick += 1`（仅 Running 状态时）
- `src/tui/ui.rs` — tool call "running" 状态使用 spinner frame 替代静态 `⟳`

---

## 4. 工具输出折叠/展开

### 现状

工具输出固定截断 2 行，无法查看完整内容。

### 设计

- 每个 tool call 有 `expanded: bool` 状态
- 默认折叠（2 行 + `[+N lines]` 提示）
- 用户通过快捷键 `e` 在 Idle 状态下切换最近一个 tool call 的展开状态
- 展开后最多显示 40 行（超过仍截断，带提示）

### 改动文件

- `src/tui/types.rs` — `ChatMessage::ToolCall` 添加 `expanded: bool` 字段
- `src/tui/ui.rs` — 渲染逻辑根据 expanded 选择截断行数
- `src/tui/app.rs` — Idle 快捷键 `e` 切换展开
- `src/tui/shortcuts.rs` — 注册 `e` 为 `ExpandToolOutput`

---

## 执行顺序

1. **Spinner** (最小改动，立即可见)
2. **并发读工具** (核心性能提升)
3. **Bash 流式输出** (体验提升)
4. **工具输出折叠** (UI 优化)

---

## 验证方式

1. **Spinner**: 运行 orca 发送任何 prompt，观察工具执行时图标是否旋转
2. **并发读工具**: 发送需要多次 read_file 的 prompt（如 "读取 src/ 下所有 .rs 文件的前 10 行"），对比前后总耗时
3. **Bash 流式**: 执行 `sleep 3 && echo done` 或 `for i in 1 2 3; do echo $i; sleep 1; done`，观察 TUI 是否逐行显示
4. **折叠**: 触发一个 grep 工具（输出较长），确认默认截断，按 `e` 可展开
5. **回归**: 运行 `cargo test` 确保现有 contract 测试全部通过
