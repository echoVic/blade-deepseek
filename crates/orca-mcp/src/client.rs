use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

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
    config: McpServerConfig,
    server_name: String,
    transport: Mutex<Box<dyn McpTransport>>,
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

    Ok((
        McpClient {
            config: config.clone(),
            server_name: server_name.to_string(),
            transport: Mutex::new(transport),
        },
        tools,
    ))
}

impl McpRegistry {
    #[cfg(any(test, feature = "test-utils"))]
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
        self.call_tool_inner(tool_ref, arguments)
    }

    pub fn call_tool_or_cancel(
        &self,
        tool_ref: &McpToolRef,
        arguments: Value,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<McpCallOutput, String> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string());
        }

        let registry = self.clone();
        let tool_ref = tool_ref.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(registry.call_tool_inner(&tool_ref, arguments));
        });

        loop {
            if should_cancel() {
                return Err("MCP tool call cancelled".to_string());
            }
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("MCP tool call worker stopped before returning".to_string());
                }
            }
        }
    }

    fn call_tool_inner(
        &self,
        tool_ref: &McpToolRef,
        arguments: Value,
    ) -> Result<McpCallOutput, String> {
        let client = self
            .inner
            .clients
            .get(&tool_ref.server)
            .ok_or_else(|| format!("MCP server '{}' is not connected", tool_ref.server))?;
        let result = client.call_tool(&tool_ref.tool, arguments)?;
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

impl McpClient {
    fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        match self.call_tool_once(name, arguments) {
            Err(error) if should_reconnect_after_mcp_error(&error) => {
                let _ = self.reconnect();
                Err(error)
            }
            result => result,
        }
    }

    fn call_tool_once(&self, name: &str, arguments: Value) -> Result<Value, String> {
        let transport = self
            .transport
            .lock()
            .map_err(|_| format!("MCP server '{}' transport lock poisoned", self.server_name))?;
        transport.call_tool(name, arguments)
    }

    fn reconnect(&self) -> Result<(), String> {
        let transport = transport::connect(&self.config)?;
        transport.initialize()?;
        let _ = transport.list_tools()?;
        let mut current = self
            .transport
            .lock()
            .map_err(|_| format!("MCP server '{}' transport lock poisoned", self.server_name))?;
        *current = transport;
        Ok(())
    }
}

fn should_reconnect_after_mcp_error(error: &str) -> bool {
    error.contains("timed out")
        || error.contains("reader stopped")
        || error.contains("server closed stdout")
        || error.contains("failed to write MCP request")
}

#[derive(Debug)]
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
    use crate::transport::McpTransport;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

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

    #[test]
    fn tools_preserve_insertion_order() {
        // The DeepSeek tool schema is byte-pinned for prefix caching, so the
        // registry must expose tools in a stable order. `tools()` is backed by a
        // Vec (insertion order), never a HashMap; lock that here so a future
        // refactor to a map-backed store fails loudly.
        let make = |schema_name: &str| McpTool {
            server: "srv".to_string(),
            name: schema_name.to_string(),
            schema_name: schema_name.to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
        };
        let order = vec!["mcp__srv__zzz", "mcp__srv__aaa", "mcp__srv__mmm"];
        let registry =
            McpRegistry::from_tools_for_test(order.iter().map(|n| make(n)).collect::<Vec<_>>());
        let got: Vec<&str> = registry
            .tools()
            .iter()
            .map(|tool| tool.schema_name.as_str())
            .collect();
        assert_eq!(got, order);
    }

    #[test]
    fn call_tool_or_cancel_returns_promptly_when_cancelled() {
        struct BlockingTransport;

        impl McpTransport for BlockingTransport {
            fn initialize(&self) -> Result<(), String> {
                Ok(())
            }

            fn list_tools(&self) -> Result<Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
                std::thread::sleep(Duration::from_secs(5));
                Ok(serde_json::json!({
                    "content": [{"type": "text", "text": "too late"}],
                    "isError": false
                }))
            }
        }

        let tool = McpTool {
            server: "slow".to_string(),
            name: "wait".to_string(),
            schema_name: "mcp__slow__wait".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
        };
        let registry = McpRegistry {
            inner: Arc::new(McpRegistryInner {
                clients: HashMap::from([(
                    "slow".to_string(),
                    Arc::new(McpClient {
                        config: McpServerConfig {
                            name: "slow".to_string(),
                            ..Default::default()
                        },
                        server_name: "slow".to_string(),
                        transport: Mutex::new(Box::new(BlockingTransport)),
                    }),
                )]),
                tools: vec![tool.clone()],
                lookup: HashMap::from([(
                    tool.schema_name.clone(),
                    McpToolRef {
                        server: tool.server,
                        tool: tool.name,
                        schema_name: tool.schema_name,
                    },
                )]),
                errors: Vec::new(),
            }),
        };
        let tool_ref = registry
            .resolve_tool("mcp__slow__wait")
            .expect("tool ref for slow MCP tool");
        let cancelled = AtomicBool::new(false);
        let started = Instant::now();

        let result =
            registry.call_tool_or_cancel(&tool_ref, Value::Object(Default::default()), &|| {
                if started.elapsed() >= Duration::from_millis(100) {
                    cancelled.store(true, Ordering::SeqCst);
                }
                cancelled.load(Ordering::SeqCst)
            });

        assert!(started.elapsed() < Duration::from_millis(750));
        assert_eq!(result.unwrap_err(), "MCP tool call cancelled");
    }

    #[cfg(unix)]
    #[test]
    fn stdio_client_reconnects_after_timed_out_tool_call() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let state_dir = temp_dir.path().join("state");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let server = temp_dir.path().join("reconnecting_mcp_server.sh");
        std::fs::write(
            &server,
            r#"#!/bin/sh
state_dir="$1"
run_file="$state_dir/run-count"
call_file="$state_dir/call-count"
run_count=0
if [ -f "$run_file" ]; then
  run_count=$(cat "$run_file")
fi
run_count=$((run_count + 1))
printf '%s' "$run_count" > "$run_file"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"slow","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      call_count=0
      if [ -f "$call_file" ]; then
        call_count=$(cat "$call_file")
      fi
      call_count=$((call_count + 1))
      printf '%s' "$call_count" > "$call_file"
      if [ "$call_count" -eq 1 ]; then
        sleep 5
        printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"too late"}],"isError":false}}\n'
      else
        printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"reconnected"}],"isError":false}}\n'
      fi
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&server).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
        }
        let registry = initialize_registry(&[McpServerConfig {
            name: "slow".to_string(),
            transport: orca_core::mcp_types::McpTransportKind::Stdio,
            command: Some(server.to_string_lossy().into_owned()),
            args: vec![state_dir.to_string_lossy().into_owned()],
            url: None,
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            startup_timeout_ms: Some(5000),
            tool_timeout_ms: Some(100),
        }]);
        assert!(
            registry.errors().is_empty(),
            "registry errors: {:?}",
            registry.errors()
        );
        let tool_ref = registry
            .resolve_tool("mcp__slow__wait")
            .expect("registered MCP tool");

        let first = registry.call_tool(&tool_ref, serde_json::json!({}));
        assert!(
            first
                .unwrap_err()
                .contains("MCP request 'tools/call' timed out after 100ms")
        );

        let second = registry
            .call_tool(&tool_ref, serde_json::json!({}))
            .expect("second call should reconnect");

        assert_eq!(second.output, "reconnected");
        let runs = std::fs::read_to_string(state_dir.join("run-count")).expect("run count");
        assert_eq!(runs, "2");
    }
}
