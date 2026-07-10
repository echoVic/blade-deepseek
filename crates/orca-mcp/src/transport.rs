use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

use orca_core::mcp_types::{McpServerConfig, McpTransportKind};

#[derive(Clone, Debug, PartialEq)]
pub enum McpElicitationMode {
    Form,
    Url,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpElicitationRequest {
    pub server_name: String,
    pub id: String,
    pub mode: McpElicitationMode,
    pub message: String,
    pub url: Option<String>,
    pub requested_schema: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum McpElicitationResponse {
    Accept { content: Value },
    Decline,
}

impl McpElicitationResponse {
    pub fn accept(content: Value) -> Self {
        Self::Accept { content }
    }

    pub fn decline() -> Self {
        Self::Decline
    }
}

pub trait McpElicitationHandler {
    fn handle_elicitation(
        &self,
        request: McpElicitationRequest,
    ) -> Result<McpElicitationResponse, String>;
}

pub trait McpTransport: Send + Sync {
    fn initialize(&self) -> Result<Value, String>;
    fn list_tools(&self) -> Result<Value, String>;
    fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String>;
    fn call_tool_with_elicitation_handler(
        &self,
        name: &str,
        arguments: Value,
        _handler: Option<&dyn McpElicitationHandler>,
    ) -> Result<Value, String> {
        self.call_tool(name, arguments)
    }
    fn call_tool_with_elicitation_handler_or_cancel(
        &self,
        name: &str,
        arguments: Value,
        handler: Option<&dyn McpElicitationHandler>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string());
        }
        self.call_tool_with_elicitation_handler(name, arguments, handler)
    }
    fn list_resources(&self) -> Result<Value, String>;
    fn list_resource_templates(&self) -> Result<Value, String>;
    fn read_resource(&self, uri: &str) -> Result<Value, String>;
}

pub fn connect(config: &McpServerConfig) -> Result<Box<dyn McpTransport>, String> {
    match config.transport {
        McpTransportKind::Stdio => Ok(Box::new(StdioTransport::start(config)?)),
        McpTransportKind::Sse => Ok(Box::new(SseTransport::new(config)?)),
    }
}

struct StdioTransport {
    server_name: String,
    state: Mutex<StdioState>,
    startup_timeout: Duration,
    tool_timeout: Duration,
}

struct StdioState {
    _child: Child,
    stdin: ChildStdin,
    responses: mpsc::Receiver<Result<Value, String>>,
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
        let (response_tx, responses) = mpsc::channel();
        std::thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            loop {
                match read_json_line(&mut stdout) {
                    Ok(value) => {
                        if response_tx.send(Ok(value)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = response_tx.send(Err(error));
                        break;
                    }
                }
            }
        });

        Ok(Self {
            server_name: config.name.clone(),
            state: Mutex::new(StdioState {
                _child: child,
                stdin,
                responses,
                next_id: 1,
            }),
            startup_timeout: timeout_from_ms(config.startup_timeout_ms),
            tool_timeout: timeout_from_ms(config.tool_timeout_ms),
        })
    }
}

impl McpTransport for StdioTransport {
    fn initialize(&self) -> Result<Value, String> {
        let result = self.request_with_timeout(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "orca",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
            self.startup_timeout,
            None,
            None,
        )?;
        self.notify("notifications/initialized", json!({}))?;
        Ok(result)
    }

    fn list_tools(&self) -> Result<Value, String> {
        self.request_with_timeout("tools/list", json!({}), self.startup_timeout, None, None)
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.call_tool_with_elicitation_handler(name, arguments, None)
    }

    fn call_tool_with_elicitation_handler(
        &self,
        name: &str,
        arguments: Value,
        handler: Option<&dyn McpElicitationHandler>,
    ) -> Result<Value, String> {
        self.request_with_timeout(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
            self.tool_timeout,
            handler,
            None,
        )
    }

    fn call_tool_with_elicitation_handler_or_cancel(
        &self,
        name: &str,
        arguments: Value,
        handler: Option<&dyn McpElicitationHandler>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request_with_timeout(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
            self.tool_timeout,
            handler,
            Some(should_cancel),
        )
    }

    fn list_resources(&self) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/list",
            json!({}),
            self.startup_timeout,
            None,
            None,
        )
    }

    fn list_resource_templates(&self) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/templates/list",
            json!({}),
            self.startup_timeout,
            None,
            None,
        )
    }

    fn read_resource(&self, uri: &str) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/read",
            json!({
                "uri": uri
            }),
            self.tool_timeout,
            None,
            None,
        )
    }
}

impl StdioTransport {
    fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        elicitation_handler: Option<&dyn McpElicitationHandler>,
        should_cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<Value, String> {
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

        let deadline = std::time::Instant::now() + timeout;
        let mut iterations = 0u32;
        loop {
            if should_cancel.is_some_and(|should_cancel| should_cancel()) {
                let _ = state._child.kill();
                return Err("MCP tool call cancelled".to_string());
            }
            if iterations >= 1000 {
                return Err(format!(
                    "MCP request '{method}' exceeded max notification count"
                ));
            }
            if std::time::Instant::now() >= deadline {
                let _ = state._child.kill();
                return Err(format!(
                    "MCP request '{method}' timed out after {}",
                    format_duration(timeout)
                ));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let wait = match should_cancel {
                Some(_) => remaining.min(Duration::from_millis(25)),
                None => remaining,
            };
            let response = match state.responses.recv_timeout(wait) {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => return Err(error),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if should_cancel.is_some() {
                        continue;
                    }
                    let _ = state._child.kill();
                    return Err(format!(
                        "MCP request '{method}' timed out after {}",
                        format_duration(timeout)
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("MCP stdio reader stopped before returning".to_string());
                }
            };
            iterations += 1;
            if is_elicitation_create_request(&response) {
                handle_elicitation_create_request(
                    &self.server_name,
                    &mut state.stdin,
                    &response,
                    elicitation_handler,
                )?;
                continue;
            }
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

fn is_elicitation_create_request(message: &Value) -> bool {
    message.get("method").and_then(Value::as_str) == Some("elicitation/create")
}

fn handle_elicitation_create_request(
    server_name: &str,
    stdin: &mut ChildStdin,
    message: &Value,
    handler: Option<&dyn McpElicitationHandler>,
) -> Result<(), String> {
    let id = message
        .get("id")
        .cloned()
        .ok_or_else(|| "MCP elicitation request missing id".to_string())?;
    let request = mcp_elicitation_request_from_json(server_name, message)?;
    let response = match handler {
        Some(handler) => handler.handle_elicitation(request),
        None => Ok(McpElicitationResponse::decline()),
    };

    let message = match response {
        Ok(response) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": mcp_elicitation_response_to_json(response)
        }),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32000,
                "message": error
            }
        }),
    };
    write_json_line(stdin, &message)
}

fn mcp_elicitation_request_from_json(
    server_name: &str,
    message: &Value,
) -> Result<McpElicitationRequest, String> {
    let id = message
        .get("id")
        .map(json_rpc_id_to_string)
        .ok_or_else(|| "MCP elicitation request missing id".to_string())?;
    let params = message
        .get("params")
        .ok_or_else(|| "MCP elicitation request missing params".to_string())?;
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string);
    let requested_schema = params
        .get("requestedSchema")
        .or_else(|| params.get("requested_schema"))
        .cloned();
    let mode = if url.is_some() {
        McpElicitationMode::Url
    } else {
        McpElicitationMode::Form
    };
    Ok(McpElicitationRequest {
        server_name: server_name.to_string(),
        id,
        mode,
        message,
        url,
        requested_schema,
    })
}

fn json_rpc_id_to_string(id: &Value) -> String {
    match id {
        Value::String(value) => value.clone(),
        _ => id.to_string(),
    }
}

fn mcp_elicitation_response_to_json(response: McpElicitationResponse) -> Value {
    match response {
        McpElicitationResponse::Accept { content } => json!({
            "action": "accept",
            "content": content
        }),
        McpElicitationResponse::Decline => json!({
            "action": "decline"
        }),
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
    startup_timeout: Duration,
    tool_timeout: Duration,
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
            startup_timeout: timeout_from_ms(config.startup_timeout_ms),
            tool_timeout: timeout_from_ms(config.tool_timeout_ms),
        })
    }
}

impl McpTransport for SseTransport {
    fn initialize(&self) -> Result<Value, String> {
        let result = self.request_with_timeout(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "orca",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
            self.startup_timeout,
        )?;
        self.notify("notifications/initialized", json!({}))?;
        Ok(result)
    }

    fn list_tools(&self) -> Result<Value, String> {
        self.request_with_timeout("tools/list", json!({}), self.startup_timeout)
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.request_with_timeout(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
            self.tool_timeout,
        )
    }

    fn call_tool_with_elicitation_handler_or_cancel(
        &self,
        name: &str,
        arguments: Value,
        _handler: Option<&dyn McpElicitationHandler>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request_with_timeout_or_cancel(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
            self.tool_timeout,
            should_cancel,
        )
    }

    fn list_resources(&self) -> Result<Value, String> {
        self.request_with_timeout("resources/list", json!({}), self.startup_timeout)
    }

    fn list_resource_templates(&self) -> Result<Value, String> {
        self.request_with_timeout("resources/templates/list", json!({}), self.startup_timeout)
    }

    fn read_resource(&self, uri: &str) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/read",
            json!({
                "uri": uri
            }),
            self.tool_timeout,
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

    fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_request_id()?;
        request_sse_with_client(
            self.client.clone(),
            self.endpoint.clone(),
            self.headers.clone(),
            id,
            method.to_string(),
            params,
            timeout,
        )
    }

    fn request_with_timeout_or_cancel(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string());
        }
        let id = self.next_request_id()?;
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let headers = self.headers.clone();
        let method = method.to_string();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result =
                request_sse_with_client(client, endpoint, headers, id, method, params, timeout);
            let _ = sender.send(result);
        });
        loop {
            if should_cancel() {
                return Err("MCP tool call cancelled".to_string());
            }
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("MCP SSE worker stopped before returning".to_string());
                }
            }
        }
    }

    fn next_request_id(&self) -> Result<u64, String> {
        let mut next_id = self
            .next_id
            .lock()
            .map_err(|_| "MCP SSE id lock poisoned".to_string())?;
        let id = *next_id;
        *next_id += 1;
        Ok(id)
    }
}

fn request_sse_with_client(
    client: reqwest::blocking::Client,
    endpoint: String,
    headers: HashMap<String, String>,
    id: u64,
    method: String,
    params: Value,
    timeout: Duration,
) -> Result<Value, String> {
    let mut builder = client.post(&endpoint);
    for (key, value) in &headers {
        builder = builder.header(key, value);
    }
    let response = builder
        .timeout(timeout)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .send()
        .map_err(|error| {
            if error.is_timeout() {
                format!(
                    "MCP SSE request '{method}' timed out after {}",
                    format_duration(timeout)
                )
            } else {
                format!("MCP SSE request '{method}' failed: {error}")
            }
        })?;

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

fn timeout_from_ms(timeout_ms: Option<u64>) -> Duration {
    Duration::from_millis(timeout_ms.unwrap_or(30_000).max(1))
}

fn format_duration(duration: Duration) -> String {
    if duration.as_millis().is_multiple_of(1000) {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::{Duration, Instant};

    const STDIO_TEST_STARTUP_TIMEOUT_MS: u64 = 15_000;

    #[cfg(unix)]
    #[test]
    fn stdio_tool_call_uses_configured_tool_timeout() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("slow_mcp_server.sh");
        fs::write(
            &server,
            r#"#!/bin/sh
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
      sleep 5
      printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"too late"}],"isError":false}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&server).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
        }
        let transport = connect(&McpServerConfig {
            name: "slow".to_string(),
            transport: McpTransportKind::Stdio,
            command: Some("/bin/sh".to_string()),
            args: vec![server.to_string_lossy().into_owned()],
            url: None,
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            startup_timeout_ms: Some(STDIO_TEST_STARTUP_TIMEOUT_MS),
            tool_timeout_ms: Some(100),
        })
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");
        transport.list_tools().expect("list tools");

        let started = Instant::now();
        let result = transport.call_tool("wait", Value::Object(Default::default()));

        assert!(
            started.elapsed() < Duration::from_millis(750),
            "tool call took {:?}",
            started.elapsed()
        );
        assert!(
            result
                .unwrap_err()
                .contains("MCP request 'tools/call' timed out after 100ms")
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdio_tool_call_routes_elicitation_request_before_final_response() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("elicitation_mcp_server.sh");
        fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"elicits","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"authorize","description":"needs user input","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":"prompt-1","method":"elicitation/create","params":{"message":"Authorize GitHub","url":"https://github.com/login/device","elicitationId":"device-flow"}}\n'
      IFS= read -r response
      case "$response" in
        *'"id":"prompt-1"'*'"action":"accept"'*'"code":"1234"'*)
          printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"authorized"}],"isError":false}}\n'
          ;;
        *)
          printf '{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"missing elicitation response"}}\n'
          ;;
      esac
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&server).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
        }
        let transport = connect(&McpServerConfig {
            name: "elicits".to_string(),
            transport: McpTransportKind::Stdio,
            command: Some("/bin/sh".to_string()),
            args: vec![server.to_string_lossy().into_owned()],
            url: None,
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            startup_timeout_ms: Some(STDIO_TEST_STARTUP_TIMEOUT_MS),
            tool_timeout_ms: Some(1000),
        })
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");
        transport.list_tools().expect("list tools");
        let handler = RecordingElicitationHandler::new(McpElicitationResponse::accept(
            serde_json::json!({"code":"1234"}),
        ));

        let result = transport
            .call_tool_with_elicitation_handler(
                "authorize",
                Value::Object(Default::default()),
                Some(&handler),
            )
            .expect("tool result after elicitation");

        assert_eq!(result["content"][0]["text"], "authorized");
        assert_eq!(
            handler.requests.lock().unwrap().as_slice(),
            &[McpElicitationRequest {
                server_name: "elicits".to_string(),
                id: "prompt-1".to_string(),
                mode: McpElicitationMode::Url,
                message: "Authorize GitHub".to_string(),
                url: Some("https://github.com/login/device".to_string()),
                requested_schema: None,
            }]
        );
    }

    struct RecordingElicitationHandler {
        response: McpElicitationResponse,
        requests: StdMutex<Vec<McpElicitationRequest>>,
    }

    impl RecordingElicitationHandler {
        fn new(response: McpElicitationResponse) -> Self {
            Self {
                response,
                requests: StdMutex::new(Vec::new()),
            }
        }
    }

    impl McpElicitationHandler for RecordingElicitationHandler {
        fn handle_elicitation(
            &self,
            request: McpElicitationRequest,
        ) -> Result<McpElicitationResponse, String> {
            self.requests.lock().unwrap().push(request);
            Ok(self.response.clone())
        }
    }

    #[test]
    fn sse_tool_call_uses_configured_tool_timeout() {
        let server = SlowSseServer::start();
        let transport = connect(&McpServerConfig {
            name: "slow_sse".to_string(),
            transport: McpTransportKind::Sse,
            command: None,
            args: Vec::new(),
            url: Some(server.url()),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            startup_timeout_ms: Some(5000),
            tool_timeout_ms: Some(100),
        })
        .expect("connect SSE MCP");
        transport.initialize().expect("initialize SSE MCP");
        transport.list_tools().expect("list SSE tools");

        let started = Instant::now();
        let result = transport.call_tool("wait", Value::Object(Default::default()));

        assert!(
            started.elapsed() < Duration::from_millis(750),
            "tool call took {:?}",
            started.elapsed()
        );
        assert!(
            result
                .unwrap_err()
                .contains("MCP SSE request 'tools/call' timed out after 100ms")
        );
    }

    #[test]
    fn sse_handler_cancel_returns_promptly() {
        let server = SlowSseServer::start();
        let transport = connect(&McpServerConfig {
            name: "slow_sse".to_string(),
            transport: McpTransportKind::Sse,
            command: None,
            args: Vec::new(),
            url: Some(server.url()),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            startup_timeout_ms: Some(5000),
            tool_timeout_ms: Some(5000),
        })
        .expect("connect SSE MCP");
        transport.initialize().expect("initialize SSE MCP");
        transport.list_tools().expect("list SSE tools");
        let handler = RecordingElicitationHandler::new(McpElicitationResponse::decline());

        let started = Instant::now();
        let result = transport.call_tool_with_elicitation_handler_or_cancel(
            "wait",
            Value::Object(Default::default()),
            Some(&handler),
            &|| started.elapsed() >= Duration::from_millis(100),
        );

        assert!(
            started.elapsed() < Duration::from_millis(750),
            "tool call took {:?}",
            started.elapsed()
        );
        assert_eq!(result.unwrap_err(), "MCP tool call cancelled");
        assert!(
            handler.requests.lock().unwrap().is_empty(),
            "SSE transport does not support elicitation/create routing"
        );
    }

    struct SlowSseServer {
        addr: std::net::SocketAddr,
    }

    impl SlowSseServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE fixture");
            let addr = listener.local_addr().expect("SSE fixture addr");
            let listener = Arc::new(listener);
            let acceptor = Arc::clone(&listener);
            std::thread::spawn(move || {
                for stream in acceptor.incoming() {
                    match stream {
                        Ok(mut stream) => handle_sse_fixture_request(&mut stream),
                        Err(_) => break,
                    }
                }
            });
            Self { addr }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    fn handle_sse_fixture_request(stream: &mut TcpStream) {
        let request = read_http_request(stream);
        if request.contains(r#""method":"tools/call""#) {
            std::thread::sleep(Duration::from_secs(5));
            write_json_response(
                stream,
                r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"too late"}],"isError":false}}"#,
            );
            return;
        }
        if request.contains(r#""method":"tools/list""#) {
            write_json_response(
                stream,
                r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}"#,
            );
            return;
        }
        if request.contains(r#""method":"initialize""#) {
            write_json_response(
                stream,
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"slow_sse","version":"1"}}}"#,
            );
            return;
        }
        write_json_response(stream, r#"{"jsonrpc":"2.0","result":{}}"#);
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let read = stream.read(&mut chunk).expect("read request");
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            let request = String::from_utf8_lossy(&buffer);
            if let Some(header_end) = request.find("\r\n\r\n") {
                let content_length = request
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if buffer.len() >= header_end + 4 + content_length {
                    return request.into_owned();
                }
            }
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }

    fn write_json_response(stream: &mut TcpStream, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    }
}
