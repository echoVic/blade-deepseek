use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use orca_core::cancel::CancelToken;
use serde_json::{Value, json};

use crate::lifecycle::{
    RuntimePermissionRequest, RuntimePermissionRequestHandler, RuntimePermissionResponse,
};
use crate::protocol::{self, ServerEvent};
use crate::runtime_host::GenerationFence;
use crate::unstable_surface::{
    RuntimeSurfaceClientHandle, SurfaceInteractionId, SurfaceInteractionKind,
};

use super::opaque_permission_router::{
    JsonlCommittedReplay, JsonlConnectionAdmission, JsonlOpaquePermissionRouter,
    JsonlOwnerSettlement, JsonlResponseDigest, JsonlRetiredRequestOwner,
    JsonlRetiredRequestSettlement,
};
use super::{lock_error, write_locked_event};

#[derive(Clone)]
pub(super) struct PendingCommandExecPermissionRequest {
    pub(super) thread_id: String,
    pub(super) runtime_workspace_roots: Vec<PathBuf>,
    pub(super) command: Vec<String>,
    pub(super) process_id: Option<String>,
    pub(super) cwd: Option<PathBuf>,
    pub(super) env: protocol::CommandEnvOverrides,
    pub(super) options: protocol::CommandExecOptions,
    pub(super) terminal: crate::shell_session::ShellTerminalMode,
    pub(super) event_id: Value,
}

#[derive(Clone)]
pub(super) enum PendingPermissionRequest {
    Runtime {
        sender: mpsc::Sender<RuntimePermissionResponse>,
        thread_id: String,
        turn_id: String,
        generation: GenerationFence,
        runtime_workspace_roots: Vec<PathBuf>,
    },
    Surface {
        client: RuntimeSurfaceClientHandle,
        interaction_id: SurfaceInteractionId,
        target: SurfaceInteractionKind,
        thread_id: String,
        runtime_workspace_roots: Vec<PathBuf>,
    },
    CommandExec {
        request: Box<PendingCommandExecPermissionRequest>,
    },
}

impl PendingPermissionRequest {
    pub(super) fn thread_id(&self) -> &str {
        match self {
            Self::Runtime { thread_id, .. } | Self::Surface { thread_id, .. } => thread_id,
            Self::CommandExec { request } => &request.thread_id,
        }
    }

    pub(super) fn runtime_workspace_roots(&self) -> &[PathBuf] {
        match self {
            Self::Runtime {
                runtime_workspace_roots,
                ..
            }
            | Self::Surface {
                runtime_workspace_roots,
                ..
            } => runtime_workspace_roots,
            Self::CommandExec { request } => &request.runtime_workspace_roots,
        }
    }

    pub(super) fn runtime_generation(&self) -> Option<(&str, &str, GenerationFence)> {
        match self {
            Self::Runtime {
                thread_id,
                turn_id,
                generation,
                ..
            } => Some((thread_id, turn_id, *generation)),
            Self::Surface { .. } | Self::CommandExec { .. } => None,
        }
    }
}

#[derive(Default)]
struct PendingPermissionState {
    closed: bool,
}

#[derive(Clone)]
pub(super) struct PendingPermissionManager {
    state: Arc<Mutex<PendingPermissionState>>,
    router: JsonlOpaquePermissionRouter<PendingPermissionRequest>,
}

impl Default for PendingPermissionManager {
    fn default() -> Self {
        Self::new(JsonlOpaquePermissionRouter::new(
            JsonlConnectionAdmission::new_ephemeral(),
        ))
    }
}

impl PendingPermissionManager {
    pub(super) fn new(router: JsonlOpaquePermissionRouter<PendingPermissionRequest>) -> Self {
        Self {
            state: Arc::new(Mutex::new(PendingPermissionState::default())),
            router,
        }
    }

    pub(super) fn insert_command_exec(
        &self,
        request_id: String,
        request: PendingCommandExecPermissionRequest,
    ) -> io::Result<String> {
        {
            let state = self.state.lock().map_err(lock_error)?;
            Self::ensure_open(&state)?;
        }
        if self.router.route(&request_id)?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate pending permission request id: {request_id}"),
            ));
        }
        self.router.register(
            request_id,
            JsonlRetiredRequestOwner::CommandExecPermission,
            PendingPermissionRequest::CommandExec {
                request: Box::new(request),
            },
        )
    }

    pub(super) fn insert_surface(
        &self,
        request_id: String,
        client: RuntimeSurfaceClientHandle,
        interaction_id: SurfaceInteractionId,
        target: SurfaceInteractionKind,
        thread_id: String,
        runtime_workspace_roots: Vec<PathBuf>,
    ) -> io::Result<String> {
        {
            let state = self.state.lock().map_err(lock_error)?;
            Self::ensure_open(&state)?;
        }
        if self.router.route(&request_id)?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate pending permission request id: {request_id}"),
            ));
        }
        self.router.register(
            request_id,
            JsonlRetiredRequestOwner::ThreadPermission,
            PendingPermissionRequest::Surface {
                client,
                interaction_id,
                target,
                thread_id,
                runtime_workspace_roots,
            },
        )
    }

    pub(super) fn route(&self, request_id: &str) -> io::Result<Option<PendingPermissionRequest>> {
        self.router.route(request_id)
    }

    pub(super) fn published_route(
        &self,
        request_id: &str,
    ) -> io::Result<Option<PendingPermissionRequest>> {
        self.router.published_route(request_id)
    }

    pub(super) fn mark_writing(
        &self,
        request_id: &str,
        frame_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        self.router.mark_writing(request_id, frame_digest)
    }

    pub(super) fn mark_published(
        &self,
        request_id: &str,
        frame_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        self.router.mark_published(request_id, frame_digest)
    }

    pub(super) fn settle(
        &self,
        request_id: &str,
        response_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        self.router
            .settle(
                request_id,
                JsonlRetiredRequestSettlement::PermissionCommitted { response_digest },
            )
            .map(|_| ())
    }

    pub(super) fn committed_replay(
        &self,
        request_id: &str,
        response_digest: JsonlResponseDigest,
    ) -> io::Result<JsonlCommittedReplay> {
        self.router.committed_replay(request_id, response_digest)
    }

    pub(super) fn mark_committed_pending(
        &self,
        request_id: &str,
        mutation: &crate::unstable_surface::DeferredMutation,
        response_digest: JsonlResponseDigest,
    ) -> io::Result<()> {
        self.router
            .mark_committed_pending(request_id, mutation, response_digest)
    }

    pub(super) fn retire_unavailable(&self, request_id: &str) -> io::Result<()> {
        self.router
            .settle(
                request_id,
                JsonlRetiredRequestSettlement::TransportRetired {
                    owner_settlement: JsonlOwnerSettlement::InteractionRecoveryRetained,
                },
            )
            .map(|_| ())
    }

    pub(super) fn close(&self) -> io::Result<()> {
        self.seal_legacy()?;
        self.router.close_routes_by_owner().map(|_| ())
    }

    pub(super) fn seal_legacy(&self) -> io::Result<()> {
        let mut state = self.state.lock().map_err(lock_error)?;
        state.closed = true;
        Ok(())
    }

    fn insert_runtime(
        &self,
        request_id: String,
        request: PendingPermissionRequest,
    ) -> io::Result<String> {
        {
            let state = self.state.lock().map_err(lock_error)?;
            Self::ensure_open(&state)?;
        }
        let owner = match &request {
            PendingPermissionRequest::CommandExec { .. } => {
                JsonlRetiredRequestOwner::CommandExecPermission
            }
            PendingPermissionRequest::Runtime { .. } | PendingPermissionRequest::Surface { .. } => {
                JsonlRetiredRequestOwner::ThreadPermission
            }
        };
        if self.router.route(&request_id)?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate pending permission request id: {request_id}"),
            ));
        }
        self.router.register(request_id, owner, request)
    }

    fn ensure_open(state: &PendingPermissionState) -> io::Result<()> {
        if state.closed {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "server permission request manager is closed",
            ))
        } else {
            Ok(())
        }
    }
}

pub(super) struct ServerPermissionRequestHandler<W: Write + Send + 'static> {
    writer: Arc<Mutex<W>>,
    pending: PendingPermissionManager,
    event_id: Value,
    thread_id: String,
    turn_id: String,
    generation: GenerationFence,
    cancel: CancelToken,
    runtime_workspace_roots: Vec<PathBuf>,
}

impl<W: Write + Send + 'static> ServerPermissionRequestHandler<W> {
    pub(super) fn new(
        writer: Arc<Mutex<W>>,
        pending: PendingPermissionManager,
        event_id: Value,
        thread_id: String,
        turn_id: String,
        generation: GenerationFence,
        cancel: CancelToken,
        runtime_workspace_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            writer,
            pending,
            event_id,
            thread_id,
            turn_id,
            generation,
            cancel,
            runtime_workspace_roots,
        }
    }
}

impl<W: Write + Send + 'static> RuntimePermissionRequestHandler
    for ServerPermissionRequestHandler<W>
{
    fn request_permissions(
        &self,
        request: &RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        let request_id = super::generation_scoped_id(
            format!("permission-{}-{}", self.turn_id, request.id),
            self.generation,
        );
        let (sender, receiver) = mpsc::channel();
        let request_id = self.pending.insert_runtime(
            request_id.clone(),
            PendingPermissionRequest::Runtime {
                sender,
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                generation: self.generation,
                runtime_workspace_roots: self.runtime_workspace_roots.clone(),
            },
        )?;
        let frame_digest = super::opaque_permission_router::jsonl_response_digest(&json!({
            "id": &self.event_id,
            "event": "permission_request",
            "requestId": &request_id,
            "threadId": &self.thread_id,
            "turnId": &self.turn_id,
            "reason": &request.reason,
            "permissions": &request.permissions,
        }))?;
        self.pending.mark_writing(&request_id, frame_digest)?;
        if let Err(error) = write_locked_event(
            &self.writer,
            &self.event_id,
            ServerEvent::PermissionRequest {
                request_id: json!(request_id.clone()),
                thread_id: json!(self.thread_id),
                turn_id: json!(self.turn_id),
                reason: request
                    .reason
                    .as_ref()
                    .map(|reason| json!(reason))
                    .unwrap_or(Value::Null),
                permissions: serde_json::to_value(&request.permissions).unwrap_or(Value::Null),
            },
        ) {
            let _ = self.pending.retire_unavailable(&request_id);
            return Err(error);
        }
        self.writer.lock().map_err(lock_error)?.flush()?;
        self.pending.mark_published(&request_id, frame_digest)?;
        loop {
            if self.cancel.is_cancelled() {
                let _ = self.pending.retire_unavailable(&request_id);
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "permission request cancelled",
                ));
            }
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(response) => return Ok(response),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("permission response channel closed"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn generation(id: u64) -> GenerationFence {
        GenerationFence::for_test(id)
    }

    #[test]
    fn pending_permission_manager_rejects_duplicate_runtime_request_id_without_overwriting() {
        let manager = PendingPermissionManager::default();
        let (first_sender, _first_receiver) = mpsc::channel();
        let (second_sender, _second_receiver) = mpsc::channel();

        manager
            .insert_runtime(
                "permission-turn-1-ask".to_string(),
                PendingPermissionRequest::Runtime {
                    sender: first_sender,
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    generation: generation(0),
                    runtime_workspace_roots: vec![PathBuf::from("/repo")],
                },
            )
            .expect("insert first request");
        assert!(
            manager
                .insert_runtime(
                    "permission-turn-1-ask".to_string(),
                    PendingPermissionRequest::Runtime {
                        sender: second_sender,
                        thread_id: "thread-2".to_string(),
                        turn_id: "turn-2".to_string(),
                        generation: generation(0),
                        runtime_workspace_roots: vec![PathBuf::from("/other")],
                    },
                )
                .is_err(),
            "duplicate pending request ids must not replace the original waiter"
        );

        let pending = manager
            .route("permission-turn-1-ask")
            .expect("route pending")
            .expect("original request still pending");
        assert_eq!(pending.thread_id(), "thread-1");
        assert_eq!(pending.runtime_workspace_roots(), &[PathBuf::from("/repo")]);
    }

    #[test]
    fn closing_permission_manager_disconnects_waiters_and_rejects_late_requests() {
        let manager = PendingPermissionManager::default();
        let (sender, receiver) = mpsc::channel();
        manager
            .insert_runtime(
                "permission-turn-1-ask".to_string(),
                PendingPermissionRequest::Runtime {
                    sender,
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    generation: generation(0),
                    runtime_workspace_roots: Vec::new(),
                },
            )
            .expect("insert pending request");

        manager.close().expect("close manager");

        assert_eq!(
            receiver.recv_timeout(std::time::Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
        let (late_sender, _late_receiver) = mpsc::channel();
        let error = manager
            .insert_runtime(
                "permission-turn-2-ask".to_string(),
                PendingPermissionRequest::Runtime {
                    sender: late_sender,
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-2".to_string(),
                    generation: generation(0),
                    runtime_workspace_roots: Vec::new(),
                },
            )
            .expect_err("closed manager must reject late requests");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn cancelled_generation_releases_permission_waiter_and_removes_request() {
        let writer = Arc::new(Mutex::new(Vec::new()));
        let manager = PendingPermissionManager::default();
        let cancel = CancelToken::new();
        let handler = ServerPermissionRequestHandler::new(
            Arc::clone(&writer),
            manager.clone(),
            json!("turn"),
            "thread-1".to_string(),
            "turn-1".to_string(),
            generation(1),
            cancel.clone(),
            Vec::new(),
        );
        let worker = std::thread::spawn(move || {
            handler.request_permissions(&RuntimePermissionRequest {
                id: "ask".to_string(),
                reason: Some("need permission".to_string()),
                permissions: Default::default(),
            })
        });

        wait_for_output(&writer);
        cancel.cancel();

        let error = worker
            .join()
            .expect("permission worker")
            .expect_err("cancelled generation must release waiter");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(
            manager
                .route("permission-turn-1-ask-generation-1")
                .expect("route pending")
                .is_none()
        );
    }

    fn wait_for_output(writer: &Arc<Mutex<Vec<u8>>>) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !writer.lock().expect("writer").is_empty() {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for event");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
