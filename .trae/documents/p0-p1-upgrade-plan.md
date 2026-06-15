# Orca P0/P1 功能升级计划

## Context

Orca 的 agent loop 架构已完成，但 `deepseek_http.rs` 的 HTTP 层仍是最简原型状态：无超时、无重试、无流式、无上下文管理、`finish_reason` 被忽略、系统提示词是占位符。这些缺陷导致无法可靠地使用真实 DeepSeek API 执行多轮任务。

参考了 Codex CLI (Rust) 和 deepseek-tui 的实现模式后，设计如下升级方案。

---

## 实施顺序

```
Step 1: System prompt 完善（独立，最简单）
Step 2: finish_reason 处理（3 行改动）
Step 3: HTTP client 单例 + 超时 + 重试退避（新模块）
Step 4: 上下文窗口管理（新模块）
Step 5: SSE 流式支持（最复杂，依赖 Step 3）
```

---

## Step 1: System Prompt 完善

**文件**: `src/provider/system_prompt.rs`

将当前 ~15 行占位符扩展为结构化提示词（~2KB），包含：
- 工具参数的精确格式说明（每个工具列出 parameters + required）
- 安全边界（不执行破坏性命令、不泄露密钥、不操作 workspace 外文件）
- 工作流指引（先读后改、最小变更、验证结果）
- 多轮行为规则（失败时换策略、无法完成时说明原因）
- 动态注入 `cwd` 和 `os`

---

## Step 2: finish_reason 处理

**文件**: `src/provider/deepseek_http.rs` (第 130 行)

将 `let _finish_reason = ...` 改为：
- `"length"` → push `ProviderStep::Error("Response truncated: hit max_tokens limit")`
- `"content_filter"` → return `Err("Response blocked by content filter")`
- `"stop"` / `"tool_calls"` / `""` → 正常，不处理
- 其他 → push warning error step

---

## Step 3: HTTP Client + 超时 + 重试

**新文件**: `src/provider/http_client.rs`

**Cargo.toml 变更**: 新增 `fastrand = "2"` 依赖

**设计**:
- `LazyLock<reqwest::blocking::Client>` 全局单例，配置：
  - `connect_timeout`: 30s
  - `timeout`(read): 120s（非流式）
- 第二个单例 `streaming_client()` 用更长超时：
  - `connect_timeout`: 30s  
  - `read_timeout`: 300s（SSE idle 超时）
- `execute_with_retry()` 函数：
  - 最多 3 次重试（共 4 次尝试）
  - 可重试状态码: `[429, 500, 502, 503, 504]`
  - 指数退避: `initial=1s, factor=2.0, max=60s, jitter=±10%`
  - 解析 `Retry-After` header（如有则用其值）
  - 网络错误和超时也重试

**修改 `deepseek_http.rs`**: 删除 `reqwest::blocking::Client::new()`，改用 `http_client::client()` + `execute_with_retry()`。

---

## Step 4: 上下文窗口管理

**新文件**: `src/provider/context.rs`

**设计**:
- Token 估算: `chars.count() / 4`（参考 deepseek-tui 的做法）
- 默认上下文窗口: 128K tokens
- 触发阈值: 窗口的 80%（减去 4096 response 预留）
- 压缩策略（简单截断）：
  - 始终保留: system message + 最近 N 轮对话
  - 从最旧的 assistant+tool 对开始丢弃
  - 插入 `[Earlier history truncated]` 标记

**修改 `src/runtime/controller.rs`**: 在每次 `provider::call()` 前检查 `needs_compaction()`，触发则执行 `compact()`。

---

## Step 5: SSE 流式支持

**新文件**: `src/provider/streaming.rs`

**架构**: 保持同步（不引入 tokio），利用 `reqwest::blocking::Response` 实现 `Read` trait 的特性，用 `BufReader` 逐行读取 SSE。

**关键数据结构**:
```rust
StreamChunk { choices: [StreamChoice] }
StreamChoice { delta: StreamDelta, finish_reason: Option<String> }
StreamDelta { content, reasoning_content, tool_calls: [StreamToolCallDelta] }
StreamToolCallDelta { index, id, function: { name, arguments } }
```

**SSE 解析逻辑**:
1. `BufReader::lines()` 逐行读取
2. 跳过空行和 `:` 注释行
3. `data: [DONE]` → 结束
4. `data: {json}` → 解析为 `StreamChunk`
5. 累积 reasoning_content / content / tool_calls（tool_calls 按 `index` 跟踪）

**Provider 接口变更**:

在 `src/provider/mod.rs` 新增:
```rust
pub fn call_streaming(
    kind: ProviderKind,
    conversation: &Conversation,
    on_step: &mut dyn FnMut(&ProviderStep),
) -> ProviderResponse
```

Mock/Fixture provider 退化为非流式调用后逐个回调。

**Controller 变更**: `run_agent_loop` 中替换 `provider::call()` 为 `provider::call_streaming()`，回调中直接 `sink.emit()` delta 事件实现实时输出。

**DeepSeek 特殊处理**:
- `reasoning_content` 和 `reasoning` 双字段兼容（`#[serde(alias)]`）
- 有 tool_calls 但无 reasoning_content 时注入 `"(reasoning omitted)"` 占位（API 要求）
- tool_calls 流式按 `index` 增量累积 id/name/arguments

---

## 验证方式

1. `cargo test` — 所有现有 16 个测试继续通过（Mock/Fixture 不受影响）
2. 新增 `#[cfg(test)]` 单元测试:
   - `http_client`: backoff 计算正确性、jitter 范围
   - `context`: token 估算、compaction 保留最近消息
   - `streaming`: SSE 行解析、`[DONE]` 处理、tool_call 累积
3. 手动 E2E: `DEEPSEEK_API_KEY=xxx orca exec --provider deepseek "explain this repo"` 验证流式实时输出

---

## 关键文件清单

| 操作 | 文件 |
|------|------|
| 新建 | `src/provider/http_client.rs` |
| 新建 | `src/provider/context.rs` |
| 新建 | `src/provider/streaming.rs` |
| 修改 | `src/provider/system_prompt.rs` |
| 修改 | `src/provider/deepseek_http.rs` |
| 修改 | `src/provider/mod.rs` |
| 修改 | `src/runtime/controller.rs` |
| 修改 | `Cargo.toml` |
