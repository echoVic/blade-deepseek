# Tool System Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Redesign Orca's tool system so tool metadata, policy, aliases, prompt exposure, result semantics, and rendering are driven by specs instead of scattered name checks.

**Architecture:** Add a Codex-inspired internal tool layer while preserving Orca's current public tool names and JSONL compatibility. `ToolSpec` owns names, aliases, capabilities, exposure, schemas, and renderer hints; executors dispatch through canonical specs; runtime and TUI policy derive from capabilities. Keep the existing `ToolName` enum during the first migration and add namespace-aware helpers so current `ToolName::ReadFile` call sites continue to compile.

**Tech Stack:** Rust 2024, `serde`, `serde_json`, `tempfile`, existing Orca workspace crates, new workspace dependencies `globset = "0.4"` and `ignore = "0.4"` for the `glob` tool.

## Global Constraints

- Do not remove `bash`, `list_files`, `edit`, or `write_file` in the first migration.
- Do not build Codex's full code-mode or deferred-tool search stack in the first migration.
- Do not change the public JSONL event shape until the typed result model is proven internally.
- Structured file tools stay first-class; `bash` must not become the only file exploration path.
- Existing sessions and prompts must continue to parse current tool names.
- Missing optional read/search paths must complete instead of rendering as red failures.

---

## File Structure

- Modify `crates/orca-core/src/tool_types.rs`: namespace-aware `ToolName`, `ToolCapability`, `ToolExposure`, `ToolSpec`, `RendererHint`, `ToolResultKind`.
- Modify `crates/orca-tools/src/registry.rs`: register built-ins from specs, resolve aliases, expose direct tools, derive `ActionKind`, dispatch canonical specs.
- Modify `crates/orca-tools/src/lib.rs`: replace `ToolName::is_read_only()` fallback with registry capability checks.
- Create `crates/orca-tools/src/glob.rs`: first-class `glob` executor.
- Modify `crates/orca-tools/src/list_files.rs`: keep legacy list behavior while sharing filesystem discovery semantics.
- Modify `crates/orca-tools/Cargo.toml` and root `Cargo.toml`: add `globset` and `ignore` workspace dependencies if the `glob` executor uses them.
- Modify `crates/orca-runtime/src/controller.rs`: capability-based read batching and runtime availability checks.
- Modify `crates/orca-tui/src/bridge.rs`: mirror runtime read batching and execution behavior.
- Modify `crates/orca-provider/src/tool_schema.rs`: schema generation filters by exposure and context.
- Modify `crates/orca-provider/src/system_prompt.rs`: generate available tool text from specs.
- Modify `crates/orca-tui/src/ui.rs`: result-kind-aware display when tool result kinds are available.
- Modify `docs/harness-contract.md`: document `glob`, `list_files` compatibility, and empty/no-match semantics.

---

### Task 1: Core Tool Metadata

**Files:**
- Modify: `crates/orca-core/src/tool_types.rs`

**Interfaces:**
- Consumes: existing `ToolName`, `ToolRequest`, `ToolResult`, `ToolStatus`, `ActionKind`.
- Produces: `ToolName` namespace compatibility helpers, `ToolName::Glob`, `ToolCapability`, `CapabilitySet`, `ToolExposure`, `RendererHint`, `ResultSemantics`, `ToolSpec`, `ToolResultKind`.

- [ ] **Step 1: Write failing metadata tests**

Add these tests to `crates/orca-core/src/tool_types.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_round_trips_plain_names() {
        let name = ToolName::from_str("read_file").expect("known tool");
        assert_eq!(name, ToolName::ReadFile);
        assert_eq!(name.as_str(), "read_file");
        assert_eq!(serde_json::to_string(&name).unwrap(), "\"read_file\"");
        assert_eq!(serde_json::from_str::<ToolName>("\"read_file\"").unwrap(), name);
    }

    #[test]
    fn tool_name_preserves_mcp_namespace() {
        let name = ToolName::from_str("mcp__foo__exec_command").expect("mcp tool");
        assert_eq!(name.namespace(), Some("mcp__foo"));
        assert_eq!(name.local_name(), "exec_command");
        assert_eq!(name.as_str(), "mcp__foo__exec_command");
    }

    #[test]
    fn capability_set_derives_action_kind() {
        assert_eq!(CapabilitySet::read_only_fs().action_kind(), ActionKind::Read);
        assert_eq!(CapabilitySet::filesystem_write().action_kind(), ActionKind::Write);
        assert_eq!(CapabilitySet::shell_execute().action_kind(), ActionKind::Shell);
        assert_eq!(CapabilitySet::network_search().action_kind(), ActionKind::Network);
        assert_eq!(CapabilitySet::agent_delegate().action_kind(), ActionKind::Agent);
    }

    #[test]
    fn completed_result_kinds_remain_completed_status() {
        assert_eq!(ToolResultKind::Success.status(), ToolStatus::Completed);
        assert_eq!(ToolResultKind::Empty.status(), ToolStatus::Completed);
        assert_eq!(ToolResultKind::NoMatches.status(), ToolStatus::Completed);
        assert_eq!(ToolResultKind::Truncated.status(), ToolStatus::Completed);
        assert_eq!(ToolResultKind::InvalidInput.status(), ToolStatus::Failed);
        assert_eq!(ToolResultKind::RuntimeError.status(), ToolStatus::Failed);
    }
}
```

- [ ] **Step 2: Run test to verify failure**

Run:

```bash
cargo test -p orca-core tool_name_round_trips_plain_names tool_name_preserves_mcp_namespace capability_set_derives_action_kind completed_result_kinds_remain_completed_status
```

Expected: compile fails because the new metadata types and methods do not exist.

- [ ] **Step 3: Extend `ToolName` without breaking variants**

In `crates/orca-core/src/tool_types.rs`, keep the current enum variants and add `Glob` plus a namespace-aware variant:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolName {
    ReadFile,
    ListFiles,
    Glob,
    Grep,
    Bash,
    Edit,
    WriteFile,
    GitStatus,
    Subagent,
    Workflow,
    WebSearch,
    UpdateGoal,
    UpdatePlan,
    Namespaced {
        namespace: String,
        name: String,
        serialized: String,
    },
    Mcp(String),
    External(String),
}
```

Add helpers without removing current `ToolName::ReadFile` style call sites:

```rust
impl ToolName {
    pub fn plain(name: impl Into<String>) -> Self {
        let name = name.into();
        match name.as_str() {
            "read_file" => Self::ReadFile,
            "list_files" => Self::ListFiles,
            "glob" => Self::Glob,
            "grep" => Self::Grep,
            "bash" => Self::Bash,
            "edit" => Self::Edit,
            "write_file" => Self::WriteFile,
            "git_status" => Self::GitStatus,
            "subagent" => Self::Subagent,
            "Workflow" | "workflow" => Self::Workflow,
            "web_search" => Self::WebSearch,
            "update_goal" => Self::UpdateGoal,
            "update_plan" => Self::UpdatePlan,
            other => Self::External(other.to_string()),
        }
    }

    pub fn namespaced(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        let namespace = namespace.into();
        let name = name.into();
        let serialized = format!("{namespace}__{name}");
        Self::Namespaced {
            namespace,
            name,
            serialized,
        }
    }

    pub fn namespace(&self) -> Option<&str> {
        match self {
            Self::Namespaced { namespace, .. } => Some(namespace),
            Self::Mcp(name) => name.rsplit_once("__").map(|(namespace, _)| namespace),
            _ => None,
        }
    }

    pub fn local_name(&self) -> &str {
        match self {
            Self::ReadFile => "read_file",
            Self::ListFiles => "list_files",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::Bash => "bash",
            Self::Edit => "edit",
            Self::WriteFile => "write_file",
            Self::GitStatus => "git_status",
            Self::Subagent => "subagent",
            Self::Workflow => "Workflow",
            Self::WebSearch => "web_search",
            Self::UpdateGoal => "update_goal",
            Self::UpdatePlan => "update_plan",
            Self::Namespaced { name, .. } => name,
            Self::Mcp(name) => name.rsplit_once("__").map(|(_, local)| local).unwrap_or(name),
            Self::External(name) => name,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::ReadFile => "read_file",
            Self::ListFiles => "list_files",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::Bash => "bash",
            Self::Edit => "edit",
            Self::WriteFile => "write_file",
            Self::GitStatus => "git_status",
            Self::Subagent => "subagent",
            Self::Workflow => "Workflow",
            Self::WebSearch => "web_search",
            Self::UpdateGoal => "update_goal",
            Self::UpdatePlan => "update_plan",
            Self::Namespaced { serialized, .. } => serialized,
            Self::Mcp(name) | Self::External(name) => name,
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        if let Some((namespace, name)) = parse_namespaced_tool(value) {
            return Some(Self::namespaced(namespace, name));
        }
        Some(match value {
            "read_file" => Self::ReadFile,
            "list_files" => Self::ListFiles,
            "glob" => Self::Glob,
            "grep" => Self::Grep,
            "bash" => Self::Bash,
            "edit" => Self::Edit,
            "write_file" => Self::WriteFile,
            "git_status" => Self::GitStatus,
            "subagent" => Self::Subagent,
            "Workflow" | "workflow" => Self::Workflow,
            "web_search" => Self::WebSearch,
            "update_goal" => Self::UpdateGoal,
            "update_plan" => Self::UpdatePlan,
            other if other.starts_with("mcp__") => Self::Mcp(other.to_string()),
            other => Self::External(other.to_string()),
        })
    }

    pub fn is_builtin(&self, builtin: &str) -> bool {
        self.namespace().is_none() && self.as_str() == builtin
    }
}

fn parse_namespaced_tool(value: &str) -> Option<(&str, &str)> {
    let (namespace, name) = value.rsplit_once("__")?;
    if namespace.starts_with("mcp__") && !name.is_empty() {
        Some((namespace, name))
    } else {
        None
    }
}
```

Keep `Serialize` and `Deserialize` implementations serializing `as_str()`. Do not update existing call sites from `ToolName::ReadFile`; this task is intentionally compatibility-first.

- [ ] **Step 4: Add capability and spec types**

Append these definitions below `ToolName`:

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolCapability {
    FsRead,
    FsList,
    FsSearch,
    FsWrite,
    ShellExecute,
    GitInspect,
    NetworkSearch,
    AgentDelegate,
    WorkflowRun,
    PlanUpdate,
    GoalUpdate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilitySet {
    capabilities: Vec<ToolCapability>,
}

impl CapabilitySet {
    pub fn new(capabilities: Vec<ToolCapability>) -> Self {
        Self { capabilities }
    }

    pub fn read_only_fs() -> Self {
        Self::new(vec![ToolCapability::FsRead])
    }

    pub fn filesystem_write() -> Self {
        Self::new(vec![ToolCapability::FsWrite])
    }

    pub fn shell_execute() -> Self {
        Self::new(vec![ToolCapability::ShellExecute])
    }

    pub fn network_search() -> Self {
        Self::new(vec![ToolCapability::NetworkSearch])
    }

    pub fn agent_delegate() -> Self {
        Self::new(vec![ToolCapability::AgentDelegate])
    }

    pub fn contains(&self, capability: ToolCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn is_read_only(&self) -> bool {
        self.capabilities.iter().all(|capability| {
            matches!(
                capability,
                ToolCapability::FsRead
                    | ToolCapability::FsList
                    | ToolCapability::FsSearch
                    | ToolCapability::GitInspect
                    | ToolCapability::PlanUpdate
                    | ToolCapability::GoalUpdate
            )
        })
    }

    pub fn action_kind(&self) -> ActionKind {
        if self.contains(ToolCapability::ShellExecute) {
            ActionKind::Shell
        } else if self.contains(ToolCapability::FsWrite) {
            ActionKind::Write
        } else if self.contains(ToolCapability::NetworkSearch) {
            ActionKind::Network
        } else if self.contains(ToolCapability::AgentDelegate)
            || self.contains(ToolCapability::WorkflowRun)
        {
            ActionKind::Agent
        } else {
            ActionKind::Read
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolExposure {
    Direct,
    Deferred,
    ModelOnly,
    Hidden,
}

impl ToolExposure {
    pub fn is_model_visible(self) -> bool {
        matches!(self, Self::Direct | Self::ModelOnly)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RendererHint {
    FileRead,
    FileList,
    FileSearch,
    Shell,
    Write,
    Network,
    Agent,
    State,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResultSemantics {
    Standard,
    EmptyIsSuccess,
    NoMatchesIsSuccess,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultKind {
    Success,
    Empty,
    NoMatches,
    Truncated,
    PermissionDenied,
    InvalidInput,
    RuntimeError,
}

impl ToolResultKind {
    pub fn status(self) -> ToolStatus {
        match self {
            Self::Success | Self::Empty | Self::NoMatches | Self::Truncated => {
                ToolStatus::Completed
            }
            Self::PermissionDenied => ToolStatus::Denied,
            Self::InvalidInput | Self::RuntimeError => ToolStatus::Failed,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: ToolName,
    pub aliases: Vec<ToolName>,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: Option<serde_json::Value>,
    pub capabilities: CapabilitySet,
    pub exposure: ToolExposure,
    pub result_semantics: ResultSemantics,
    pub renderer: RendererHint,
    pub concurrent_safe: bool,
}
```

- [ ] **Step 5: Add result constructors with kind**

Extend `ToolResult` with a skipped compatibility field and constructors:

```rust
#[serde(default = "ToolResultKind::success", skip_serializing_if = "ToolResultKind::is_success")]
pub kind: ToolResultKind,
```

Implement:

```rust
impl ToolResultKind {
    pub fn success() -> Self {
        Self::Success
    }

    pub fn is_success(&self) -> bool {
        *self == Self::Success
    }
}

impl Default for ToolResultKind {
    fn default() -> Self {
        Self::Success
    }
}

impl ToolResult {
    pub fn completed_kind(
        request: &ToolRequest,
        output: String,
        truncated: bool,
        kind: ToolResultKind,
    ) -> Self {
        Self {
            id: request.id.clone(),
            name: request.name.clone(),
            status: kind.status(),
            output: Some(output),
            error: None,
            exit_code: Some(0),
            truncated,
            kind,
        }
    }
}
```

Update existing `completed`, `failed`, and `denied` constructors to set `kind`.

- [ ] **Step 6: Run core tests**

Run:

```bash
cargo test -p orca-core
```

Expected: all `orca-core` tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/orca-core/src/tool_types.rs
git commit -m "refactor: add structured tool metadata"
```

---

### Task 2: Spec-Driven Registry

**Files:**
- Modify: `crates/orca-tools/src/registry.rs`
- Modify: `crates/orca-tools/src/lib.rs`
- Modify: `crates/orca-provider/src/tool_schema.rs`

**Interfaces:**
- Consumes: `ToolSpec`, `ToolExposure`, `CapabilitySet`, `ToolName`.
- Produces: `Tool::spec() -> &ToolSpec`, `ToolRegistry::resolve()`, `ToolRegistry::model_visible_tools()`, capability-based read checks.

- [ ] **Step 1: Write failing registry tests**

Add these tests to `crates/orca-tools/src/lib.rs`:

```rust
#[test]
fn registry_resolves_list_files_to_discovery_capabilities() {
    let reg = registry::default_tool_registry();
    let resolved = reg.resolve("list_files").expect("list_files alias");

    assert_eq!(resolved.tool.name(), "glob");
    assert!(resolved.spec.capabilities.is_read_only());
    assert_eq!(resolved.requested_name.as_str(), "list_files");
}

#[test]
fn model_visible_tools_hide_list_files_after_glob_exists() {
    let reg = registry::default_tool_registry();
    let names = reg
        .model_visible_tools()
        .map(|tool| tool.name().to_string())
        .collect::<Vec<_>>();

    assert!(names.contains(&"glob".to_string()));
    assert!(!names.contains(&"list_files".to_string()));
}

#[test]
fn readonly_batch_ignores_caller_supplied_write_action_for_read_tool() {
    let request = ToolRequest {
        id: "read".to_string(),
        name: ToolName::ReadFile,
        action: ActionKind::Write,
        target: Some("README.md".to_string()),
        raw_arguments: None,
    };

    assert!(should_run_readonly_batch(2, &request));
}
```

Add this test to `crates/orca-provider/src/tool_schema.rs`:

```rust
#[test]
fn generated_schema_uses_model_visible_tools_only() {
    let registry = orca_tools::registry::default_tool_registry();
    let tools = deepseek_tools_schema_from_registry(registry);
    let names = tools
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect::<Vec<_>>();

    assert!(names.contains(&"glob"));
    assert!(!names.contains(&"list_files"));
}
```

- [ ] **Step 2: Run tests to verify failure**

Run:

```bash
cargo test -p orca-tools registry_resolves_list_files_to_discovery_capabilities model_visible_tools_hide_list_files_after_glob_exists readonly_batch_ignores_caller_supplied_write_action_for_read_tool
cargo test -p orca-provider generated_schema_uses_model_visible_tools_only
```

Expected: compile fails because `resolve`, `model_visible_tools`, and `glob` are not implemented.

- [ ] **Step 3: Extend the `Tool` trait**

Change `crates/orca-tools/src/registry.rs`:

```rust
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;

    fn name(&self) -> &str {
        self.spec().name.as_str()
    }

    fn description(&self) -> &str {
        &self.spec().description
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": self.description(),
                "parameters": self.spec().input_schema
            }
        })
    }

    fn action_kind(&self) -> ActionKind {
        self.spec().capabilities.action_kind()
    }

    fn is_read_only(&self, _input: &ToolRequest) -> bool {
        self.spec().capabilities.is_read_only()
    }

    fn is_concurrent_safe(&self, input: &ToolRequest) -> bool;
    fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult;
}
```

- [ ] **Step 4: Add registry alias resolution**

Add:

```rust
pub struct ResolvedTool<'a> {
    pub tool: &'a dyn Tool,
    pub spec: &'a ToolSpec,
    pub requested_name: ToolName,
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    by_name: HashMap<String, usize>,
    aliases: HashMap<String, usize>,
}

impl ToolRegistry {
    pub fn resolve(&self, name: &str) -> Option<ResolvedTool<'_>> {
        if let Some(idx) = self.by_name.get(name) {
            let tool = self.tools.get(*idx)?.as_ref();
            return Some(ResolvedTool {
                tool,
                spec: tool.spec(),
                requested_name: ToolName::from_str(name)?,
            });
        }
        let idx = self.aliases.get(name)?;
        let tool = self.tools.get(*idx)?.as_ref();
        Some(ResolvedTool {
            tool,
            spec: tool.spec(),
            requested_name: ToolName::from_str(name)?,
        })
    }

    pub fn model_visible_tools(&self) -> impl Iterator<Item = &dyn Tool> {
        self.tools
            .iter()
            .map(|tool| tool.as_ref())
            .filter(|tool| tool.spec().exposure.is_model_visible())
    }
}
```

In `register`, insert canonical name into `by_name` and each `spec.aliases` entry into `aliases`.

- [ ] **Step 5: Update built-in registration**

Change `BuiltinTool` to own a `ToolSpec`:

```rust
struct BuiltinTool {
    spec: ToolSpec,
    executor: BuiltinExecutor,
}
```

Add helper constructors:

```rust
fn builtin_spec(
    name: &str,
    description: &str,
    input_schema: Value,
    capabilities: CapabilitySet,
    exposure: ToolExposure,
    renderer: RendererHint,
    concurrent_safe: bool,
) -> ToolSpec {
    ToolSpec {
        name: ToolName::plain(name),
        aliases: Vec::new(),
        description: description.to_string(),
        input_schema,
        output_schema: None,
        capabilities,
        exposure,
        result_semantics: ResultSemantics::Standard,
        renderer,
        concurrent_safe,
    }
}
```

Register `glob` as direct and add `list_files` as an alias:

```rust
let mut glob = builtin_spec(
    "glob",
    "Find files and directories matching a glob pattern. Use this for project file discovery.",
    json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Glob pattern such as **/*.rs"
            },
            "path": {
                "type": "string",
                "description": "Directory to search in (default: '.')"
            }
        },
        "required": ["pattern"]
    }),
    CapabilitySet::new(vec![ToolCapability::FsList, ToolCapability::FsSearch]),
    ToolExposure::Direct,
    RendererHint::FileSearch,
    true,
);
glob.aliases.push(ToolName::plain("list_files"));
registry.register(BuiltinTool::new(glob, BuiltinExecutor::Glob));
```

Keep old `list_files` schema support in the executor by checking the requested name.

- [ ] **Step 6: Update registry dispatch and read batching**

In `ToolRegistry::execute`, use `resolve`:

```rust
let Some(resolved) = self.resolve(request.name.as_str()) else {
    return ToolResult::failed(request, format!("unknown tool: {}", request.name.as_str()), None);
};
resolved.tool.execute(request, ctx)
```

In `crates/orca-tools/src/lib.rs`, change `is_concurrent_safe_read` to resolve the spec and ignore caller-supplied `ActionKind`:

```rust
fn is_concurrent_safe_read(request: &ToolRequest) -> bool {
    let reg = registry::default_tool_registry();
    reg.resolve(request.name.as_str())
        .map(|resolved| {
            resolved.spec.capabilities.is_read_only()
                && resolved.spec.concurrent_safe
                && resolved.tool.is_concurrent_safe(request)
        })
        .unwrap_or(false)
}
```

- [ ] **Step 7: Filter model-visible schema**

In `crates/orca-provider/src/tool_schema.rs`, change:

```rust
registry.iter().map(|tool| tool.schema()).collect()
```

to:

```rust
registry.model_visible_tools().map(|tool| tool.schema()).collect()
```

- [ ] **Step 8: Run focused tests**

Run:

```bash
cargo test -p orca-core
cargo test -p orca-tools
cargo test -p orca-provider tool_schema
```

Expected: all pass.

- [ ] **Step 9: Commit**

```bash
git add crates/orca-core/src/tool_types.rs crates/orca-tools/src/registry.rs crates/orca-tools/src/lib.rs crates/orca-provider/src/tool_schema.rs
git commit -m "refactor: drive tool registry from specs"
```

---

### Task 3: Capability-Based Runtime Policy

**Files:**
- Modify: `crates/orca-runtime/src/controller.rs`
- Modify: `crates/orca-tui/src/bridge.rs`
- Modify: `crates/orca-tools/src/lib.rs`

**Interfaces:**
- Consumes: `ToolRegistry::resolve`, `CapabilitySet`, `ToolExposure`.
- Produces: read-only batching and approval behavior derived from canonical specs.

- [ ] **Step 1: Write failing runtime tests**

In `crates/orca-runtime/src/controller.rs`, update `readonly_batch_skips_approval_actions` to assert the new desired behavior:

```rust
#[test]
fn readonly_batch_uses_spec_not_request_action() {
    let config = config(SubagentConfig::default());
    let request = tool_request("a", tool_types::ToolName::ReadFile, ActionKind::Write);

    assert!(orca_tools::should_run_readonly_batch(
        config.tools.max_read_parallel,
        &request
    ));
}
```

Add a test ensuring shell still does not batch:

```rust
#[test]
fn readonly_batch_rejects_shell_by_capability() {
    let config = config(SubagentConfig::default());
    let request = tool_request("bash", tool_types::ToolName::Bash, ActionKind::Read);

    assert!(!orca_tools::should_run_readonly_batch(
        config.tools.max_read_parallel,
        &request
    ));
}
```

- [ ] **Step 2: Run test to verify failure**

Run:

```bash
cargo test -p orca-runtime readonly_batch_uses_spec_not_request_action readonly_batch_rejects_shell_by_capability
```

Expected: first test fails until batching ignores the request action.

- [ ] **Step 3: Add canonical policy helper**

In `crates/orca-tools/src/lib.rs`, expose:

```rust
pub fn tool_is_available_readonly_concurrent(request: &ToolRequest) -> bool {
    let reg = registry::default_tool_registry();
    reg.resolve(request.name.as_str())
        .map(|resolved| resolved.spec.capabilities.is_read_only() && resolved.spec.concurrent_safe)
        .unwrap_or(false)
}
```

Use this helper in `should_run_readonly_batch` and `collect_readonly_batch`.

- [ ] **Step 4: Replace direct `ToolName::is_read_only` usage**

Search:

```bash
rg -n "is_read_only\\(" crates
```

Replace remaining runtime policy reads with registry helpers. Keep `Tool::is_read_only` only as a trait method backed by spec metadata.

- [ ] **Step 5: Run runtime and TUI tests**

Run:

```bash
cargo test -p orca-tools
cargo test -p orca-runtime readonly_batch
cargo test -p orca-tui readonly
```

Expected: all pass. If `orca-tui readonly` has no matching tests, Cargo reports zero tests for that filter and exits successfully.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-tools/src/lib.rs crates/orca-runtime/src/controller.rs crates/orca-tui/src/bridge.rs
git commit -m "refactor: derive tool policy from capabilities"
```

---

### Task 4: Glob Executor

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/orca-tools/Cargo.toml`
- Create: `crates/orca-tools/src/glob.rs`
- Modify: `crates/orca-tools/src/lib.rs`
- Modify: `crates/orca-tools/src/registry.rs`
- Modify: `docs/harness-contract.md`

**Interfaces:**
- Consumes: `BuiltinExecutor::Glob`, `ToolSpec` alias handling.
- Produces: `glob::execute(request, cwd, max_bytes) -> ToolResult`.

- [ ] **Step 1: Add failing glob tests**

Create `crates/orca-tools/src/glob.rs` with tests first:

```rust
#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolRequest, ToolStatus};

    use super::*;

    #[test]
    fn glob_returns_sorted_relative_matches() {
        let cwd = temp_dir("glob-sorted");
        fs::create_dir_all(cwd.join("src/bin")).unwrap();
        fs::write(cwd.join("src/lib.rs"), "").unwrap();
        fs::write(cwd.join("src/bin/main.rs"), "").unwrap();
        fs::write(cwd.join("README.md"), "").unwrap();

        let request = ToolRequest {
            id: "glob-1".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: Some("**/*.rs".to_string()),
            raw_arguments: None,
        };

        let result = execute(&request, &cwd, 4096);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("src/bin/main.rs\nsrc/lib.rs"));
    }

    #[test]
    fn glob_missing_path_returns_no_matches() {
        let cwd = temp_dir("glob-missing");
        fs::create_dir_all(&cwd).unwrap();
        let request = ToolRequest {
            id: "glob-2".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: Some("**/*.rs".to_string()),
            raw_arguments: Some(r#"{"path":"missing"}"#.to_string()),
        };

        let result = execute(&request, &cwd, 4096);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("(no matches)"));
    }

    #[test]
    fn list_files_alias_keeps_empty_text() {
        let cwd = temp_dir("glob-list-files");
        fs::create_dir_all(&cwd).unwrap();
        let request = ToolRequest {
            id: "list-1".to_string(),
            name: ToolName::ListFiles,
            action: ActionKind::Read,
            target: Some(".orca/workflows".to_string()),
            raw_arguments: None,
        };

        let result = execute(&request, &cwd, 4096);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("(empty)"));
    }

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "orca-{prefix}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
```

- [ ] **Step 2: Run test to verify failure**

Run:

```bash
cargo test -p orca-tools glob_returns_sorted_relative_matches glob_missing_path_returns_no_matches list_files_alias_keeps_empty_text
```

Expected: compile fails because `execute` is not implemented.

- [ ] **Step 3: Add dependencies**

Add to root `Cargo.toml` workspace dependencies:

```toml
globset = "0.4"
ignore = "0.4"
```

Add to `crates/orca-tools/Cargo.toml`:

```toml
globset = { workspace = true }
ignore = { workspace = true }
```

- [ ] **Step 4: Implement glob executor**

Implement `crates/orca-tools/src/glob.rs`:

```rust
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind, truncate_output};
use serde_json::Value;

use crate::resolve_workspace_path;

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    if request.name.is_builtin("list_files") {
        return crate::list_files::execute(request, cwd, max_bytes);
    }

    let pattern = match request.target.as_deref().filter(|value| !value.is_empty()) {
        Some(pattern) => pattern,
        None => return ToolResult::failed(request, "glob pattern is required", None),
    };
    let search_path = request
        .raw_arguments
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|args| args["path"].as_str().map(String::from))
        .unwrap_or_else(|| ".".to_string());
    let root = match resolve_workspace_path(cwd, Some(&search_path)) {
        Ok(path) => path,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    if !root.exists() {
        return ToolResult::completed_kind(
            request,
            "(no matches)".to_string(),
            false,
            ToolResultKind::NoMatches,
        );
    }

    let matcher = match build_matcher(pattern) {
        Ok(matcher) => matcher,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let mut matches = Vec::new();
    for entry in WalkBuilder::new(&root).standard_filters(true).build() {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        let relative_to_root = path.strip_prefix(&root).unwrap_or(path);
        if matcher.is_match(relative_to_root) {
            matches.push(relative_path(cwd, path));
        }
    }
    matches.sort();

    if matches.is_empty() {
        return ToolResult::completed_kind(
            request,
            "(no matches)".to_string(),
            false,
            ToolResultKind::NoMatches,
        );
    }

    let (output, truncated) = truncate_output(matches.join("\n"), max_bytes);
    ToolResult::completed_kind(
        request,
        output,
        truncated,
        if truncated {
            ToolResultKind::Truncated
        } else {
            ToolResultKind::Success
        },
    )
}

fn build_matcher(pattern: &str) -> Result<globset::GlobSet, String> {
    let glob = Glob::new(pattern).map_err(|error| format!("invalid glob pattern: {error}"))?;
    let mut builder = GlobSetBuilder::new();
    builder.add(glob);
    builder
        .build()
        .map_err(|error| format!("invalid glob matcher: {error}"))
}

fn relative_path(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}
```

- [ ] **Step 5: Wire module and executor**

In `crates/orca-tools/src/lib.rs`, add:

```rust
pub mod glob;
```

In `crates/orca-tools/src/registry.rs`, add `BuiltinExecutor::Glob` and dispatch:

```rust
BuiltinExecutor::Glob => glob::execute(request, ctx.cwd, ctx.max_output_bytes),
```

- [ ] **Step 6: Update harness contract**

In `docs/harness-contract.md`, add a `glob` row:

```markdown
| `glob` | read | Finds files/directories by glob pattern, sorted relative paths, `(no matches)` for missing path or no matches |
```

Update `list_files` row:

```markdown
| `list_files` | read | Compatibility directory listing entry; returns `(empty)` for missing or empty directories |
```

- [ ] **Step 7: Run focused tests**

Run:

```bash
cargo test -p orca-tools glob
cargo test -p orca-provider generated_schema_uses_model_visible_tools_only
```

Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml crates/orca-tools/Cargo.toml crates/orca-tools/src/glob.rs crates/orca-tools/src/lib.rs crates/orca-tools/src/registry.rs docs/harness-contract.md
git commit -m "feat: add glob file discovery tool"
```

---

### Task 5: Typed Result Rendering

**Files:**
- Modify: `crates/orca-tools/src/list_files.rs`
- Modify: `crates/orca-tools/src/grep.rs`
- Modify: `crates/orca-tools/src/read_file.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/bridge.rs`

**Interfaces:**
- Consumes: `ToolResultKind`.
- Produces: completed empty/no-match results with neutral TUI rendering.

- [ ] **Step 1: Write failing tool-kind tests**

Update existing tests in `crates/orca-tools/src/list_files.rs` and `crates/orca-tools/src/grep.rs`:

```rust
assert_eq!(result.kind, ToolResultKind::Empty);
```

for `list_files` missing directory, and:

```rust
assert_eq!(result.kind, ToolResultKind::NoMatches);
```

for `grep` missing path.

- [ ] **Step 2: Run tests to verify failure**

Run:

```bash
cargo test -p orca-tools missing_directory_completes_with_empty_listing missing_search_path_completes_with_no_matches
```

Expected: tests fail until executors set result kind.

- [ ] **Step 3: Set result kinds in file tools**

In `list_files.rs`, return:

```rust
ToolResult::completed_kind(request, "(empty)".to_string(), false, ToolResultKind::Empty)
```

for missing directory and empty listings.

In `grep.rs`, return:

```rust
ToolResult::completed_kind(request, "(no matches)".to_string(), false, ToolResultKind::NoMatches)
```

for missing search path and `rg` exit code `1`.

In `read_file.rs`, use `ToolResultKind::Truncated` when truncation happens:

```rust
ToolResult::completed_kind(
    request,
    output,
    truncated,
    if truncated { ToolResultKind::Truncated } else { ToolResultKind::Success },
)
```

- [ ] **Step 4: Carry result kind through TUI events**

In `crates/orca-tui/src/types.rs`, add `kind: Option<String>` to the tool completion event/state structure that backs rendered tool messages.

In `crates/orca-tui/src/bridge.rs`, populate it:

```rust
kind: Some(tool_result_kind_label(result.kind).to_string()),
```

Add this explicit helper near the TUI event conversion helpers:

```rust
fn tool_result_kind_label(kind: ToolResultKind) -> &'static str {
    match kind {
        ToolResultKind::Success => "success",
        ToolResultKind::Empty => "empty",
        ToolResultKind::NoMatches => "no_matches",
        ToolResultKind::Truncated => "truncated",
        ToolResultKind::PermissionDenied => "permission_denied",
        ToolResultKind::InvalidInput => "invalid_input",
        ToolResultKind::RuntimeError => "runtime_error",
    }
}
```

- [ ] **Step 5: Render completed empty/no-match as neutral**

In `crates/orca-tui/src/ui.rs`, update the tool rendering branch so:

```rust
let neutral_completed = tool.status == "completed"
    && matches!(tool.kind.as_deref(), Some("empty" | "no_matches"));
```

Use the same completed marker as other successful tools, with dim styling for the output label. Keep failed and denied statuses red.

- [ ] **Step 6: Run TUI and tool tests**

Run:

```bash
cargo test -p orca-tools
cargo test -p orca-tui
```

Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add crates/orca-tools/src/list_files.rs crates/orca-tools/src/grep.rs crates/orca-tools/src/read_file.rs crates/orca-tui/src/types.rs crates/orca-tui/src/bridge.rs crates/orca-tui/src/ui.rs
git commit -m "feat: preserve tool result semantics"
```

---

### Task 6: Spec-Generated Prompt And Context Exposure

**Files:**
- Modify: `crates/orca-provider/src/system_prompt.rs`
- Modify: `crates/orca-provider/src/tool_schema.rs`
- Modify: `crates/orca-runtime/src/agent_common.rs`
- Modify: `docs/harness-contract.md`

**Interfaces:**
- Consumes: `ToolRegistry::model_visible_tools`, `ToolExposure`, context flags for goal/workflow availability.
- Produces: prompt text that reflects enabled direct tools and hides compatibility aliases.

- [ ] **Step 1: Write failing prompt tests**

Add tests to `crates/orca-provider/src/system_prompt.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_recommends_glob_and_hides_list_files() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(prompt.contains("### glob"));
        assert!(!prompt.contains("### list_files"));
        assert!(prompt.contains("prefer `read_file`, `glob`, and `grep`"));
    }

    #[test]
    fn prompt_keeps_bash_for_tests_and_builds() {
        let prompt = build_system_prompt(std::path::Path::new("/repo"));

        assert!(prompt.contains("### bash"));
        assert!(prompt.contains("tests, builds, project scripts"));
    }
}
```

- [ ] **Step 2: Run tests to verify failure**

Run:

```bash
cargo test -p orca-provider prompt_recommends_glob_and_hides_list_files prompt_keeps_bash_for_tests_and_builds
```

Expected: tests fail until prompt generation changes.

- [ ] **Step 3: Add prompt rendering helper**

In `crates/orca-provider/src/system_prompt.rs`, add:

```rust
fn render_tool_prompt_section() -> String {
    let registry = orca_tools::registry::default_tool_registry();
    let mut output = String::new();
    for tool in registry.model_visible_tools() {
        output.push_str(&format!(
            "\n### {}\n{}\nParameters: `{}`.\n",
            tool.name(),
            tool.description(),
            tool.spec().input_schema
        ));
    }
    output
}
```

Replace the hard-coded tool list under `## Available Tools` with:

```rust
{tools}
```

and pass `tools = render_tool_prompt_section()` to `format!`.

- [ ] **Step 4: Add explicit shell guidance**

Under the generated tools section, keep this static guidance:

```markdown
Use `bash` for tests, builds, project scripts, and complex shell-only tasks. For file inspection, prefer `read_file`, `glob`, and `grep`.
```

- [ ] **Step 5: Keep goal-mode context separate**

Do not advertise `update_goal` from the base prompt unless the existing goal-mode path adds it. Verify `crates/orca-runtime/src/agent_common.rs` still injects goal-specific instructions while goal mode is active.

- [ ] **Step 6: Run provider tests**

Run:

```bash
cargo test -p orca-provider
```

Expected: all pass.

- [ ] **Step 7: Update docs**

In `docs/harness-contract.md`, document:

```markdown
`glob` is the preferred file discovery tool. `list_files` remains accepted for compatibility but is not recommended in the system prompt.
```

- [ ] **Step 8: Run final verification**

Run:

```bash
cargo test -p orca-core
cargo test -p orca-tools
cargo test -p orca-provider
cargo test -p orca-runtime
cargo test -p orca-tui
```

Expected: all pass.

- [ ] **Step 9: Commit**

```bash
git add crates/orca-provider/src/system_prompt.rs crates/orca-provider/src/tool_schema.rs crates/orca-runtime/src/agent_common.rs docs/harness-contract.md
git commit -m "feat: generate tool prompt from specs"
```

---

## Self-Review

Spec coverage:

- Codex-inspired `ToolSpec`, executor, exposure, namespace, and capability model: Tasks 1 and 2.
- Compatibility aliases and `list_files` demotion: Tasks 2 and 4.
- `glob` preferred file discovery: Task 4.
- Typed empty/no-match semantics: Task 5.
- Capability-based approval and batching: Task 3.
- Prompt generated from specs: Task 6.
- Deferred tool discovery and shell `exec_command` migration: intentionally deferred by the spec's non-goals and Phase 6 note.

Placeholder scan:

- No open implementation placeholders remain.
- Open questions from the design are either deferred or converted into concrete choices for this plan.

Type consistency:

- `ToolName::plain`, `ToolName::namespaced`, `ToolName::from_str`, `CapabilitySet`, `ToolExposure`, `ToolSpec`, and `ToolResultKind` are introduced in Task 1 before use in later tasks.
- `ToolRegistry::resolve` and `ToolRegistry::model_visible_tools` are introduced in Task 2 before runtime and provider use.
- `glob::execute` is introduced in Task 4 before prompt and docs advertise it.
