# 修复 Code Review 发现的 25 个问题

## Context

Phase 2/3/4 实现完成后的 code review 发现了 3 Critical / 4 High / 11 Medium / 7 Low 问题。主要涉及安全漏洞（路径穿越）、资源泄漏（MCP 子进程）、稳定性风险（无限循环）和正确性缺陷。

---

## 修复计划（按优先级分批）

### 批次 1：Critical

| # | 文件 | 修复 |
|---|------|------|
| C1 | `src/tools/mod.rs` | `resolve_workspace_path` 返回 `Result<PathBuf, String>`，加 normalize + starts_with + canonicalize 检查。更新 `read_file.rs`、`list_files.rs` 调用点 |
| C2 | `src/mcp/transport.rs` | `StdioTransport::request()` 循环加 30s deadline + 1000 次迭代上限 |

### 批次 2：High

| # | 文件 | 修复 |
|---|------|------|
| H1 | `src/mcp/transport.rs` | 为 `StdioState` 实现 `Drop`（kill + wait child） |
| H2 | `src/runtime/subagent_config.rs` | `DEFAULT_MAX_SUBAGENT_DEPTH` 从 2 改为 1 |
| H3 | `src/tui/diff.rs` | `resolve_inside_workspace` 对已存在路径做 canonicalize 检查 symlink |
| H4 | `src/runtime/hooks.rs` | 添加 `sanitize_env_value()`：替换 `\n\r\0` 为空格，限制 4096 字符 |
| H5 | `src/runtime/controller.rs` | 子代理使用 `cancel.clone()` 替代 `CancelToken::new()` |
| H6 | `src/runtime/controller.rs` | 工具失败仅在 `ApprovalRequired` 时终止循环，`Failed` 继续 |

### 批次 3：Medium

| # | 文件 | 修复 |
|---|------|------|
| M1 | `src/mcp/transport.rs` | `SseTransport` 将 `Client` 存为字段，`new()` 时创建 |
| M2 | `src/mcp/transport.rs` | SSE `initialize()` 后发送 `notifications/initialized` |
| M3 | `src/mcp/client.rs` | `lookup.insert()` 前检查冲突，push warning 到 errors |
| M4 | `src/tui/commands/mod.rs` | `Model`/`Mode` 改为包含 `Option<String>`，无参数返回 `Some(...)` |
| M5 | `src/approval/rules.rs` | glob `?` 按 UTF-8 字符长度前进（1-4字节） |
| M6 | `src/runtime/memory.rs` | `project_hash` 用 FNV-1a 替换 `DefaultHasher` |
| M7 | `src/tools/mod.rs` | `truncate_output` 小预算 fallback 不追加 `[truncated]` 后缀 |

### 批次 4：Low

| # | 文件 | 修复 |
|---|------|------|
| L1 | `src/runtime/cancel.rs` | store 用 `Release`，load 用 `Acquire` |
| L2 | `src/runtime/hooks.rs` | stdout/stderr 截断到 64KB |
| L3 | `src/runtime/memory.rs` | `format_messages_for_memory` 加 32KB 上限 |
| L4 | `src/tools/web_search.rs` / `bash.rs` | bash Command 加 `.env_remove("BRAVE_SEARCH_API_KEY")` |
| L5 | `src/runtime/controller.rs` | batch 结果全部加入 conversation 后再判断终止 |
| L6 | `src/runtime/memory.rs` | `append_note` 添加注释说明无锁限制 |

---

## 关键实现细节

### C1: resolve_workspace_path

```rust
fn resolve_workspace_path(cwd: &Path, target: Option<&str>) -> Result<PathBuf, String> {
    let target = target.unwrap_or(".");
    let candidate = PathBuf::from(target);
    let joined = if candidate.is_absolute() { candidate } else { cwd.join(candidate) };

    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => { normalized.pop(); }
            Component::CurDir => {}
            _ => normalized.push(component),
        }
    }

    if !normalized.starts_with(cwd) {
        return Err(format!("path escapes workspace: {target}"));
    }

    if normalized.exists() {
        let canonical = normalized.canonicalize().map_err(|e| format!("cannot resolve: {e}"))?;
        let canonical_cwd = cwd.canonicalize().map_err(|e| format!("cannot resolve cwd: {e}"))?;
        if !canonical.starts_with(&canonical_cwd) {
            return Err(format!("path escapes workspace via symlink: {target}"));
        }
    }
    Ok(normalized)
}
```

### H5+H6+L5: controller.rs 逻辑重构

- `execute_subagent_batch` / `execute_subagent_tool` 接收 `cancel: &CancelToken` 参数，内部使用 `cancel.clone()`
- 结果循环：所有 tool results 先全部 add 到 conversation，然后仅检查是否有 `ApprovalRequired`

### M5: glob UTF-8 修复

```rust
b'?' => {
    if value[v] == b'/' { return false; }
    let char_len = match value[v] {
        b if b < 0x80 => 1,
        b if b < 0xE0 => 2,
        b if b < 0xF0 => 3,
        _ => 4,
    };
    p += 1;
    v += char_len.min(value.len() - v);
}
```

### M6: FNV-1a 替换 DefaultHasher

```rust
fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
```

---

## 验证策略

1. 每批修复后运行 `cargo test` 确认全部通过
2. 为以下修复添加新测试：
   - `resolve_workspace_path`：路径穿越拒绝 + 合法路径通过
   - `sanitize_env_value`：换行/null/超长
   - glob UTF-8：多字节字符匹配
   - `project_hash` FNV-1a：确定性验证
   - `/model` 无参数解析
   - MCP tool 冲突检测
3. 最终运行 `cargo clippy` 确认无警告

---

## 受影响的关键文件

- `src/tools/mod.rs` + `read_file.rs` + `list_files.rs`
- `src/mcp/transport.rs` + `client.rs`
- `src/runtime/controller.rs`
- `src/runtime/subagent_config.rs`
- `src/runtime/cancel.rs`
- `src/runtime/hooks.rs`
- `src/runtime/memory.rs`
- `src/tui/diff.rs`
- `src/tui/commands/mod.rs`
- `src/approval/rules.rs`
- `src/tools/bash.rs`（env_remove）
