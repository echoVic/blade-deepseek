//! ACP [`Agent`] implementation projected onto the runtime-owned typed surface.
//!
//! The adapter retains only ACP transport correlation. Runtime threads,
//! operation lifecycle, interactions, cancellation and terminal facts remain
//! owned by the runtime surface.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};

use agent_client_protocol::{
    Agent, AgentCapabilities, AuthenticateRequest, AuthenticateResponse, CancelNotification,
    ContentBlock, EmbeddedResourceResource, Error, Implementation, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PermissionOption, PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority,
    PlanEntryStatus, PromptRequest, PromptResponse, ProtocolVersion, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome, SessionId,
    SessionNotification, SessionUpdate, StopReason, ToolCall, ToolCallContent, ToolCallId,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use orca_core::config::{HistoryMode, RunConfig};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, mpsc};

use crate::runtime_host::RuntimeThreadStartRequest;
use crate::surface::{
    AcpRequestId, AssistantPatch, AttachResult, CanonicalMime, CanonicalUri, DisplayText,
    FreshAttachRequest, MutationReply, NonEmptyText, NonEmptyVec, NotAdmittedReason,
    OperationBudget, OperationIngressCorrelation, OperationKind, OperationRequestIntent,
    OperationSettingsPreparation, OperationTerminal, PermissionGrantScope, ReplayabilityRequest,
    RuntimeSurfaceClientHandle, RuntimeSurfaceHandle, RuntimeSurfaceHostHandle, SequenceNumber,
    Sha256Digest, SurfaceAllowDeny, SurfaceAttachmentRole, SurfaceCapability,
    SurfaceClientInteractionAnswer, SurfaceEvent, SurfaceInputRequest, SurfaceInputRequestBlock,
    SurfaceInteractionKind, SurfaceInteractionRequest, SurfaceInteractionView, SurfaceOperationId,
    SurfacePermissionClientDecision, SurfacePermissionProfile, SurfaceRequestId,
    SurfaceSubscriptionItem, SurfaceToolResultKind, ToolPatch, TurnRequestBudgetScope,
    UncommittedMutation,
};

use crate::runtime_surface::{
    OperationPatch, SurfaceItem, SurfacePlanPriority, SurfacePlanStatus, SurfaceToolAction,
    SurfaceToolViewState,
};

pub(crate) const ACP_NOTIFICATION_CAPACITY: usize = 256;
const ACP_PERMISSION_REQUEST_CAPACITY: usize = 64;

#[derive(Clone)]
pub(crate) enum AcpNotificationSender {
    Buffered(mpsc::Sender<SessionNotification>),
    Acknowledged(mpsc::Sender<AcpNotificationDelivery>),
}

pub(crate) struct AcpNotificationDelivery {
    pub(crate) notification: SessionNotification,
    pub(crate) acknowledgement: std_mpsc::SyncSender<Result<(), String>>,
}

impl AcpNotificationSender {
    fn send(&self, notification: SessionNotification) -> Result<(), ()> {
        match self {
            Self::Buffered(sender) => sender.blocking_send(notification).map_err(|_| ()),
            Self::Acknowledged(sender) => {
                let (acknowledgement, receipt) = std_mpsc::sync_channel(1);
                sender
                    .blocking_send(AcpNotificationDelivery {
                        notification,
                        acknowledgement,
                    })
                    .map_err(|_| ())?;
                receipt.recv().map_err(|_| ())?.map_err(|_| ())
            }
        }
    }
}

/// Per-session runtime state held on the single-threaded ACP task.
struct SessionEntry {
    surface: RuntimeSurfaceHandle,
    prompt_binding: Option<AcpPromptBinding>,
    next_prompt_seq: u64,
}

enum AcpPromptBinding {
    Decoded {
        ready: Rc<Notify>,
    },
    Bound {
        ready: Rc<Notify>,
        client: RuntimeSurfaceClientHandle,
        operation_id: SurfaceOperationId,
    },
}

#[derive(Default)]
struct AgentState {
    sessions: HashMap<SessionId, SessionEntry>,
}

/// ACP agent backed by the Orca runtime host.
pub struct OrcaAcpAgent {
    surface_host: RuntimeSurfaceHostHandle,
    base_config: RunConfig,
    note_tx: AcpNotificationSender,
    state: Rc<RefCell<AgentState>>,
    client_bridge: Option<Arc<AcpClientBridge>>,
}

pub(crate) struct AcpClientBridge {
    request_tx: mpsc::Sender<AcpPermissionRequest>,
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
    pub(crate) fn new() -> (Arc<Self>, mpsc::Receiver<AcpPermissionRequest>) {
        let (request_tx, request_rx) = mpsc::channel(ACP_PERMISSION_REQUEST_CAPACITY);
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
            .try_send(AcpPermissionRequest {
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
        host: RuntimeSurfaceHostHandle,
        base_config: RunConfig,
        note_tx: mpsc::Sender<SessionNotification>,
    ) -> Self {
        Self {
            surface_host: host,
            base_config,
            note_tx: AcpNotificationSender::Buffered(note_tx),
            state: Rc::new(RefCell::new(AgentState::default())),
            client_bridge: None,
        }
    }

    pub(crate) fn new_supervised(
        host: RuntimeSurfaceHostHandle,
        base_config: RunConfig,
        note_tx: mpsc::Sender<AcpNotificationDelivery>,
    ) -> Self {
        Self {
            surface_host: host,
            base_config,
            note_tx: AcpNotificationSender::Acknowledged(note_tx),
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

    pub(crate) async fn admit_prompt(
        &self,
        args: PromptRequest,
        inbound_seq: Option<u64>,
    ) -> Result<AdmittedAcpPrompt, Error> {
        let ready = Rc::new(Notify::new());
        let (surface, inbound_seq) = {
            let mut state = self.state.borrow_mut();
            let entry = state
                .sessions
                .get_mut(&args.session_id)
                .ok_or_else(Error::invalid_params)?;
            if entry.prompt_binding.is_some() {
                return Err(Error::invalid_params().data("session already has an active prompt"));
            }
            let sequence = match inbound_seq {
                Some(sequence) => sequence,
                None => {
                    let sequence = entry.next_prompt_seq;
                    entry.next_prompt_seq =
                        entry.next_prompt_seq.checked_add(1).ok_or_else(|| {
                            Error::internal_error().data("ACP prompt sequence exhausted")
                        })?;
                    sequence
                }
            };
            entry.prompt_binding = Some(AcpPromptBinding::Decoded {
                ready: ready.clone(),
            });
            (entry.surface.clone(), sequence)
        };
        if let Some(bridge) = self.client_bridge.as_ref() {
            bridge.begin_session(&args.session_id);
        }
        let input = match decode_prompt_content(&args.prompt) {
            Ok(input) => input,
            Err(message) => {
                self.clear_prompt_binding(&args.session_id, &ready);
                return Err(Error::invalid_params().data(message));
            }
        };
        let session_id = args.session_id.clone();
        let client_bridge = self.client_bridge.clone();
        let prepared = match tokio::task::spawn_blocking(move || {
            prepare_surface_prompt(&surface, &session_id, input, inbound_seq, client_bridge)
        })
        .await
        {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => {
                self.clear_prompt_binding(&args.session_id, &ready);
                return Err(error.into_protocol_error());
            }
            Err(error) => {
                self.clear_prompt_binding(&args.session_id, &ready);
                return Err(Error::into_internal_error(error));
            }
        };

        {
            let mut state = self.state.borrow_mut();
            let entry = state
                .sessions
                .get_mut(&args.session_id)
                .ok_or_else(Error::invalid_params)?;
            entry.prompt_binding = Some(AcpPromptBinding::Bound {
                ready: ready.clone(),
                client: prepared.client.clone(),
                operation_id: prepared.operation_id.clone(),
            });
        }
        ready.notify_waiters();
        Ok(AdmittedAcpPrompt {
            session_id: args.session_id,
            ready,
            prepared,
        })
    }

    pub(crate) async fn complete_prompt(
        &self,
        admitted: AdmittedAcpPrompt,
    ) -> Result<PromptResponse, Error> {
        let note_tx = self.note_tx.clone();
        let session_id = admitted.session_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            drain_surface_prompt(admitted.prepared, session_id, note_tx)
        })
        .await;

        self.clear_prompt_binding(&admitted.session_id, &admitted.ready);
        let result = result
            .map_err(Error::into_internal_error)?
            .map_err(|message| Error::internal_error().data(message))?;
        Ok(PromptResponse::new(result))
    }

    fn clear_prompt_binding(&self, session_id: &SessionId, ready: &Rc<Notify>) {
        let mut state = self.state.borrow_mut();
        if let Some(entry) = state.sessions.get_mut(session_id) {
            let matches_prompt = match entry.prompt_binding.as_ref() {
                Some(AcpPromptBinding::Decoded { ready: current })
                | Some(AcpPromptBinding::Bound { ready: current, .. }) => {
                    Rc::ptr_eq(current, ready)
                }
                None => false,
            };
            if matches_prompt {
                entry.prompt_binding = None;
            }
        }
        ready.notify_waiters();
    }
}

pub(crate) struct AdmittedAcpPrompt {
    session_id: SessionId,
    ready: Rc<Notify>,
    prepared: PreparedSurfacePrompt,
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

/// Decodes ACP prompt content into the closed runtime input algebra without
/// flattening away block identity or order.
fn decode_prompt_content(blocks: &[ContentBlock]) -> Result<SurfaceInputRequest, String> {
    let mut decoded = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                decoded.push(SurfaceInputRequestBlock::Text {
                    text: DisplayText::new(text.text.clone()),
                });
            }
            ContentBlock::ResourceLink(link) => {
                let uri = CanonicalUri::try_new(link.uri.clone())
                    .map_err(|error| format!("invalid ACP resource link URI: {error}"))?;
                let name = NonEmptyText::try_new(link.name.clone())
                    .map_err(|error| format!("invalid ACP resource link name: {error}"))?;
                let mime = link
                    .mime_type
                    .as_ref()
                    .map(|mime| {
                        CanonicalMime::try_new(mime.clone())
                            .map_err(|error| format!("invalid ACP resource link MIME: {error}"))
                    })
                    .transpose()?;
                decoded.push(SurfaceInputRequestBlock::ResourceLink {
                    uri,
                    name,
                    description: link.description.clone().map(DisplayText::new),
                    mime,
                });
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(resource) => {
                    let mime = resource.mime_type.as_deref().unwrap_or("text/plain");
                    if !mime.starts_with("text/") {
                        return Err(format!("unsupported ACP embedded text MIME: {mime}"));
                    }
                    let uri = CanonicalUri::try_new(resource.uri.clone())
                        .map_err(|error| format!("invalid ACP embedded resource URI: {error}"))?;
                    let mime = CanonicalMime::try_new(mime.to_string())
                        .map_err(|error| format!("invalid ACP embedded resource MIME: {error}"))?;
                    let digest = Sha256Digest::new(Sha256::digest(resource.text.as_bytes()).into());
                    decoded.push(SurfaceInputRequestBlock::EmbeddedText {
                        uri,
                        mime,
                        text: DisplayText::new(resource.text.clone()),
                        digest,
                    });
                }
                EmbeddedResourceResource::BlobResourceContents(_) => {
                    return Err("unsupported ACP prompt content block: embedded_blob".to_string());
                }
                _ => {
                    return Err(
                        "unsupported ACP prompt content block: embedded_resource".to_string()
                    );
                }
            },
            _ => {
                return Err(format!(
                    "unsupported ACP prompt content block: {}",
                    content_block_name(block)
                ));
            }
        }
    }
    let blocks = NonEmptyVec::try_new(decoded)
        .map_err(|error| format!("invalid ACP prompt content: {error}"))?;
    Ok(SurfaceInputRequest { blocks })
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

#[derive(Debug)]
enum AcpPromptPrepareError {
    InvalidInput(String),
    Internal(String),
}

impl AcpPromptPrepareError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    fn into_protocol_error(self) -> Error {
        match self {
            Self::InvalidInput(message) => Error::invalid_params().data(message),
            Self::Internal(message) => Error::internal_error().data(message),
        }
    }
}

fn prepare_surface_prompt(
    surface: &RuntimeSurfaceHandle,
    session_id: &SessionId,
    input: SurfaceInputRequest,
    inbound_seq: u64,
    client_bridge: Option<Arc<AcpClientBridge>>,
) -> Result<PreparedSurfacePrompt, AcpPromptPrepareError> {
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
        AttachResult::Denied { .. } => {
            return Err(AcpPromptPrepareError::internal(
                "ACP surface attachment denied",
            ));
        }
        AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. }
        | AttachResult::ThreadClosed { .. }
        | AttachResult::Unavailable { .. } => {
            return Err(AcpPromptPrepareError::internal(
                "ACP surface attachment unavailable",
            ));
        }
    };
    let mut cleanup = SurfaceAttachmentGuard::new(surface, attachment.client.clone());
    let subscription = surface
        .claim_subscription(&attachment.subscription)
        .ok_or_else(|| AcpPromptPrepareError::internal("ACP surface subscription unavailable"))?;
    let session_id = NonEmptyText::try_new(session_id.to_string()).map_err(|error| {
        AcpPromptPrepareError::invalid(format!("invalid ACP session id: {error}"))
    })?;
    let intent = OperationRequestIntent {
        correlation: OperationIngressCorrelation::AcpPrompt {
            session_id,
            inbound_seq: SequenceNumber::new(inbound_seq),
            rpc_request_id: AcpRequestId::String(
                NonEmptyText::try_new(format!("prompt-{}", uuid::Uuid::new_v4())).map_err(
                    |error| {
                        AcpPromptPrepareError::invalid(format!("invalid ACP request id: {error}"))
                    },
                )?,
            ),
        },
        kind: OperationKind::UserTurn,
        input: Some(input),
        replayability: ReplayabilityRequest::CaptureReplayableCapsule,
        settings_preparation: OperationSettingsPreparation::UseCurrent {
            expected_settings_revision: attachment.baseline.snapshot.settings.thread_revision,
            expected_policy_epoch: attachment.baseline.snapshot.settings.effective.policy_epoch,
        },
    };
    let reserved = match attachment
        .client
        .reserve_operation(SurfaceRequestId::new(), intent)
        .map_err(|error| {
            AcpPromptPrepareError::internal(format!("ACP surface reserve failed: {error:?}"))
        })? {
        MutationReply::Committed { value, .. } => value,
        MutationReply::Deferred { .. } => {
            return Err(AcpPromptPrepareError::internal(
                "ACP surface reserve did not commit",
            ));
        }
        MutationReply::Uncommitted { mutation } => {
            let message = uncommitted_mutation_message(&mutation).to_string();
            return Err(match mutation {
                UncommittedMutation::Invalid { .. } | UncommittedMutation::Stale { .. } => {
                    AcpPromptPrepareError::invalid(message)
                }
                UncommittedMutation::Unavailable { .. }
                | UncommittedMutation::CommitFailed { .. } => {
                    AcpPromptPrepareError::internal(message)
                }
            });
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
        .map_err(|error| {
            AcpPromptPrepareError::internal(format!("ACP surface admission failed: {error:?}"))
        })? {
        MutationReply::Committed { .. } => {}
        MutationReply::Deferred { .. } | MutationReply::Uncommitted { .. } => {
            return Err(AcpPromptPrepareError::internal(
                "ACP surface admission did not commit",
            ));
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

fn uncommitted_mutation_message(mutation: &UncommittedMutation) -> &str {
    match mutation {
        UncommittedMutation::Invalid { error, .. } => error.error().message.as_str(),
        UncommittedMutation::Stale { error, .. } => error.error().message.as_str(),
        UncommittedMutation::Unavailable { error, .. } => error.error().message.as_str(),
        UncommittedMutation::CommitFailed { error, .. } => error.error().message.as_str(),
    }
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
    let terminal = loop {
        let Some(item) = prepared
            .subscription
            .recv_timeout(std::time::Duration::from_millis(100))
        else {
            continue;
        };
        match item {
            SurfaceSubscriptionItem::Batch { batch } => {
                let mut terminal = None;
                for envelope in batch.events.as_slice() {
                    project_surface_event(&mut prepared, &session_id, &note_tx, &envelope.event)?;
                    if let SurfaceEvent::Operation(OperationPatch::Terminal { record }) =
                        &envelope.event
                        && record.operation_id == prepared.operation_id
                    {
                        terminal = Some(record.terminal.clone());
                    }
                }
                if let Some(terminal) = terminal {
                    break terminal;
                }
            }
            SurfaceSubscriptionItem::Gap { .. } => {
                return reconcile_lost_subscription(&mut prepared, "gap");
            }
            SurfaceSubscriptionItem::Sealed { .. } => {
                return reconcile_lost_subscription(&mut prepared, "sealed");
            }
        }
    };
    prepared.detach_once();
    terminal_to_stop_reason(&terminal)
}

fn reconcile_lost_subscription(
    prepared: &mut PreparedSurfacePrompt,
    loss: &str,
) -> Result<StopReason, String> {
    let sealed_snapshot = prepared.subscription.sealed_snapshot();
    prepared.detach_once();
    if let Some(snapshot) = sealed_snapshot {
        return reconcile_operation_snapshot(prepared, loss, snapshot.snapshot.as_ref());
    }
    let attachment = match prepared.surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Acp,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        AttachResult::Denied { .. }
        | AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. }
        | AttachResult::ThreadClosed { .. }
        | AttachResult::Unavailable { .. } => {
            if let Some(snapshot) = prepared.subscription.sealed_snapshot() {
                return reconcile_operation_snapshot(prepared, loss, snapshot.snapshot.as_ref());
            }
            return Err(format!(
                "ACP surface subscription {loss}; durable snapshot reconciliation unavailable"
            ));
        }
    };
    let cleanup = SurfaceAttachmentGuard::new(&prepared.surface, attachment.client.clone());
    let result =
        reconcile_operation_snapshot(prepared, loss, attachment.baseline.snapshot.as_ref());
    cleanup.disarm();
    let _ = prepared.surface.detach(
        &attachment.client,
        crate::surface::DetachRequest {
            request_id: SurfaceRequestId::new(),
        },
    );
    result
}

fn reconcile_operation_snapshot(
    prepared: &PreparedSurfacePrompt,
    loss: &str,
    snapshot: &crate::runtime_surface::SurfaceSnapshot,
) -> Result<StopReason, String> {
    let terminal = snapshot
        .foreground_operation
        .iter()
        .chain(snapshot.queued_operations.iter())
        .chain(snapshot.operation_history.iter())
        .find(|operation| operation.operation_id == prepared.operation_id)
        .and_then(|operation| operation.terminal.as_ref())
        .map(|record| record.terminal.clone());
    match terminal {
        Some(terminal) => {
            let reason = terminal_to_stop_reason(&terminal)
                .map(|reason| format!("{reason:?}"))
                .unwrap_or_else(|error| error);
            Err(format!(
                "ACP surface subscription {loss} after durable terminal {reason}; reload session"
            ))
        }
        None => Err(format!(
            "ACP surface subscription {loss} before terminal; reload session"
        )),
    }
}

fn terminal_to_stop_reason(terminal: &OperationTerminal) -> Result<StopReason, String> {
    match terminal {
        OperationTerminal::Succeeded { .. } => Ok(StopReason::EndTurn),
        OperationTerminal::Cancelled { .. } => Ok(StopReason::Cancelled),
        OperationTerminal::BudgetExhausted {
            budget: OperationBudget::ModelTokens { .. },
        } => Ok(StopReason::MaxTokens),
        OperationTerminal::BudgetExhausted {
            budget:
                OperationBudget::TurnRequests {
                    scope: TurnRequestBudgetScope::AgentLoop,
                    ..
                },
        } => Ok(StopReason::MaxTurnRequests),
        OperationTerminal::BudgetExhausted { budget } => {
            Err(format!("ACP budget exhausted: {budget:?}"))
        }
        OperationTerminal::NotAdmitted {
            reason: NotAdmittedReason::CancelledBeforeAdmission,
        } => Ok(StopReason::Cancelled),
        OperationTerminal::NotAdmitted { reason } => {
            Err(format!("ACP operation was not admitted: {reason:?}"))
        }
        OperationTerminal::Failed { message, .. }
        | OperationTerminal::Panicked { message }
        | OperationTerminal::JoinFailed { message } => Err(message.as_str().to_string()),
        OperationTerminal::AbortedByRuntimeRestart { .. } => {
            Err("ACP operation aborted by runtime restart".to_string())
        }
        OperationTerminal::Shutdown { .. } => Ok(StopReason::Cancelled),
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
        let surface_host = self.surface_host.clone();
        let thread =
            tokio::task::spawn_blocking(move || surface_host.start_thread(config, "ACP session"))
                .await
                .map_err(Error::into_internal_error)?
                .map_err(Error::into_internal_error)?;
        let surface = thread
            .acp_surface()
            .ok_or_else(|| Error::internal_error().data("ACP surface unavailable"))?;

        let session_id: SessionId = match thread.session_id() {
            Some(id) => SessionId::new(id),
            None => SessionId::new(uuid::Uuid::new_v4().to_string()),
        };

        self.state.borrow_mut().sessions.insert(
            session_id.clone(),
            SessionEntry {
                surface,
                prompt_binding: None,
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
        let transcript = tokio::task::spawn_blocking(move || {
            RuntimeSurfaceHostHandle::load_saved_session(&selector)
        })
        .await
        .map_err(Error::into_internal_error)?
        .map_err(Error::into_internal_error)?;

        let mut config = self.build_session_config(args.cwd);
        config.history_mode = HistoryMode::Resume(args.session_id.to_string());
        let request =
            RuntimeThreadStartRequest::new(config, "ACP session").with_preloaded(transcript);
        let surface_host = self.surface_host.clone();
        let thread =
            tokio::task::spawn_blocking(move || surface_host.start_thread_with_request(request))
                .await
                .map_err(Error::into_internal_error)?
                .map_err(Error::into_internal_error)?;
        let surface = thread
            .acp_surface()
            .ok_or_else(|| Error::internal_error().data("ACP surface unavailable"))?;
        let replay_surface = surface.clone();
        let session_id = args.session_id.clone();
        let note_tx = self.note_tx.clone();
        tokio::task::spawn_blocking(move || {
            replay_surface_snapshot(&replay_surface, &session_id, &note_tx)
        })
        .await
        .map_err(Error::into_internal_error)?
        .map_err(|message| Error::internal_error().data(message))?;

        self.state.borrow_mut().sessions.insert(
            args.session_id.clone(),
            SessionEntry {
                surface,
                prompt_binding: None,
                next_prompt_seq: 1,
            },
        );
        Ok(LoadSessionResponse::new())
    }

    async fn prompt(&self, args: PromptRequest) -> Result<PromptResponse, Error> {
        let admitted = self.admit_prompt(args, None).await?;
        self.complete_prompt(admitted).await
    }

    async fn cancel(&self, args: CancelNotification) -> Result<(), Error> {
        loop {
            let binding = {
                let state = self.state.borrow();
                match state.sessions.get(&args.session_id) {
                    Some(SessionEntry {
                        prompt_binding:
                            Some(AcpPromptBinding::Bound {
                                client,
                                operation_id,
                                ..
                            }),
                        ..
                    }) => Some(Ok((client.clone(), operation_id.clone()))),
                    Some(SessionEntry {
                        prompt_binding: Some(AcpPromptBinding::Decoded { ready }),
                        ..
                    }) => Some(Err(ready.clone())),
                    Some(_) | None => None,
                }
            };
            match binding {
                Some(Ok((client, operation_id))) => {
                    let _ = tokio::task::spawn_blocking(move || {
                        client.cancel_operation(SurfaceRequestId::new(), operation_id)
                    })
                    .await
                    .map_err(Error::into_internal_error)?;
                    break;
                }
                Some(Err(ready)) => ready.notified().await,
                None => break,
            }
        }
        if let Some(bridge) = self.client_bridge.as_ref() {
            bridge.cancel_session(&args.session_id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::runtime_host::{
        GenerationContext, HostedTurnRequest, RuntimeHost, ThreadOperationExecutor,
        ThreadOperationOutcome,
    };
    use crate::runtime_surface::{
        SurfaceEvent, SurfaceToolRequest, SurfaceToolResult, SurfaceToolTerminal,
        ToolInvocationStarted, ToolTerminalSource,
    };
    use crate::thread::RuntimeThread;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::cancel::CancelToken;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::thread_identity::TurnId;

    struct CompleteImmediatelyExecutor;

    impl ThreadOperationExecutor for CompleteImmediatelyExecutor {
        fn run_turn(
            &self,
            _thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            _generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            Ok(RunStatus::Success.into())
        }
    }

    fn permission_request(session_id: &str, tool_call_id: &str) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            SessionId::new(session_id),
            ToolCallUpdate::new(ToolCallId::new(tool_call_id), ToolCallUpdateFields::new()),
            Vec::new(),
        )
    }

    #[test]
    fn prompt_content_decodes_supported_blocks_in_original_order() {
        use agent_client_protocol::{
            EmbeddedResource, EmbeddedResourceResource, ResourceLink, TextResourceContents,
        };

        let blocks = vec![
            ContentBlock::from("first".to_string()),
            ContentBlock::ResourceLink(
                ResourceLink::new("notes", "file:///workspace/notes.txt")
                    .description("notes description")
                    .mime_type("text/plain"),
            ),
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(
                    TextResourceContents::new("embedded", "file:///workspace/context.txt")
                        .mime_type("text/markdown"),
                ),
            )),
            ContentBlock::from("last".to_string()),
        ];

        let decoded = decode_prompt_content(&blocks).expect("supported ACP prompt content");
        let decoded = decoded.blocks.as_slice();
        assert_eq!(decoded.len(), 4);
        assert!(matches!(
            &decoded[0],
            SurfaceInputRequestBlock::Text { text } if text.as_str() == "first"
        ));
        assert!(matches!(
            &decoded[1],
            SurfaceInputRequestBlock::ResourceLink {
                uri,
                name,
                description: Some(description),
                mime: Some(mime),
            } if uri.as_str() == "file:///workspace/notes.txt"
                && name.as_str() == "notes"
                && description.as_str() == "notes description"
                && mime.as_str() == "text/plain"
        ));
        assert!(matches!(
            &decoded[2],
            SurfaceInputRequestBlock::EmbeddedText {
                uri,
                mime,
                text,
                ..
            } if uri.as_str() == "file:///workspace/context.txt"
                && mime.as_str() == "text/markdown"
                && text.as_str() == "embedded"
        ));
        assert!(matches!(
            &decoded[3],
            SurfaceInputRequestBlock::Text { text } if text.as_str() == "last"
        ));
    }

    #[test]
    fn prompt_content_rejects_binary_blocks_before_surface_reservation() {
        use agent_client_protocol::ImageContent;

        let error = decode_prompt_content(&[ContentBlock::Image(ImageContent::new(
            "base64-payload",
            "image/png",
        ))])
        .expect_err("image content lacks a frozen runtime mapping");
        assert!(error.contains("unsupported ACP prompt content block: image"));
    }

    #[test]
    fn terminal_mapping_preserves_only_exact_standard_stop_reasons() {
        use crate::runtime_surface::{
            NotAdmittedReason, OperationBudget, SurfaceShutdownReason, TurnRequestBudgetScope,
        };

        assert_eq!(
            terminal_to_stop_reason(&OperationTerminal::BudgetExhausted {
                budget: OperationBudget::ModelTokens {
                    limit: Some(100),
                    observed: Some(100),
                },
            }),
            Ok(StopReason::MaxTokens)
        );
        assert_eq!(
            terminal_to_stop_reason(&OperationTerminal::BudgetExhausted {
                budget: OperationBudget::TurnRequests {
                    scope: TurnRequestBudgetScope::AgentLoop,
                    limit: 8,
                    observed: 8,
                },
            }),
            Ok(StopReason::MaxTurnRequests)
        );
        assert!(
            terminal_to_stop_reason(&OperationTerminal::BudgetExhausted {
                budget: OperationBudget::TurnRequests {
                    scope: TurnRequestBudgetScope::Subagent,
                    limit: 4,
                    observed: 4,
                },
            })
            .expect_err("subagent budget is not the ACP agent-loop turn limit")
            .contains("Subagent")
        );
        assert_eq!(
            terminal_to_stop_reason(&OperationTerminal::NotAdmitted {
                reason: NotAdmittedReason::CancelledBeforeAdmission,
            }),
            Ok(StopReason::Cancelled)
        );
        assert_eq!(
            terminal_to_stop_reason(&OperationTerminal::Shutdown {
                reason: SurfaceShutdownReason::HostShutdown,
            }),
            Ok(StopReason::Cancelled)
        );
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
    fn permission_response_releases_exact_waiter() {
        let (bridge, mut requests) = AcpClientBridge::new();
        let waiter_bridge = Arc::clone(&bridge);
        let waiter = std::thread::spawn(move || {
            waiter_bridge.request_permission(permission_request("respond", "tool-1"))
        });
        let request = requests
            .blocking_recv()
            .expect("permission request is queued");
        bridge.complete_permission(
            &request.key,
            Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow_once")),
            )),
        );
        let response = waiter
            .join()
            .expect("permission waiter joins")
            .expect("permission response succeeds");
        assert!(matches!(
            response.outcome,
            RequestPermissionOutcome::Selected(_)
        ));
    }

    #[test]
    fn lost_subscription_reconciles_durable_terminal_without_cancelling_operation() {
        let host = RuntimeHost::start_with_executor(Arc::new(CompleteImmediatelyExecutor)).unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let surface_host = host.surface_handle();
        let config = test_run_config(cwd.path().to_path_buf());
        let thread =
            std::thread::spawn(move || surface_host.start_thread(config, "ACP reconcile").unwrap())
                .join()
                .unwrap();
        let surface = thread.acp_surface().expect("ACP surface");
        let input = decode_prompt_content(&[ContentBlock::from("complete".to_string())])
            .expect("decode typed prompt");
        let mut prepared =
            prepare_surface_prompt(&surface, &SessionId::new("reconcile"), input, 1, None).unwrap();
        let operation_id = prepared.operation_id.clone();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "operation did not reach a terminal event"
            );
            let Some(item) = prepared
                .subscription
                .recv_timeout(Duration::from_millis(50))
            else {
                continue;
            };
            let SurfaceSubscriptionItem::Batch { batch } = item else {
                continue;
            };
            if batch.events.as_slice().iter().any(|envelope| {
                matches!(
                    &envelope.event,
                    SurfaceEvent::Operation(OperationPatch::Terminal { record })
                        if record.operation_id == operation_id
                )
            }) {
                break;
            }
        }

        let error = reconcile_lost_subscription(&mut prepared, "gap").unwrap_err();
        assert!(error.contains("after durable terminal EndTurn"));
        drop(prepared);

        let snapshot = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Acp,
            requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
            _ => panic!("unexpected snapshot attachment"),
        };
        let terminal = snapshot
            .operation_history
            .iter()
            .chain(snapshot.foreground_operation.iter())
            .find(|operation| operation.operation_id == operation_id)
            .and_then(|operation| operation.terminal.as_ref())
            .expect("durable operation terminal");
        assert!(matches!(
            terminal.terminal,
            OperationTerminal::Succeeded { .. }
        ));
        host.shutdown().unwrap();
    }

    #[test]
    fn sealed_subscription_reconciles_terminal_from_retained_runtime_snapshot() {
        let host = RuntimeHost::start_with_executor(Arc::new(CompleteImmediatelyExecutor)).unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let surface_host = host.surface_handle();
        let config = test_run_config(cwd.path().to_path_buf());
        let thread =
            std::thread::spawn(move || surface_host.start_thread(config, "ACP sealed").unwrap())
                .join()
                .unwrap();
        let surface = thread.acp_surface().expect("ACP surface");
        let input = decode_prompt_content(&[ContentBlock::from("complete".to_string())])
            .expect("decode typed prompt");
        let mut prepared =
            prepare_surface_prompt(&surface, &SessionId::new("sealed"), input, 1, None).unwrap();
        let operation_id = prepared.operation_id.clone();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "operation did not reach terminal before shutdown"
            );
            let Some(item) = prepared
                .subscription
                .recv_timeout(Duration::from_millis(50))
            else {
                continue;
            };
            let SurfaceSubscriptionItem::Batch { batch } = item else {
                continue;
            };
            if batch.events.as_slice().iter().any(|envelope| {
                matches!(
                    &envelope.event,
                    SurfaceEvent::Operation(OperationPatch::Terminal { record })
                        if record.operation_id == operation_id
                )
            }) {
                break;
            }
        }

        host.shutdown().unwrap();
        let sealed = prepared
            .subscription
            .recv_timeout(Duration::from_secs(5))
            .expect("runtime shutdown seals the subscription");
        assert!(matches!(
            sealed,
            SurfaceSubscriptionItem::Sealed {
                reason: crate::runtime_surface::SurfaceSubscriptionSealReason::HostShutdown,
            }
        ));
        let error = reconcile_lost_subscription(&mut prepared, "sealed").unwrap_err();
        assert!(error.contains("after durable terminal EndTurn"));
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
        let (note_tx, mut note_rx) = mpsc::channel(8);
        let note_tx = AcpNotificationSender::Buffered(note_tx);
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

    fn test_run_config(cwd: PathBuf) -> RunConfig {
        RunConfig {
            app_version: "test".to_string(),
            prompt: String::new(),
            cwd: Some(cwd),
            output_format: OutputFormat::Jsonl,
            approval_mode: ApprovalMode::FullAuto,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Record,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: HashMap::new(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::default(),
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }
}
