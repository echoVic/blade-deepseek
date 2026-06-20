# Feature 4: write_file 工具 — 剩余步骤

## 概况

write_file 工具的核心实现已完成（`src/tools/write_file.rs`, `src/tools/mod.rs`, `src/provider/tool_schema.rs`），还需要完成以下 3 处连线：

## 待修改文件

### 1. `src/provider/deepseek_http.rs` — parse_tool_call 增加 "write_file" arm

在第 378 行 `"edit"` arm 之后、`"git_status"` arm 之前插入：

```rust
"write_file" => (
    ToolName::WriteFile,
    ActionKind::Write,
    args["path"].as_str().map(String::from),
),
```

### 2. `src/provider/system_prompt.rs` — Available Tools 段增加 write_file 描述

在 `### edit` 段之后、`### git_status` 段之前插入：

```
### write_file
Create or overwrite a file with the given content.
Parameters:
- `path` (required): File path relative to workspace root.
- `content` (required): The full content to write to the file.
```

### 3. `src/runtime/subagent_types.rs` — allowed_tools 增加 "write_file"

在以下类型的 `allowed_tools()` 向量中加入 `"write_file"`：
- `General` — 通用代理需要创建文件能力
- `TestWriter` — 测试编写需要创建测试文件
- `Debugger` — 调试场景偶尔需要创建临时文件
- `Documenter` — 文档代理需要创建文档文件
- `Custom` — 与 General 保持一致

不添加到 `CodeReviewer`（只读审查，不需要写入能力）。

## 验证步骤

1. `cargo build` 无错误
2. `cargo test` 全量通过
3. `cargo clippy` 无 warning
