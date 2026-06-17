# 重构：智能路由改为 Claude Code 风格

## Context

当前 `auto` 模式基于 prompt 关键词启发式自动升降级（flash↔pro），存在问题：
1. `has_tool_results` 导致第二轮起永远走 Pro，路由形同虚设
2. 启发式关键词匹配不可预测，用户对回答质量波动感到困惑
3. 与 Claude Code / Codex CLI 的设计哲学不符——它们都是**用户选主模型，系统只对辅助任务降级**

**目标**：采用 Claude Code 风格——主循环固定用用户选择的模型（auto 默认 Pro），辅助任务（compaction、memory extraction）强制使用 Flash。

## 设计方案

### 1. 简化 `ModelSelection::route()` 逻辑

**文件**: `src/model.rs`

重构后的路由：
- `auto` / `None` → 主循环固定用 **Pro**（不再做 prompt 分析）
- `flash` → 固定 Flash
- `pro` → 固定 Pro
- 子代理 override 仍然优先级最高

删除：
- `should_route_to_pro()` 函数
- `route_reason_for_pro()` 函数
- `prompt_looks_complex()` 函数
- `ModelRouteReason::ComplexPrompt` 和 `ToolContinuation` 变体
- `ModelRouteContext` 中的 `has_tool_results` 和 `prompt` 字段

保留：
- `ModelRouteReason::Explicit` — 用户显式选了 flash 或 pro
- `ModelRouteReason::DefaultPro` — auto 模式默认 Pro（原 DefaultFlash 改名）
- `ModelRouteReason::SubagentType` — 但语义改为：特定子代理**降级**到 Flash（如 Documenter、TestWriter 等轻量角色）
- `ModelRouteReason::SubagentOverride` — 不变

新增：
- `ModelRouteReason::AuxiliaryTask` — 辅助任务强制用 Flash

### 2. 引入 `auxiliary_model()` 函数

**文件**: `src/model.rs`

```rust
pub fn auxiliary_model() -> &'static str {
    FLASH_MODEL
}
```

所有辅助/后台 LLM 调用统一通过这个函数获取模型名，确保 SSOT。

### 3. 修正辅助任务调用处

**文件**: `src/runtime/controller.rs` 和 `src/tui/bridge.rs`

调用 `extract_project_memory` 和 `compact_with_summary` 时，传入 `auxiliary_model()` 而非 `config.summary_model.or(main_model)`。

去掉 `config.summary_model` 配置项（不再需要，辅助任务永远是 Flash）。

### 4. 精简 `ModelRouteContext`

路由不再需要分析 prompt 内容和 tool results，context 精简为：

```rust
pub struct ModelRouteContext<'a> {
    pub subagent_type: &'a SubagentType,
    pub subagent_model: Option<&'a str>,
}
```

`turn` 字段也不再需要。

### 5. 子代理路由调整

- `CodeReviewer` / `Debugger` → 用主模型（Pro）——它们需要深度推理
- `Documenter` / `TestWriter` → 可选降级到 Flash（节省成本，但这些任务也不简单）
- 暂定：所有子代理默认继承主模型（Pro），除非显式 override

### 6. 更新测试

修改 `src/model.rs` 中的 4 个现有测试以适配新逻辑。删除与 `prompt_looks_complex` 相关的测试，新增：
- `auto_defaults_to_pro`
- `auxiliary_model_returns_flash`

### 7. 去掉 `summary_model` 配置

**文件**: `src/config/file.rs`, `src/config/mod.rs`

删除 `summary_model` 字段，辅助任务模型不再可配（永远 Flash）。

## 涉及文件

| 文件 | 修改类型 |
|------|---------|
| `src/model.rs` | 重写路由逻辑 |
| `src/runtime/controller.rs` | 简化 route 调用、修正辅助任务模型 |
| `src/tui/bridge.rs` | 同上 |
| `src/config/file.rs` | 删除 `summary_model` |
| `src/config/mod.rs` | 删除 `summary_model` 引用 |
| `src/provider/context.rs` | 简化 `request_summary` 参数 |
| `src/runtime/memory.rs` | 简化 model 参数传递 |
| `src/event/schema.rs` | 更新 reason enum 序列化 |

## 验证

1. `cargo build` 编译通过
2. `cargo test` 全部通过
3. 手动测试：启动 TUI，确认 auto 模式下 `model.routed` 事件显示 `default_pro`
4. 确认 `/model flash` 切换后主循环用 flash
