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
use crate::runtime_host::GenerationFence;
use crate::runtime_pending_interaction::{RuntimeMcpElicitationMode, RuntimeMcpElicitationRequest};
use crate::unstable_surface::{RuntimeSurfaceClientHandle, SurfaceInteractionId};

use super::direct_interaction_adapter::{
    JsonlDirectInteractionAdapter, JsonlDirectInteractionKind, JsonlDirectInteractionRoute,
};
use super::opaque_permission_router::{
    JsonlCommittedReplay, JsonlConnectionAdmission, JsonlResponseDigest,
};
use super::{lock_error, write_locked_event};

#[derive(Clone)]
pub(super) struct PendingMcpElicitationRequest {
    pub(super) sender: mpsc::Sender<McpElicitationResponse>,
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) generation: GenerationFence,
}

impl PendingMcpElicitationRequest {
    pub(super) fn generation_scope(&self) -> (&str, &str, GenerationFence) {
        (&self.thread_id, &self.turn_id, self.generation)
    }
}

#[derive(Default)]
struct PendingMcpElicitationState {
    closed: bool,
    pending: HashMap<String, PendingMcpElicitationRequest>,
}

#[derive(Clone)]
pub(super) struct PendingSurfaceMcpElicitationRequest {
    pub(super) client: RuntimeSurfaceClientHandle,
    pub(super) interaction_id: SurfaceInteractionId,
}

#[derive(Clone)]
pub(super) struct PendingMcpElicitationManager {
    state: Arc<Mutex<PendingMcpElicitationState>>,
    direct: JsonlDirectInteractionAdapter<JsonlDirectInteractionRoute>,
}

impl Default for PendingMcpElicitationManager {
    fn default() -> Self {
        Self::new(JsonlDirectInteractionAdapter::new(
            JsonlConnectionAdmission::new_ephemeral(),
        ))
    }
}

impl PendingMcpElicitationManager {
    pub(super) fn new(direct: JsonlDirectInteractionAdapter<JsonlDirectInteractionRoute>) -> Self {
        Self {
            state: Arc::new(Mutex::new(PendingMcpElicitationState::default())),
            direct,
        }
    }

    pub(super) fn insert(
        &self,
        request_id: String,
        request: PendingMcpElicitationRequest,
    ) -> io::Result<()> {
        let mut state = self.state.lock().map_err(lock_error)?;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "server MCP elicitation manager is closed",
            ));
        }
        if state.pending.contains_key(&request_id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate pending MCP elicitation request id: {request_id}"),
            ));
        }
        state.pending.insert(request_id, request);
        Ok(())
    }

    pub(super) fn route_legacy(
        &self,
        request_id: &str,
    ) -> io::Result<Option<PendingMcpElicitationRequest>> {
        let state = self.state.lock().map_err(lock_error)?;
        Ok(state.pending.get(request_id).cloned())
    }

    pub(super) fn settle_legacy(&self, request_id: &str) -> io::Result<()> {
        self.state
            .lock()
            .map_err(lock_error)?
            .pending
            .remove(request_id);
        Ok(())
    }

    pub(super) fn insert_surface(
        &self,
        request_id: String,
        request: PendingSurfaceMcpElicitationRequest,
    ) -> io::Result<String> {
        if self.state.lock().map_err(lock_error)?.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "server MCP elicitation manager is closed",
            ));
        }
        if self
            .state
            .lock()
            .map_err(lock_error)?
            .pending
            .contains_key(&request_id)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate pending MCP elicitation request id: {request_id}"),
            ));
        }
        self.direct.register(
            request_id,
            JsonlDirectInteractionKind::McpElicitation,
            JsonlDirectInteractionRoute::McpElicitation {
                client: request.client,
                interaction_id: request.interaction_id,
            },
        )
    }

    pub(super) fn surface_route(
        &self,
        request_id: &str,
    ) -> io::Result<Option<PendingSurfaceMcpElicitationRequest>> {
        Ok(
            match self
                .direct
                .published_route(request_id, JsonlDirectInteractionKind::McpElicitation)?
            {
                Some(JsonlDirectInteractionRoute::McpElicitation {
                    client,
                    interaction_id,
                }) => Some(PendingSurfaceMcpElicitationRequest {
                    client,
                    interaction_id,
                }),
                Some(JsonlDirectInteractionRoute::UserInput { .. }) | None => None,
            },
        )
    }

    pub(super) fn mark_surface_writing(
        &self,
        request_id: &str,
        frame_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        self.direct.mark_writing(request_id, frame_digest)
    }

    pub(super) fn mark_surface_published(
        &self,
        request_id: &str,
        frame_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        self.direct.mark_published(request_id, frame_digest)
    }

    pub(super) fn settle_surface(
        &self,
        request_id: &str,
        response_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        self.direct
            .settle_committed(request_id, response_digest)
            .map(|_| ())
    }

    pub(super) fn surface_committed_replay(
        &self,
        request_id: &str,
        response_digest: JsonlResponseDigest,
    ) -> io::Result<JsonlCommittedReplay> {
        self.direct.committed_replay(request_id, response_digest)
    }

    pub(super) fn mark_surface_committed_pending(
        &self,
        request_id: &str,
        mutation: &crate::unstable_surface::DeferredMutation,
        response_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        self.direct
            .mark_committed_pending(request_id, mutation, response_digest)
    }

    pub(super) fn close(&self) -> io::Result<()> {
        let pending = {
            let mut state = self.state.lock().map_err(lock_error)?;
            state.closed = true;
            std::mem::take(&mut state.pending)
        };
        for request in pending.into_values() {
            let _ = request.sender.send(McpElicitationResponse::decline());
        }
        Ok(())
    }
}

pub(super) struct ServerMcpElicitationRequestHandler<W: Write + Send + 'static> {
    writer: Arc<Mutex<W>>,
    pending: PendingMcpElicitationManager,
    event_id: Value,
    thread_id: String,
    turn_id: String,
    generation: GenerationFence,
    cancel: CancelToken,
}

impl<W: Write + Send + 'static> ServerMcpElicitationRequestHandler<W> {
    pub(super) fn new(
        writer: Arc<Mutex<W>>,
        pending: PendingMcpElicitationManager,
        event_id: Value,
        thread_id: String,
        turn_id: String,
        generation: GenerationFence,
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
            let _ = self.pending.settle_legacy(&runtime_request.id);
            return Err(error.to_string());
        }
        loop {
            if self.cancel.is_cancelled() {
                let _ = self.pending.settle_legacy(&runtime_request.id);
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

    fn generation(id: u64) -> GenerationFence {
        GenerationFence::for_test(id)
    }

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
                    generation: generation(0),
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
                        generation: generation(0),
                    },
                )
                .is_err(),
            "duplicate pending request ids must not replace the original waiter"
        );

        let pending = manager
            .route_legacy("mcp_elicitation:github:device-flow")
            .expect("route pending")
            .expect("original request still pending");
        pending
            .sender
            .send(McpElicitationResponse::accept(json!({"code": "first"})))
            .expect("original sender still active");
        manager
            .settle_legacy("mcp_elicitation:github:device-flow")
            .expect("settle pending");
        assert_eq!(
            first_receiver.recv().expect("first receiver"),
            McpElicitationResponse::accept(json!({"code": "first"}))
        );
    }

    #[test]
    fn pending_mcp_elicitation_manager_close_declines_waiters_and_rejects_new_requests() {
        let manager = PendingMcpElicitationManager::default();
        let (sender, receiver) = mpsc::channel();
        manager
            .insert(
                "mcp_elicitation:github:device-flow".to_string(),
                PendingMcpElicitationRequest {
                    sender,
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    generation: generation(0),
                },
            )
            .expect("insert pending request");

        manager.close().expect("close manager");
        assert_eq!(
            receiver.recv().expect("close settlement"),
            McpElicitationResponse::decline()
        );
        let (sender, _receiver) = mpsc::channel();
        assert_eq!(
            manager
                .insert(
                    "mcp_elicitation:github:next".to_string(),
                    PendingMcpElicitationRequest {
                        sender,
                        thread_id: "thread-1".to_string(),
                        turn_id: "turn-2".to_string(),
                        generation: generation(1),
                    },
                )
                .expect_err("closed manager rejects new requests")
                .kind(),
            io::ErrorKind::BrokenPipe
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
            generation(0),
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
            .route_legacy("mcp_elicitation:turn-1:github:device-flow")
            .expect("route pending")
            .expect("pending request");
        pending
            .sender
            .send(McpElicitationResponse::accept(json!({"code": "ABCD-1234"})))
            .expect("send response");
        manager
            .settle_legacy("mcp_elicitation:turn-1:github:device-flow")
            .expect("settle pending");

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
            generation(0),
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
                .route_legacy(request_id)
                .expect("route pending")
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
