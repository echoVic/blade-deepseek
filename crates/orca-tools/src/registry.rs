use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use serde_json::{Value, json};

use orca_core::approval_types::ActionKind;
use orca_core::external_config::ExternalToolConfig;
use orca_core::mcp_types::McpTool;
use orca_core::tool_types::{MAX_TOOL_OUTPUT_BYTES, ToolName, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;

use crate::{bash, edit, external, git, grep, list_files, read_file, update_plan, web_search, write_file};

#[allow(dead_code)]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    fn action_kind(&self) -> ActionKind;
    fn is_read_only(&self, input: &ToolRequest) -> bool;
    fn is_concurrent_safe(&self, input: &ToolRequest) -> bool;
    fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult;

    fn timeout(&self) -> Duration {
        Duration::from_secs(60)
    }
}

pub struct ToolContext<'a> {
    pub cwd: &'a Path,
    pub max_output_bytes: usize,
    pub mcp_registry: Option<&'a McpRegistry>,
}

impl<'a> ToolContext<'a> {
    pub fn new(cwd: &'a Path) -> Self {
        Self {
            cwd,
            max_output_bytes: MAX_TOOL_OUTPUT_BYTES,
            mcp_registry: None,
        }
    }

    pub fn with_mcp(mut self, mcp_registry: &'a McpRegistry) -> Self {
        self.mcp_registry = Some(mcp_registry);
        self
    }
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    by_name: HashMap<String, usize>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            by_name: HashMap::new(),
        }
    }

    pub fn register<T>(&mut self, tool: T)
    where
        T: Tool + 'static,
    {
        let name = tool.name().to_string();
        if self.by_name.contains_key(&name) {
            return;
        }
        self.by_name.insert(name, self.tools.len());
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

    pub fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult {
        let name = request.name.as_str();
        let Some(tool) = self.get(name) else {
            return ToolResult::failed(request, format!("unknown tool: {name}"), None);
        };
        tool.execute(request, ctx)
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
        "read_file",
        "Read the contents of a file at the given path relative to workspace root.",
        ActionKind::Read,
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
        BuiltinExecutor::ReadFile,
    ));
    registry.register(BuiltinTool::new(
        "list_files",
        "List files and directories in the given path.",
        ActionKind::Read,
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path relative to workspace root (default: '.')"
                }
            },
            "required": []
        }),
        BuiltinExecutor::ListFiles,
    ));
    registry.register(BuiltinTool::new(
        "grep",
        "Search for a regex pattern in files using ripgrep. Returns matching lines with line numbers.",
        ActionKind::Read,
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
        BuiltinExecutor::Grep,
    ));
    registry.register(BuiltinTool::new(
        "bash",
        "Execute a shell command via sh -c. Use for running tests, builds, git operations, etc.",
        ActionKind::Shell,
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
        BuiltinExecutor::Bash,
    ));
    registry.register(BuiltinTool::new(
        "edit",
        "Edit a file by replacing exact text. The old_text must match exactly one location in the file.",
        ActionKind::Write,
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
        BuiltinExecutor::Edit,
    ));
    registry.register(BuiltinTool::new(
        "write_file",
        "Create or overwrite a file with the given content. Use for creating new files or completely replacing file contents.",
        ActionKind::Write,
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
        BuiltinExecutor::WriteFile,
    ));
    registry.register(BuiltinTool::new(
        "git_status",
        "Show the git working tree status in short format.",
        ActionKind::Read,
        json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
        BuiltinExecutor::GitStatus,
    ));
    registry.register(BuiltinTool::new(
        "web_search",
        "Search the web for current information using Brave Search. Returns top results with title, summary, and URL.",
        ActionKind::Network,
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
                }
            },
            "required": ["query"]
        }),
        BuiltinExecutor::WebSearch,
    ));
    registry.register(BuiltinTool::new(
        "subagent",
        "Launch a synchronous child agent for a complex, multi-step subtask. The child runs independently and returns a concise result summary.",
        ActionKind::Agent,
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
        BuiltinExecutor::Subagent,
    ));
    registry.register(BuiltinTool::new(
        "update_plan",
        "Update the current task plan. Use for complex multi-step tasks or when the user asks for a todo/task list. At most one step may be in_progress. Maximum 50 items, each step max 200 chars.",
        ActionKind::Read,
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
        BuiltinExecutor::UpdatePlan,
    ));
}

struct BuiltinTool {
    name: &'static str,
    description: &'static str,
    action_kind: ActionKind,
    parameters: Value,
    executor: BuiltinExecutor,
}

impl BuiltinTool {
    fn new(
        name: &'static str,
        description: &'static str,
        action_kind: ActionKind,
        parameters: Value,
        executor: BuiltinExecutor,
    ) -> Self {
        Self {
            name,
            description,
            action_kind,
            parameters,
            executor,
        }
    }
}

impl Tool for BuiltinTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters
            }
        })
    }

    fn action_kind(&self) -> ActionKind {
        self.action_kind
    }

    fn is_read_only(&self, _input: &ToolRequest) -> bool {
        matches!(self.action_kind, ActionKind::Read)
    }

    fn is_concurrent_safe(&self, input: &ToolRequest) -> bool {
        self.is_read_only(input) && !matches!(self.executor, BuiltinExecutor::UpdatePlan)
    }

    fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult {
        match self.executor {
            BuiltinExecutor::ReadFile => read_file::execute(request, ctx.cwd, ctx.max_output_bytes),
            BuiltinExecutor::ListFiles => {
                list_files::execute(request, ctx.cwd, ctx.max_output_bytes)
            }
            BuiltinExecutor::Grep => grep::execute(request, ctx.cwd, ctx.max_output_bytes),
            BuiltinExecutor::Bash => bash::execute(request, ctx.cwd, ctx.max_output_bytes),
            BuiltinExecutor::Edit => edit::execute(request, ctx.cwd),
            BuiltinExecutor::WriteFile => write_file::execute(request, ctx.cwd),
            BuiltinExecutor::GitStatus => git::status(request, ctx.cwd, ctx.max_output_bytes),
            BuiltinExecutor::WebSearch => web_search::execute(request, ctx.max_output_bytes),
            BuiltinExecutor::Subagent => ToolResult::failed(
                request,
                "subagent tool must be executed by the runtime",
                None,
            ),
            BuiltinExecutor::UpdatePlan => update_plan::execute(request),
        }
    }
}

#[derive(Clone, Copy)]
enum BuiltinExecutor {
    ReadFile,
    ListFiles,
    Grep,
    Bash,
    Edit,
    WriteFile,
    GitStatus,
    WebSearch,
    Subagent,
    UpdatePlan,
}

struct McpProxyTool {
    tool: McpTool,
}

impl McpProxyTool {
    fn new(tool: McpTool) -> Self {
        Self { tool }
    }
}

impl Tool for McpProxyTool {
    fn name(&self) -> &str {
        &self.tool.schema_name
    }

    fn description(&self) -> &str {
        self.tool.description.as_deref().unwrap_or("MCP tool")
    }

    fn schema(&self) -> Value {
        self.tool.to_deepseek_schema()
    }

    fn action_kind(&self) -> ActionKind {
        ActionKind::Write
    }

    fn is_read_only(&self, _input: &ToolRequest) -> bool {
        false
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
}

impl ExternalTool {
    fn new(tool: ExternalToolConfig) -> Self {
        Self { tool }
    }
}

impl Tool for ExternalTool {
    fn name(&self) -> &str {
        &self.tool.name
    }

    fn description(&self) -> &str {
        &self.tool.description
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.tool.name,
                "description": self.tool.description,
                "parameters": self.tool.parameters_schema()
            }
        })
    }

    fn action_kind(&self) -> ActionKind {
        self.tool.action_kind
    }

    fn is_read_only(&self, _input: &ToolRequest) -> bool {
        matches!(self.tool.action_kind, ActionKind::Read)
    }

    fn is_concurrent_safe(&self, input: &ToolRequest) -> bool {
        self.is_read_only(input)
    }

    fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult {
        external::execute_external_tool(&self.tool, request, ctx.cwd, ctx.max_output_bytes)
    }
}

pub fn tool_name_from_schema_name(name: &str) -> Option<ToolName> {
    ToolName::from_str(name)
}
