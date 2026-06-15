# TUI 增强计划

## Context

TUI MVP 已完成基础交互，但存在 4 个需要改进的问题：
1. 首次配置 API key 的引导过于简陋（单步、无美化）
2. AI 回复是纯文本，不解析 markdown（代码块、粗体等不渲染）
3. 审批确认只在状态栏显示一行，交互不友好
4. **Bug**: 任务执行完后无法向上滚动查看历史输出

## 方案概要

### 1. 优化 Setup 引导 — Step-by-step 欢迎流程

将 `AppStatus::Setup` 拆为多步引导，使用结构化的欢迎页面：

**步骤设计：**
- Step 1: 欢迎页面（显示 Orca logo/banner + 简要说明）
- Step 2: 输入 API Key（带验证提示和链接）
- Step 3: 完成确认（显示保存路径 + 提示开始使用）

**实现方式：**
- 在 `types.rs` 中添加 `setup_step: u8` 字段
- 在 `ui.rs` 中为 Setup 状态渲染专属的居中欢迎面板（不是消息列表）
- 使用 ASCII art banner + 彩色渐进提示

**关键文件：**
- `src/tui/types.rs` — 添加 `setup_step` 字段
- `src/tui/ui.rs` — 新增 `render_setup()` 函数，根据 step 渲染不同内容
- `src/tui/app.rs` — Setup 状态按键处理按步骤进行

### 2. Markdown 渲染

**选型：** `tui-markdown = "0.3"`（ratatui 核心维护者出品，API 一行调用）

**实现方式：**
- `Cargo.toml` 添加 `tui-markdown = "0.3"` 依赖
- 在 `ui.rs` 中 `ChatMessage::Assistant(text)` 分支替换当前的逐行 `Span::raw` 为 `tui_markdown::from_str(text)`
- 返回的 `Text<'static>` 中每个 `Line` 直接追加到 `lines` 中

**效果：** 标题加粗、代码块背景色、列表缩进、粗体/斜体样式。

**关键文件：**
- `Cargo.toml` — 添加依赖
- `src/tui/ui.rs` — `render_messages` 中 Assistant 分支改用 `tui_markdown::from_str`

### 3. 审批确认弹框

将审批从"状态栏 + y/n 按键"升级为居中弹出对话框：

**UI 设计：**
```
┌─────── Approval Required ────────┐
│                                  │
│  Tool: bash                      │
│  Target: rm -rf ./temp           │
│                                  │
│  ▸ Allow                         │
│    Deny                          │
│                                  │
│  ↑↓ to select, Enter to confirm │
└──────────────────────────────────┘
```

**实现方式：**
- 在 `types.rs` 中将 `approval_info` 扩展为结构体：`ApprovalDialog { tool, target, selected: usize }`
- 在 `ui.rs` 中新增 `render_approval_dialog()` 函数：用 `Clear` 清除区域 + 居中 `Block` + 高亮选中项
- 在 `app.rs` 中 WaitingApproval 状态：Up/Down 切换选项、Enter 确认
- 状态栏保留 "● waiting approval" 但不再是主要交互入口

**关键文件：**
- `src/tui/types.rs` — `ApprovalDialog` 结构体，替换 `approval_info: Option<String>`
- `src/tui/ui.rs` — `render_approval_dialog()` 居中弹窗渲染
- `src/tui/app.rs` — Up/Down/Enter 处理

### 4. 滚动修复

**Bug 根因：** `scroll_offset` 字段存在但从未被使用。`render_messages` 始终计算"自动滚到底部"的 offset，无法手动上滚。

**修复方式：**
- 引入 `auto_scroll: bool` 标志（默认 true），当新消息到来时且 auto_scroll 为 true 则自动滚到底部
- 用户按 Up/PageUp 时：减少 scroll_offset，设 auto_scroll = false
- 用户按 Down/PageDown 时：增加 scroll_offset，如果到达底部则恢复 auto_scroll = true
- `render_messages` 使用 `state.scroll_offset` 而非硬算
- 在 `Idle` 和 `Running` 状态下都允许滚动（Running 时用户也应能回看）

**键位绑定：**
- `PageUp` / `Ctrl+U` — 向上翻一屏
- `PageDown` / `Ctrl+D` — 向下翻一屏
- `Up` — 向上滚 1 行（仅在 Idle 时，Running 时 Up 仍可滚动）
- `Down` — 向下滚 1 行
- 新消息到来时如果 auto_scroll=true 自动更新 scroll_offset 到底部

**关键文件：**
- `src/tui/types.rs` — 去除 `#[allow(dead_code)]`，添加 `auto_scroll: bool`，在 `update()` 中触发自动滚动
- `src/tui/ui.rs` — `render_messages` 使用 `state.scroll_offset`
- `src/tui/app.rs` — 添加 PageUp/PageDown/Up/Down 的键盘处理（在所有非 Setup 状态生效）

## 实现顺序

1. **滚动修复**（Bug fix，优先）
2. **审批弹框**（提升交互安全感）
3. **Markdown 渲染**（提升内容可读性）
4. **Setup 引导优化**（提升首次体验）

## 新增依赖

```toml
tui-markdown = "0.3"
```

## 验证方式

1. `cargo build` — 编译通过
2. `cargo test` — 现有 80 个测试不受影响
3. `cargo clippy` — 无警告
4. 手动验证：
   - 删除 `~/.config/orca/config.toml` → `cargo run` → 看到分步引导
   - 输入 API key → 提交 prompt → 看到 markdown 格式化的回复
   - 触发 bash 命令 → 看到弹框 → 上下箭头切换 → Enter 确认
   - 长对话后 → PageUp 向上滚动 → 看到历史消息 → PageDown 回到底部
