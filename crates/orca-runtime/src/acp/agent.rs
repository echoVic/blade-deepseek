//! ACP [`Agent`] implementation projected onto the Orca [`RuntimeHost`].
//!
//! The adapter is intentionally thin: ACP sessions map to runtime threads,
//! ACP prompts map to hosted turns, and runtime [`EventEnvelope`]s are
//! projected to `session/update` notifications via [`event_map`].

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, mpsc as std_mpsc};

use agent_client_protocol::{
    Agent, AgentCapabilities, AuthenticateRequest, AuthenticateResponse, CancelNotification,
    ContentBlock, Error, Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, ProtocolVersion, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome, SessionId,
    SessionNotification, SessionUpdate, StopReason, ToolCallId, ToolCallUpdate,
    ToolCallUpdateFields,
};
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::event_sink::EventObserver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::runtime_host::{
    HostedTurnRequest, OperationHandle, OperationOutcome, RuntimeHostHandle, RuntimeThreadHandle,
    RuntimeThreadStartRequest,
};
use crate::surface::{
    AssistantPatch, AttachResult, DisplayText, FreshAttachRequest, MutationReply, NonEmptyText,
    NonEmptyVec, OperationIngressCorrelation, OperationKind, OperationRequestIntent,
    OperationSettingsPreparation, OperationTerminal, PermissionGrantScope, ReplayabilityRequest,
    RuntimeSurfaceClientHandle, RuntimeSurfaceHandle, RuntimeSurfaceHostHandle, SurfaceAllowDeny,
    SurfaceAttachmentRole, SurfaceCapability, SurfaceClientInteractionAnswer, SurfaceEvent,
    SurfaceInputRequest, SurfaceInputRequestBlock, SurfaceInteractionKind,
    SurfaceInteractionRequest, SurfaceInteractionView, SurfaceOperationId,
    SurfacePermissionClientDecision, SurfacePermissionProfile, SurfaceRequestId,
    SurfaceSubscriptionItem, ToolPatch, WaitOperationTerminalResult,
};

use super::event_map;
use crate::runtime_surface::SurfaceItem;

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
    client_bridge: Option<Arc<AcpClientBridge>>,
}

pub(crate) struct AcpClientBridge {
    request_tx: UnboundedSender<AcpPermissionRequest>,
}

pub(crate) struct AcpPermissionRequest {
    pub request: RequestPermissionRequest,
    pub reply: std_mpsc::SyncSender<Result<RequestPermissionResponse, String>>,
}

impl AcpClientBridge {
    pub(crate) fn new() -> (Arc<Self>, UnboundedReceiver<AcpPermissionRequest>) {
        let (request_tx, request_rx) = unbounded_channel();
        (Arc::new(Self { request_tx }), request_rx)
    }

    fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, String> {
        let (reply_tx, reply_rx) = std_mpsc::sync_channel(1);
        self.request_tx
            .send(AcpPermissionRequest {
                request,
                reply: reply_tx,
            })
            .map_err(|_| "ACP client permission bridge is closed".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "ACP client permission response was dropped")?
    }
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
            client_bridge: None,
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
            client_bridge: None,
        }
    }

    pub(crate) fn with_client_bridge(mut self, bridge: Arc<AcpClientBridge>) -> Self {
        self.client_bridge = Some(bridge);
        self
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
        let prompt = flatten_prompt(&args.prompt)
            .map_err(|message| Error::invalid_params().data(message))?;
        let session_id = args.session_id.clone();
        let client_bridge = self.client_bridge.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_surface_prompt(&surface, &session_id, &prompt, client_bridge)
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
    client_bridge: Option<Arc<AcpClientBridge>>,
}

/// Flattens ACP content blocks into a single prompt string.
///
/// This adapter currently supports text prompts only. Unsupported blocks must
/// be rejected explicitly so a client-provided resource or media block is
/// never silently lost before the runtime operation is reserved.
fn flatten_prompt(blocks: &[ContentBlock]) -> Result<String, String> {
    let mut out = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&text.text);
            }
            _ => {
                return Err(format!(
                    "unsupported ACP prompt content block: {}",
                    content_block_name(block)
                ));
            }
        }
    }
    Ok(out)
}

fn content_block_name(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text(_) => "text",
        ContentBlock::Image(_) => "image",
        ContentBlock::Audio(_) => "audio",
        ContentBlock::ResourceLink(_) => "resource_link",
        ContentBlock::Resource(_) => "resource",
        _ => "unknown",
    }
}

fn replay_surface_snapshot(
    surface: &RuntimeSurfaceHandle,
    session_id: &SessionId,
    note_tx: &UnboundedSender<SessionNotification>,
) -> Result<(), String> {
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Acp,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        AttachResult::Denied { .. } => return Err("ACP history attachment denied".to_string()),
        AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. }
        | AttachResult::ThreadClosed { .. }
        | AttachResult::Unavailable { .. } => {
            return Err("ACP history snapshot unavailable".to_string());
        }
    };
    for item in attachment.baseline.snapshot.items.iter() {
        let update = match item {
            SurfaceItem::AssistantMessage { text, .. } => Some(SessionUpdate::AgentMessageChunk(
                agent_client_protocol::ContentChunk::new(ContentBlock::from(
                    text.as_str().to_string(),
                )),
            )),
            SurfaceItem::AssistantReasoning { content, .. } => Some(
                SessionUpdate::AgentThoughtChunk(agent_client_protocol::ContentChunk::new(
                    ContentBlock::from(content.as_str().to_string()),
                )),
            ),
            SurfaceItem::UserMessage { .. }
            | SurfaceItem::SystemMessage { .. }
            | SurfaceItem::AssistantPlan { .. }
            | SurfaceItem::ToolResultMessage { .. } => None,
        };
        if let Some(update) = update {
            let _ = note_tx.send(SessionNotification::new(session_id.clone(), update));
        }
    }
    let _ = surface.detach(
        &attachment.client,
        crate::surface::DetachRequest {
            request_id: SurfaceRequestId::new(),
        },
    );
    Ok(())
}

fn prepare_surface_prompt(
    surface: &RuntimeSurfaceHandle,
    session_id: &SessionId,
    prompt: &str,
    client_bridge: Option<Arc<AcpClientBridge>>,
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
        client_bridge,
    })
}

#[derive(Clone)]
enum AcpPermissionTarget {
    ToolApproval,
    PermissionRequest {
        permissions: SurfacePermissionProfile,
    },
}

fn build_permission_request(
    session_id: &SessionId,
    interaction: &SurfaceInteractionView,
) -> Result<(RequestPermissionRequest, AcpPermissionTarget), String> {
    let (tool_call_id, title, target) = match &interaction.request {
        SurfaceInteractionRequest::ToolApproval {
            tool, description, ..
        } => (
            tool.tool_call_id.as_str().to_string(),
            description.as_str().to_string(),
            AcpPermissionTarget::ToolApproval,
        ),
        SurfaceInteractionRequest::PermissionRequest {
            tool_call_id,
            permissions,
            ..
        } => (
            tool_call_id.as_str().to_string(),
            "Permission requested".to_string(),
            AcpPermissionTarget::PermissionRequest {
                permissions: permissions.clone(),
            },
        ),
        SurfaceInteractionRequest::UserInput { .. }
        | SurfaceInteractionRequest::McpElicitation { .. }
        | SurfaceInteractionRequest::BackgroundApproval { .. } => {
            return Err("ACP client bridge does not support this interaction kind".to_string());
        }
    };
    let fields = ToolCallUpdateFields::new().title(title);
    let tool_call = ToolCallUpdate::new(ToolCallId::new(tool_call_id), fields);
    let options = vec![
        PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            "allow_always",
            "Allow for session",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            "reject_once",
            "Reject once",
            PermissionOptionKind::RejectOnce,
        ),
        PermissionOption::new(
            "reject_always",
            "Reject for session",
            PermissionOptionKind::RejectAlways,
        ),
    ];
    Ok((
        RequestPermissionRequest::new(session_id.clone(), tool_call, options),
        target,
    ))
}

fn permission_answer(
    response: RequestPermissionResponse,
    target: AcpPermissionTarget,
) -> Result<SurfaceClientInteractionAnswer, String> {
    let (allow, scope) = match response.outcome {
        RequestPermissionOutcome::Cancelled => (false, PermissionGrantScope::Turn),
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. }) => {
            match option_id.to_string().as_str() {
                "allow_once" => (true, PermissionGrantScope::Turn),
                "allow_always" => (true, PermissionGrantScope::Session),
                "reject_once" => (false, PermissionGrantScope::Turn),
                "reject_always" => (false, PermissionGrantScope::Session),
                other => return Err(format!("unknown ACP permission option '{other}'")),
            }
        }
        _ => return Err("unsupported ACP permission outcome".to_string()),
    };
    Ok(match target {
        AcpPermissionTarget::ToolApproval => SurfaceClientInteractionAnswer::ToolApproval {
            decision: if allow {
                SurfaceAllowDeny::Allow
            } else {
                SurfaceAllowDeny::Deny
            },
        },
        AcpPermissionTarget::PermissionRequest { permissions } => {
            SurfaceClientInteractionAnswer::PermissionRequest {
                decision: if allow {
                    SurfacePermissionClientDecision::Allow {
                        scope,
                        permissions,
                        strict_auto_review: false,
                    }
                } else {
                    SurfacePermissionClientDecision::Deny {
                        scope,
                        permissions,
                        strict_auto_review: false,
                    }
                },
            }
        }
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
                        project_surface_event(
                            &mut prepared,
                            &session_id,
                            &note_tx,
                            &envelope.event,
                        )?;
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
                                project_surface_event(
                                    &mut prepared,
                                    &session_id,
                                    &note_tx,
                                    &envelope.event,
                                )?;
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

fn project_surface_event(
    prepared: &mut PreparedSurfacePrompt,
    session_id: &SessionId,
    note_tx: &UnboundedSender<SessionNotification>,
    event: &SurfaceEvent,
) -> Result<(), String> {
    if let SurfaceEvent::Interaction(crate::surface::InteractionPatch::Requested { interaction }) =
        event
    {
        let Some(bridge) = prepared.client_bridge.as_ref() else {
            let _ = prepared
                .client
                .cancel_operation(SurfaceRequestId::new(), prepared.operation_id.clone());
            return Err("ACP interaction requires a connected client bridge".to_string());
        };
        let (request, target) = build_permission_request(session_id, interaction)?;
        let response = bridge.request_permission(request)?;
        let answer = permission_answer(response, target)?;
        match prepared.client.respond_interaction_by_id(
            SurfaceRequestId::new(),
            interaction.interaction_id.clone(),
            answer,
        ) {
            Ok(MutationReply::Committed { .. }) => {}
            Ok(MutationReply::Deferred { .. }) => {
                return Err("ACP interaction response was deferred".to_string());
            }
            Ok(MutationReply::Uncommitted { .. }) => {
                return Err("ACP interaction response did not commit".to_string());
            }
            Err(error) => return Err(format!("ACP interaction response failed: {error:?}")),
        }
    }
    emit_surface_event(session_id, note_tx, event);
    Ok(())
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

        let mut config = self.build_session_config(args.cwd);
        config.history_mode = HistoryMode::Resume(args.session_id.to_string());
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

        if let Some(surface) = surface.as_ref() {
            replay_surface_snapshot(surface, &args.session_id, &self.note_tx)
                .map_err(|message| Error::internal_error().data(message))?;
        }

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

        let prompt = flatten_prompt(&args.prompt)
            .map_err(|message| Error::invalid_params().data(message))?;
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
