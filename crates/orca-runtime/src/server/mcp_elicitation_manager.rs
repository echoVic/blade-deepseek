use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use orca_core::cancel::CancelToken;
use orca_mcp::{
    McpElicitationHandler, McpElicitationMode, McpElicitationRequest, McpElicitationResponse,
};
use serde_json::{Value, json};

use crate::protocol::ServerEvent;
use crate::runtime_pending_interaction::{RuntimeMcpElicitationMode, RuntimeMcpElicitationRequest};

use super::{lock_error, write_locked_event};

pub(super) struct PendingMcpElicitationRequest {
    pub(super) sender: mpsc::Sender<McpElicitationResponse>,
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) generation: u64,
}

impl PendingMcpElicitationRequest {
    pub(super) fn generation_scope(&self) -> (&str, &str, u64) {
        (&self.thread_id, &self.turn_id, self.generation)
    }
}

#[derive(Clone, Default)]
pub(super) struct PendingMcpElicitationManager {
    pending: Arc<Mutex<HashMap<String, PendingMcpElicitationRequest>>>,
}

impl PendingMcpElicitationManager {
    pub(super) fn insert(
        &self,
        request_id: String,
        request: PendingMcpElicitationRequest,
    ) -> io::Result<()> {
        let mut pending = self.pending.lock().map_err(lock_error)?;
        if pending.contains_key(&request_id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate pending MCP elicitation request id: {request_id}"),
            ));
        }
        pending.insert(request_id, request);
        Ok(())
    }

    pub(super) fn remove(
        &self,
        request_id: &str,
    ) -> io::Result<Option<PendingMcpElicitationRequest>> {
        let mut pending = self.pending.lock().map_err(lock_error)?;
        Ok(pending.remove(request_id))
    }
}

pub(super) struct ServerMcpElicitationRequestHandler<W: Write + Send + 'static> {
    writer: Arc<Mutex<W>>,
    pending: PendingMcpElicitationManager,
    event_id: Value,
    thread_id: String,
    turn_id: String,
    generation: u64,
    cancel: CancelToken,
}

impl<W: Write + Send + 'static> ServerMcpElicitationRequestHandler<W> {
    pub(super) fn new(
        writer: Arc<Mutex<W>>,
        pending: PendingMcpElicitationManager,
        event_id: Value,
        thread_id: String,
        turn_id: String,
        generation: u64,
        cancel: CancelToken,
    ) -> Self {
        Self {
            writer,
            pending,
            event_id,
            thread_id,
            turn_id,
            generation,
            cancel,
        }
    }
}

impl<W: Write + Send + 'static> McpElicitationHandler for ServerMcpElicitationRequestHandler<W> {
    fn handle_elicitation(
        &self,
        request: McpElicitationRequest,
    ) -> Result<McpElicitationResponse, String> {
        let mode = match request.mode {
            McpElicitationMode::Form => RuntimeMcpElicitationMode::Form,
            McpElicitationMode::Url => RuntimeMcpElicitationMode::Url,
        };
        let requested_schema_json = request
            .requested_schema
            .as_ref()
            .map(serde_json::Value::to_string);
        let scoped_turn_id = super::generation_scoped_id(self.turn_id.clone(), self.generation);
        let runtime_request = RuntimeMcpElicitationRequest::new_scoped(
            &scoped_turn_id,
            request.server_name,
            request.id,
            mode,
            request.message,
            request.url,
            requested_schema_json,
        );
        let requested_schema = runtime_request
            .requested_schema_json
            .as_ref()
            .and_then(|schema| serde_json::from_str::<Value>(schema).ok())
            .unwrap_or(Value::Null);
        let mode_value = match runtime_request.mode {
            RuntimeMcpElicitationMode::Form => json!("form"),
            RuntimeMcpElicitationMode::Url => json!("url"),
        };
        let (sender, receiver) = mpsc::channel();
        self.pending
            .insert(
                runtime_request.id.clone(),
                PendingMcpElicitationRequest {
                    sender,
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    generation: self.generation,
                },
            )
            .map_err(|error| error.to_string())?;
        if let Err(error) = write_locked_event(
            &self.writer,
            &self.event_id,
            ServerEvent::McpElicitationRequest {
                request_id: json!(runtime_request.id.clone()),
                thread_id: json!(self.thread_id),
                turn_id: json!(self.turn_id),
                server_name: json!(runtime_request.server_name),
                mode: mode_value,
                message: json!(runtime_request.message),
                url: json!(runtime_request.url),
                requested_schema,
            },
        ) {
            let _ = self.pending.remove(&runtime_request.id);
            return Err(error.to_string());
        }
        loop {
            if self.cancel.is_cancelled() {
                let _ = self.pending.remove(&runtime_request.id);
                return Err("MCP elicitation request cancelled".to_string());
            }
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(response) => return Ok(response),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("MCP elicitation response channel closed".to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn pending_mcp_elicitation_manager_rejects_duplicate_request_id_without_overwriting() {
        let manager = PendingMcpElicitationManager::default();
        let (first_sender, first_receiver) = mpsc::channel();
        let (second_sender, _second_receiver) = mpsc::channel();

        manager
            .insert(
                "mcp_elicitation:github:device-flow".to_string(),
                PendingMcpElicitationRequest {
                    sender: first_sender,
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    generation: 0,
                },
            )
            .expect("insert first request");
        assert!(
            manager
                .insert(
                    "mcp_elicitation:github:device-flow".to_string(),
                    PendingMcpElicitationRequest {
                        sender: second_sender,
                        thread_id: "thread-2".to_string(),
                        turn_id: "turn-2".to_string(),
                        generation: 0,
                    },
                )
                .is_err(),
            "duplicate pending request ids must not replace the original waiter"
        );

        let pending = manager
            .remove("mcp_elicitation:github:device-flow")
            .expect("remove pending")
            .expect("original request still pending");
        pending
            .sender
            .send(McpElicitationResponse::accept(json!({"code": "first"})))
            .expect("original sender still active");
        assert_eq!(
            first_receiver.recv().expect("first receiver"),
            McpElicitationResponse::accept(json!({"code": "first"}))
        );
    }

    #[test]
    fn server_mcp_elicitation_handler_emits_request_and_waits_for_response() {
        let writer = Arc::new(Mutex::new(Vec::new()));
        let manager = PendingMcpElicitationManager::default();
        let handler = ServerMcpElicitationRequestHandler::new(
            Arc::clone(&writer),
            manager.clone(),
            json!("turn"),
            "thread-1".to_string(),
            "turn-1".to_string(),
            0,
            CancelToken::new(),
        );

        let worker = std::thread::spawn(move || {
            handler.handle_elicitation(McpElicitationRequest {
                server_name: "github".to_string(),
                id: "device-flow".to_string(),
                mode: McpElicitationMode::Url,
                message: "Authorize GitHub".to_string(),
                url: Some("https://github.com/login/device".to_string()),
                requested_schema: Some(json!({"type": "object"})),
            })
        });

        let request = wait_for_written_event(&writer, Duration::from_secs(2));
        assert_eq!(request["id"], "turn");
        assert_eq!(request["event"], "mcp_elicitation_request");
        assert_eq!(
            request["requestId"],
            "mcp_elicitation:turn-1:github:device-flow"
        );
        assert_eq!(request["threadId"], "thread-1");
        assert_eq!(request["turnId"], "turn-1");
        assert_eq!(request["serverName"], "github");
        assert_eq!(request["mode"], "url");
        assert_eq!(request["message"], "Authorize GitHub");
        assert_eq!(request["url"], "https://github.com/login/device");
        assert_eq!(request["requestedSchema"], json!({"type": "object"}));

        let pending = manager
            .remove("mcp_elicitation:turn-1:github:device-flow")
            .expect("remove pending")
            .expect("pending request");
        pending
            .sender
            .send(McpElicitationResponse::accept(json!({"code": "ABCD-1234"})))
            .expect("send response");

        assert_eq!(
            worker.join().expect("handler thread"),
            Ok(McpElicitationResponse::accept(json!({"code": "ABCD-1234"})))
        );
    }

    #[test]
    fn server_mcp_elicitation_handler_cleans_pending_request_when_cancelled() {
        let writer = Arc::new(Mutex::new(Vec::new()));
        let manager = PendingMcpElicitationManager::default();
        let cancel = CancelToken::new();
        let handler = ServerMcpElicitationRequestHandler::new(
            Arc::clone(&writer),
            manager.clone(),
            json!("turn"),
            "thread-1".to_string(),
            "turn-1".to_string(),
            0,
            cancel.clone(),
        );

        let worker = std::thread::spawn(move || {
            handler.handle_elicitation(McpElicitationRequest {
                server_name: "github".to_string(),
                id: "device-flow".to_string(),
                mode: McpElicitationMode::Url,
                message: "Authorize GitHub".to_string(),
                url: Some("https://github.com/login/device".to_string()),
                requested_schema: None,
            })
        });

        let request = wait_for_written_event(&writer, Duration::from_secs(2));
        let request_id = request["requestId"].as_str().expect("request id");
        assert_eq!(request_id, "mcp_elicitation:turn-1:github:device-flow");

        cancel.cancel();

        assert_eq!(
            worker.join().expect("handler thread"),
            Err("MCP elicitation request cancelled".to_string())
        );
        assert!(
            manager
                .remove(request_id)
                .expect("remove pending")
                .is_none(),
            "cancelled request should be removed from pending map"
        );
    }

    fn wait_for_written_event(writer: &Arc<Mutex<Vec<u8>>>, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let output = writer.lock().expect("writer").clone();
            if !output.is_empty() {
                let line = String::from_utf8(output).expect("utf8");
                return serde_json::from_str(line.lines().next().expect("jsonl"))
                    .expect("server event");
            }
            assert!(Instant::now() < deadline, "timed out waiting for event");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
