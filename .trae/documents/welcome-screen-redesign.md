# Welcome Screen Redesign

## Context

当前 Orca 在正常启动（API Key 已配置）时，消息区域完全空白，只显示一个 " Orca " 标题的空框。用户无法快速了解当前环境信息。参考 Claude Code 和 Codex CLI 的做法，需要在空状态展示有用的欢迎信息。

## 设计目标

参考 Claude Code（logo + tips + what's new）和 Codex（header box + model/dir + tip），设计一个信息丰富但不臃肿的欢迎界面。

## 实现方案

### 渲染位置

在 `src/tui/ui.rs` 的 `render_messages` 函数中，当 `state.messages.is_empty()` 时，渲染欢迎内容替代空白区域。

### 欢迎内容布局

```
┌ Orca ─────────────────────────────────────────────────────┐
│                                                           │
│    ___                                                    │
│   / _ \ _ __ ___ __ _                                     │
│  | | | | '__/ __/ _` |                                    │
│  | |_| | | | (_| (_| |                                    │
│   \___/|_|  \___\__,_|    v0.1.0                          │
│                                                           │
│  model:      deepseek-v4-flash                            │
│  directory:  ~/Documents/GitHub/blade-deepseek            │
│                                                           │
│  Tips                                                     │
│  • Shift+Enter to insert newline, Enter to send           │
│  • /model to switch model, /compact to compress context   │
│  • Ctrl+K or F1 for keyboard shortcuts                    │
│                                                           │
└───────────────────────────────────────────────────────────┘
```

### 需要修改的文件

1. **`src/tui/ui.rs`** — 在 `render_messages` 中增加空状态分支，渲染欢迎内容
2. **`src/tui/types.rs`** — `AppState` 添加 `cwd: String` 字段
3. **`src/tui/app.rs`** — 初始化 `AppState` 时传入 cwd

### 实现细节

- 版本号通过 `env!("CARGO_PKG_VERSION")` 编译时获取
- cwd 从 `config.cwd` 获取并 `~` 缩短（home 目录替换为 `~`）
- ASCII art logo 复用已有的 setup 步骤中的 logo
- Tips 内容硬编码，使用 theme 颜色
- 颜色方案：logo 用 `theme.border`（Cyan），meta 用 `theme.text`，tips 用 `theme.muted`

### 验证

运行 `./target/release/orca`，启动后应看到欢迎界面。输入第一条消息后欢迎内容消失，切换为正常的对话视图。
