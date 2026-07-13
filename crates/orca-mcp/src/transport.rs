use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde_json::{Value, json};

use orca_core::mcp_types::{McpServerConfig, McpTransportKind};

const STDIO_RESPONSE_QUEUE_CAPACITY: usize = 8;
const MAX_STDIO_RESPONSE_LINE_BYTES: usize = 1024 * 1024;
const MAX_SSE_RESPONSE_BYTES: usize = 1024 * 1024;

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
    child: StdioChild,
    stdin: ChildStdin,
    responses: Option<mpsc::Receiver<Result<Value, String>>>,
    next_id: u64,
}

impl StdioState {
    fn terminate(&mut self) {
        self.responses.take();
        self.child.terminate();
    }

    fn terminal_error<T>(&mut self, error: String) -> Result<T, String> {
        self.terminate();
        Err(error)
    }
}

struct StdioChild {
    child: Option<Child>,
}

impl StdioChild {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("stdio child is available")
    }

    fn terminate(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        kill_child_tree(&mut child);
        let _ = child.wait();
    }
}

impl Drop for StdioChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

impl StdioTransport {
    fn start(config: &McpServerConfig) -> Result<Self, String> {
        let command = config
            .command
            .as_deref()
            .ok_or_else(|| format!("MCP server '{}' is missing command", config.name))?;

        let mut child_command = Command::new(command);
        child_command
            .args(&config.args)
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            child_command.process_group(0);
        }
        let child = child_command
            .spawn()
            .map_err(|error| format!("failed to start MCP server '{}': {error}", config.name))?;
        let mut child = StdioChild::new(child);

        let stdin = child
            .child_mut()
            .stdin
            .take()
            .ok_or_else(|| format!("failed to open stdin for MCP server '{}'", config.name))?;
        let stdout = child
            .child_mut()
            .stdout
            .take()
            .ok_or_else(|| format!("failed to open stdout for MCP server '{}'", config.name))?;
        let (response_tx, responses) = mpsc::sync_channel(STDIO_RESPONSE_QUEUE_CAPACITY);
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
                child,
                stdin,
                responses: Some(responses),
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
        if let Err(error) = write_json_line(&mut state.stdin, &message) {
            return state.terminal_error(error);
        }

        let deadline = std::time::Instant::now() + timeout;
        let mut iterations = 0u32;
        loop {
            if should_cancel.is_some_and(|should_cancel| should_cancel()) {
                return state.terminal_error("MCP tool call cancelled".to_string());
            }
            if iterations >= 1000 {
                return state.terminal_error(format!(
                    "MCP request '{method}' exceeded max notification count"
                ));
            }
            if std::time::Instant::now() >= deadline {
                return state.terminal_error(format!(
                    "MCP request '{method}' timed out after {}",
                    format_duration(timeout)
                ));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let wait = match should_cancel {
                Some(_) => remaining.min(Duration::from_millis(25)),
                None => remaining,
            };
            let received = match state.responses.as_ref() {
                Some(responses) => responses.recv_timeout(wait),
                None => {
                    return state.terminal_error("MCP stdio reader is unavailable".to_string());
                }
            };
            let response = match received {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => return state.terminal_error(error),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if should_cancel.is_some() {
                        continue;
                    }
                    return state.terminal_error(format!(
                        "MCP request '{method}' timed out after {}",
                        format_duration(timeout)
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return state
                        .terminal_error("MCP stdio reader stopped before returning".to_string());
                }
            };
            iterations += 1;
            if is_elicitation_create_request(&response) {
                if let Err(error) = handle_elicitation_create_request(
                    &self.server_name,
                    &mut state.stdin,
                    &response,
                    elicitation_handler,
                ) {
                    return state.terminal_error(error);
                }
                continue;
            }
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                return Err(format!("MCP request '{method}' failed: {error}"));
            }
            return match response.get("result").cloned() {
                Some(result) => Ok(result),
                None => state.terminal_error(format!("MCP request '{method}' missing result")),
            };
        }
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "MCP stdio transport lock poisoned".to_string())?;
        if let Err(error) = write_json_line(
            &mut state.stdin,
            &json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params
            }),
        ) {
            return state.terminal_error(error);
        }
        Ok(())
    }
}

fn kill_child_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        kill_process_group(child.id());
    }
    let _ = child.kill();
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    const SIGKILL: i32 = 9;
    let pgid = -(pid as i32);
    unsafe {
        let _ = kill(pgid, SIGKILL);
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

fn read_json_line<R: BufRead>(stdout: &mut R) -> Result<Value, String> {
    let mut line = Vec::new();
    loop {
        let buffer = stdout
            .fill_buf()
            .map_err(|error| format!("failed to read MCP response: {error}"))?;
        if buffer.is_empty() {
            if line.is_empty() {
                return Err("MCP server closed stdout".to_string());
            }
            break;
        }

        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let data_len = newline.unwrap_or(buffer.len());
        if data_len > MAX_STDIO_RESPONSE_LINE_BYTES.saturating_sub(line.len()) {
            return Err(format!(
                "MCP response exceeded maximum line size of {MAX_STDIO_RESPONSE_LINE_BYTES} bytes"
            ));
        }
        line.extend_from_slice(&buffer[..data_len]);
        stdout.consume(data_len + usize::from(newline.is_some()));
        if newline.is_some() {
            break;
        }
    }

    let start = line
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(line.len());
    let end = line
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    serde_json::from_slice(&line[start..end])
        .map_err(|error| format!("invalid MCP response JSON: {error}"))
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
        self.notify("notifications/initialized", json!({}), self.startup_timeout)?;
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
    fn notify(&self, method: &str, params: Value, timeout: Duration) -> Result<(), String> {
        let mut builder = self.client.post(&self.endpoint);
        for (key, value) in &self.headers {
            builder = builder.header(key, value);
        }
        builder
            .timeout(timeout)
            .json(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .send()
            .map_err(|error| {
                if error.is_timeout() {
                    format!(
                        "MCP SSE notify '{method}' timed out after {}",
                        format_duration(timeout)
                    )
                } else {
                    format!("MCP SSE notify '{method}' failed: {error}")
                }
            })?;
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
        let endpoint = self.endpoint.clone();
        let headers = self.headers.clone();
        let method = method.to_string();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("failed to start MCP SSE request runtime: {error}"))
                .and_then(|runtime| {
                    runtime.block_on(request_sse_with_async_client(
                        reqwest::Client::new(),
                        endpoint,
                        headers,
                        id,
                        method,
                        params,
                        timeout,
                        worker_cancel,
                    ))
                });
            let _ = sender.send(result);
        });
        loop {
            if should_cancel() {
                cancel.store(true, Ordering::Release);
                let _ = receiver.recv();
                worker
                    .join()
                    .map_err(|_| "MCP SSE worker panicked during cancellation".to_string())?;
                return Err("MCP tool call cancelled".to_string());
            }
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(result) => {
                    worker
                        .join()
                        .map_err(|_| "MCP SSE worker panicked before returning".to_string())?;
                    return result;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    worker
                        .join()
                        .map_err(|_| "MCP SSE worker panicked before returning".to_string())?;
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

async fn request_sse_with_async_client(
    client: reqwest::Client,
    endpoint: String,
    headers: HashMap<String, String>,
    id: u64,
    method: String,
    params: Value,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
) -> Result<Value, String> {
    let mut builder = client.post(&endpoint);
    for (key, value) in &headers {
        builder = builder.header(key, value);
    }
    let request = builder
        .timeout(timeout)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .send();
    tokio::pin!(request);
    let response = loop {
        tokio::select! {
            result = &mut request => {
                break result.map_err(|error| {
                    if error.is_timeout() {
                        format!(
                            "MCP SSE request '{method}' timed out after {}",
                            format_duration(timeout)
                        )
                    } else {
                        format!("MCP SSE request '{method}' failed: {error}")
                    }
                })?;
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {
                if cancel.load(Ordering::Acquire) {
                    return Err("MCP tool call cancelled".to_string());
                }
            }
        }
    };

    let status = response.status();
    if !status.is_success() {
        return Err(format!("MCP SSE request '{method}' failed with {status}"));
    }
    let text = read_bounded_async_sse_response(response, &cancel).await?;
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

async fn read_bounded_async_sse_response(
    mut response: reqwest::Response,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let mut bytes = Vec::with_capacity(MAX_SSE_RESPONSE_BYTES.min(8 * 1024));
    loop {
        let chunk = tokio::select! {
            result = response.chunk() => result
                .map_err(|error| format!("failed to read MCP SSE response: {error}"))?,
            _ = tokio::time::sleep(Duration::from_millis(25)) => {
                if cancel.load(Ordering::Acquire) {
                    return Err("MCP tool call cancelled".to_string());
                }
                continue;
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_SSE_RESPONSE_BYTES {
            return Err(format!(
                "MCP SSE response exceeded maximum body size of {MAX_SSE_RESPONSE_BYTES} bytes"
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes)
        .map_err(|error| format!("MCP SSE response was not valid UTF-8: {error}"))
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
    let text = read_bounded_sse_response(response)?;
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

fn read_bounded_sse_response(response: reqwest::blocking::Response) -> Result<String, String> {
    let read_limit = MAX_SSE_RESPONSE_BYTES.saturating_add(1) as u64;
    let mut bytes = Vec::with_capacity(MAX_SSE_RESPONSE_BYTES.min(8 * 1024));
    response
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read MCP SSE response: {error}"))?;
    if bytes.len() > MAX_SSE_RESPONSE_BYTES {
        return Err(format!(
            "MCP SSE response exceeded maximum body size of {MAX_SSE_RESPONSE_BYTES} bytes"
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| format!("MCP SSE response was not valid UTF-8: {error}"))
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

    #[test]
    fn stdio_json_line_limit_is_enforced_across_small_read_buffers() {
        let mut at_limit = vec![b' '; MAX_STDIO_RESPONSE_LINE_BYTES - 2];
        at_limit.extend_from_slice(b"{}\n");
        let mut reader = BufReader::with_capacity(7, std::io::Cursor::new(at_limit));
        assert_eq!(
            read_json_line(&mut reader).expect("JSON response at byte limit"),
            json!({})
        );

        let mut over_limit = vec![b' '; MAX_STDIO_RESPONSE_LINE_BYTES - 1];
        over_limit.extend_from_slice(b"{}\n");
        let mut reader = BufReader::with_capacity(7, std::io::Cursor::new(over_limit));
        assert!(
            read_json_line(&mut reader)
                .unwrap_err()
                .contains("MCP response exceeded maximum line size")
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdio_reader_backpressures_unsolicited_response_floods() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("flooding_mcp_server.sh");
        let flood = temp_dir.path().join("responses.jsonl");
        let completed = temp_dir.path().join("flood-completed");
        let response = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{\"message\":\"{}\"}}}}\n",
            "x".repeat(1024)
        );
        fs::write(&flood, response.repeat(2048)).expect("write MCP response flood");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"flood","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      cat "$1"
      : > "$2"
      sleep 5
      ;;
  esac
done
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "flood",
            &server,
            vec![
                flood.to_string_lossy().into_owned(),
                completed.to_string_lossy().into_owned(),
            ],
            5_000,
        ))
        .expect("connect stdio MCP");

        transport.initialize().expect("initialize MCP");
        std::thread::sleep(Duration::from_millis(500));

        assert!(
            !completed.exists(),
            "MCP reader drained an unsolicited flood into memory instead of applying backpressure"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdio_oversized_json_line_is_rejected_and_reaps_descendants() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("oversized_mcp_server.sh");
        let response_file = temp_dir.path().join("oversized-response.jsonl");
        let survivor_marker = temp_dir.path().join("oversized-survivor");
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "payload": "x".repeat(MAX_STDIO_RESPONSE_LINE_BYTES) }
        });
        fs::write(
            &response_file,
            format!(
                "{}\n",
                serde_json::to_string(&response).expect("serialize response")
            ),
        )
        .expect("write oversized response");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
IFS= read -r line
(sleep 0.4; : > "$2") &
cat "$1"
wait
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "oversized",
            &server,
            vec![
                response_file.to_string_lossy().into_owned(),
                survivor_marker.to_string_lossy().into_owned(),
            ],
            5_000,
        ))
        .expect("connect stdio MCP");

        let error = transport
            .initialize()
            .expect_err("oversized response must fail");

        assert!(
            error.contains("MCP response exceeded maximum line size"),
            "unexpected oversized response error: {error}"
        );
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_reader_eof_reaps_descendant_processes() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("closed_stdout_mcp_server.sh");
        let survivor_marker = temp_dir.path().join("reader-eof-survivor");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
IFS= read -r line
(sleep 0.4; : > "$1") >/dev/null 2>&1 &
exec 1>&-
wait
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "closed-stdout",
            &server,
            vec![survivor_marker.to_string_lossy().into_owned()],
            5_000,
        ))
        .expect("connect stdio MCP");

        let error = transport
            .initialize()
            .expect_err("closed MCP stdout must fail");

        assert_eq!(error, "MCP server closed stdout");
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_notification_limit_reaps_descendant_processes() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("notification_flood_mcp_server.sh");
        let flood = temp_dir.path().join("notifications.jsonl");
        let survivor_marker = temp_dir.path().join("notification-survivor");
        fs::write(
            &flood,
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{}}
"#
            .repeat(1000),
        )
        .expect("write notification flood");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
IFS= read -r line
(sleep 0.4; : > "$2") &
cat "$1"
wait
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "notification-flood",
            &server,
            vec![
                flood.to_string_lossy().into_owned(),
                survivor_marker.to_string_lossy().into_owned(),
            ],
            5_000,
        ))
        .expect("connect stdio MCP");

        let error = transport
            .initialize()
            .expect_err("notification flood must fail");

        assert!(
            error.contains("exceeded max notification count"),
            "unexpected notification flood error: {error}"
        );
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_malformed_elicitation_reaps_descendant_processes() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("malformed_elicitation_mcp_server.sh");
        let survivor_marker = temp_dir.path().join("elicitation-survivor");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
IFS= read -r line
(sleep 0.4; : > "$1") &
printf '{"jsonrpc":"2.0","id":"prompt-1","method":"elicitation/create"}\n'
wait
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "malformed-elicitation",
            &server,
            vec![survivor_marker.to_string_lossy().into_owned()],
            5_000,
        ))
        .expect("connect stdio MCP");

        let error = transport
            .initialize()
            .expect_err("malformed elicitation must fail");

        assert!(
            error.contains("missing params"),
            "unexpected malformed elicitation error: {error}"
        );
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_elicitation_write_failure_reaps_descendant_processes() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir
            .path()
            .join("closed_elicitation_stdin_mcp_server.sh");
        let survivor_marker = temp_dir.path().join("elicitation-write-survivor");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"elicitation-write","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"authorize","description":"authorizes","inputSchema":{"type":"object"}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      exec 0<&-
      (sleep 0.4; : > "$1") &
      printf '{"jsonrpc":"2.0","id":"prompt-1","method":"elicitation/create","params":{"message":"Authorize"}}\n'
      wait
      ;;
  esac
done
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "elicitation-write",
            &server,
            vec![survivor_marker.to_string_lossy().into_owned()],
            5_000,
        ))
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");
        transport.list_tools().expect("list tools");

        let error = transport
            .call_tool("authorize", json!({}))
            .expect_err("closed MCP stdin must reject elicitation response");

        assert!(
            error.contains("failed to write MCP request"),
            "unexpected elicitation write failure: {error}"
        );
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_request_write_failure_reaps_descendant_processes() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("closed_stdin_mcp_server.sh");
        let survivor_marker = temp_dir.path().join("write-survivor");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
exec 0<&-
(sleep 0.4; : > "$1") &
wait
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "closed-stdin",
            &server,
            vec![survivor_marker.to_string_lossy().into_owned()],
            5_000,
        ))
        .expect("connect stdio MCP");
        std::thread::sleep(Duration::from_millis(100));

        let error = transport
            .initialize()
            .expect_err("closed MCP stdin must fail");

        assert!(
            error.contains("failed to write MCP request"),
            "unexpected write failure: {error}"
        );
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_json_rpc_error_preserves_connection_for_later_requests() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("recoverable_rpc_error_mcp_server.sh");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"resources":{}},"serverInfo":{"name":"recoverable","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"tools unavailable"}}\n'
      ;;
    *'"method":"resources/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"resources":[]}}\n'
      ;;
  esac
done
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "recoverable",
            &server,
            Vec::new(),
            5_000,
        ))
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");

        let error = transport.list_tools().expect_err("tools/list RPC error");

        assert!(error.contains("tools unavailable"));
        assert_eq!(
            transport
                .list_resources()
                .expect("connection remains usable after JSON-RPC error"),
            json!({ "resources": [] })
        );
    }

    #[cfg(unix)]
    fn write_executable_stdio_fixture(path: &std::path::Path, contents: &str) {
        fs::write(path, contents).expect("write MCP fixture");
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod MCP fixture");
    }

    #[cfg(unix)]
    fn stdio_test_config(
        name: &str,
        server: &std::path::Path,
        args: Vec<String>,
        timeout_ms: u64,
    ) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransportKind::Stdio,
            command: Some("/bin/sh".to_string()),
            args: std::iter::once(server.to_string_lossy().into_owned())
                .chain(args)
                .collect(),
            url: None,
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            startup_timeout_ms: Some(timeout_ms),
            tool_timeout_ms: Some(timeout_ms),
        }
    }

    #[cfg(unix)]
    #[test]
    fn stdio_tool_call_uses_configured_tool_timeout() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let survivor_marker = temp_dir.path().join("timeout-survivor");
        let transport = stalling_stdio_transport(&temp_dir, &survivor_marker, 100);

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
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_tool_call_cancel_reaps_descendant_processes() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let survivor_marker = temp_dir.path().join("cancel-survivor");
        let transport = stalling_stdio_transport(&temp_dir, &survivor_marker, 5_000);

        let started = Instant::now();
        let result = transport.call_tool_with_elicitation_handler_or_cancel(
            "wait",
            Value::Object(Default::default()),
            None,
            &|| started.elapsed() >= Duration::from_millis(100),
        );

        assert!(
            started.elapsed() < Duration::from_millis(750),
            "tool cancellation took {:?}",
            started.elapsed()
        );
        assert_eq!(result.unwrap_err(), "MCP tool call cancelled");
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    fn stalling_stdio_transport(
        temp_dir: &tempfile::TempDir,
        survivor_marker: &std::path::Path,
        tool_timeout_ms: u64,
    ) -> Box<dyn McpTransport> {
        let server = temp_dir.path().join("stalling_mcp_server.sh");
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
      (sleep 0.4; : > "$1") &
      wait
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
            args: vec![
                server.to_string_lossy().into_owned(),
                survivor_marker.to_string_lossy().into_owned(),
            ],
            url: None,
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            startup_timeout_ms: Some(STDIO_TEST_STARTUP_TIMEOUT_MS),
            tool_timeout_ms: Some(tool_timeout_ms),
        })
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");
        transport.list_tools().expect("list tools");
        transport
    }

    #[cfg(unix)]
    fn assert_descendant_did_not_survive(survivor_marker: &std::path::Path) {
        std::thread::sleep(Duration::from_millis(600));
        assert!(
            !survivor_marker.exists(),
            "MCP descendant continued running after transport termination"
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

    #[test]
    fn sse_handler_cancel_closes_peer_before_returning() {
        let (peer_closed_tx, peer_closed_rx) = mpsc::channel();
        let server = OneShotSseServer::start(move |stream| {
            let _ = read_http_request(stream);
            stream
                .set_read_timeout(Some(Duration::from_millis(50)))
                .expect("set peer-close timeout");
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut byte = [0_u8; 1];
            loop {
                match stream.read(&mut byte) {
                    Ok(0) => {
                        let _ = peer_closed_tx.send(true);
                        return;
                    }
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => {
                        let _ = peer_closed_tx.send(true);
                        return;
                    }
                }
                if Instant::now() >= deadline {
                    let _ = peer_closed_tx.send(false);
                    return;
                }
            }
        });
        let transport = SseTransport::new(&McpServerConfig {
            name: "cancel_peer_sse".to_string(),
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
        .expect("connect cancellable SSE MCP");
        let started = Instant::now();

        let error = transport
            .call_tool_with_elicitation_handler_or_cancel(
                "wait",
                Value::Object(Default::default()),
                None,
                &|| started.elapsed() >= Duration::from_millis(100),
            )
            .expect_err("SSE tool call should be cancelled");

        assert_eq!(error, "MCP tool call cancelled");
        assert!(
            peer_closed_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("server should observe cancellation peer close"),
            "cancelled SSE request remained connected after the call returned"
        );
    }

    #[test]
    fn sse_response_body_is_bounded() {
        let server = OneShotSseServer::start(|stream| {
            let _ = read_http_request(stream);
            let body = vec![b'x'; MAX_SSE_RESPONSE_BYTES + 1];
            write_bytes_response(stream, &body);
        });

        let error = request_sse_with_client(
            reqwest::blocking::Client::new(),
            server.url(),
            HashMap::new(),
            1,
            "tools/list".to_string(),
            json!({}),
            Duration::from_secs(2),
        )
        .expect_err("oversized SSE response must be rejected");

        assert!(
            error.contains("exceeded maximum body size"),
            "unexpected oversized response error: {error}"
        );
    }

    #[test]
    fn sse_initialized_notification_uses_startup_timeout() {
        let server = SlowSseServer::start_with_stalling_notification();
        let transport = connect(&McpServerConfig {
            name: "slow_notify_sse".to_string(),
            transport: McpTransportKind::Sse,
            command: None,
            args: Vec::new(),
            url: Some(server.url()),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            startup_timeout_ms: Some(100),
            tool_timeout_ms: Some(100),
        })
        .expect("connect SSE MCP");

        let started = Instant::now();
        let error = transport
            .initialize()
            .expect_err("stalled initialized notification must time out");

        assert!(started.elapsed() < Duration::from_millis(750));
        assert!(
            error.contains("notify 'notifications/initialized' timed out after 100ms"),
            "unexpected notification timeout: {error}"
        );
    }

    struct SlowSseServer {
        addr: std::net::SocketAddr,
    }

    impl SlowSseServer {
        fn start() -> Self {
            Self::start_with_behavior(false)
        }

        fn start_with_stalling_notification() -> Self {
            Self::start_with_behavior(true)
        }

        fn start_with_behavior(stall_notification: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE fixture");
            let addr = listener.local_addr().expect("SSE fixture addr");
            let listener = Arc::new(listener);
            let acceptor = Arc::clone(&listener);
            std::thread::spawn(move || {
                for stream in acceptor.incoming() {
                    match stream {
                        Ok(mut stream) => {
                            handle_sse_fixture_request(&mut stream, stall_notification)
                        }
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

    struct OneShotSseServer {
        addr: std::net::SocketAddr,
    }

    impl OneShotSseServer {
        fn start(handler: impl FnOnce(&mut TcpStream) + Send + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE fixture");
            let addr = listener.local_addr().expect("SSE fixture addr");
            std::thread::spawn(move || {
                if let Ok(mut stream) = listener.accept().map(|(stream, _)| stream) {
                    handler(&mut stream);
                }
            });
            Self { addr }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    fn handle_sse_fixture_request(stream: &mut TcpStream, stall_notification: bool) {
        let request = read_http_request(stream);
        if stall_notification && request.contains(r#""method":"notifications/initialized""#) {
            std::thread::sleep(Duration::from_secs(5));
            return;
        }
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
        write_bytes_response(stream, body.as_bytes());
    }

    fn write_bytes_response(stream: &mut TcpStream, body: &[u8]) {
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
        let _ = stream.write_all(body);
    }
}
