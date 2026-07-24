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
use std::sync::{Arc, mpsc as std_mpsc};

use agent_client_protocol::{
    Agent, AgentCapabilities, AuthenticateRequest, AuthenticateResponse, CancelNotification,
    ContentBlock, Error, Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    ProtocolVersion, SessionId, SessionNotification, SessionUpdate, StopReason,
};
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::event_sink::EventObserver;
use tokio::sync::mpsc::UnboundedSender;

use crate::runtime_host::{
    HostedTurnRequest, OperationHandle, OperationOutcome, RuntimeHostHandle, RuntimeThreadHandle,
    RuntimeThreadStartRequest,
};
use crate::surface::{
    AssistantPatch, AttachResult, DisplayText, FreshAttachRequest, MutationReply, NonEmptyText,
    NonEmptyVec, OperationIngressCorrelation, OperationKind, OperationRequestIntent,
    OperationSettingsPreparation, OperationTerminal, ReplayabilityRequest,
    RuntimeSurfaceClientHandle, RuntimeSurfaceHandle, RuntimeSurfaceHostHandle,
    SurfaceAttachmentRole, SurfaceCapability, SurfaceEvent, SurfaceInputRequest,
    SurfaceInputRequestBlock, SurfaceInteractionKind, SurfaceOperationId, SurfaceRequestId,
    SurfaceSubscriptionItem, ToolPatch, WaitOperationTerminalResult,
};

use super::event_map;

/// Per-session runtime state held on the single-threaded ACP task.
struct SessionEntry {
    thread: RuntimeThreadHandle,
    surface: Option<RuntimeSurfaceHandle>,
    config: RunConfig,
    current_op: Option<Arc<OperationHandle>>,
    current_surface_op: Option<(RuntimeSurfaceClientHandle, SurfaceOperationId)>,
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
    host: Option<RuntimeHostHandle>,
    surface_host: Option<RuntimeSurfaceHostHandle>,
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
            host: Some(host),
            surface_host: None,
            base_config,
            note_tx,
            state: Rc::new(RefCell::new(AgentState::default())),
        }
    }

    pub fn new_typed(
        host: RuntimeSurfaceHostHandle,
        base_config: RunConfig,
        note_tx: UnboundedSender<SessionNotification>,
    ) -> Self {
        Self {
            host: None,
            surface_host: Some(host),
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

    async fn prompt_typed(
        &self,
        args: PromptRequest,
        surface: RuntimeSurfaceHandle,
    ) -> Result<PromptResponse, Error> {
        let prompt = flatten_prompt(&args.prompt);
        let session_id = args.session_id.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_surface_prompt(&surface, &session_id, &prompt)
        })
        .await
        .map_err(Error::into_internal_error)?
        .map_err(|message| Error::internal_error().data(message))?;

        let cancel_requested = {
            let mut state = self.state.borrow_mut();
            let entry = state
                .sessions
                .get_mut(&args.session_id)
                .ok_or_else(Error::invalid_params)?;
            entry.current_surface_op =
                Some((prepared.client.clone(), prepared.operation_id.clone()));
            entry.cancel_requested
        };
        if cancel_requested {
            let _ = prepared
                .client
                .cancel_operation(SurfaceRequestId::new(), prepared.operation_id.clone());
        }

        let note_tx = self.note_tx.clone();
        let session_id = args.session_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            drain_surface_prompt(prepared, session_id, note_tx)
        })
        .await;

        if let Some(entry) = self.state.borrow_mut().sessions.get_mut(&args.session_id) {
            entry.current_surface_op = None;
        }
        let result = result
            .map_err(Error::into_internal_error)?
            .map_err(|message| Error::internal_error().data(message))?;
        Ok(PromptResponse::new(result))
    }
}

struct PreparedSurfacePrompt {
    surface: RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
    operation_id: SurfaceOperationId,
    subscription: crate::surface::SurfaceSubscriptionReceiver,
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

fn prepare_surface_prompt(
    surface: &RuntimeSurfaceHandle,
    session_id: &SessionId,
    prompt: &str,
) -> Result<PreparedSurfacePrompt, String> {
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Acp,
        requested_capabilities: std::collections::BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
            SurfaceCapability::RespondGrantedInteraction,
        ]),
        interaction_capabilities: std::collections::BTreeSet::from([
            SurfaceInteractionKind::ToolApproval,
            SurfaceInteractionKind::PermissionRequest,
            SurfaceInteractionKind::UserInput,
            SurfaceInteractionKind::McpElicitation,
        ]),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        AttachResult::Denied { .. } => return Err("ACP surface attachment denied".to_string()),
        AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. }
        | AttachResult::ThreadClosed { .. }
        | AttachResult::Unavailable { .. } => {
            return Err("ACP surface attachment unavailable".to_string());
        }
    };
    let subscription = surface
        .claim_subscription(&attachment.subscription)
        .ok_or_else(|| "ACP surface subscription unavailable".to_string())?;
    let session_id = NonEmptyText::try_new(session_id.to_string())
        .map_err(|error| format!("invalid ACP session id: {error}"))?;
    let input = NonEmptyVec::try_new(vec![SurfaceInputRequestBlock::Text {
        text: DisplayText::new(prompt),
    }])
    .map_err(|error| format!("invalid ACP prompt: {error}"))?;
    let intent = OperationRequestIntent {
        correlation: OperationIngressCorrelation::AcpPrompt {
            session_id,
            inbound_seq: crate::surface::SequenceNumber::new(1),
            rpc_request_id: crate::surface::AcpRequestId::String(
                NonEmptyText::try_new("prompt").expect("static ACP request id is non-empty"),
            ),
        },
        kind: OperationKind::UserTurn,
        input: Some(SurfaceInputRequest { blocks: input }),
        replayability: ReplayabilityRequest::CaptureReplayableCapsule,
        settings_preparation: OperationSettingsPreparation::UseCurrent {
            expected_settings_revision: attachment.baseline.snapshot.settings.thread_revision,
            expected_policy_epoch: attachment.baseline.snapshot.settings.effective.policy_epoch,
        },
    };
    let reserved = match attachment
        .client
        .reserve_operation(SurfaceRequestId::new(), intent)
        .map_err(|error| format!("ACP surface reserve failed: {error:?}"))?
    {
        MutationReply::Committed { value, .. } => value,
        MutationReply::Deferred { .. } | MutationReply::Uncommitted { .. } => {
            return Err("ACP surface reserve did not commit".to_string());
        }
    };
    let operation_id = reserved.operation_id.clone();
    match attachment
        .client
        .admit_reserved(
            SurfaceRequestId::new(),
            operation_id.clone(),
            reserved.lease.lease_id,
        )
        .map_err(|error| format!("ACP surface admission failed: {error:?}"))?
    {
        MutationReply::Committed { .. } => {}
        MutationReply::Deferred { .. } | MutationReply::Uncommitted { .. } => {
            return Err("ACP surface admission did not commit".to_string());
        }
    }
    Ok(PreparedSurfacePrompt {
        surface: surface.clone(),
        client: attachment.client,
        operation_id,
        subscription,
    })
}

fn drain_surface_prompt(
    mut prepared: PreparedSurfacePrompt,
    session_id: SessionId,
    note_tx: UnboundedSender<SessionNotification>,
) -> Result<StopReason, String> {
    let (wait_tx, wait_rx) = std_mpsc::sync_channel(1);
    let waiter_client = prepared.client.clone();
    let waiter_operation_id = prepared.operation_id.clone();
    std::thread::spawn(move || {
        let result =
            waiter_client.wait_operation_terminal(SurfaceRequestId::new(), waiter_operation_id);
        let _ = wait_tx.send(result);
    });

    let mut last_cursor = None;
    let terminal = loop {
        while let Some(item) = prepared.subscription.try_recv() {
            match item {
                SurfaceSubscriptionItem::Batch { batch } => {
                    last_cursor = Some(batch.cursor_after.clone());
                    for envelope in batch.events.as_slice() {
                        emit_surface_event(&session_id, &note_tx, &envelope.event);
                    }
                }
                SurfaceSubscriptionItem::Gap { .. } => {
                    return Err("ACP surface subscription requires snapshot reload".to_string());
                }
                SurfaceSubscriptionItem::Sealed { .. } => {
                    return Err("ACP surface subscription sealed before terminal".to_string());
                }
            }
        }
        match wait_rx.try_recv() {
            Ok(result) => break result,
            Err(std_mpsc::TryRecvError::Empty) => {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                return Err("ACP surface terminal waiter disconnected".to_string());
            }
        }
    };
    let terminal = terminal.map_err(|error| format!("ACP surface wait failed: {error:?}"))?;
    let terminal_cursor = match &terminal {
        WaitOperationTerminalResult::Terminal { value } => Some(value.cursor.clone()),
        _ => None,
    };
    if let Some(terminal_cursor) = terminal_cursor {
        if last_cursor.as_ref() != Some(&terminal_cursor) {
            loop {
                let mut reached_terminal_cursor = false;
                while let Some(item) = prepared.subscription.try_recv() {
                    match item {
                        SurfaceSubscriptionItem::Batch { batch } => {
                            reached_terminal_cursor = batch.cursor_after == terminal_cursor;
                            for envelope in batch.events.as_slice() {
                                emit_surface_event(&session_id, &note_tx, &envelope.event);
                            }
                        }
                        SurfaceSubscriptionItem::Gap { .. } => {
                            return Err(
                                "ACP surface subscription requires snapshot reload".to_string()
                            );
                        }
                        SurfaceSubscriptionItem::Sealed { .. } => {
                            return Err(
                                "ACP surface subscription sealed before terminal".to_string()
                            );
                        }
                    }
                    if reached_terminal_cursor {
                        break;
                    }
                }
                if reached_terminal_cursor {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
    let _ = prepared.surface.detach(
        &prepared.client,
        crate::surface::DetachRequest {
            request_id: SurfaceRequestId::new(),
        },
    );
    match terminal {
        WaitOperationTerminalResult::Terminal { value } => terminal_to_stop_reason(&value.terminal),
        WaitOperationTerminalResult::TerminalCommitFailure { .. }
        | WaitOperationTerminalResult::TerminalProjectionFailure { .. } => {
            Err("ACP surface terminal requires runtime recovery".to_string())
        }
        WaitOperationTerminalResult::UnknownOperation { .. }
        | WaitOperationTerminalResult::WrongThread { .. }
        | WaitOperationTerminalResult::WaitCancelled { .. } => {
            Err("ACP surface operation became unavailable".to_string())
        }
    }
}

fn terminal_to_stop_reason(terminal: &OperationTerminal) -> Result<StopReason, String> {
    match terminal {
        OperationTerminal::Succeeded { .. } => Ok(StopReason::EndTurn),
        OperationTerminal::Cancelled { .. } => Ok(StopReason::Cancelled),
        OperationTerminal::BudgetExhausted { .. } => Ok(StopReason::MaxTokens),
        OperationTerminal::NotAdmitted { reason } => {
            Err(format!("ACP operation was not admitted: {reason:?}"))
        }
        OperationTerminal::Failed { message, .. }
        | OperationTerminal::Panicked { message }
        | OperationTerminal::JoinFailed { message } => Err(message.as_str().to_string()),
        OperationTerminal::AbortedByRuntimeRestart { .. } => {
            Err("ACP operation aborted by runtime restart".to_string())
        }
        OperationTerminal::Shutdown { reason } => {
            Err(format!("ACP operation shut down: {reason:?}"))
        }
    }
}

fn emit_surface_event(
    session_id: &SessionId,
    note_tx: &UnboundedSender<SessionNotification>,
    event: &SurfaceEvent,
) {
    let update = match event {
        SurfaceEvent::Assistant(AssistantPatch::Delta { text, .. }) => Some(
            SessionUpdate::AgentMessageChunk(agent_client_protocol::ContentChunk::new(
                ContentBlock::from(text.as_str().to_string()),
            )),
        ),
        SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted { response }) => {
            response.message_item.as_ref().map(|item| {
                SessionUpdate::AgentMessageChunk(agent_client_protocol::ContentChunk::new(
                    ContentBlock::from(item.text.as_str().to_string()),
                ))
            })
        }
        SurfaceEvent::Tool(ToolPatch::OutputDelta { chunk, .. }) => Some(
            SessionUpdate::AgentMessageChunk(agent_client_protocol::ContentChunk::new(
                ContentBlock::from(chunk.as_str().to_string()),
            )),
        ),
        _ => None,
    };
    if let Some(update) = update {
        let _ = note_tx.send(SessionNotification::new(session_id.clone(), update));
    }
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
                Implementation::new("orca", self.base_config.app_version.clone())
                    .title("Orca".to_string()),
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
        let (thread, surface) = if let Some(surface_host) = self.surface_host.clone() {
            let thread = tokio::task::spawn_blocking(move || {
                surface_host.start_thread(config, "ACP session")
            })
            .await
            .map_err(Error::into_internal_error)?
            .map_err(Error::into_internal_error)?;
            let surface = thread
                .acp_surface()
                .ok_or_else(|| Error::internal_error().data("ACP surface unavailable"))?;
            (thread.legacy(), Some(surface))
        } else {
            let host = self
                .host
                .clone()
                .ok_or_else(|| Error::internal_error().data("legacy ACP host unavailable"))?;
            let thread =
                tokio::task::spawn_blocking(move || host.start_thread(config, "ACP session"))
                    .await
                    .map_err(Error::into_internal_error)?
                    .map_err(Error::into_internal_error)?;
            (thread, None)
        };

        let session_id: SessionId = match thread.session_id() {
            Some(id) => SessionId::new(id),
            None => SessionId::new(uuid::Uuid::new_v4().to_string()),
        };

        self.state.borrow_mut().sessions.insert(
            session_id.clone(),
            SessionEntry {
                thread,
                surface,
                config: session_config,
                current_op: None,
                current_surface_op: None,
                cancel_requested: false,
            },
        );
        Ok(NewSessionResponse::new(session_id))
    }

    async fn load_session(&self, args: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        let selector = args.session_id.to_string();
        let transcript = tokio::task::spawn_blocking(move || orca_runtime_history_load(&selector))
            .await
            .map_err(Error::into_internal_error)?
            .map_err(Error::into_internal_error)?;

        let config = self.build_session_config(args.cwd);
        let session_config = config.clone();
        let request =
            RuntimeThreadStartRequest::new(config, "ACP session").with_preloaded(transcript);
        let (thread, surface) = if let Some(surface_host) = self.surface_host.clone() {
            let thread = tokio::task::spawn_blocking(move || {
                surface_host.start_thread_with_request(request)
            })
            .await
            .map_err(Error::into_internal_error)?
            .map_err(Error::into_internal_error)?;
            let surface = thread
                .acp_surface()
                .ok_or_else(|| Error::internal_error().data("ACP surface unavailable"))?;
            (thread.legacy(), Some(surface))
        } else {
            let host = self
                .host
                .clone()
                .ok_or_else(|| Error::internal_error().data("legacy ACP host unavailable"))?;
            let thread =
                tokio::task::spawn_blocking(move || host.start_thread_with_request(request))
                    .await
                    .map_err(Error::into_internal_error)?
                    .map_err(Error::into_internal_error)?;
            (thread, None)
        };

        self.state.borrow_mut().sessions.insert(
            args.session_id.clone(),
            SessionEntry {
                thread,
                surface,
                config: session_config,
                current_op: None,
                current_surface_op: None,
                cancel_requested: false,
            },
        );
        Ok(LoadSessionResponse::new())
    }

    async fn prompt(&self, args: PromptRequest) -> Result<PromptResponse, Error> {
        let (thread, surface, config) = {
            let mut state = self.state.borrow_mut();
            let entry = state
                .sessions
                .get_mut(&args.session_id)
                .ok_or_else(Error::invalid_params)?;
            if entry.current_op.is_some() || entry.current_surface_op.is_some() {
                return Err(Error::invalid_params().data("session already has an active prompt"));
            }
            entry.cancel_requested = false;
            (
                entry.thread.clone(),
                entry.surface.clone(),
                entry.config.clone(),
            )
        };

        if let Some(surface) = surface {
            return self.prompt_typed(args, surface).await;
        }

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
        let (op, surface_op) = {
            let mut state = self.state.borrow_mut();
            match state.sessions.get_mut(&args.session_id) {
                Some(entry) => {
                    entry.cancel_requested = true;
                    (entry.current_op.clone(), entry.current_surface_op.clone())
                }
                None => (None, None),
            }
        };
        if let Some(op) = op {
            let _ = op.interrupt();
        }
        if let Some((client, operation_id)) = surface_op {
            let _ = client.cancel_operation(SurfaceRequestId::new(), operation_id);
        }
        Ok(())
    }
}

/// Loads a session transcript by selector, reusing the runtime history layer.
fn orca_runtime_history_load(selector: &str) -> io::Result<crate::thread_store::SessionTranscript> {
    crate::history::load_session(selector)
}
