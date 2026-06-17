# Slash Command 自动补全菜单

## Context

当前斜杠命令只是文本解析，没有交互式体验。用户输入 `/` 时应弹出候选列表，选择 `/model` 后应弹出模型选择子菜单。参考主流 CLI 工具（Claude Code、Codex）的交互。

## 实现方案

### 新增状态

`src/tui/types.rs` 添加：

```rust
pub struct SlashMenu {
    pub items: Vec<SlashMenuItem>,
    pub selected: usize,
}

pub struct SlashMenuItem {
    pub command: String,      // "/model"
    pub description: String,  // "Switch model"
    pub sub_items: Option<Vec<String>>, // ["/model" 下的子选项]
}
```

`AppState` 添加 `pub slash_menu: Option<SlashMenu>` 字段。

### 输入拦截逻辑

`src/tui/app.rs` 的 `AppStatus::Idle` 分支中：

1. **菜单已打开时**（`state.slash_menu.is_some()`）：在 `idle_shortcut` 之前拦截按键
   - `Up` / `Down` → 移动 `selected`
   - `Enter` → 选中当前项，将命令文本填入 textarea（如果有子菜单则进入子菜单）
   - `Esc` → 关闭菜单
   - 其他字符 → 传入 textarea 并更新过滤（如输入 `/mo` 过滤出 `/model` 和 `/mode`）
   - `Backspace` 删除到 `/` 消失时 → 关闭菜单

2. **菜单未打开时**：在 `textarea.input()` 之后检查——如果 textarea 内容以 `/` 开头且只有一行，打开菜单

### 菜单数据

`src/tui/commands/mod.rs` 添加 `pub fn all_commands()` 返回命令列表和描述：

```rust
pub fn all_commands() -> Vec<(&'static str, &'static str)> {
    vec![
        ("/help", "Show available commands"),
        ("/model", "Switch or show current model"),
        ("/compact", "Compress conversation context"),
        ("/clear", "Clear message history"),
        ("/cost", "Show session cost"),
        ("/mode", "Switch approval mode"),
        ("/plan", "Toggle plan mode"),
        ("/remember", "Save a note to memory"),
        ("/history", "Browse session history"),
        ("/exit", "Exit Orca"),
    ]
}
```

### 渲染

`src/tui/ui.rs` 添加 `render_slash_menu`：
- 位置：**紧贴输入框上方**（不是屏幕居中），宽度 = 输入框宽度，高度 = items 数量 + 2（边框）
- 样式：复用 approval dialog 的模式——`Clear` + `Block` + 逐行渲染，选中项加 `▸` 前缀 + 高亮色
- 在 `render()` 主函数中，`render_input` 之后调用（确保叠在消息区之上）

### /model 子菜单

选择 `/model` 后，菜单切换为子项列表：`["deepseek-v4-flash", "deepseek-v4-pro"]`。选中后执行模型切换。

### 修改文件清单

1. `src/tui/types.rs` — 添加 `SlashMenu` 结构体和 `AppState` 字段
2. `src/tui/commands/mod.rs` — 添加 `all_commands()` 
3. `src/tui/app.rs` — 输入拦截逻辑（菜单打开时的按键处理 + 检测 `/` 打开菜单）
4. `src/tui/ui.rs` — `render_slash_menu` 函数 + 在 `render()` 中调用

### 验证

1. `cargo build --release`
2. 运行 `./target/release/orca`
3. 输入 `/` → 弹出命令列表
4. 上下键选择 → 高亮移动
5. 输入 `/mo` → 列表过滤为 `/model` 和 `/mode`
6. Enter 选择 `/model` → 弹出模型子菜单
7. 选择模型 → 切换成功
8. Esc → 关闭菜单
