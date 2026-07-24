//! ACP [`Agent`] implementation projected onto the Orca [`RuntimeHost`].
//!
//! The adapter is intentionally thin: ACP sessions map to runtime threads,
//! ACP prompts map to hosted turns, and runtime [`EventEnvelope`]s are
//! projected to `session/update` notifications via [`event_map`].

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};

use agent_client_protocol::{
    Agent, AgentCapabilities, AuthenticateRequest, AuthenticateResponse, CancelNotification,
    ContentBlock, Error, Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, PromptRequest,
    PromptResponse, ProtocolVersion, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionId, SessionNotification,
    SessionUpdate, StopReason, ToolCall, ToolCallContent, ToolCallId, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::event_sink::EventObserver;
use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::runtime_host::{
    HostedTurnRequest, OperationHandle, OperationOutcome, RuntimeHostHandle, RuntimeThreadHandle,
    RuntimeThreadStartRequest,
};
use crate::surface::{
    AcpRequestId, AssistantPatch, AttachResult, DisplayText, FreshAttachRequest, MutationReply,
    NonEmptyText, NonEmptyVec, OperationIngressCorrelation, OperationKind, OperationRequestIntent,
    OperationSettingsPreparation, OperationTerminal, PermissionGrantScope, ReplayabilityRequest,
    RuntimeSurfaceClientHandle, RuntimeSurfaceHandle, RuntimeSurfaceHostHandle, SequenceNumber,
    SurfaceAllowDeny, SurfaceAttachmentRole, SurfaceCapability, SurfaceClientInteractionAnswer,
    SurfaceEvent, SurfaceInputRequest, SurfaceInputRequestBlock, SurfaceInteractionKind,
    SurfaceInteractionRequest, SurfaceInteractionView, SurfaceOperationId,
    SurfacePermissionClientDecision, SurfacePermissionProfile, SurfaceRequestId,
    SurfaceSubscriptionItem, SurfaceToolResultKind, ToolPatch, WaitOperationTerminalResult,
};

use super::event_map;
use crate::runtime_surface::{
    SurfaceItem, SurfacePlanPriority, SurfacePlanStatus, SurfaceToolAction, SurfaceToolViewState,
};

#[derive(Clone)]
pub(crate) enum AcpNotificationSender {
    Unbounded(UnboundedSender<SessionNotification>),
    Bounded(Sender<SessionNotification>),
}

impl AcpNotificationSender {
    fn send(&self, notification: SessionNotification) -> Result<(), ()> {
        match self {
            Self::Unbounded(sender) => sender.send(notification).map_err(|_| ()),
            Self::Bounded(sender) => sender.blocking_send(notification).map_err(|_| ()),
        }
    }
}

/// Per-session runtime state held on the single-threaded ACP task.
struct SessionEntry {
    thread: RuntimeThreadHandle,
    surface: Option<RuntimeSurfaceHandle>,
    config: RunConfig,
    current_op: Option<Arc<OperationHandle>>,
    current_surface_op: Option<(RuntimeSurfaceClientHandle, SurfaceOperationId)>,
    cancel_requested: bool,
    next_prompt_seq: u64,
}

#[derive(Default)]
struct AgentState {
    sessions: HashMap<SessionId, SessionEntry>,
}

/// Event observer that forwards projected updates onto the notification
/// channel. Runs synchronously on the runtime host thread; `send` is
/// non-blocking, so it never stalls the runtime.
struct AcpEventObserver {
    note_tx: AcpNotificationSender,
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
    note_tx: AcpNotificationSender,
    state: Rc<RefCell<AgentState>>,
    client_bridge: Option<Arc<AcpClientBridge>>,
}

pub(crate) struct AcpClientBridge {
    request_tx: UnboundedSender<AcpPermissionRequest>,
    state: Mutex<AcpClientBridgeState>,
    next_key: AtomicU64,
}

struct AcpClientBridgeState {
    pending: HashMap<
        String,
        std_mpsc::SyncSender<Result<RequestPermissionResponse, AcpPermissionWaitError>>,
    >,
    cancelled_sessions: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AcpPermissionWaitError {
    Cancelled,
    BridgeClosed,
    ResponseDropped,
    Client(String),
}

pub(crate) struct AcpPermissionRequest {
    pub request: RequestPermissionRequest,
    pub key: String,
}

impl AcpClientBridge {
    pub(crate) fn new() -> (Arc<Self>, UnboundedReceiver<AcpPermissionRequest>) {
        let (request_tx, request_rx) = unbounded_channel();
        (
            Arc::new(Self {
                request_tx,
                state: Mutex::new(AcpClientBridgeState {
                    pending: HashMap::new(),
                    cancelled_sessions: HashSet::new(),
                }),
                next_key: AtomicU64::new(1),
            }),
            request_rx,
        )
    }

    fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, AcpPermissionWaitError> {
        let key = format!(
            "{}\0{}",
            request.session_id,
            self.next_key.fetch_add(1, Ordering::Relaxed)
        );
        let (reply_tx, reply_rx) = std_mpsc::sync_channel(1);
        let session_id = request.session_id.to_string();
        let mut state = self
            .state
            .lock()
            .expect("ACP permission bridge mutex is not poisoned");
        if state.cancelled_sessions.contains(&session_id) {
            return Err(AcpPermissionWaitError::Cancelled);
        }
        state.pending.insert(key.clone(), reply_tx);
        drop(state);
        self.request_tx
            .send(AcpPermissionRequest {
                request,
                key: key.clone(),
            })
            .map_err(|_| {
                self.state
                    .lock()
                    .expect("ACP permission bridge mutex is not poisoned")
                    .pending
                    .remove(&key);
                AcpPermissionWaitError::BridgeClosed
            })?;
        let result = reply_rx
            .recv()
            .map_err(|_| AcpPermissionWaitError::ResponseDropped)?;
        self.state
            .lock()
            .expect("ACP permission bridge mutex is not poisoned")
            .pending
            .remove(&key);
        result
    }

    pub(crate) fn begin_session(&self, session_id: &SessionId) {
        self.state
            .lock()
            .expect("ACP permission bridge mutex is not poisoned")
            .cancelled_sessions
            .remove(&session_id.to_string());
    }

    pub(crate) fn cancel_session(&self, session_id: &SessionId) {
        let prefix = format!("{}\0", session_id);
        let pending = {
            let mut pending = self
                .state
                .lock()
                .expect("ACP permission bridge mutex is not poisoned");
            pending.cancelled_sessions.insert(session_id.to_string());
            let keys = pending
                .pending
                .keys()
                .filter(|key| key.starts_with(&prefix))
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| pending.pending.remove(&key))
                .collect::<Vec<_>>()
        };
        for reply in pending {
            let _ = reply.send(Err(AcpPermissionWaitError::Cancelled));
        }
    }

    pub(crate) fn complete_permission(
        &self,
        key: &str,
        result: Result<RequestPermissionResponse, AcpPermissionWaitError>,
    ) {
        let reply = self
            .state
            .lock()
            .expect("ACP permission bridge mutex is not poisoned")
            .pending
            .remove(key);
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
    }

    pub(crate) fn is_pending(&self, key: &str) -> bool {
        self.state
            .lock()
            .expect("ACP permission bridge mutex is not poisoned")
            .pending
            .contains_key(key)
    }

    pub(crate) fn cancel_all(&self) {
        let pending = {
            let mut state = self
                .state
                .lock()
                .expect("ACP permission bridge mutex is not poisoned");
            state
                .pending
                .drain()
                .map(|(_, reply)| reply)
                .collect::<Vec<_>>()
        };
        for reply in pending {
            let _ = reply.send(Err(AcpPermissionWaitError::Cancelled));
        }
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
            note_tx: AcpNotificationSender::Unbounded(note_tx),
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
            note_tx: AcpNotificationSender::Unbounded(note_tx),
            state: Rc::new(RefCell::new(AgentState::default())),
            client_bridge: None,
        }
    }

    pub(crate) fn new_typed_bounded(
        host: RuntimeSurfaceHostHandle,
        base_config: RunConfig,
        note_tx: Sender<SessionNotification>,
    ) -> Self {
        Self {
            host: None,
            surface_host: Some(host),
            base_config,
            note_tx: AcpNotificationSender::Bounded(note_tx),
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
        if let Some(bridge) = self.client_bridge.as_ref() {
            bridge.begin_session(&args.session_id);
        }
        let prompt = flatten_prompt(&args.prompt)
            .map_err(|message| Error::invalid_params().data(message))?;
        let session_id = args.session_id.clone();
        let client_bridge = self.client_bridge.clone();
        let inbound_seq = {
            let mut state = self.state.borrow_mut();
            let entry = state
                .sessions
                .get_mut(&args.session_id)
                .ok_or_else(Error::invalid_params)?;
            let sequence = entry.next_prompt_seq;
            entry.next_prompt_seq = entry.next_prompt_seq.saturating_add(1);
            sequence
        };
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_surface_prompt(&surface, &session_id, &prompt, inbound_seq, client_bridge)
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
        let cancelled = self
            .state
            .borrow()
            .sessions
            .get(&args.session_id)
            .is_some_and(|entry| entry.cancel_requested);
        Ok(PromptResponse::new(if cancelled {
            StopReason::Cancelled
        } else {
            result
        }))
    }
}

struct PreparedSurfacePrompt {
    surface: RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
    operation_id: SurfaceOperationId,
    subscription: crate::surface::SurfaceSubscriptionReceiver,
    client_bridge: Option<Arc<AcpClientBridge>>,
    tool_outputs: HashMap<String, ToolOutputAccumulator>,
    detached: bool,
}

#[derive(Default)]
struct ToolOutputAccumulator {
    text: String,
    next_offset: u64,
}

impl PreparedSurfacePrompt {
    fn detach_once(&mut self) {
        if self.detached {
            return;
        }
        let _ = self.surface.detach(
            &self.client,
            crate::surface::DetachRequest {
                request_id: SurfaceRequestId::new(),
            },
        );
        self.detached = true;
    }
}

impl Drop for PreparedSurfacePrompt {
    fn drop(&mut self) {
        if self.detached {
            return;
        }
        let _ = self
            .client
            .cancel_operation(SurfaceRequestId::new(), self.operation_id.clone());
        self.detach_once();
    }
}

struct SurfaceAttachmentGuard<'a> {
    surface: &'a RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
    operation_id: Option<SurfaceOperationId>,
    armed: bool,
}

impl<'a> SurfaceAttachmentGuard<'a> {
    fn new(surface: &'a RuntimeSurfaceHandle, client: RuntimeSurfaceClientHandle) -> Self {
        Self {
            surface,
            client,
            operation_id: None,
            armed: true,
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for SurfaceAttachmentGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(operation_id) = self.operation_id.take() {
            let _ = self
                .client
                .cancel_operation(SurfaceRequestId::new(), operation_id);
        }
        let _ = self.surface.detach(
            &self.client,
            crate::surface::DetachRequest {
                request_id: SurfaceRequestId::new(),
            },
        );
    }
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
    note_tx: &AcpNotificationSender,
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
    let cleanup = SurfaceAttachmentGuard::new(surface, attachment.client.clone());
    for item in attachment.baseline.snapshot.items.iter() {
        let update = match item {
            SurfaceItem::UserMessage { input, .. } => replay_user_update(input),
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
            SurfaceItem::AssistantPlan { .. } => None,
            SurfaceItem::ToolResultMessage { .. } => None,
            SurfaceItem::SystemMessage { .. } => None,
        };
        if let Some(update) = update {
            let _ = note_tx.send(SessionNotification::new(session_id.clone(), update));
        }
    }
    let known_tool_ids = attachment
        .baseline
        .snapshot
        .tools
        .iter()
        .map(|tool| tool.request.tool_call_id.clone())
        .collect::<HashSet<_>>();
    for tool in attachment.baseline.snapshot.tools.iter() {
        let call = ToolCall::new(
            ToolCallId::new(tool.request.tool_call_id.as_str().to_string()),
            tool_call_title(&tool.request),
        )
        .kind(tool_kind(tool.request.action))
        .status(match tool.state {
            SurfaceToolViewState::Requested => ToolCallStatus::Pending,
            SurfaceToolViewState::Running => ToolCallStatus::InProgress,
            SurfaceToolViewState::Completed => tool
                .result
                .as_ref()
                .map(|result| tool_status(result.terminal.kind))
                .unwrap_or(ToolCallStatus::Completed),
        })
        .raw_input(serde_json::from_str(tool.request.raw_arguments.as_str()).ok());
        let _ = note_tx.send(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::ToolCall(call),
        ));
        if let Some(result) = tool.result.as_ref() {
            let output = result
                .output
                .as_ref()
                .or(result.error.as_ref())
                .map(|value| value.as_str().to_string())
                .or_else(|| {
                    (!tool.streamed_output.as_str().is_empty())
                        .then(|| tool.streamed_output.as_str().to_string())
                });
            let mut fields = ToolCallUpdateFields::new().status(tool_status(result.terminal.kind));
            if let Some(output) = output {
                fields = fields.content(vec![ToolCallContent::from(ContentBlock::from(output))]);
            }
            let _ = note_tx.send(SessionNotification::new(
                session_id.clone(),
                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    ToolCallId::new(tool.request.tool_call_id.as_str().to_string()),
                    fields,
                )),
            ));
        }
    }
    for item in attachment.baseline.snapshot.items.iter() {
        let SurfaceItem::ToolResultMessage {
            tool_call_id,
            content,
            terminal,
            ..
        } = item
        else {
            continue;
        };
        if known_tool_ids.contains(tool_call_id) {
            continue;
        }
        let _ = note_tx.send(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(tool_call_id.as_str().to_string()),
                ToolCallUpdateFields::new()
                    .status(tool_status(terminal.kind))
                    .content(vec![ToolCallContent::from(ContentBlock::from(
                        content.as_str().to_string(),
                    ))]),
            )),
        ));
    }
    let plan = Plan::new(
        attachment
            .baseline
            .snapshot
            .plan
            .items
            .iter()
            .map(|item| {
                PlanEntry::new(
                    item.step.as_str(),
                    match item.priority {
                        SurfacePlanPriority::Low => PlanEntryPriority::Low,
                        SurfacePlanPriority::Medium => PlanEntryPriority::Medium,
                        SurfacePlanPriority::High => PlanEntryPriority::High,
                    },
                    match item.status {
                        SurfacePlanStatus::Pending => PlanEntryStatus::Pending,
                        SurfacePlanStatus::InProgress => PlanEntryStatus::InProgress,
                        SurfacePlanStatus::Completed => PlanEntryStatus::Completed,
                    },
                )
            })
            .collect(),
    );
    let _ = note_tx.send(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::Plan(plan),
    ));
    cleanup.disarm();
    Ok(())
}

fn replay_user_update(
    input: &crate::runtime_surface::SurfaceUserInputState,
) -> Option<SessionUpdate> {
    let text = match input {
        crate::runtime_surface::SurfaceUserInputState::Pending { presentation, .. }
        | crate::runtime_surface::SurfaceUserInputState::ResolutionFailed {
            presentation, ..
        } => match presentation {
            crate::runtime_surface::SurfaceInputPresentation::Visible { text } => {
                Some(text.as_str().to_string())
            }
            crate::runtime_surface::SurfaceInputPresentation::Redacted => None,
        },
        crate::runtime_surface::SurfaceUserInputState::Resolved { fact } => match fact {
            crate::runtime_surface::SurfaceResolvedInputFact::Replayable { input, .. } => {
                Some(input.canonical_text.as_str().to_string())
            }
            crate::runtime_surface::SurfaceResolvedInputFact::NonReplayable {
                presentation,
                ..
            } => match presentation {
                crate::runtime_surface::SurfaceInputPresentation::Visible { text } => {
                    Some(text.as_str().to_string())
                }
                crate::runtime_surface::SurfaceInputPresentation::Redacted => None,
            },
        },
    }?;
    Some(SessionUpdate::UserMessageChunk(
        agent_client_protocol::ContentChunk::new(ContentBlock::from(text)),
    ))
}

fn tool_status(kind: SurfaceToolResultKind) -> ToolCallStatus {
    if kind == SurfaceToolResultKind::Success {
        ToolCallStatus::Completed
    } else {
        ToolCallStatus::Failed
    }
}

fn prepare_surface_prompt(
    surface: &RuntimeSurfaceHandle,
    session_id: &SessionId,
    prompt: &str,
    inbound_seq: u64,
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
    let mut cleanup = SurfaceAttachmentGuard::new(surface, attachment.client.clone());
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
            inbound_seq: SequenceNumber::new(inbound_seq),
            rpc_request_id: AcpRequestId::String(
                NonEmptyText::try_new(format!("prompt-{}", uuid::Uuid::new_v4()))
                    .map_err(|error| format!("invalid ACP request id: {error}"))?,
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
    cleanup.operation_id = Some(operation_id.clone());
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
    cleanup.disarm();
    Ok(PreparedSurfacePrompt {
        surface: surface.clone(),
        client: attachment.client,
        operation_id,
        subscription,
        client_bridge,
        tool_outputs: HashMap::new(),
        detached: false,
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
    note_tx: AcpNotificationSender,
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
    prepared.detach_once();
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
    note_tx: &AcpNotificationSender,
    event: &SurfaceEvent,
    tool_outputs: &mut HashMap<String, ToolOutputAccumulator>,
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
        SurfaceEvent::Tool(ToolPatch::Requested { request }) => Some(SessionUpdate::ToolCall(
            ToolCall::new(
                ToolCallId::new(request.tool_call_id.as_str().to_string()),
                tool_call_title(request),
            )
            .kind(tool_kind(request.action))
            .status(ToolCallStatus::Pending)
            .raw_input(serde_json::from_str(request.raw_arguments.as_str()).ok()),
        )),
        SurfaceEvent::Tool(ToolPatch::OutputDelta {
            tool_call_id,
            offset,
            chunk,
        }) => {
            let output = tool_outputs
                .entry(tool_call_id.as_str().to_string())
                .or_default();
            let start = offset.get();
            if start > output.next_offset {
                return;
            }
            let overlap = output.next_offset.saturating_sub(start) as usize;
            if overlap >= chunk.as_str().len() || !chunk.as_str().is_char_boundary(overlap) {
                return;
            }
            output.text.push_str(&chunk.as_str()[overlap..]);
            output.next_offset = start + chunk.as_str().len() as u64;
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(tool_call_id.as_str().to_string()),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::InProgress)
                    .content(vec![ToolCallContent::from(ContentBlock::from(
                        output.text.clone(),
                    ))]),
            )))
        }
        SurfaceEvent::Tool(ToolPatch::Completed { result }) => {
            let output = result
                .output
                .as_ref()
                .or(result.error.as_ref())
                .map(|text| text.as_str().to_string());
            let accumulated = tool_outputs
                .entry(result.tool_call_id.as_str().to_string())
                .or_default();
            if let Some(output) = output {
                accumulated.next_offset = output.len() as u64;
                accumulated.text = output;
            }
            let status = if result.terminal.kind == SurfaceToolResultKind::Success {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            };
            let mut fields = ToolCallUpdateFields::new().status(status);
            if !accumulated.text.is_empty() {
                fields = fields.content(vec![ToolCallContent::from(ContentBlock::from(
                    accumulated.text.clone(),
                ))]);
            }
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(result.tool_call_id.as_str().to_string()),
                fields,
            )))
        }
        _ => None,
    };
    if let Some(update) = update {
        let _ = note_tx.send(SessionNotification::new(session_id.clone(), update));
    }
}

fn project_surface_event(
    prepared: &mut PreparedSurfacePrompt,
    session_id: &SessionId,
    note_tx: &AcpNotificationSender,
    event: &SurfaceEvent,
) -> Result<(), String> {
    if let SurfaceEvent::Interaction(crate::surface::InteractionPatch::Requested { interaction }) =
        event
    {
        let Some(bridge) = prepared.client_bridge.as_ref() else {
            cancel_surface_operation(prepared)?;
            return Err("ACP interaction requires a connected client bridge".to_string());
        };
        let (request, target) = match build_permission_request(session_id, interaction) {
            Ok(value) => value,
            Err(error) => {
                cancel_surface_operation(prepared)?;
                return Err(error);
            }
        };
        let response = match bridge.request_permission(request) {
            Ok(response) => response,
            Err(AcpPermissionWaitError::Cancelled) => {
                let _ = cancel_surface_operation(prepared);
                return Ok(());
            }
            Err(error) => {
                cancel_surface_operation(prepared)?;
                return Err(format!("ACP permission request failed: {error:?}"));
            }
        };
        let answer = match permission_answer(response, target) {
            Ok(answer) => answer,
            Err(error) => {
                let _ = cancel_surface_operation(prepared);
                return Err(error);
            }
        };
        match prepared.client.respond_interaction_by_id(
            SurfaceRequestId::new(),
            interaction.interaction_id.clone(),
            answer,
        ) {
            Ok(MutationReply::Committed { .. }) => {}
            Ok(MutationReply::Deferred { .. }) => {
                let _ = cancel_surface_operation(prepared);
                return Err("ACP interaction response was deferred".to_string());
            }
            Ok(MutationReply::Uncommitted { .. }) => {
                let _ = cancel_surface_operation(prepared);
                return Err("ACP interaction response did not commit".to_string());
            }
            Err(error) => {
                let _ = cancel_surface_operation(prepared);
                return Err(format!("ACP interaction response failed: {error:?}"));
            }
        }
    }
    emit_surface_event(session_id, note_tx, event, &mut prepared.tool_outputs);
    Ok(())
}

fn tool_call_title(request: &crate::runtime_surface::SurfaceToolRequest) -> String {
    request
        .target
        .as_ref()
        .map(|target| format!("{}: {}", request.name.as_str(), target.as_str()))
        .unwrap_or_else(|| request.name.as_str().to_string())
}

fn tool_kind(action: SurfaceToolAction) -> ToolKind {
    match action {
        SurfaceToolAction::Read => ToolKind::Read,
        SurfaceToolAction::Write => ToolKind::Edit,
        SurfaceToolAction::Network => ToolKind::Fetch,
        SurfaceToolAction::Agent => ToolKind::Think,
        SurfaceToolAction::Shell => ToolKind::Execute,
    }
}

fn cancel_surface_operation(prepared: &PreparedSurfacePrompt) -> Result<(), String> {
    match prepared
        .client
        .cancel_operation(SurfaceRequestId::new(), prepared.operation_id.clone())
        .map_err(|error| format!("ACP surface cancellation failed: {error:?}"))?
    {
        MutationReply::Committed { .. } | MutationReply::Deferred { .. } => Ok(()),
        MutationReply::Uncommitted { .. } => {
            Err("ACP surface cancellation did not commit".to_string())
        }
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
                next_prompt_seq: 1,
            },
        );
        Ok(NewSessionResponse::new(session_id))
    }

    async fn load_session(&self, args: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        if self.state.borrow().sessions.contains_key(&args.session_id) {
            return Err(Error::invalid_params().data("ACP session is already loaded"));
        }
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
            let surface = surface.clone();
            let session_id = args.session_id.clone();
            let note_tx = self.note_tx.clone();
            tokio::task::spawn_blocking(move || {
                replay_surface_snapshot(&surface, &session_id, &note_tx)
            })
            .await
            .map_err(Error::into_internal_error)?
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
                next_prompt_seq: 1,
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
        if let Some(bridge) = self.client_bridge.as_ref() {
            bridge.cancel_session(&args.session_id);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_surface::{
        SurfaceEvent, SurfaceToolRequest, SurfaceToolResult, SurfaceToolTerminal,
        ToolInvocationStarted, ToolTerminalSource,
    };
    use orca_core::thread_identity::TurnId;

    fn permission_request(session_id: &str, tool_call_id: &str) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            SessionId::new(session_id),
            ToolCallUpdate::new(ToolCallId::new(tool_call_id), ToolCallUpdateFields::new()),
            Vec::new(),
        )
    }

    #[test]
    fn permission_cancel_before_registration_is_not_lost() {
        let (bridge, _requests) = AcpClientBridge::new();
        let session_id = SessionId::new("cancel-before-register");
        bridge.cancel_session(&session_id);

        let result =
            bridge.request_permission(permission_request("cancel-before-register", "tool-1"));
        assert_eq!(result, Err(AcpPermissionWaitError::Cancelled));
    }

    #[test]
    fn permission_cancel_wakes_waiter_and_does_not_reuse_key() {
        let (bridge, mut requests) = AcpClientBridge::new();
        let session_id = SessionId::new("cancel-waiter");
        let waiter_bridge = Arc::clone(&bridge);
        let waiter = std::thread::spawn(move || {
            waiter_bridge.request_permission(permission_request("cancel-waiter", "tool-1"))
        });
        let request = requests
            .blocking_recv()
            .expect("permission request is queued");
        assert!(bridge.is_pending(&request.key));
        bridge.cancel_session(&session_id);
        assert_eq!(
            waiter.join().expect("permission waiter joins"),
            Err(AcpPermissionWaitError::Cancelled)
        );
        assert!(!bridge.is_pending(&request.key));
    }

    #[test]
    fn tool_events_project_as_typed_acp_updates() {
        let request = SurfaceToolRequest {
            tool_call_id: crate::runtime_surface::SurfaceToolCallId::try_new("tool-typed").unwrap(),
            source_response_id: None,
            turn_id: TurnId::new(),
            name: NonEmptyText::try_new("shell").unwrap(),
            action: SurfaceToolAction::Shell,
            target: Some(DisplayText::new("cargo test")),
            raw_arguments: DisplayText::new(r#"{"command":"cargo test"}"#),
            arguments_digest: crate::runtime_surface::Sha256Digest::new([0; 32]),
        };
        let (note_tx, mut note_rx) = unbounded_channel();
        let note_tx = AcpNotificationSender::Unbounded(note_tx);
        let mut tool_outputs = HashMap::new();
        emit_surface_event(
            &SessionId::new("typed-tools"),
            &note_tx,
            &SurfaceEvent::Tool(ToolPatch::Requested {
                request: request.clone(),
            }),
            &mut tool_outputs,
        );
        let update = note_rx.try_recv().expect("tool request update");
        match update.update {
            SessionUpdate::ToolCall(call) => {
                assert_eq!(call.kind, ToolKind::Execute);
                assert_eq!(call.status, ToolCallStatus::Pending);
                assert_eq!(call.title, "shell: cargo test");
                assert!(call.raw_input.is_some());
            }
            other => panic!("expected typed tool call, got {other:?}"),
        }

        emit_surface_event(
            &SessionId::new("typed-tools"),
            &note_tx,
            &SurfaceEvent::Tool(ToolPatch::OutputDelta {
                tool_call_id: request.tool_call_id.clone(),
                offset: crate::runtime_surface::ByteOffset::new(0),
                chunk: DisplayText::new("done"),
            }),
            &mut tool_outputs,
        );
        let _ = note_rx.try_recv().expect("tool output update");
        emit_surface_event(
            &SessionId::new("typed-tools"),
            &note_tx,
            &SurfaceEvent::Tool(ToolPatch::OutputDelta {
                tool_call_id: request.tool_call_id.clone(),
                offset: crate::runtime_surface::ByteOffset::new(0),
                chunk: DisplayText::new("done"),
            }),
            &mut tool_outputs,
        );
        assert!(
            note_rx.try_recv().is_err(),
            "duplicate output must be suppressed"
        );
        emit_surface_event(
            &SessionId::new("typed-tools"),
            &note_tx,
            &SurfaceEvent::Tool(ToolPatch::Completed {
                result: SurfaceToolResult {
                    tool_call_id: request.tool_call_id,
                    name: request.name,
                    terminal: SurfaceToolTerminal {
                        kind: SurfaceToolResultKind::Success,
                        source: ToolTerminalSource::Observed,
                        invocation_started: ToolInvocationStarted::Yes,
                    },
                    output: Some(DisplayText::new("done")),
                    error: None,
                    exit_code: Some(0),
                    truncated: false,
                    file_change: None,
                },
            }),
            &mut tool_outputs,
        );
        let update = note_rx.try_recv().expect("tool completion update");
        match update.update {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
            }
            other => panic!("expected typed tool completion, got {other:?}"),
        }
    }
}
