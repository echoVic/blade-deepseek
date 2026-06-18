use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use serde_json::{Value, json};

use orca_core::mcp_types::{McpServerConfig, McpTransportKind};

pub trait McpTransport: Send + Sync {
    fn initialize(&self) -> Result<(), String>;
    fn list_tools(&self) -> Result<Value, String>;
    fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String>;
}

pub fn connect(config: &McpServerConfig) -> Result<Box<dyn McpTransport>, String> {
    match config.transport {
        McpTransportKind::Stdio => Ok(Box::new(StdioTransport::start(config)?)),
        McpTransportKind::Sse => Ok(Box::new(SseTransport::new(config)?)),
    }
}

struct StdioTransport {
    state: Mutex<StdioState>,
}

struct StdioState {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Drop for StdioState {
    fn drop(&mut self) {
        let _ = self._child.kill();
        let _ = self._child.wait();
    }
}

impl StdioTransport {
    fn start(config: &McpServerConfig) -> Result<Self, String> {
        let command = config
            .command
            .as_deref()
            .ok_or_else(|| format!("MCP server '{}' is missing command", config.name))?;

        let mut child = Command::new(command)
            .args(&config.args)
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("failed to start MCP server '{}': {error}", config.name))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("failed to open stdin for MCP server '{}'", config.name))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("failed to open stdout for MCP server '{}'", config.name))?;

        Ok(Self {
            state: Mutex::new(StdioState {
                _child: child,
                stdin,
                stdout: BufReader::new(stdout),
                next_id: 1,
            }),
        })
    }
}

impl McpTransport for StdioTransport {
    fn initialize(&self) -> Result<(), String> {
        let _ = self.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "orca",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )?;
        self.notify("notifications/initialized", json!({}))
    }

    fn list_tools(&self) -> Result<Value, String> {
        self.request("tools/list", json!({}))
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.request(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
        )
    }
}

impl StdioTransport {
    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "MCP stdio transport lock poisoned".to_string())?;
        let id = state.next_id;
        state.next_id += 1;

        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        write_json_line(&mut state.stdin, &message)?;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut iterations = 0u32;
        loop {
            if iterations >= 1000 {
                return Err(format!("MCP request '{method}' exceeded max notification count"));
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("MCP request '{method}' timed out after 30s"));
            }
            iterations += 1;

            let response = read_json_line(&mut state.stdout)?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                return Err(format!("MCP request '{method}' failed: {error}"));
            }
            return response
                .get("result")
                .cloned()
                .ok_or_else(|| format!("MCP request '{method}' missing result"));
        }
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "MCP stdio transport lock poisoned".to_string())?;
        write_json_line(
            &mut state.stdin,
            &json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params
            }),
        )
    }
}

fn write_json_line(stdin: &mut ChildStdin, message: &Value) -> Result<(), String> {
    let mut line = serde_json::to_vec(message).map_err(|error| error.to_string())?;
    line.push(b'\n');
    stdin
        .write_all(&line)
        .and_then(|_| stdin.flush())
        .map_err(|error| format!("failed to write MCP request: {error}"))
}

fn read_json_line(stdout: &mut BufReader<ChildStdout>) -> Result<Value, String> {
    let mut line = String::new();
    let read = stdout
        .read_line(&mut line)
        .map_err(|error| format!("failed to read MCP response: {error}"))?;
    if read == 0 {
        return Err("MCP server closed stdout".to_string());
    }
    serde_json::from_str(line.trim()).map_err(|error| format!("invalid MCP response JSON: {error}"))
}

struct SseTransport {
    endpoint: String,
    headers: HashMap<String, String>,
    next_id: Mutex<u64>,
    client: reqwest::blocking::Client,
}

impl SseTransport {
    fn new(config: &McpServerConfig) -> Result<Self, String> {
        let endpoint = config
            .url
            .clone()
            .ok_or_else(|| format!("MCP SSE server '{}' is missing url", config.name))?;
        let client = reqwest::blocking::Client::new();
        Ok(Self {
            endpoint,
            headers: config.headers.clone(),
            next_id: Mutex::new(1),
            client,
        })
    }
}

impl McpTransport for SseTransport {
    fn initialize(&self) -> Result<(), String> {
        let _ = self.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "orca",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )?;
        self.notify("notifications/initialized", json!({}))
    }

    fn list_tools(&self) -> Result<Value, String> {
        self.request("tools/list", json!({}))
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.request(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
        )
    }
}

impl SseTransport {
    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let mut builder = self.client.post(&self.endpoint);
        for (key, value) in &self.headers {
            builder = builder.header(key, value);
        }
        builder
            .json(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .send()
            .map_err(|e| format!("MCP SSE notify '{method}' failed: {e}"))?;
        Ok(())
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = {
            let mut next_id = self
                .next_id
                .lock()
                .map_err(|_| "MCP SSE id lock poisoned".to_string())?;
            let id = *next_id;
            *next_id += 1;
            id
        };

        let mut builder = self.client.post(&self.endpoint);
        for (key, value) in &self.headers {
            builder = builder.header(key, value);
        }
        let response = builder
            .json(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params
            }))
            .send()
            .map_err(|error| format!("MCP SSE request '{method}' failed: {error}"))?;

        let status = response.status();
        if !status.is_success() {
            return Err(format!("MCP SSE request '{method}' failed with {status}"));
        }
        let text = response
            .text()
            .map_err(|error| format!("failed to read MCP SSE response: {error}"))?;
        let response = parse_sse_or_json_response(&text)
            .map_err(|error| format!("invalid MCP SSE response for '{method}': {error}"))?;
        if let Some(error) = response.get("error") {
            return Err(format!("MCP SSE request '{method}' failed: {error}"));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| format!("MCP SSE request '{method}' missing result"))
    }
}

fn parse_sse_or_json_response(text: &str) -> Result<Value, String> {
    if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
        return Ok(value);
    }

    let data = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Err("response was neither JSON nor SSE data".to_string());
    }
    serde_json::from_str(&data).map_err(|error| error.to_string())
}
