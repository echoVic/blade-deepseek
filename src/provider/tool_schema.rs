use serde_json::{Value, json};

use crate::mcp::McpRegistry;
use crate::runtime::subagent_types::SubagentType;

pub fn deepseek_tools_schema() -> Vec<Value> {
    builtin_tools_schema()
}

pub fn deepseek_tools_schema_with_mcp(mcp_registry: Option<&McpRegistry>) -> Vec<Value> {
    let mut schema = builtin_tools_schema();
    if let Some(registry) = mcp_registry {
        schema.extend(
            registry
                .tools()
                .iter()
                .map(|tool| tool.to_deepseek_schema()),
        );
    }
    schema
}

fn builtin_tools_schema() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read the contents of a file at the given path relative to workspace root.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path relative to workspace root"
                        }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_files",
                "description": "List files and directories in the given path.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path relative to workspace root (default: '.')"
                        }
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search for a regex pattern in files using ripgrep. Returns matching lines with line numbers.",
                "parameters": {
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
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Execute a shell command via sh -c. Use for running tests, builds, git operations, etc.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute"
                        }
                    },
                    "required": ["command"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "edit",
                "description": "Edit a file by replacing exact text. The old_text must match exactly one location in the file.",
                "parameters": {
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
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create or overwrite a file with the given content. Use for creating new files or completely replacing file contents.",
                "parameters": {
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
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_status",
                "description": "Show the git working tree status in short format.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web for current information using Brave Search. Returns top results with title, summary, and URL.",
                "parameters": {
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
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "subagent",
                "description": "Launch a synchronous child agent for a complex, multi-step subtask. The child runs independently and returns a concise result summary.",
                "parameters": {
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
                }
            }
        }),
    ]
}

pub fn deepseek_tools_schema_for_type_with_mcp(
    subagent_type: &SubagentType,
    mcp_registry: Option<&McpRegistry>,
) -> Vec<Value> {
    let allowed = subagent_type.allowed_tools();
    let mut tools: Vec<Value> = deepseek_tools_schema()
        .into_iter()
        .filter(|tool| {
            tool["function"]["name"]
                .as_str()
                .map(|name| name != "subagent" && allowed.contains(&name))
                .unwrap_or(false)
        })
        .collect();

    if let Some(registry) = mcp_registry {
        tools.extend(
            registry
                .tools()
                .iter()
                .map(|tool| tool.to_deepseek_schema()),
        );
    }

    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::client::McpRegistry;
    use crate::mcp::types::McpTool;

    #[test]
    fn merges_mcp_tools_into_schema() {
        let registry = McpRegistry::from_tools_for_test(vec![McpTool {
            server: "demo".to_string(),
            name: "search".to_string(),
            schema_name: "mcp__demo__search".to_string(),
            description: Some("search docs".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }),
        }]);

        let schema = deepseek_tools_schema_with_mcp(Some(&registry));
        assert!(
            schema
                .iter()
                .any(|tool| { tool["function"]["name"] == "mcp__demo__search" })
        );
    }
}
