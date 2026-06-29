use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportKind {
    Stdio,
    Sse,
}

impl Default for McpTransportKind {
    fn default() -> Self {
        Self::Stdio
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default)]
    pub transport: McpTransportKind,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub startup_timeout_ms: Option<u64>,
    #[serde(default)]
    pub tool_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpResource {
    pub server: String,
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    pub schema_name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

impl McpTool {
    pub fn to_deepseek_schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.schema_name,
                "description": self.description.clone().unwrap_or_else(|| {
                    format!("MCP tool {} from server {}", self.name, self.server)
                }),
                "parameters": self.input_schema
            }
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpToolRef {
    pub server: String,
    pub tool: String,
    pub schema_name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolsListResult {
    #[serde(default)]
    pub tools: Vec<McpToolDescriptor>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ResourcesListResult {
    #[serde(default)]
    pub resources: Vec<McpResourceDescriptor>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct McpResourceDescriptor {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct McpToolDescriptor {
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default = "default_input_schema")]
    pub input_schema: Value,
}

fn default_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "required": []
    })
}

#[derive(Clone, Debug, Deserialize)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<McpContent>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadResourceResult {
    #[serde(default)]
    pub contents: Vec<McpResourceContent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpResourceContent {
    pub uri: String,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum McpContent {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}
