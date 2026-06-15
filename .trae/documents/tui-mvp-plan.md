# TUI MVP 实现计划

## Context

当前 Orca 只能通过 `orca exec "prompt"` 单次运行，输出为原始文本或 JSONL。用户希望实现类似 Codex CLI / DeepSeek-TUI / OpenCode 的终端交互界面——输入 prompt、实时看到模型推理和回复、工具调用结果展示、可追问。

参考了：
- **DeepSeek-TUI (CodeWhale)**: Rust + ratatui + tokio，Turn 事件驱动，Transcript + Composer + StatusBar 布局
- **OpenCode**: Go + Bubble Tea，pubsub 事件总线，MVU 架构
- **Claude Code**: TypeScript + ink (React for CLI)

MVP 目标：**极简交互**——单输入框 + 流式消息展示 + 状态栏。无会话管理、无侧边栏、无 markdown 渲染。

## 技术选型

- **ratatui** + **crossterm** — 终端 UI 渲染和事件读取
- **std::thread** + **mpsc::channel** — 后台线程跑 agent loop，channel 推事件给 TUI
- **不引入 tokio** — 保持 reqwest::blocking，MVP 改动最小
- **tui-textarea** — 输入框组件（支持多行、光标移动）

## UI 布局

```
┌──────────────────────────────────────────┐
│  Messages Area (滚动消息列表)             │
│  ┌──────────────────────────────────────┐│
│  │ > user: 分析下当前项目                ││
│  │                                      ││
│  │ [thinking] Let me look at the...     ││
│  │                                      ││
│  │ assistant: 项目结构如下...            ││
│  │                                      ││
│  │ [tool] read_file: src/main.rs        ││
│  │ [tool] ✓ completed (128 bytes)       ││
│  └──────────────────────────────────────┘│
├──────────────────────────────────────────┤
│  Input Area (1-3 行输入框)               │
│  > _                                     │
├──────────────────────────────────────────┤
│  Status: ● running | model: v4-flash    │
└──────────────────────────────────────────┘
```

- **Messages Area**: 占据大部分空间，可滚动
- **Input Area**: 底部输入框，Enter 发送，Shift+Enter 换行
- **Status Bar**: 最底部一行，显示状态(idle/running/waiting approval)和模型名

## 架构设计

### 并发模型

```
┌─────────────────────┐     mpsc::channel      ┌─────────────────────┐
│   TUI Main Thread   │ <──── TuiEvent ─────── │  Agent Thread       │
│                     │                         │                     │
│  crossterm events   │                         │  controller logic   │
│  render loop        │                         │  provider streaming │
│  state management   │ ──── UserAction ──────> │  tool execution     │
│                     │     (prompt/approve)     │                     │
└─────────────────────┘                         └─────────────────────┘
```

- **TUI → Agent**: 通过 `mpsc::Sender<UserAction>` 发送用户输入和审批结果
- **Agent → TUI**: 通过 `mpsc::Sender<TuiEvent>` 发送流式事件

### 核心类型

```rust
// Agent → TUI 的事件
enum TuiEvent {
    TurnStarted { turn: u32 },
    ReasoningDelta(String),
    MessageDelta(String),
    ToolRequested { name: String, target: Option<String> },
    ToolCompleted { name: String, status: String, output: String },
    ApprovalNeeded { id: String, tool: String, target: Option<String> },
    Error(String),
    SessionCompleted { status: String },
}

// TUI → Agent 的用户动作
enum UserAction {
    Submit(String),           // 用户提交 prompt
    Approve(bool),            // 审批结果 y/n
    Cancel,                   // Ctrl+C 中断当前任务
}

// App 状态
struct AppState {
    messages: Vec<ChatMessage>,
    input: String,
    status: AppStatus,          // Idle / Running / WaitingApproval
    scroll_offset: u16,
    model_name: String,
}

enum ChatMessage {
    User(String),
    Reasoning(String),
    Assistant(String),
    ToolCall { name: String, target: Option<String>, status: String, output: Option<String> },
    Error(String),
}
```

### 主循环伪代码

```rust
fn run_tui(config: RunConfig) -> i32 {
    // 1. 初始化终端
    enable_raw_mode();
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()));

    // 2. 创建双向 channel
    let (event_tx, event_rx) = mpsc::channel::<TuiEvent>();
    let (action_tx, action_rx) = mpsc::channel::<UserAction>();

    // 3. TUI 主循环
    loop {
        // 渲染
        terminal.draw(|f| render_ui(f, &state));

        // 处理事件 (非阻塞)
        if crossterm::event::poll(Duration::from_millis(50))? {
            match crossterm::event::read()? {
                Key(Enter) => action_tx.send(UserAction::Submit(state.input.drain())),
                Key(Ctrl+C) => break / action_tx.send(Cancel),
                Key(char) => state.input.push(char),
                // ... 其他键
            }
        }

        // 消费 agent 事件 (非阻塞)
        while let Ok(event) = event_rx.try_recv() {
            update_state(&mut state, event);
        }
    }

    // 4. 清理
    disable_raw_mode();
    terminal.show_cursor();
}
```

### Agent 后台线程

```rust
fn spawn_agent(config: RunConfig, event_tx: Sender<TuiEvent>, action_rx: Receiver<UserAction>) {
    std::thread::spawn(move || {
        // 等待用户提交 prompt
        loop {
            match action_rx.recv() {
                Ok(UserAction::Submit(prompt)) => {
                    // 跑 agent loop，但 event sink 改为通过 channel 发送
                    run_agent_with_channel(config, &prompt, &event_tx, &action_rx);
                }
                Ok(UserAction::Cancel) | Err(_) => break,
            }
        }
    });
}
```

## 实现步骤

### Step 1: 添加依赖

```toml
ratatui = "0.29"
crossterm = "0.28"
tui-textarea = "0.7"
```

### Step 2: 定义 TUI 类型 (`src/tui/types.rs`)

`TuiEvent`, `UserAction`, `AppState`, `ChatMessage`, `AppStatus` 枚举和结构体。

### Step 3: 实现 TUI 渲染 (`src/tui/ui.rs`)

- `render_messages(f, area, state)` — 渲染消息列表
- `render_input(f, area, state)` — 渲染输入框
- `render_status(f, area, state)` — 渲染状态栏
- 布局：`Layout::vertical([Fill(1), Length(3), Length(1)]).split(area)`

### Step 4: 实现 TUI 主循环 (`src/tui/app.rs`)

- `run_tui(config: RunConfig) -> i32`
- 初始化 terminal、spawn agent 线程、事件循环、清理

### Step 5: 实现 Channel-based EventOutput (`src/tui/bridge.rs`)

将 controller 的 `EventSink` 适配为通过 channel 发送 `TuiEvent`。两种方式：
- **方案 A**: 引入 `EventOutput` trait，controller 通过 trait 输出（侵入性重构）
- **方案 B（推荐）**: 新建 `run_agent_for_tui()` 函数，复用 controller 内部逻辑但输出到 channel

具体做法：在 `controller.rs` 中提取 `run_agent_loop_with_callbacks` 函数，接受回调而非 EventSink。TUI 的 bridge 层实现这些回调为 channel send。

### Step 6: TUI 模式下的审批确认

当 agent 遇到需要审批的操作时：
1. Agent 线程通过 channel 发送 `TuiEvent::ApprovalNeeded { ... }`
2. TUI 主循环收到后，设置 `state.status = WaitingApproval`
3. UI 显示审批提示：`[y] approve / [n] deny`
4. 用户按 y/n → 通过 `action_tx.send(UserAction::Approve(bool))` 回传
5. Agent 线程 `action_rx.recv()` 收到审批结果，继续执行

### Step 7: CLI 集成

在 `src/cli.rs` 中修改 `run_placeholder`：
- 无参数时启动 TUI 模式
- 或者加 `orca "prompt"` 直接进入 TUI 并自动提交第一个 prompt

### Step 8: 测试验证

- `cargo build` 编译通过
- `cargo test` 现有测试不受影响（TUI 是独立模块）
- 手动运行 `orca` 进入 TUI，测试：
  - 输入 prompt，看到流式输出
  - 工具调用显示
  - Ctrl+C 退出
  - 审批提示 y/n

## 文件结构

```
src/
├── tui/
│   ├── mod.rs         — pub mod 声明
│   ├── types.rs       — TuiEvent, UserAction, AppState, ChatMessage
│   ├── app.rs         — run_tui() 主循环，事件处理，状态更新
│   ├── ui.rs          — 渲染函数（messages, input, status）
│   └── bridge.rs      — agent loop 到 TUI channel 的桥接
├── cli.rs             — 修改 run_placeholder 启动 TUI
└── runtime/
    └── controller.rs  — 提取可复用的 agent loop 核心逻辑
```

## 关键决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 并发模型 | std::thread + mpsc | 不需要引入 tokio，改动最小，reqwest::blocking 继续用 |
| TUI 库 | ratatui + crossterm | Rust TUI 标准选择，DeepSeek-TUI 也用 |
| 输入组件 | tui-textarea | 支持多行、光标移动、粘贴，开箱即用 |
| Controller 适配 | 提取回调版本 | 不破坏现有 exec 路径，新增并行路径 |
| 流式显示 | 追加到最后一条 ChatMessage | 每收到 delta 就追加并重绘，极简实现 |

## 验证方式

1. `cargo build` — 编译通过
2. `cargo test` — 现有 80 个测试不受影响
3. `cargo clippy` — 无警告
4. 手动验证：
   - `orca` → 进入 TUI
   - 输入 "list files" → 看到流式推理 + 工具调用 + 结果
   - 输入 "edit something" → 看到审批提示 → 按 y/n
   - Ctrl+C → 正常退出，终端恢复
5. `orca exec "hello"` — 现有 exec 路径不受影响

## 不在 MVP 范围内

- 会话持久化 / 多会话管理
- Markdown 渲染 / 代码高亮
- 侧边栏
- 模型切换 UI
- diff 展示
- 文件补全
- 滚动回看（仅自动滚到底部）
