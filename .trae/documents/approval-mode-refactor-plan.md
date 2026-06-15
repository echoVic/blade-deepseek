# 审批模式重构计划

## Context

当前审批模式参考了 Codex CLI 的三级模型（ReadOnly / WorkspaceWrite / FullAuto），但实现为"纯策略自动决策"——拒绝时直接终止，没有交互确认。用户希望改为更接近 Codex CLI 和 Claude Code 的模式：默认需要用户确认写和 shell 操作，而不是直接拒绝。

参考了 Claude Code 的权限模型（`default` / `acceptEdits` / `bypassPermissions`），核心是三态决策：`allow` / `ask` / `deny`。

## 目标模式

| 模式 | 读操作 | 写操作(edit) | Shell(bash) | 说明 |
|------|--------|-------------|-------------|------|
| `suggest`（默认） | allow | ask | ask | 所有危险操作需确认 |
| `auto-edit` | allow | allow | ask | 文件操作自动放行 |
| `full-auto` | allow | allow | allow | 全部自动执行 |

- `ask` = 在 text 模式下通过 stderr 向用户询问 y/n；在 jsonl 模式下自动 deny（无法交互）

## 实现步骤

### Step 1: 修改枚举定义 (`src/approval/policy.rs`)

```rust
pub enum ApprovalMode {
    #[default]
    Suggest,      // "suggest"
    AutoEdit,     // "auto-edit"  
    FullAuto,     // "full-auto"
}

pub enum ApprovalDecision {
    Allow,
    Ask,   // 新增：需要交互确认
    Deny,
}
```

`resolve()` 返回逻辑：

| Mode | Read | Write | Shell |
|------|------|-------|-------|
| Suggest | Allow | Ask | Ask |
| AutoEdit | Allow | Allow | Ask |
| FullAuto | Allow | Allow | Allow |

### Step 2: 新建交互确认模块 (`src/approval/confirm.rs`)

简单的 stderr 输出 + stdin 读取 y/n：

```rust
pub fn prompt_user(tool_name: &str, target: Option<&str>) -> io::Result<bool>;
pub fn prompt_user_with_io(description: &str, input: &mut impl BufRead, output: &mut impl Write) -> io::Result<bool>;
```

- 输出到 stderr（stdout 被 events 占用）
- 阻塞式读取
- `prompt_user_with_io` 支持单元测试注入

### Step 3: 修改控制器逻辑 (`src/runtime/controller.rs`)

在 `execute_tool_with_approval` 中：
- `Allow` → 直接执行
- `Ask` → 检查 output_format：
  - `Text` → 调用 `confirm::prompt_user()`，用户批准则执行，拒绝则终止
  - `Jsonl` → 自动 deny（附带原因 "interactive confirmation unavailable in jsonl mode"）
- `Deny` → 直接终止（保留给未来扩展）

需要将 `output_format` 传入 controller 或在 `execute_tool_with_approval` 中可用。

### Step 4: 更新 CLI (`src/cli.rs`)

默认值改为 `ApprovalMode::Suggest`，clap value names 改为 `suggest` / `auto-edit` / `full-auto`。

### Step 5: 更新测试

- **单元测试** (`src/approval/policy.rs`): 重写 resolve 测试覆盖新的三态矩阵
- **新单元测试** (`src/approval/confirm.rs`): 测试 y/yes/n/empty 输入
- **集成测试** (`tests/approval_contract.rs`): `--approval-mode suggest` + jsonl → 写操作自动 deny
- **集成测试** (`tests/tool_contract.rs`): 涉及 bash/edit 的测试需要加 `--approval-mode full-auto`（因为默认改为 suggest，在 jsonl 下无法交互）
- **其他集成测试**: 确认 mock provider 触发的 read 操作不受影响

### Step 6: 更新文档

README.md 和 harness-contract.md 中的审批模式描述。

## 关键文件

- `src/approval/policy.rs` — 枚举定义 + resolve 逻辑
- `src/approval/confirm.rs` — 新建，交互确认
- `src/approval/mod.rs` — 添加 `pub mod confirm`
- `src/runtime/controller.rs` — 集成交互逻辑
- `src/cli.rs` — 默认值更新
- `tests/approval_contract.rs` — 测试更新
- `tests/tool_contract.rs` — bash/edit 测试需加 `--approval-mode full-auto`

## 验证方式

1. `cargo test` 全部通过
2. `cargo clippy` 无警告
3. 手动验证 text 模式：`orca exec "edit some file"` → 应出现确认提示
4. 手动验证 jsonl 模式：`orca exec --output-format jsonl --provider mock "bash echo hi"` → 应自动 deny (exit 3)
5. 手动验证 full-auto：`orca exec --approval-mode full-auto "bash echo hi"` → 应直接执行
