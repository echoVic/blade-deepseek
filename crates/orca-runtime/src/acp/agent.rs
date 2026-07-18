//! ACP [`Agent`] implementation projected onto the Orca [`RuntimeHost`].
//!
//! The adapter is intentionally thin: ACP sessions map to runtime threads,
//! ACP prompts map to hosted turns, and runtime [`EventEnvelope`]s are
//! projected to `session/update` notifications via [`event_map`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use agent_client_protocol::{
    Agent, AgentCapabilities, AuthenticateRequest, AuthenticateResponse, CancelNotification,
    ContentBlock, Error, Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    ProtocolVersion, SessionId, SessionNotification, StopReason,
};
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::event_sink::EventObserver;
use tokio::sync::mpsc::UnboundedSender;

use crate::runtime_host::{
    HostedTurnRequest, OperationHandle, OperationOutcome, RuntimeHostHandle, RuntimeThreadHandle,
    RuntimeThreadStartRequest,
};

use super::event_map;

/// Per-session runtime state held on the single-threaded ACP task.
struct SessionEntry {
    thread: RuntimeThreadHandle,
    config: RunConfig,
    current_op: Option<Arc<OperationHandle>>,
    cancel_requested: bool,
}

#[derive(Default)]
struct AgentState {
    sessions: HashMap<SessionId, SessionEntry>,
}

/// Event observer that forwards projected updates onto the notification
/// channel. Runs synchronously on the runtime host thread; `send` is
/// non-blocking, so it never stalls the runtime.
struct AcpEventObserver {
    note_tx: UnboundedSender<SessionNotification>,
    session_id: SessionId,
}

impl EventObserver for AcpEventObserver {
    fn observe(&self, event: &orca_core::event_schema::EventEnvelope) -> io::Result<()> {
        if let Some(update) = event_map::event_to_session_update(event) {
            let _ = self
                .note_tx
                .send(SessionNotification::new(self.session_id.clone(), update));
        }
        Ok(())
    }
}

/// ACP agent backed by the Orca runtime host.
pub struct OrcaAcpAgent {
    host: RuntimeHostHandle,
    base_config: RunConfig,
    note_tx: UnboundedSender<SessionNotification>,
    state: Rc<RefCell<AgentState>>,
}

impl OrcaAcpAgent {
    pub fn new(
        host: RuntimeHostHandle,
        base_config: RunConfig,
        note_tx: UnboundedSender<SessionNotification>,
    ) -> Self {
        Self {
            host,
            base_config,
            note_tx,
            state: Rc::new(RefCell::new(AgentState::default())),
        }
    }

    /// Builds a per-session config from the base config with the session cwd
    /// applied. Events flow through the observer, not the writer, so the
    /// output format is irrelevant.
    fn build_session_config(&self, cwd: PathBuf) -> RunConfig {
        let mut config = self.base_config.clone();
        config.prompt = String::new();
        config.cwd = Some(cwd);
        config.show_session_picker = false;
        config.desktop_notifications = false;
        config.history_mode = HistoryMode::Record;
        config
    }
}

/// Flattens ACP content blocks into a single prompt string. Non-text blocks
/// are skipped (this version only forwards text prompts to the runtime).
fn flatten_prompt(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text(text) = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text.text);
        }
    }
    out
}

/// Resolves the ACP stop reason from a completed operation, honoring an
/// explicit cancellation request.
fn outcome_to_stop_reason(
    outcome: &OperationOutcome,
    cancel_requested: bool,
) -> Result<StopReason, Error> {
    if cancel_requested {
        return Ok(StopReason::Cancelled);
    }
    match outcome {
        OperationOutcome::Completed(status) => Ok(event_map::run_status_to_stop_reason(*status)),
        OperationOutcome::Backgrounded { .. } => Ok(StopReason::EndTurn),
        OperationOutcome::ExecutionFailed { message, .. } => {
            Err(Error::internal_error().data(message.clone()))
        }
        OperationOutcome::Panicked { message } => {
            Err(Error::internal_error().data(message.clone()))
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Agent for OrcaAcpAgent {
    async fn initialize(&self, _args: InitializeRequest) -> Result<InitializeResponse, Error> {
        Ok(InitializeResponse::new(ProtocolVersion::V1)
            .agent_capabilities(AgentCapabilities::new().load_session(true))
            .agent_info(
                Implementation::new("orca", env!("CARGO_PKG_VERSION")).title("Orca".to_string()),
            ))
    }

    async fn authenticate(
        &self,
        _args: AuthenticateRequest,
    ) -> Result<AuthenticateResponse, Error> {
        Ok(AuthenticateResponse::new())
    }

    async fn new_session(&self, args: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        let config = self.build_session_config(args.cwd);
        let session_config = config.clone();
        let host = self.host.clone();
        let thread = tokio::task::spawn_blocking(move || host.start_thread(config, "ACP session"))
            .await
            .map_err(Error::into_internal_error)?
            .map_err(Error::into_internal_error)?;

        let session_id: SessionId = match thread.session_id() {
            Some(id) => SessionId::new(id),
            None => SessionId::new(uuid::Uuid::new_v4().to_string()),
        };

        self.state.borrow_mut().sessions.insert(
            session_id.clone(),
            SessionEntry {
                thread,
                config: session_config,
                current_op: None,
                cancel_requested: false,
            },
        );
        Ok(NewSessionResponse::new(session_id))
    }

    async fn load_session(&self, args: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        let selector = args.session_id.to_string();
        let transcript = tokio::task::spawn_blocking(move || {
            orca_runtime_history_load(&selector)
        })
        .await
        .map_err(Error::into_internal_error)?
        .map_err(Error::into_internal_error)?;

        let config = self.build_session_config(args.cwd);
        let session_config = config.clone();
        let host = self.host.clone();
        let request =
            RuntimeThreadStartRequest::new(config, "ACP session").with_preloaded(transcript);
        let thread =
            tokio::task::spawn_blocking(move || host.start_thread_with_request(request))
                .await
                .map_err(Error::into_internal_error)?
                .map_err(Error::into_internal_error)?;

        self.state.borrow_mut().sessions.insert(
            args.session_id.clone(),
            SessionEntry {
                thread,
                config: session_config,
                current_op: None,
                cancel_requested: false,
            },
        );
        Ok(LoadSessionResponse::new())
    }

    async fn prompt(&self, args: PromptRequest) -> Result<PromptResponse, Error> {
        let (thread, config) = {
            let mut state = self.state.borrow_mut();
            let entry = state
                .sessions
                .get_mut(&args.session_id)
                .ok_or_else(Error::invalid_params)?;
            if entry.current_op.is_some() {
                return Err(Error::invalid_params().data("session already has an active prompt"));
            }
            entry.cancel_requested = false;
            (entry.thread.clone(), entry.config.clone())
        };

        let prompt = flatten_prompt(&args.prompt);
        let observer: Arc<dyn EventObserver> = Arc::new(AcpEventObserver {
            note_tx: self.note_tx.clone(),
            session_id: args.session_id.clone(),
        });

        let request = HostedTurnRequest::new(prompt).with_event_observer(observer);
        let op = tokio::task::spawn_blocking(move || {
            thread.start_turn_with_config(request, io::sink(), config)
        })
        .await
        .map_err(Error::into_internal_error)?
        .map_err(Error::into_internal_error)?;
        let op = Arc::new(op);

        let cancel_requested = {
            let mut state = self.state.borrow_mut();
            let Some(entry) = state.sessions.get_mut(&args.session_id) else {
                return Err(Error::invalid_params());
            };
            entry.current_op = Some(op.clone());
            entry.cancel_requested
        };
        if cancel_requested {
            let _ = op.interrupt();
        }

        let completion = op.completion();
        let terminal = tokio::task::spawn_blocking(move || completion.wait())
            .await
            .map_err(Error::into_internal_error)?;

        let cancel_requested = {
            let mut state = self.state.borrow_mut();
            let entry = state.sessions.get_mut(&args.session_id);
            match entry {
                Some(entry) => {
                    entry.current_op = None;
                    entry.cancel_requested
                }
                None => false,
            }
        };

        let stop_reason = outcome_to_stop_reason(terminal.outcome(), cancel_requested)?;
        Ok(PromptResponse::new(stop_reason))
    }

    async fn cancel(&self, args: CancelNotification) -> Result<(), Error> {
        let op = {
            let mut state = self.state.borrow_mut();
            match state.sessions.get_mut(&args.session_id) {
                Some(entry) => {
                    entry.cancel_requested = true;
                    entry.current_op.clone()
                }
                None => None,
            }
        };
        if let Some(op) = op {
            let _ = op.interrupt();
        }
        Ok(())
    }
}

/// Loads a session transcript by selector, reusing the runtime history layer.
fn orca_runtime_history_load(
    selector: &str,
) -> io::Result<crate::thread_store::SessionTranscript> {
    crate::history::load_session(selector)
}
