# 配置文件实现计划

## Context

当前 Orca 所有运行时参数通过 CLI 参数传递，API key / base_url / model 等敏感和模型配置完全依赖环境变量（在 `deepseek_http.rs` 内部 `env::var()` 读取）。这有几个问题：
- 每次运行都需要 `export` 环境变量或在命令行前拼接
- 无法持久化用户偏好（默认 provider、approval_mode 等）
- 参考 Codex CLI 的分层配置是更好的体验

目标：实现 TOML 配置文件支持，分为**用户级**和**项目级**两层，配合 CLI 参数形成三层优先级覆盖。

## 设计

### 配置文件位置

| 层级 | 路径 | 说明 |
|------|------|------|
| 用户级 | `~/.config/orca/config.toml` | 全局默认配置（XDG 兼容） |
| 项目级 | `<workspace>/.orca/config.toml` | 项目特定覆盖 |

优先级：CLI 参数 > 项目级 > 用户级 > 硬编码默认值

### 配置文件字段

```toml
# ~/.config/orca/config.toml

# Provider 配置
model = "deepseek-chat"
api_key = "sk-..."
base_url = "https://api.deepseek.com"
provider = "deepseek"

# 运行时默认值
approval_mode = "workspace-write"
max_turns = 10
```

项目级 `.orca/config.toml` 不允许设置 `api_key`（安全考虑，只能用户级或环境变量），其他字段可覆盖。

### 优先级示例（以 model 为例）

1. 环境变量 `DEEPSEEK_MODEL`（兼容现有行为）
2. CLI `--model deepseek-reasoner`
3. `.orca/config.toml` 中的 `model = "..."`
4. `~/.config/orca/config.toml` 中的 `model = "..."`
5. 硬编码 `"deepseek-chat"`

### 敏感字段处理

- `api_key` 只从以下来源读取（不读项目级配置）：
  1. 环境变量 `DEEPSEEK_API_KEY`（最高优先级）
  2. 用户级 `~/.config/orca/config.toml` 的 `api_key` 字段

## 实现步骤

### Step 1: 添加 `toml` 依赖

在 `Cargo.toml` 中添加 `toml = "0.8"` 和 `dirs = "6"` (获取 home 目录)。

### Step 2: 新建 `src/config/file.rs` 配置文件模块

- 定义 `FileConfig` 结构体（所有字段 `Option<T>`，支持部分配置）
- 实现 `load_user_config()` → 读取 `~/.config/orca/config.toml`
- 实现 `load_project_config(cwd)` → 读取 `<cwd>/.orca/config.toml`
- 实现 `merge(user, project)` → 项目级覆盖用户级，生成合并后的 `FileConfig`

### Step 3: 将 `src/config.rs` 改为 `src/config/mod.rs` 模块目录

- 保留 `RunConfig`、`ProviderKind`、`OutputFormat` 在 `mod.rs`
- 添加 `pub mod file;`
- 在 `RunConfig` 中新增 `model`、`api_key`、`base_url` 字段（`Option<String>`）

### Step 4: CLI 层合并配置

在 `cli.rs` 的 `run_exec()` 中：
1. 加载合并后的 FileConfig
2. CLI 参数覆盖 FileConfig
3. 环境变量作为最终 fallback（兼容现有行为）
4. 构建 RunConfig 传入 controller

### Step 5: Provider 从 RunConfig 读取配置

修改 `deepseek_http.rs`：
- `call()` 接收 RunConfig 中的 `api_key`/`base_url`/`model`
- 不再直接 `env::var()`，统一从上层传入
- 需要调整 `provider::call()` 签名或在 Conversation 中携带配置

### Step 6: 更新 .gitignore

添加 `.env` 忽略规则（安全）。

### Step 7: 测试

- 单元测试：`FileConfig` 解析、merge 逻辑、优先级覆盖
- 集成测试：用临时 config.toml 文件运行 CLI 验证行为
- `cargo test` + `cargo clippy` 全部通过

## 关键文件

- `Cargo.toml` — 新增 `toml`、`dirs` 依赖
- `src/config.rs` → `src/config/mod.rs` — 模块化
- `src/config/file.rs` — 新增配置文件加载逻辑
- `src/cli.rs` — 合并 FileConfig → RunConfig
- `src/provider/deepseek_http.rs` — 从 RunConfig 获取 api_key/model/base_url
- `src/provider/mod.rs` — 调整 `call()` 签名传入配置
- `.gitignore` — 添加 `.env`

## 验证

1. `cargo test` — 全部通过
2. `cargo clippy` — 无 warning
3. 手动测试：
   - 创建 `~/.config/orca/config.toml` 设置 api_key + model
   - 运行 `orca exec --provider deepseek "hello"` 不设环境变量 → 从配置文件读取
   - 设 `DEEPSEEK_API_KEY=xxx` → 环境变量覆盖配置文件
   - 项目目录创建 `.orca/config.toml` 设置 model → 验证项目级覆盖
