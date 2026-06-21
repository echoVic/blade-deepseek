use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::transport::{self, McpTransport};
use orca_core::mcp_types::{
    CallToolResult, McpContent, McpServerConfig, McpTool, McpToolRef, ToolsListResult,
};

#[derive(Clone, Default)]
pub struct McpRegistry {
    inner: Arc<McpRegistryInner>,
}

#[derive(Default)]
struct McpRegistryInner {
    clients: HashMap<String, Arc<McpClient>>,
    tools: Vec<McpTool>,
    lookup: HashMap<String, McpToolRef>,
    errors: Vec<String>,
}

struct McpClient {
    transport: Box<dyn McpTransport>,
}

pub fn initialize_registry(configs: &[McpServerConfig]) -> McpRegistry {
    let mut clients = HashMap::new();
    let mut tools = Vec::new();
    let mut lookup = HashMap::new();
    let mut errors = Vec::new();

    for config in configs.iter().filter(|config| !config.disabled) {
        let server_name = sanitize_name(&config.name);
        if server_name.is_empty() {
            errors.push("skipping MCP server with empty name".to_string());
            continue;
        }

        match connect_server(config, &server_name) {
            Ok((client, server_tools)) => {
                for tool in server_tools {
                    if lookup.contains_key(&tool.schema_name) {
                        errors.push(format!(
                            "MCP tool name conflict: '{}' already registered, skipping from '{}'",
                            tool.schema_name, server_name
                        ));
                        continue;
                    }
                    lookup.insert(
                        tool.schema_name.clone(),
                        McpToolRef {
                            server: tool.server.clone(),
                            tool: tool.name.clone(),
                            schema_name: tool.schema_name.clone(),
                        },
                    );
                    tools.push(tool);
                }
                clients.insert(server_name, Arc::new(client));
            }
            Err(error) => errors.push(error),
        }
    }

    McpRegistry {
        inner: Arc::new(McpRegistryInner {
            clients,
            tools,
            lookup,
            errors,
        }),
    }
}

fn connect_server(
    config: &McpServerConfig,
    server_name: &str,
) -> Result<(McpClient, Vec<McpTool>), String> {
    let transport = transport::connect(config)?;
    transport.initialize()?;
    let result = transport.list_tools()?;
    let list: ToolsListResult = serde_json::from_value(result)
        .map_err(|error| format!("invalid tools/list result for '{server_name}': {error}"))?;

    let tools = list
        .tools
        .into_iter()
        .map(|tool| {
            let tool_name = sanitize_name(&tool.name);
            McpTool {
                server: server_name.to_string(),
                name: tool.name,
                schema_name: format!("mcp__{server_name}__{tool_name}"),
                description: tool.description,
                input_schema: normalize_schema(tool.input_schema),
            }
        })
        .collect();

    Ok((McpClient { transport }, tools))
}

impl McpRegistry {
    #[cfg(test)]
    pub fn from_tools_for_test(tools: Vec<McpTool>) -> Self {
        let lookup = tools
            .iter()
            .map(|tool| {
                (
                    tool.schema_name.clone(),
                    McpToolRef {
                        server: tool.server.clone(),
                        tool: tool.name.clone(),
                        schema_name: tool.schema_name.clone(),
                    },
                )
            })
            .collect();
        Self {
            inner: Arc::new(McpRegistryInner {
                clients: HashMap::new(),
                tools,
                lookup,
                errors: Vec::new(),
            }),
        }
    }

    pub fn tools(&self) -> &[McpTool] {
        &self.inner.tools
    }

    pub fn errors(&self) -> &[String] {
        &self.inner.errors
    }

    pub fn resolve_tool(&self, schema_name: &str) -> Option<McpToolRef> {
        self.inner.lookup.get(schema_name).cloned()
    }

    pub fn call_tool(
        &self,
        tool_ref: &McpToolRef,
        arguments: Value,
    ) -> Result<McpCallOutput, String> {
        let client = self
            .inner
            .clients
            .get(&tool_ref.server)
            .ok_or_else(|| format!("MCP server '{}' is not connected", tool_ref.server))?;
        let result = client.transport.call_tool(&tool_ref.tool, arguments)?;
        let result: CallToolResult = serde_json::from_value(result)
            .map_err(|error| format!("invalid MCP tool result: {error}"))?;
        let output = result
            .content
            .into_iter()
            .filter_map(|content| match content {
                McpContent::Text { text } => Some(text),
                McpContent::Other => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(McpCallOutput {
            output: if output.is_empty() {
                "(MCP tool returned no text content)".to_string()
            } else {
                output
            },
            is_error: result.is_error,
        })
    }
}

pub struct McpCallOutput {
    pub output: String,
    pub is_error: bool,
}

fn normalize_schema(schema: Value) -> Value {
    if schema.get("type").is_some() {
        schema
    } else {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }
}

fn sanitize_name(name: &str) -> String {
    let mut sanitized = String::new();
    let mut last_was_underscore = false;
    for ch in name.chars() {
        let next = if ch.is_ascii_alphanumeric() { ch } else { '_' };
        if next == '_' {
            if !last_was_underscore {
                sanitized.push(next);
            }
            last_was_underscore = true;
        } else {
            sanitized.push(next.to_ascii_lowercase());
            last_was_underscore = false;
        }
    }
    sanitized.trim_matches('_').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_mcp_schema_names() {
        assert_eq!(sanitize_name("GitHub Files"), "github_files");
        assert_eq!(sanitize_name("search.repos"), "search_repos");
    }

    #[test]
    fn normalizes_non_object_schema() {
        let schema = normalize_schema(Value::Null);
        assert_eq!(schema["type"], "object");
    }
}
