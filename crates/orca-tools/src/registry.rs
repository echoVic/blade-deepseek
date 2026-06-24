use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use serde_json::{Value, json};

use orca_core::approval_types::ActionKind;
use orca_core::external_config::ExternalToolConfig;
use orca_core::mcp_types::McpTool;
use orca_core::tool_types::{
    CapabilitySet, MAX_TOOL_OUTPUT_BYTES, RendererHint, ResultSemantics, ToolCapability,
    ToolExposure, ToolName, ToolOutputTruncation, ToolRequest, ToolResult, ToolSpec,
};
use orca_mcp::McpRegistry;

use crate::{
    bash, edit, external, git, glob, grep, list_files, read_file, skills, update_goal, update_plan,
    web_search, write_file,
};

#[allow(dead_code)]
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

    fn timeout(&self) -> Duration {
        Duration::from_secs(60)
    }
}

pub struct ToolContext<'a> {
    pub cwd: &'a Path,
    pub output_truncation: ToolOutputTruncation,
    pub mcp_registry: Option<&'a McpRegistry>,
}

impl<'a> ToolContext<'a> {
    pub fn new(cwd: &'a Path) -> Self {
        Self {
            cwd,
            output_truncation: ToolOutputTruncation::bytes(MAX_TOOL_OUTPUT_BYTES),
            mcp_registry: None,
        }
    }

    pub fn with_output_truncation(mut self, output_truncation: ToolOutputTruncation) -> Self {
        self.output_truncation = output_truncation.normalized();
        self
    }

    pub fn max_output_bytes(&self) -> usize {
        match self.output_truncation {
            ToolOutputTruncation::Bytes { limit } => limit,
            ToolOutputTruncation::Tokens { limit } => limit.saturating_mul(4),
        }
    }

    pub fn with_mcp(mut self, mcp_registry: &'a McpRegistry) -> Self {
        self.mcp_registry = Some(mcp_registry);
        self
    }
}

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
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            by_name: HashMap::new(),
            aliases: HashMap::new(),
        }
    }

    pub fn register<T>(&mut self, tool: T)
    where
        T: Tool + 'static,
    {
        let name = tool.name().to_string();
        if self.by_name.contains_key(&name) || self.aliases.contains_key(&name) {
            return;
        }
        let idx = self.tools.len();
        self.by_name.insert(name, idx);
        for alias in &tool.spec().aliases {
            let alias = alias.as_str().to_string();
            if !self.by_name.contains_key(&alias) && !self.aliases.contains_key(&alias) {
                self.aliases.insert(alias, idx);
            }
        }
        self.tools.push(Box::new(tool));
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.by_name
            .get(name)
            .and_then(|idx| self.tools.get(*idx))
            .map(|tool| tool.as_ref())
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn Tool> {
        self.tools.iter().map(|tool| tool.as_ref())
    }

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

    pub fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult {
        let Some(resolved) = self.resolve(request.name.as_str()) else {
            return ToolResult::failed(
                request,
                format!("unknown tool: {}", request.name.as_str()),
                None,
            );
        };
        if let Err(error) = validate_arguments(request, &resolved.spec.input_schema) {
            return ToolResult::invalid_input(
                request,
                format!("tool arguments failed schema validation: {error}"),
            );
        }
        resolved.tool.execute(request, ctx)
    }
}

pub fn validate_tool_request(registry: &ToolRegistry, request: &ToolRequest) -> Result<(), String> {
    let Some(resolved) = registry.resolve(request.name.as_str()) else {
        return Err(format!("unknown tool: {}", request.name.as_str()));
    };
    validate_arguments(request, &resolved.spec.input_schema)
}

fn validate_arguments(request: &ToolRequest, schema: &Value) -> Result<(), String> {
    let raw = request.raw_arguments.as_deref().unwrap_or("{}");
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| format!("arguments are not valid JSON: {error}"))?;
    validate_value("$", &value, schema)
}

fn validate_value(path: &str, value: &Value, schema: &Value) -> Result<(), String> {
    if let Some(expected_type) = schema.get("type").and_then(Value::as_str) {
        match expected_type {
            "object" => validate_object(path, value, schema)?,
            "array" => validate_array(path, value, schema)?,
            "string" if !value.is_string() => {
                return Err(format!(
                    "{path}: expected string, got {}",
                    value_type(value)
                ));
            }
            "number" if !value.is_number() => {
                return Err(format!(
                    "{path}: expected number, got {}",
                    value_type(value)
                ));
            }
            "integer" if value.as_i64().is_none() && value.as_u64().is_none() => {
                return Err(format!(
                    "{path}: expected integer, got {}",
                    value_type(value)
                ));
            }
            "boolean" if !value.is_boolean() => {
                return Err(format!(
                    "{path}: expected boolean, got {}",
                    value_type(value)
                ));
            }
            _ => {}
        }
    }

    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.iter().any(|allowed| allowed == value)
    {
        let allowed = values
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!("{path}: expected one of [{allowed}], got {value}"));
    }

    Ok(())
}

fn validate_object(path: &str, value: &Value, schema: &Value) -> Result<(), String> {
    let Some(object) = value.as_object() else {
        return Err(format!(
            "{path}: expected object, got {}",
            value_type(value)
        ));
    };
    let properties = schema.get("properties").and_then(Value::as_object);

    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for field in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(field) {
                return Err(format!("{path}: missing required property \"{field}\""));
            }
        }
    }

    if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false)
        && let Some(properties) = properties
    {
        for key in object.keys() {
            if !properties.contains_key(key) {
                return Err(format!("{path}: unexpected property \"{key}\""));
            }
        }
    }

    if let Some(properties) = properties {
        for (key, child_schema) in properties {
            if let Some(child_value) = object.get(key) {
                validate_value(&format!("{path}.{key}"), child_value, child_schema)?;
            }
        }
    }

    Ok(())
}

fn validate_array(path: &str, value: &Value, schema: &Value) -> Result<(), String> {
    let Some(items) = value.as_array() else {
        return Err(format!("{path}: expected array, got {}", value_type(value)));
    };
    if let Some(item_schema) = schema.get("items") {
        for (idx, item) in items.iter().enumerate() {
            validate_value(&format!("{path}[{idx}]"), item, item_schema)?;
        }
    }
    Ok(())
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

static DEFAULT_REGISTRY: LazyLock<ToolRegistry> = LazyLock::new(|| {
    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry);
    registry
});

pub fn default_tool_registry() -> &'static ToolRegistry {
    &DEFAULT_REGISTRY
}

pub fn tool_registry_with_mcp_and_external(
    mcp_registry: Option<&McpRegistry>,
    external_tools: &[ExternalToolConfig],
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry);
    for tool in external_tools {
        registry.register(ExternalTool::new(tool.clone()));
    }
    if let Some(mcp_registry) = mcp_registry {
        for tool in mcp_registry.tools() {
            registry.register(McpProxyTool::new(tool.clone()));
        }
    }
    registry
}

fn register_builtin_tools(registry: &mut ToolRegistry) {
    registry.register(BuiltinTool::new(
        builtin_spec(
            "read_file",
            "Read the contents of a file at the given path relative to workspace root.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to workspace root"
                    }
                },
                "required": ["path"]
            }),
            CapabilitySet::read_only_fs(),
            ToolExposure::Direct,
            RendererHint::FileRead,
            true,
        ),
        BuiltinExecutor::ReadFile,
    ));
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
    registry.register(BuiltinTool::new(
        builtin_spec(
            "grep",
            "Search for a regex pattern in files using ripgrep. Returns matching lines with line numbers.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory or file to search in (default: '.')"
                    }
                },
                "required": ["pattern"]
            }),
            CapabilitySet::new(vec![ToolCapability::FsSearch]),
            ToolExposure::Direct,
            RendererHint::FileSearch,
            true,
        ),
        BuiltinExecutor::Grep,
    ));
    registry.register(BuiltinTool::new(
        builtin_spec(
            "bash",
            "Execute a shell command via sh -c. Use for running tests, builds, git operations, etc.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    }
                },
                "required": ["command"]
            }),
            CapabilitySet::shell_execute(),
            ToolExposure::Direct,
            RendererHint::Shell,
            false,
        ),
        BuiltinExecutor::Bash,
    ));
    registry.register(BuiltinTool::new(
        builtin_spec(
            "edit",
            "Edit a file by replacing exact text. The old_text must match exactly one location in the file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to workspace root"
                    },
                    "old_text": {
                        "type": "string",
                        "description": "Exact text to find (must match uniquely in the file)"
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Replacement text"
                    }
                },
                "required": ["path", "old_text", "new_text"]
            }),
            CapabilitySet::filesystem_write(),
            ToolExposure::Direct,
            RendererHint::Write,
            false,
        ),
        BuiltinExecutor::Edit,
    ));
    registry.register(BuiltinTool::new(
        builtin_spec(
            "write_file",
            "Create or overwrite a file with the given content. Use for creating new files or completely replacing file contents.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to workspace root"
                    },
                    "content": {
                        "type": "string",
                        "description": "The full content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
            CapabilitySet::filesystem_write(),
            ToolExposure::Direct,
            RendererHint::Write,
            false,
        ),
        BuiltinExecutor::WriteFile,
    ));
    registry.register(BuiltinTool::new(
        builtin_spec(
            "git_status",
            "Show the git working tree status in short format.",
            json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            CapabilitySet::new(vec![ToolCapability::GitInspect]),
            ToolExposure::Direct,
            RendererHint::State,
            true,
        ),
        BuiltinExecutor::GitStatus,
    ));
    registry.register(BuiltinTool::new(
        builtin_spec(
            "web_search",
            "Search the web for current information using Brave Search. Returns top results with title, summary, and URL.",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query"
                    },
                    "count": {
                        "type": "integer",
                        "description": "Number of results to return, 1-10 (default: 5)"
                    },
                    "freshness": {
                        "type": "string",
                        "description": "Optional recency filter. Use pd for last 24 hours, pw for last 7 days, pm for last 31 days, py for last year, or YYYY-MM-DDtoYYYY-MM-DD."
                    }
                },
                "required": ["query"]
            }),
            CapabilitySet::network_search(),
            ToolExposure::Direct,
            RendererHint::Network,
            false,
        ),
        BuiltinExecutor::WebSearch,
    ));
    registry.register(BuiltinTool::new(
        builtin_spec(
            "subagent",
            "Launch a synchronous child agent for a complex, multi-step subtask. The child runs independently and returns a concise result summary.",
            json!({
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "Short 3-8 word label for the delegated task"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Full standalone instructions for the child agent"
                    },
                    "subagent_type": {
                        "type": "string",
                        "enum": ["general", "code_reviewer", "test_writer", "debugger", "documenter"],
                        "description": "Optional specialized agent type that restricts tools and provides focused expertise"
                    },
                    "model": {
                        "type": "string",
                        "enum": ["auto", "deepseek-v4-flash", "deepseek-v4-pro"],
                        "description": "Optional model override for this child agent. auto uses Orca's router, flash is faster, pro is stronger for deep reasoning."
                    }
                },
                "required": ["description", "prompt"]
            }),
            CapabilitySet::agent_delegate(),
            ToolExposure::Direct,
            RendererHint::Agent,
            false,
        ),
        BuiltinExecutor::Subagent,
    ));
    registry.register(BuiltinTool::new(
        builtin_spec(
            "Workflow",
            "Launch a dynamic workflow: a JavaScript script that orchestrates many subagents in the background. The tool returns task metadata immediately; the final report is delivered later as a task notification.",
            json!({
                "type": "object",
                "properties": {
                    "script": {
                        "type": "string",
                        "description": "Self-contained workflow script. Use export const meta = { name, description, phases: [{ name, tasks }] }, or export const meta = { name, description } plus export const phases = [{ name, tasks }]. String phase names are for hand-written agent()/phase() scripts."
                    },
                    "name": {
                        "type": "string",
                        "description": "Name of a predefined workflow from .orca/workflows/ or the user workflow directory."
                    },
                    "description": {
                        "type": "string",
                        "description": "Compatibility field ignored by the runtime; use meta.description in the script."
                    },
                    "title": {
                        "type": "string",
                        "description": "Compatibility field ignored by the runtime; use meta.name in the script."
                    },
                    "args": {
                        "type": "object",
                        "description": "Structured input exposed to the workflow script as the global args value."
                    },
                    "scriptPath": {
                        "type": "string",
                        "description": "Path to a workflow script file. Takes precedence over script and name."
                    },
                    "resumeFromRunId": {
                        "type": "string",
                        "description": "Run id of a prior same-session workflow invocation to resume from."
                    }
                },
                "required": []
            }),
            CapabilitySet::new(vec![ToolCapability::WorkflowRun]),
            ToolExposure::Direct,
            RendererHint::Agent,
            false,
        ),
        BuiltinExecutor::Workflow,
    ));
    registry.register(BuiltinTool::new(
        builtin_spec(
            "update_plan",
            "Update the current task plan. Use for complex multi-step tasks or when the user asks for a todo/task list. At most one step may be in_progress. Maximum 50 items, each step max 200 chars.",
            json!({
                "type": "object",
                "properties": {
                    "explanation": {
                        "type": "string",
                        "description": "Optional short explanation for this plan update"
                    },
                    "plan": {
                        "type": "array",
                        "description": "The complete current list of task steps",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": {
                                    "type": "string",
                                    "description": "Task step text"
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Step status"
                                }
                            },
                            "required": ["step", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["plan"],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::PlanUpdate]),
            ToolExposure::Direct,
            RendererHint::State,
            false,
        ),
        BuiltinExecutor::UpdatePlan,
    ));
    registry.register(BuiltinTool::new(
        builtin_spec(
            "get_goal",
            "Read the active persistent goal for the current goal-mode session, including objective, status, usage, and budget.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::GoalUpdate]),
            ToolExposure::Direct,
            RendererHint::State,
            false,
        ),
        BuiltinExecutor::GetGoal,
    ));
    registry.register(BuiltinTool::new(
        builtin_spec(
            "create_goal",
            "Create a new active persistent goal for the current goal-mode session. This cannot replace an unfinished goal; complete or block the existing goal first.",
            json!({
                "type": "object",
                "properties": {
                    "objective": {
                        "type": "string",
                        "description": "User-facing goal objective to pursue"
                    },
                    "token_budget": {
                        "type": "integer",
                        "description": "Optional positive token budget for this goal"
                    }
                },
                "required": ["objective"],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::GoalUpdate]),
            ToolExposure::Direct,
            RendererHint::State,
            false,
        ),
        BuiltinExecutor::CreateGoal,
    ));
    registry.register(BuiltinTool::new(
        builtin_spec(
            "update_goal",
            "Update the active persistent goal status from goal mode. The model may only set status to complete when the goal is fully achieved or blocked after the strict blocked audit is satisfied; pause, resume, clear, budget, and objective edits are controlled by the user or system.",
            json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["blocked", "complete"],
                        "description": "Terminal model-controlled goal status"
                    },
                    "reason": {
                        "type": "string",
                        "description": "Optional short reason for the status update"
                    }
                },
                "required": ["status"],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::GoalUpdate]),
            ToolExposure::Direct,
            RendererHint::State,
            false,
        ),
        BuiltinExecutor::UpdateGoal,
    ));
    registry.register(BuiltinTool::new(
        builtin_spec(
            "list_skills",
            "List available user and project skills. Use before read_skill when the user asks for a skill or reusable procedure.",
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::SkillRead]),
            ToolExposure::Direct,
            RendererHint::State,
            true,
        ),
        BuiltinExecutor::ListSkills,
    ));
    registry.register(BuiltinTool::new(
        builtin_spec(
            "read_skill",
            "Read a skill's Markdown instructions by id after list_skills shows it is available.",
            json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Skill id, usually the skill directory name"
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::SkillRead]),
            ToolExposure::Direct,
            RendererHint::State,
            true,
        ),
        BuiltinExecutor::ReadSkill,
    ));
    registry.register(BuiltinTool::new(
        builtin_spec(
            "request_user_input",
            "Ask the user a structured clarification question. Use only when progress requires user input; headless runs return a deterministic failure instead of blocking.",
            json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "A concise question to show the user"
                    },
                    "choices": {
                        "type": "array",
                        "description": "Optional mutually exclusive answer choices",
                        "items": {
                            "type": "string"
                        }
                    }
                },
                "required": ["question"],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::UserInputRequest]),
            ToolExposure::Direct,
            RendererHint::State,
            false,
        ),
        BuiltinExecutor::RequestUserInput,
    ));
}

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

fn capability_set_for_action_kind(action_kind: ActionKind) -> CapabilitySet {
    match action_kind {
        ActionKind::Read => CapabilitySet::read_only_fs(),
        ActionKind::Write => CapabilitySet::filesystem_write(),
        ActionKind::Network => CapabilitySet::network_search(),
        ActionKind::Agent => CapabilitySet::agent_delegate(),
        ActionKind::Shell => CapabilitySet::shell_execute(),
    }
}

fn renderer_for_action_kind(action_kind: ActionKind) -> RendererHint {
    match action_kind {
        ActionKind::Read => RendererHint::FileRead,
        ActionKind::Write => RendererHint::Write,
        ActionKind::Network => RendererHint::Network,
        ActionKind::Agent => RendererHint::Agent,
        ActionKind::Shell => RendererHint::Shell,
    }
}

struct BuiltinTool {
    spec: ToolSpec,
    executor: BuiltinExecutor,
}

impl BuiltinTool {
    fn new(spec: ToolSpec, executor: BuiltinExecutor) -> Self {
        Self { spec, executor }
    }
}

impl Tool for BuiltinTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn action_kind(&self) -> ActionKind {
        self.spec.capabilities.action_kind()
    }

    fn is_read_only(&self, _input: &ToolRequest) -> bool {
        self.spec.capabilities.is_read_only()
    }

    fn is_concurrent_safe(&self, input: &ToolRequest) -> bool {
        self.is_read_only(input) && self.spec.concurrent_safe
    }

    fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult {
        match self.executor {
            BuiltinExecutor::ReadFile => {
                read_file::execute(request, ctx.cwd, ctx.max_output_bytes())
            }
            BuiltinExecutor::Glob if request.name.as_str() == "list_files" => {
                list_files::execute(request, ctx.cwd, ctx.max_output_bytes())
            }
            BuiltinExecutor::Glob => glob::execute(request, ctx.cwd, ctx.max_output_bytes()),
            BuiltinExecutor::Grep => grep::execute(request, ctx.cwd, ctx.max_output_bytes()),
            BuiltinExecutor::Bash => {
                bash::execute_with_policy(request, ctx.cwd, ctx.output_truncation)
            }
            BuiltinExecutor::Edit => edit::execute(request, ctx.cwd),
            BuiltinExecutor::WriteFile => write_file::execute(request, ctx.cwd),
            BuiltinExecutor::GitStatus => git::status(request, ctx.cwd, ctx.max_output_bytes()),
            BuiltinExecutor::WebSearch => web_search::execute(request, ctx.max_output_bytes()),
            BuiltinExecutor::Subagent => ToolResult::failed(
                request,
                "subagent tool must be executed by the runtime",
                None,
            ),
            BuiltinExecutor::Workflow => ToolResult::failed(
                request,
                "Workflow must be executed by the runtime controller",
                None,
            ),
            BuiltinExecutor::GetGoal => update_goal::execute_get(request),
            BuiltinExecutor::CreateGoal => update_goal::execute_create(request),
            BuiltinExecutor::UpdateGoal => update_goal::execute_update(request),
            BuiltinExecutor::UpdatePlan => update_plan::execute(request),
            BuiltinExecutor::ListSkills => skills::execute_list(request, ctx.cwd),
            BuiltinExecutor::ReadSkill => skills::execute_read(request, ctx.cwd),
            BuiltinExecutor::RequestUserInput => ToolResult::failed(
                request,
                "request_user_input requires an interactive TUI session",
                None,
            ),
        }
    }
}

#[derive(Clone, Copy)]
enum BuiltinExecutor {
    ReadFile,
    Glob,
    Grep,
    Bash,
    Edit,
    WriteFile,
    GitStatus,
    WebSearch,
    Subagent,
    Workflow,
    GetGoal,
    CreateGoal,
    UpdateGoal,
    UpdatePlan,
    ListSkills,
    ReadSkill,
    RequestUserInput,
}

struct McpProxyTool {
    tool: McpTool,
    spec: ToolSpec,
}

impl McpProxyTool {
    fn new(tool: McpTool) -> Self {
        let spec = ToolSpec {
            name: ToolName::from_str(&tool.schema_name)
                .unwrap_or_else(|| ToolName::Mcp(tool.schema_name.clone())),
            aliases: Vec::new(),
            description: tool
                .description
                .clone()
                .unwrap_or_else(|| format!("MCP tool {} from {}", tool.name, tool.server)),
            input_schema: tool.input_schema.clone(),
            output_schema: None,
            capabilities: CapabilitySet::filesystem_write(),
            exposure: ToolExposure::Direct,
            result_semantics: ResultSemantics::Standard,
            renderer: RendererHint::Write,
            concurrent_safe: false,
        };
        Self { tool, spec }
    }
}

impl Tool for McpProxyTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrent_safe(&self, _input: &ToolRequest) -> bool {
        false
    }

    fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult {
        let Some(registry) = ctx.mcp_registry else {
            return ToolResult::failed(request, "MCP registry is not initialized", None);
        };
        let Some(tool_ref) = registry.resolve_tool(&self.tool.schema_name) else {
            return ToolResult::failed(
                request,
                format!("unknown MCP tool: {}", self.tool.schema_name),
                None,
            );
        };
        let arguments = match request
            .raw_arguments
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
        {
            Ok(Some(value)) => value,
            Ok(None) => Value::Object(Default::default()),
            Err(error) => {
                return ToolResult::failed(
                    request,
                    format!("invalid MCP arguments JSON: {error}"),
                    None,
                );
            }
        };

        match registry.call_tool(&tool_ref, arguments) {
            Ok(result) if result.is_error => ToolResult::failed(request, result.output, None),
            Ok(result) => ToolResult::completed(request, result.output, false),
            Err(error) => ToolResult::failed(request, error, None),
        }
    }
}

struct ExternalTool {
    tool: ExternalToolConfig,
    spec: ToolSpec,
}

impl ExternalTool {
    fn new(tool: ExternalToolConfig) -> Self {
        let spec = ToolSpec {
            name: ToolName::plain(&tool.name),
            aliases: Vec::new(),
            description: tool.description.clone(),
            input_schema: tool.parameters_schema(),
            output_schema: None,
            capabilities: capability_set_for_action_kind(tool.action_kind),
            exposure: ToolExposure::Direct,
            result_semantics: ResultSemantics::Standard,
            renderer: renderer_for_action_kind(tool.action_kind),
            concurrent_safe: matches!(tool.action_kind, ActionKind::Read),
        };
        Self { tool, spec }
    }
}

impl Tool for ExternalTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrent_safe(&self, input: &ToolRequest) -> bool {
        self.is_read_only(input) && self.spec.concurrent_safe
    }

    fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult {
        external::execute_external_tool_with_policy(
            &self.tool,
            request,
            ctx.cwd,
            ctx.output_truncation,
        )
    }
}

pub fn tool_name_from_schema_name(name: &str) -> Option<ToolName> {
    ToolName::from_str(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(name: ToolName, raw_arguments: &str) -> ToolRequest {
        ToolRequest {
            id: "call-1".to_string(),
            name,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(raw_arguments.to_string()),
        }
    }

    #[test]
    fn registry_rejects_arguments_that_do_not_match_tool_schema() {
        let registry = default_tool_registry();
        let result = registry.execute(
            &request(
                ToolName::UpdatePlan,
                r#"{"plan":[{"completed":"Inspect references"}]}"#,
            ),
            &ToolContext::new(Path::new(".")),
        );

        assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("tool arguments failed schema validation"),
            "error={:?}",
            result.error
        );
    }
}
