# 工具系统对比分析

**日期**: 2026-06-16  
**对比对象**:
- Claude Code (package 3): `@anthropic-ai/claude-code 2.1.88`
- Orca (blade-deepseek): `v0.1.0`
- Codex CLI (GitHub)

---

## 执行摘要

### Claude Code 工具清单 (18个)

根据 `sdk-tools.d.ts` 的定义：

#### 核心工具 (7个)
1. **FileRead** - 文件读取（支持文本/图片/PDF/Notebook）
2. **FileWrite** - 文件写入
3. **FileEdit** - 文件编辑（精确文本替换）
4. **Bash** - Shell命令执行
5. **Grep** - 代码搜索（ripgrep）
6. **Glob** - 文件查找（glob模式）
7. **Agent** - 子代理（异步/同步模式）

#### 扩展工具 (7个)
8. **WebSearch** - Web搜索
9. **WebFetch** - Web内容获取
10. **NotebookEdit** - Jupyter Notebook编辑
11. **TodoWrite** - TODO管理
12. **McpResource** - MCP资源读取
13. **ListMcpResources** - MCP资源列表
14. **ReadMcpResource** - MCP资源详情

#### 控制工具 (4个)
15. **AskUserQuestion** - 用户交互询问
16. **Config** - 配置管理
17. **EnterWorktree** / **ExitWorktree** - Git worktree隔离
18. **TaskStop** - 任务停止

#### 特殊工具
19. **ExitPlanMode** - 计划模式退出
20. **TaskOutput** - 任务输出

### Orca 工具清单 (7个)

1. **read_file** - 文件读取（UTF-8，8KB截断）
2. **list_files** - 目录列表
3. **grep** - 代码搜索（ripgrep）
4. **bash** - Shell命令执行
5. **edit** - 文件编辑（精确文本替换）
6. **git_status** - Git状态查看
7. **subagent** - 子代理（同步模式）

---

## 一、工具功能对比矩阵

| 功能 | Claude Code | Orca | 实现复杂度 | 优先级 |
|------|------------|------|-----------|-------|
| **文件操作** |
| 文件读取 | ✅ 多格式 | ✅ 文本 | ⭐⭐⭐ | P0 |
| 文件写入 | ✅ | ❌ | ⭐ | P0 |
| 文件编辑 | ✅ | ✅ | ⭐⭐ | P0 |
| 图片读取 | ✅ Base64 | ❌ | ⭐⭐⭐ | P1 |
| PDF读取 | ✅ | ❌ | ⭐⭐⭐⭐ | P2 |
| Notebook编辑 | ✅ | ❌ | ⭐⭐⭐⭐ | P2 |
| **代码搜索** |
| Grep | ✅ | ✅ | ⭐⭐ | P0 |
| Glob | ✅ | ❌ | ⭐⭐ | P1 |
| 目录列表 | ❌ | ✅ | ⭐ | P0 |
| **执行** |
| Bash | ✅ 后台 | ✅ | ⭐⭐ | P0 |
| **Git** |
| Git状态 | ❌ | ✅ | ⭐ | P0 |
| Worktree隔离 | ✅ | ❌ | ⭐⭐⭐⭐ | P1 |
| **子代理** |
| 同步模式 | ✅ | ✅ | ⭐⭐⭐ | P0 |
| 异步模式 | ✅ | ❌ | ⭐⭐⭐⭐ | P1 |
| **Web** |
| Web搜索 | ✅ | ❌ | ⭐⭐⭐⭐ | P2 |
| Web抓取 | ✅ | ❌ | ⭐⭐⭐ | P2 |
| **MCP** |
| MCP集成 | ✅ | ❌ | ⭐⭐⭐⭐⭐ | P1 |
| **交互** |
| 用户询问 | ✅ | ❌ | ⭐⭐⭐ | P1 |
| TODO管理 | ✅ | ❌ | ⭐⭐ | P3 |
| 配置管理 | ✅ | ❌ | ⭐⭐ | P2 |

---

## 二、核心工具深度对比

### 2.1 FileRead

#### Claude Code
```typescript
type FileReadOutput =
  | { type: "text"; file: { content, numLines, startLine, totalLines } }
  | { type: "image"; file: { base64, type, dimensions } }
  | { type: "notebook"; file: { cells } }
  | { type: "pdf"; file: { base64 } }
```

**特性**:
- 多格式支持（文本/图片/PDF/Notebook）
- 分段读取（offset/limit）
- 图片自动转Base64
- PDF页面选择
- 尺寸信息（图片坐标映射）

#### Orca
```rust
pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    // 只支持UTF-8文本
    // 8KB截断
    // 无分段读取
}
```

**差距**:
- ❌ 无图片支持
- ❌ 无PDF支持
- ❌ 无Notebook支持
- ❌ 无分段读取
- ✅ 有截断标记

**改进建议**:
1. 添加 `read_file_range` 支持分段读取
2. 添加图片Base64编码（检测MIME类型）
3. 考虑添加PDF支持（使用 `poppler` 或 `pdf.js`）

---

### 2.2 FileWrite vs FileEdit

#### Claude Code
**分离设计**:
- `FileWrite`: 完整覆盖文件
- `FileEdit`: 精确文本替换（老文本 → 新文本）

**FileEdit参数**:
```typescript
interface FileEditInput {
  file_path: string;
  old_string: string;
  new_string: string;
  replace_all?: boolean;
}
```

#### Orca
**只有 Edit**:
```rust
pub struct EditRequest {
    pub file_path: String,
    pub old_text: String,
    pub new_text: String,
}
```

**差距**:
- ❌ 无独立的 Write 工具
- ❌ 无 `replace_all` 选项
- ✅ 有唯一性检查（防止多次匹配）

**改进建议**:
1. **添加 `write_file` 工具** - 用于创建新文件
2. **Edit 添加 `replace_all`** - 支持全局替换
3. **Write 用于完全覆盖** - Edit 用于部分修改

---

### 2.3 Bash

#### Claude Code
```typescript
interface BashInput {
  command: string;
  description: string;
  run_in_background?: boolean;  // 关键特性
  timeout?: number;
}
```

**后台执行**:
- 支持 `run_in_background: true`
- 返回任务ID，后续用 `Read` 查看输出
- 支持长时间运行的任务

#### Orca
```rust
pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()?;
    // 阻塞执行
}
```

**差距**:
- ❌ 无后台执行
- ❌ 无超时控制
- ❌ 阻塞式执行（长任务会卡住）

**改进建议**:
1. **添加后台执行支持** - 返回任务ID
2. **添加 `bash_status` 工具** - 查询后台任务状态
3. **添加超时参数** - 防止无限等待

---

### 2.4 Grep

#### Claude Code
```typescript
interface GrepInput {
  pattern: string;
  path?: string;
  case_sensitive?: boolean;
  include_glob?: string;  // --glob
  exclude_glob?: string;  // --glob !
}
```

#### Orca
```rust
// 基本的 ripgrep 调用
// 无glob过滤
// 无大小写选项
```

**改进建议**:
1. 添加 `include_glob` / `exclude_glob`
2. 添加 `case_sensitive` 选项
3. 添加 `context_lines` 参数（-A/-B/-C）

---

### 2.5 Agent / Subagent

#### Claude Code
```typescript
type AgentOutput =
  | { status: "completed"; content; totalToolUseCount; totalTokens; usage }
  | { status: "async_launched"; agentId; outputFile; canReadOutputFile }
```

**两种模式**:
1. **同步模式** - 阻塞等待完成
2. **异步模式** - 返回agentId，输出写入文件

**异步优势**:
- 多个子代理并行执行
- 父代理可以继续工作
- 通过 `Read` 工具检查进度

#### Orca
```rust
fn execute_subagent_tool(
    // ...
    subagent_depth: u32,
) -> io::Result<tools::ToolResult> {
    // 同步阻塞执行
    // MAX_SUBAGENT_DEPTH = 1
}
```

**差距**:
- ❌ 无异步模式
- ❌ 无并行执行
- ❌ 无进度查询
- ✅ 有深度限制

**改进建议**:
1. **添加异步模式** - `run_in_background: true`
2. **添加 `subagent_status`** - 查询子代理状态
3. **放宽深度限制** - 支持2-3层（需要循环检测）

---

## 三、缺失工具分析

### 3.1 Glob（文件查找）

**Claude Code实现**:
```typescript
interface GlobInput {
  pattern: string;        // "**/*.ts"
  max_results?: number;   // 默认100
}

interface GlobOutput {
  files: string[];
  truncated: boolean;
}
```

**Orca现状**:
- 有 `list_files` 但只能列举单个目录
- 无递归查找
- 无模式匹配

**实现方案**:
```rust
pub fn glob(pattern: &str, cwd: &Path, max_results: usize) -> ToolResult {
    use glob::glob;
    let full_pattern = cwd.join(pattern);
    let matches: Vec<_> = glob(full_pattern.to_str().unwrap())?
        .filter_map(Result::ok)
        .take(max_results + 1)
        .collect();
    
    let truncated = matches.len() > max_results;
    let files = matches.iter().take(max_results)
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    
    ToolResult::completed(request, files, truncated)
}
```

**优先级**: P1

---

### 3.2 WebFetch / WebSearch

**Claude Code实现**:
```typescript
interface WebFetchInput {
  url: string;
}

interface WebSearchInput {
  query: string;
}
```

**价值**:
- 文档查询
- API参考
- 技术问题搜索

**实现方案**:
```rust
// 使用 reqwest + html2text
pub fn web_fetch(url: &str) -> ToolResult {
    let response = reqwest::blocking::get(url)?;
    let html = response.text()?;
    let text = html2text::from_read(html.as_bytes(), 80);
    ToolResult::completed(request, text, false)
}

// 使用 DuckDuckGo API 或 Brave Search API
pub fn web_search(query: &str) -> ToolResult {
    // ...
}
```

**优先级**: P2（需要外部API）

---

### 3.3 AskUserQuestion

**Claude Code实现**:
```typescript
interface AskUserQuestionInput {
  questions: Array<{
    question: string;
    header: string;
    options: Array<{ label, description }>;
    multiSelect?: boolean;
  }>;
}
```

**价值**:
- 关键决策询问用户
- 多选项选择
- 避免猜测用户意图

**Orca现状**:
- 只有审批确认（y/n）
- 无结构化询问

**实现方案**:
```rust
pub fn ask_user(questions: Vec<Question>) -> ToolResult {
    for q in questions {
        println!("{}", q.question);
        for (i, opt) in q.options.iter().enumerate() {
            println!("  {}. {} - {}", i+1, opt.label, opt.description);
        }
        let choice = read_user_input()?;
        // ...
    }
}
```

**优先级**: P1（提升交互体验）

---

### 3.4 Worktree隔离

**Claude Code实现**:
```typescript
interface EnterWorktreeInput {
  description?: string;
}
// 自动创建临时git worktree
// 子代理在隔离环境中工作
// 完成后自动清理
```

**价值**:
- 并行子代理不冲突
- 实验性修改隔离
- 自动回滚

**实现方案**:
```rust
pub fn enter_worktree(description: &str) -> ToolResult {
    let worktree_path = create_temp_worktree()?;
    // 切换工作目录
    // 记录worktree路径
}

pub fn exit_worktree() -> ToolResult {
    // 检查是否有变更
    // 无变更则删除worktree
}
```

**优先级**: P1（支持并行子代理）

---

## 四、架构差异分析

### 4.1 工具定义方式

#### Claude Code
```typescript
// 类型安全的 TypeScript 定义
export type ToolInputSchemas =
  | BashInput
  | FileEditInput
  | ...

// JSON Schema 自动生成
// 编译时类型检查
```

#### Orca
```rust
// 枚举定义
pub enum ToolName {
    ReadFile,
    ListFiles,
    Grep,
    Bash,
    Edit,
    GitStatus,
    Subagent,
}

// 手动解析JSON
pub struct ToolRequest {
    pub id: String,
    pub name: ToolName,
    pub action: ActionKind,
    pub target: Option<String>,
    pub raw_arguments: Option<String>,
}
```

**对比**:
- Claude Code: 类型更严格，自动生成Schema
- Orca: 更灵活，但需要手动解析

---

### 4.2 工具执行模式

#### Claude Code
```typescript
// 工具编排（toolOrchestration.ts）
async function executeToolCall(tool, context) {
  // 1. 权限检查
  // 2. Hook拦截
  // 3. 执行工具
  // 4. 结果转换
  // 5. 后处理
}

// 支持：
// - 异步执行
// - 流式输出
// - 中间状态
```

#### Orca
```rust
// 同步执行
pub fn execute(request: &ToolRequest, cwd: &Path) -> ToolResult {
    match request.name {
        ToolName::ReadFile => read_file::execute(request, cwd, MAX_BYTES),
        ToolName::Bash => bash::execute(request, cwd, MAX_BYTES),
        // ...
    }
}

// 特点：
// - 阻塞式
// - 简单直接
// - 无中间状态
```

**改进方向**:
1. 添加异步执行框架
2. 支持流式输出（大文件读取、长命令执行）
3. 添加工具生命周期钩子

---

### 4.3 错误处理

#### Claude Code
```typescript
// 详细的错误类型
type ToolError =
  | { type: "permission_denied" }
  | { type: "file_not_found" }
  | { type: "invalid_input" }
  | { type: "execution_failed"; details }
```

#### Orca
```rust
pub enum ToolStatus {
    Completed,
    Failed,
    Denied,
    NotImplemented,
}

// 错误信息只是字符串
pub error: Option<String>
```

**改进建议**:
1. 定义结构化错误类型
2. 添加错误码
3. 提供恢复建议

---

## 五、优先级改进路线图

### Phase 1: 补齐核心功能（P0）
1. ✅ 所有7个工具已实现
2. ✅ Subagent同步模式已实现
3. ✅ Git状态查看已实现

### Phase 2: 扩展基础工具（P1）

#### 2.1 添加 `write_file` 工具
```rust
pub fn write_file(path: &Path, content: &str) -> ToolResult
```
- 创建新文件
- 完整覆盖现有文件
- 与 `edit` 区分用途

#### 2.2 增强 `bash` 工具
```rust
pub struct BashInput {
    command: String,
    run_in_background: bool,
    timeout: Option<u64>,
}
```
- 后台执行支持
- 超时控制
- 任务状态查询

#### 2.3 添加 `glob` 工具
```rust
pub fn glob(pattern: &str, max_results: usize) -> Vec<PathBuf>
```
- 递归文件查找
- 模式匹配

#### 2.4 增强 `subagent` 工具
```rust
pub enum SubagentMode {
    Sync,
    Async { output_file: PathBuf },
}
```
- 异步模式
- 进度查询
- 并行执行

#### 2.5 添加 `ask_user` 工具
```rust
pub fn ask_user(questions: Vec<Question>) -> Vec<Answer>
```
- 结构化询问
- 多选项支持

#### 2.6 Worktree隔离
```rust
pub fn enter_worktree() -> WorktreeGuard
pub fn exit_worktree(guard: WorktreeGuard)
```
- Git worktree自动管理
- 支持并行子代理

**预计工作量**: 2-3周

---

### Phase 3: 高级特性（P2）

#### 3.1 多格式文件读取
- 图片Base64编码
- PDF页面提取
- Notebook解析

#### 3.2 Web工具
- WebFetch（HTTP客户端）
- WebSearch（搜索API集成）

#### 3.3 MCP集成
- MCP协议实现
- 资源管理
- 工具扩展

**预计工作量**: 4-6周

---

## 六、技术债务与架构改进

### 6.1 工具注册机制

**当前**:
```rust
pub fn execute(request: &ToolRequest, cwd: &Path) -> ToolResult {
    match request.name {
        ToolName::ReadFile => read_file::execute(...),
        // 硬编码
    }
}
```

**改进**:
```rust
pub struct ToolRegistry {
    tools: HashMap<ToolName, Box<dyn Tool>>,
}

pub trait Tool {
    fn name(&self) -> ToolName;
    fn execute(&self, request: &ToolRequest) -> ToolResult;
    fn action_kind(&self) -> ActionKind;
}

impl ToolRegistry {
    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.insert(tool.name(), Box::new(tool));
    }
}
```

**优势**:
- 可扩展（插件系统）
- 可测试（Mock工具）
- 解耦

---

### 6.2 异步执行框架

**当前**: 所有工具都是同步阻塞

**改进**: 添加异步执行支持
```rust
pub enum ExecutionMode {
    Sync,
    Async,
    Background { task_id: String },
}

pub trait AsyncTool {
    async fn execute_async(&self, request: &ToolRequest) -> ToolResult;
}
```

---

### 6.3 工具结果流式输出

**当前**: 等待工具完成后返回全部输出

**改进**: 支持流式输出
```rust
pub struct ToolOutputStream {
    receiver: mpsc::Receiver<OutputChunk>,
}

pub enum OutputChunk {
    Stdout(String),
    Stderr(String),
    Progress(f32),
    Completed(ToolResult),
}
```

---

## 七、结论与建议

### 7.1 核心差距

| 维度 | Claude Code | Orca | 差距 |
|------|------------|------|-----|
| 工具数量 | 20个 | 7个 | 13个 |
| 异步支持 | ✅ | ❌ | 关键缺失 |
| 多格式文件 | ✅ | ❌ | 体验差距 |
| Web集成 | ✅ | ❌ | 功能缺失 |
| MCP支持 | ✅ | ❌ | 扩展性差距 |
| Worktree | ✅ | ❌ | 并行能力 |

### 7.2 优势保持

✅ **Orca的优势**:
1. 清晰的Rust类型系统
2. 简单的架构（易于理解和维护）
3. 独立的git_status工具
4. 完整的测试覆盖
5. 清晰的事件流设计

### 7.3 行动建议

**立即行动（1周内）**:
1. 添加 `write_file` 工具
2. 增强 `bash` 工具（timeout参数）
3. 添加 `glob` 工具

**短期目标（1个月内）**:
4. 实现异步subagent
5. 添加 `ask_user` 工具
6. Worktree隔离支持

**中期目标（3个月内）**:
7. 多格式文件读取
8. Web工具集成
9. MCP协议支持

**长期目标（6个月内）**:
10. 完整的工具插件系统
11. 工具编排DSL
12. 分布式工具执行

---

## 附录

### A. Claude Code工具完整列表

从 `sdk-tools.d.ts` 提取：

```typescript
// 文件操作
FileRead, FileWrite, FileEdit, NotebookEdit

// 搜索查找
Grep, Glob

// 执行
Bash

// Git
EnterWorktree, ExitWorktree

// 子代理
Agent

// Web
WebSearch, WebFetch

// MCP
ListMcpResources, ReadMcpResource, Mcp

// 交互
AskUserQuestion

// 管理
Config, TodoWrite, TaskStop, ExitPlanMode, TaskOutput
```

### B. 参考文档

- Claude Code源码: `/Users/qingyun/Documents/GitHub/package 3`
- 对比分析: `/Users/qingyun/Documents/GitHub/2026-04-04-package3-vs-blade-agent-sdk-agent-core-analysis.md`
- Orca实现: `src/tools/mod.rs`, `src/runtime/controller.rs`

---

**报告生成**: 2026-06-16  
**分析者**: Claude (Opus 4.8)
