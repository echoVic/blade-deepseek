# 表格渲染优化方案

## 背景

当前 TUI 中的 Markdown 表格渲染存在问题：列宽完全由内容最大值决定，不考虑终端可用宽度。当表格内容较宽或终端较窄时，表格直接溢出屏幕，被 Paragraph 的 Wrap 暴力折行后破坏对齐结构，变得不可读。

参考 Codex CLI（codex-rs/tui/src/markdown_render.rs）的渐进降级策略进行优化。

## 核心改动文件

- `crates/orca-tui/src/ui.rs` — 修改 `render_markdown` 和 `render_table` 函数

## 实现方案

### 1. 传入可用宽度

- `render_markdown(input: &str)` → `render_markdown(input: &str, width: usize)`
- `render_table(rows, lines)` → `render_table(rows, lines, width)`
- 调用处（`build_message_lines`）需要接收 `content_width` 参数，从 `render_messages` 中的 `area.width.saturating_sub(2)` 获取

### 2. 列宽分配算法

在 `render_table` 内部：

1. 先按内容计算每列的**理想宽度**（当前逻辑，取每列最大内容宽度）
2. 计算表格总宽度 = 各列理想宽度之和 + 列间分隔符开销（每列 padding 2 字符 + 列间 gap 1 字符）
3. 如果总宽度 <= 可用宽度：按理想宽度渲染（与现在相同）
4. 如果总宽度 > 可用宽度：按比例缩放各列宽度，每列最小宽度设为 `max(该列表头宽度, 6)`
5. 如果缩放后最宽列仍然 < 12 字符：降级为 key/value 记录模式

### 3. 单元格内容换行

当列宽 < 单元格内容宽度时，对内容进行按词或按字符换行。表格的一行可能占据多行视觉行：

```
│ 能力     │ 描述                         │
│          │ 这是一段很长的描述文本，需要  │
│          │ 换行才能放得下                │
```

实现方式：每个 cell 按分配的列宽切分为多行文本（`wrap_text(cell, col_width)`），一个 table row 的视觉高度 = 该行所有 cell 的最大行数。

### 4. Key/Value 降级模式

当列宽被压缩到不可读时，渲染为 key/value 记录形式，每行数据独立展示：

```
━━━ Record 1 ━━━
  能力:  本地浏览器
  gstack: ✅
  flux-gui: ✅

━━━ Record 2 ━━━
  能力:  远程浏览器
  gstack: 通过 ngrok 隧道 (pair-agent 模式)
  flux-gui: 原生浏览器提供者抽象：支持远程浏览器实例
```

表头第一行的各列名作为 key，后续每行的对应列值作为 value。

### 5. 视觉风格更新（参考 Codex）

- 去掉外框边框（`┌┐└┘`），改为无边框风格
- 表头使用 Cyan + Bold
- 表头下方使用粗线 `━` 分隔
- 数据行间使用淡色细线 `─` 分隔（可选，仅在行数 >= 3 时）
- 列间使用 2 字符 gap 而非 `│` 分隔

### 6. 辅助函数

新增：
- `wrap_text(text: &str, width: usize) -> Vec<String>` — 按词/字符换行
- `render_table_as_records(rows, lines, width)` — key/value 降级渲染
- `allocate_column_widths(ideal_widths, available_width) -> Vec<usize>` — 列宽分配

## 验证

```bash
cargo test -p orca-tui
cargo run --release -p orca-tui
```

启动后让模型输出一个表格，然后缩小终端宽度验证：
1. 宽终端：表格正常网格渲染
2. 中等终端：表格列宽被压缩，单元格内容换行
3. 极窄终端：自动降级为 key/value 记录模式
