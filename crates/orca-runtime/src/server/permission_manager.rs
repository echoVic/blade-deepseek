use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use serde_json::{Value, json};

use crate::lifecycle::{
    RuntimePermissionRequest, RuntimePermissionRequestHandler, RuntimePermissionResponse,
};
use crate::protocol::{self, ServerEvent};

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

pub(super) enum PendingPermissionRequest {
    Runtime {
        sender: mpsc::Sender<RuntimePermissionResponse>,
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
            Self::Runtime { thread_id, .. } => thread_id,
            Self::CommandExec { request } => &request.thread_id,
        }
    }

    pub(super) fn runtime_workspace_roots(&self) -> &[PathBuf] {
        match self {
            Self::Runtime {
                runtime_workspace_roots,
                ..
            } => runtime_workspace_roots,
            Self::CommandExec { request } => &request.runtime_workspace_roots,
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct PendingPermissionManager {
    pending: Arc<Mutex<HashMap<String, PendingPermissionRequest>>>,
}

impl PendingPermissionManager {
    pub(super) fn insert_command_exec(
        &self,
        request_id: String,
        request: PendingCommandExecPermissionRequest,
    ) -> io::Result<()> {
        let mut pending = self.pending.lock().map_err(lock_error)?;
        if pending.contains_key(&request_id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate pending permission request id: {request_id}"),
            ));
        }
        pending.insert(
            request_id,
            PendingPermissionRequest::CommandExec {
                request: Box::new(request),
            },
        );
        Ok(())
    }

    pub(super) fn remove(&self, request_id: &str) -> io::Result<Option<PendingPermissionRequest>> {
        let mut pending = self.pending.lock().map_err(lock_error)?;
        Ok(pending.remove(request_id))
    }

    fn insert_runtime(
        &self,
        request_id: String,
        request: PendingPermissionRequest,
    ) -> io::Result<()> {
        let mut pending = self.pending.lock().map_err(lock_error)?;
        if pending.contains_key(&request_id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate pending permission request id: {request_id}"),
            ));
        }
        pending.insert(request_id, request);
        Ok(())
    }
}

pub(super) struct ServerPermissionRequestHandler<W: Write + Send + 'static> {
    writer: Arc<Mutex<W>>,
    pending: PendingPermissionManager,
    event_id: Value,
    thread_id: String,
    turn_id: String,
    runtime_workspace_roots: Vec<PathBuf>,
}

impl<W: Write + Send + 'static> ServerPermissionRequestHandler<W> {
    pub(super) fn new(
        writer: Arc<Mutex<W>>,
        pending: PendingPermissionManager,
        event_id: Value,
        thread_id: String,
        turn_id: String,
        runtime_workspace_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            writer,
            pending,
            event_id,
            thread_id,
            turn_id,
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
        let request_id = format!("permission-{}-{}", self.turn_id, request.id);
        let (sender, receiver) = mpsc::channel();
        self.pending.insert_runtime(
            request_id.clone(),
            PendingPermissionRequest::Runtime {
                sender,
                thread_id: self.thread_id.clone(),
                runtime_workspace_roots: self.runtime_workspace_roots.clone(),
            },
        )?;
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
            let _ = self.pending.remove(&request_id);
            return Err(error);
        }
        receiver
            .recv()
            .map_err(|_| io::Error::other("permission response channel closed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                        runtime_workspace_roots: vec![PathBuf::from("/other")],
                    },
                )
                .is_err(),
            "duplicate pending request ids must not replace the original waiter"
        );

        let pending = manager
            .remove("permission-turn-1-ask")
            .expect("remove pending")
            .expect("original request still pending");
        assert_eq!(pending.thread_id(), "thread-1");
        assert_eq!(pending.runtime_workspace_roots(), &[PathBuf::from("/repo")]);
    }
}
