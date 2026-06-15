# TUI 4 项增强 — 最终完成计划

## 概要

代码已全部写好（types.rs, ui.rs, app.rs），只差添加 `tui-markdown` 依赖并编译验证。

## 当前状态

- ✅ `src/tui/types.rs` — ApprovalDialog, auto_scroll, total_lines, visible_height, setup_step, scroll 方法
- ✅ `src/tui/ui.rs` — 滚动、markdown 渲染 (`tui_markdown::from_str`)、审批弹框、分步 Setup
- ✅ `src/tui/app.rs` — 3步 Setup、Up/Down/Enter 审批、PageUp/PageDown/Ctrl+U/D 滚动
- ❌ `Cargo.toml` — 缺少 `tui-markdown` 依赖

## 执行步骤

### Step 1: 添加 tui-markdown 依赖
在 Cargo.toml 的 `[dependencies]` 中添加：
```toml
tui-markdown = { version = "0.3", default-features = false }
```
使用 `default-features = false` 避免引入不必要的 `syntect`/`ansi-to-tui` 等可选依赖。

### Step 2: 编译验证
```bash
cargo build 2>&1
```
如遇版本兼容问题（ratatui-core），考虑降级到 tui-markdown 0.2.12 或锁定特定版本。

### Step 3: 运行测试
```bash
cargo test 2>&1
```
确保既有 68+ 测试不因新依赖破坏。

### Step 4: Clippy 检查
```bash
cargo clippy 2>&1
```

### Step 5: 快速手动验证
```bash
cargo run
```
在无 API key 环境验证 Setup 引导流程，或有 key 时验证正常使用。

## 功能总结

| # | 需求 | 实现 |
|---|------|------|
| 1 | API key 引导 | 3步式 Setup（Welcome → API Key 输入 → 完成确认） |
| 2 | Markdown 渲染 | `tui_markdown::from_str()` 替代纯文本 |
| 3 | 确认弹框 | 居中弹窗 + Up/Down/j/k 切换 + Enter 确认 + y/n 快捷键 |
| 4 | 滚动 Bug | auto_scroll 双模式 + PageUp/Down/Ctrl+U/D/Up/Down |

## 风险

- `tui-markdown` 0.3.7 的 normal dep `ratatui-core ^0.1.0` 应兼容 ratatui 0.29
- 如不兼容，降级方案：使用 `tui-markdown = "0.2"` (基于 ratatui ^0.28)
- 或备选方案：改用 `pulldown-cmark` 手动渲染（代码量增大但零外部依赖冲突）
