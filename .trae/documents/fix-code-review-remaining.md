# 剩余修复计划（从中断处续）

## 背景
之前的 Code Review 发现 25 个问题。Critical(C1-C2) 和 High(H1-H6) 已修复完成。Medium 批次进行到 M2 中断。

## 当前状态
- ✅ C1: resolve_workspace_path 路径穿越防护
- ✅ C2: MCP Stdio 超时 + StdioState Drop
- ✅ H1-H6: 全部完成
- ✅ M1: SseTransport client 字段复用
- ⚠️ M2: SSE initialize 调用了 self.notify() 但 notify 方法未添加到 SseTransport

## 剩余修复

### M2（完成）: SseTransport 添加 notify 方法
- 文件: `src/mcp/transport.rs`
- 在 `impl SseTransport` 块（第 240 行 request 方法之后）添加 notify 方法

### M3: MCP tool name 冲突检测
- 文件: `src/mcp/client.rs`
- 在 `lookup.insert()` 前检查 `lookup.contains_key()`，冲突时 push warning 到 errors 并 skip

### M4: /model /mode 无参数时返回当前值
- 文件: `src/tui/commands/mod.rs`
- `Model(String)` → `Model(Option<String>)`，`Mode(String)` → `Mode(Option<String>)`
- 解析逻辑对应调整

### M5: glob `?` UTF-8 多字节字符修复
- 文件: `src/approval/rules.rs`
- `b'?'` 分支中 `v += 1` 改为按 UTF-8 首字节计算字符长度步进

### M6: FNV-1a 替换 DefaultHasher
- 文件: `src/runtime/memory.rs`
- `project_hash()` 使用手动 FNV-1a 实现（无依赖）

### M7: truncate_output 小预算 fallback 不超出预算
- 文件: `src/tools/mod.rs`
- 小预算分支直接截断到 max_bytes，不追加 `\n[truncated]` 后缀

### L1: CancelToken 原子序改为 Release/Acquire
- 文件: `src/runtime/cancel.rs`
- store → `Ordering::Release`，load → `Ordering::Acquire`

### L2: Hook 输出截断 64KB
- 文件: `src/runtime/hooks.rs`
- `output.stdout` 和 `output.stderr` 限制到 64KB 再做 from_utf8_lossy

### L3: format_messages_for_memory 32KB 上限
- 文件: `src/runtime/memory.rs`
- 输出超过 32KB 时 break

### L4: bash env_remove 敏感变量
- 文件: `src/tools/bash.rs`
- 在 `sandbox::bash_command()` 返回值上链式调用 `.env_remove("ORCA_API_KEY")`

### L5: 已由 H6 基本解决（工具失败不终止循环），无需额外改动

### L6: memory append_note 无锁说明
- 文件: `src/runtime/memory.rs`
- 在 append_note 上方加简短注释说明单追加操作在当前单线程使用场景下无需锁

## 验证
- `cargo check` 编译通过
- `cargo test` 全部通过
