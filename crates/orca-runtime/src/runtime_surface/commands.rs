use super::*;
use std::num::NonZeroU64;
use std::sync::Arc;

pub const SURFACE_COMMIT_BATCH_EVENT_LIMIT: u64 = 1_024;
pub const SURFACE_COMMIT_BATCH_BYTE_LIMIT: u64 = 8_388_608;
pub const SURFACE_RETAINED_EVENT_LIMIT: u64 = 8_192;
pub const SURFACE_RETAINED_BYTE_LIMIT: u64 = 33_554_432;
pub const SURFACE_SUBSCRIBER_EVENT_LIMIT: u64 = 1_024;
pub const SURFACE_SUBSCRIBER_BYTE_LIMIT: u64 = 8_388_608;
pub const ACP_MAX_INBOUND_LINE_BYTES: u64 = 8_388_608;
pub const ACP_MAX_OUTBOUND_FRAME_BYTES: u64 = 8_388_608;
pub const ACP_INGRESS_MESSAGE_LIMIT: u64 = 64;
pub const ACP_INGRESS_BYTE_LIMIT: u64 = 16_777_216;
pub const ACP_OUTGOING_MESSAGE_LIMIT: u64 = 256;
pub const ACP_OUTGOING_BYTE_LIMIT: u64 = 33_554_432;
pub const ACP_LOAD_GATE_MESSAGE_LIMIT: u64 = 4_096;
pub const ACP_LOAD_GATE_BYTE_LIMIT: u64 = 67_108_864;
pub const ACP_PROMPT_GATE_MESSAGE_LIMIT: u64 = 1_024;
pub const ACP_PROMPT_GATE_BYTE_LIMIT: u64 = 16_777_216;
pub const ACP_WRITE_FLUSH_DEADLINE_MS: u64 = 30_000;
pub const ACP_REVERSE_REQUEST_DEADLINE_MS: u64 = 120_000;
pub const ACP_CAPABILITY_CALL_DEADLINE_MS: u64 = 60_000;
pub const ACP_CAPABILITY_RESULT_CANONICAL_BYTE_LIMIT: u64 = 4_194_304;
pub const ACP_TERMINAL_KILL_DEADLINE_MS: u64 = 10_000;
pub const ACP_TERMINAL_RELEASE_DEADLINE_MS: u64 = 10_000;
pub const ACP_SUPERVISOR_JOIN_DEADLINE_MS: u64 = 5_000;
pub const ACP_TOMBSTONE_TTL_MS: u64 = 300_000;
pub const ACP_TOMBSTONE_LIMIT: u64 = 4_096;
pub const JSONL_REQUEST_TOMBSTONE_TTL_MS: u64 = 300_000;
pub const JSONL_REQUEST_TOMBSTONE_LIMIT: u64 = 4_096;
pub const JSONL_LIVE_REQUEST_LIMIT: u64 = 1_024;
pub const JSONL_REPAIR_AUTHORITY_LIMIT: u64 = 1_024;
pub const JSONL_COMMITTED_REPAIR_DRAIN_DEADLINE_MS: u64 = 5_000;
pub const JSONL_SUPERVISOR_JOIN_DEADLINE_MS: u64 = 5_000;

#[derive(Clone, PartialEq)]
pub enum SurfaceEvent {
    Operation(OperationPatch),
    Item(ItemPatch),
    Assistant(AssistantPatch),
    Tool(ToolPatch),
    Plan(SurfacePlanSnapshot),
    Usage(SurfaceUsageSnapshot),
    Context(SurfaceContextSnapshot),
    Interaction(InteractionPatch),
    Task(TaskPatch),
    Workflow(WorkflowPatch),
    Subagent(SubagentPatch),
    Goal(GoalPatchEnvelope),
    Settings(SettingsPatch),
    McpCatalog(McpCatalogPatch),
    PinnedContext(PinnedContextPatch),
    Session(SessionPatch),
}

#[derive(Clone, PartialEq)]
pub struct SurfaceEventEnvelope {
    pub ordinal: u32,
    pub event_id: SurfaceEventId,
    pub commit_class: CommitClass,
    pub scope: SurfaceScope,
    pub event: SurfaceEvent,
}

#[derive(Clone, PartialEq)]
pub struct SurfaceCommitBatch {
    pub cursor_before: SurfaceCursor,
    pub cursor_after: SurfaceCursor,
    pub commit_class: CommitClass,
    pub event_count: u32,
    pub batch_digest: Sha256Digest,
    pub events: NonEmptyVec<SurfaceEventEnvelope>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceCommitBatchPreflightResult {
    Ready {
        event_count: u32,
        canonical_encoded_bytes: u64,
        batch_digest: Sha256Digest,
    },
    Rejected {
        code: SurfaceCommitBatchPreflightErrorCode,
        observed_event_count: u64,
        observed_canonical_encoded_bytes: u64,
        event_limit: u64,
        byte_limit: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceCommitBatchPreflightErrorCode {
    CommitBatchTooLarge,
}

#[derive(Clone, PartialEq)]
pub struct SurfaceSnapshot {
    pub cursor: SurfaceCursor,
    pub thread: SurfaceThreadSnapshot,
    pub foreground_operation: Option<OperationRecord>,
    pub queued_operations: Vec<OperationRecord>,
    pub background_operations: Vec<SurfaceBackgroundOperation>,
    pub operation_history: Vec<OperationRecord>,
    pub items: Vec<SurfaceItem>,
    pub assistant_streams: Vec<SurfaceAssistantStream>,
    pub tools: Vec<SurfaceToolView>,
    pub plan: SurfacePlanSnapshot,
    pub usage: SurfaceUsageSnapshot,
    pub context: SurfaceContextSnapshot,
    pub interactions: Vec<SurfaceInteractionView>,
    pub tasks: Vec<SurfaceTask>,
    pub workflows: Vec<SurfaceWorkflow>,
    pub subagents: Vec<SurfaceSubagent>,
    pub goal: Option<SurfaceGoal>,
    pub settings: SurfaceSettingsSnapshot,
    pub mcp_catalog: SurfaceMcpCatalogSnapshot,
    pub pinned_context: SurfacePinnedContextSnapshot,
    pub session_health: SurfaceSessionHealth,
}

#[derive(Clone, PartialEq)]
pub struct SnapshotAtCursor {
    pub snapshot: Arc<SurfaceSnapshot>,
    pub cursor: SurfaceCursor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceAttachmentCapabilities {
    pub grant: SurfaceAttachmentGrant,
    pub interaction_kinds: Set<SurfaceInteractionKind>,
    pub acp_capability_revision: Option<CapabilityRevision>,
}

#[allow(dead_code)]
#[derive(Clone, Eq, PartialEq)]
pub struct SurfaceAttachAuthority {
    host_incarnation: HostIncarnation,
    thread_id: SurfaceThreadId,
    role: SurfaceAttachmentRole,
    maximum_capabilities: NonEmptySet<SurfaceCapability>,
    required_capabilities: NonEmptySet<SurfaceCapability>,
    maximum_interaction_kinds: Set<SurfaceInteractionKind>,
}

#[allow(dead_code)]
impl SurfaceAttachAuthority {
    pub(crate) fn new(
        host_incarnation: HostIncarnation,
        thread_id: SurfaceThreadId,
        role: SurfaceAttachmentRole,
        maximum_capabilities: NonEmptySet<SurfaceCapability>,
        required_capabilities: NonEmptySet<SurfaceCapability>,
        maximum_interaction_kinds: Set<SurfaceInteractionKind>,
    ) -> Self {
        Self {
            host_incarnation,
            thread_id,
            role,
            maximum_capabilities,
            required_capabilities,
            maximum_interaction_kinds,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachDeniedReason {
    RoleMismatch,
    MissingRequiredCapability,
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct SurfaceSubscriptionHandle(Arc<()>);

#[allow(dead_code)]
impl SurfaceSubscriptionHandle {
    pub(crate) fn new() -> Self {
        Self(Arc::new(()))
    }
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct RuntimeSurfaceClientHandle {
    attachment_id: SurfaceAttachmentId,
    thread_id: SurfaceThreadId,
    host_incarnation: HostIncarnation,
    capabilities: SurfaceAttachmentGrant,
    connection_id: Option<SurfaceConnectionId>,
}

#[allow(dead_code)]
impl RuntimeSurfaceClientHandle {
    pub(crate) fn new(
        attachment_id: SurfaceAttachmentId,
        thread_id: SurfaceThreadId,
        host_incarnation: HostIncarnation,
        capabilities: SurfaceAttachmentGrant,
        connection_id: Option<SurfaceConnectionId>,
    ) -> Self {
        Self {
            attachment_id,
            thread_id,
            host_incarnation,
            capabilities,
            connection_id,
        }
    }
}

#[derive(Clone)]
pub struct FreshSurfaceAttachment {
    pub attachment_id: SurfaceAttachmentId,
    pub client: RuntimeSurfaceClientHandle,
    pub baseline: SnapshotAtCursor,
    pub subscription: SurfaceSubscriptionHandle,
    pub capabilities: SurfaceAttachmentCapabilities,
}

#[derive(Clone)]
pub struct CursorSurfaceAttachment {
    pub attachment_id: SurfaceAttachmentId,
    pub client: RuntimeSurfaceClientHandle,
    pub from: SurfaceCursor,
    pub head: SurfaceCursor,
    pub replay: Vec<SurfaceCommitBatch>,
    pub subscription: SurfaceSubscriptionHandle,
    pub capabilities: SurfaceAttachmentCapabilities,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FreshAttachRequest {
    pub request_id: SurfaceRequestId,
    pub role: SurfaceAttachmentRole,
    pub requested_capabilities: Set<SurfaceCapability>,
    pub interaction_capabilities: Set<SurfaceInteractionKind>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorAttachRequest {
    pub request_id: SurfaceRequestId,
    pub cursor: SurfaceCursor,
    pub role: SurfaceAttachmentRole,
    pub requested_capabilities: Set<SurfaceCapability>,
    pub interaction_capabilities: Set<SurfaceInteractionKind>,
}

#[derive(Clone)]
pub enum AttachResult {
    FreshAttached { attachment: FreshSurfaceAttachment },
    CursorAttached { attachment: CursorSurfaceAttachment },
    Denied { reason: AttachDeniedReason },
    SnapshotRequired { required: SnapshotRequired },
    InvalidCursor { error: InvalidCursor },
    ThreadClosed { thread_id: SurfaceThreadId },
    Unavailable { reason: SurfaceUnavailableReason },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotRequiredReason {
    StaleIncarnation,
    ExpiredSuffix,
    ReplayHole,
    SlowSubscriber,
    ProjectionReset,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotRequired {
    pub reason: SnapshotRequiredReason,
    pub retained_from: Option<SurfaceCursor>,
    pub head: SurfaceCursor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidCursorReason {
    WrongThread,
    FutureSequence,
    ImpossibleSourceRevision,
    NotBatchBoundary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidCursor {
    pub reason: InvalidCursorReason,
    pub supplied: SurfaceCursor,
    pub expected_thread: SurfaceThreadId,
    pub head: SurfaceCursor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceSubscriptionSealReason {
    ThreadClosed,
    HostShutdown,
}

#[derive(Clone, PartialEq)]
pub enum SurfaceSubscriptionItem {
    Batch {
        batch: SurfaceCommitBatch,
    },
    Gap {
        required: SnapshotRequired,
    },
    Sealed {
        reason: SurfaceSubscriptionSealReason,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetachRequest {
    pub request_id: SurfaceRequestId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetachRevocationReceipt {
    pub request_id: SurfaceRequestId,
    pub attachment_id: SurfaceAttachmentId,
    pub revoked_grant_digest: Sha256Digest,
    pub affected_route_epochs: Vec<(SurfaceInteractionId, ResponseRouteEpoch)>,
    pub route_commit_id: Option<SurfaceCommitId>,
    pub route_cursor: Option<SurfaceCursor>,
}

#[derive(Clone, PartialEq)]
pub enum DetachResult {
    Detached {
        receipt: DetachRevocationReceipt,
    },
    AlreadyDetached {
        receipt: DetachRevocationReceipt,
    },
    Deferred {
        receipt: DetachRevocationReceipt,
        mutation: DeferredMutation,
    },
    StaleAttachment {
        request_id: SurfaceRequestId,
        attachment_id: SurfaceAttachmentId,
    },
}

#[derive(Clone)]
pub struct WaitOperationTerminalRequest {
    pub request_id: SurfaceRequestId,
    pub operation_id: SurfaceOperationId,
    pub caller_cancel: OptionalProcessLocalCancel,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationTerminalAtCursor {
    pub operation_id: SurfaceOperationId,
    pub terminal: OperationTerminal,
    pub cursor: SurfaceCursor,
    pub commit_class: CommitClass,
    pub batch_digest: Sha256Digest,
}

#[derive(Clone, PartialEq)]
pub enum WaitOperationTerminalResult {
    Terminal {
        value: OperationTerminalAtCursor,
    },
    TerminalCommitFailure {
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        commit_id: SurfaceCommitId,
        repair: RetryFinalizationToken,
    },
    TerminalProjectionFailure {
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        terminal_commit_id: SurfaceCommitId,
        terminal_event_id: SurfaceEventId,
        repair: RetryProjectionToken,
    },
    UnknownOperation {
        operation_id: SurfaceOperationId,
    },
    WrongThread {
        operation_id: SurfaceOperationId,
    },
    WaitCancelled {
        operation_id: SurfaceOperationId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MutationMemoryScope {
    User,
    Project { root: CanonicalPath },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MutationTarget {
    Host {
        host_incarnation: HostIncarnation,
    },
    Thread {
        thread_id: SurfaceThreadId,
    },
    Operation {
        thread_id: SurfaceThreadId,
        operation_id: SurfaceOperationId,
    },
    Generation {
        fence: SurfaceOperationFence,
    },
    Interaction {
        thread_id: SurfaceThreadId,
        interaction_id: SurfaceInteractionId,
    },
    Goal {
        goal_id: SurfaceGoalId,
    },
    Task {
        thread_id: SurfaceThreadId,
        task_id: SurfaceTaskId,
    },
    Workflow {
        thread_id: SurfaceThreadId,
        workflow_run_id: SurfaceWorkflowRunId,
    },
    Memory {
        scope: MutationMemoryScope,
    },
    FolderTrust {
        path: CanonicalPath,
    },
    RuntimeSettings {
        host_incarnation: HostIncarnation,
        thread_id: Option<SurfaceThreadId>,
    },
    SessionCatalog {
        thread_id: Option<SurfaceThreadId>,
    },
    SessionMetadata {
        thread_id: SurfaceThreadId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationDisposition {
    Accepted,
    Queued,
    AlreadyApplied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostDomainKind {
    Memory,
    FolderTrust,
    RuntimeSettings,
    SessionCatalog,
    SessionMetadata,
    HostLifecycle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyRevocationBarrierPlan {
    pub canonical_path: CanonicalPath,
    pub trust_revision: TrustRevision,
    pub policy_epoch: PolicyEpoch,
    pub expected_owner_leases: Vec<UuidV7>,
    pub expected_resources: Vec<NonEmptyText>,
    pub plan_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadCursorAckRequirement {
    pub thread_id: SurfaceThreadId,
    pub family: SurfaceFactFamily,
    pub event_id: SurfaceEventId,
    pub commit_id: SurfaceCommitId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostReceiptRequirementIdentity {
    Memory {
        scope: MutationMemoryScope,
        revision: MemoryRevision,
    },
    FolderTrust {
        path: CanonicalPath,
        revision: TrustRevision,
    },
    RuntimeSettings {
        host_incarnation: HostIncarnation,
        thread_id: Option<SurfaceThreadId>,
        revision: SettingsRevision,
    },
    SessionCatalog {
        thread_id: Option<SurfaceThreadId>,
        revision: SessionCatalogRevision,
    },
    SessionMetadata {
        thread_id: SurfaceThreadId,
        revision: SessionMetadataRevision,
    },
    HostLifecycle {
        host_incarnation: HostIncarnation,
        revision: HostLifecycleRevision,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostReceiptAckRequirement {
    pub host_incarnation: HostIncarnation,
    pub identity: HostReceiptRequirementIdentity,
    pub commit_id: SurfaceCommitId,
    pub receipt_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationTerminalAckRequirement {
    pub thread_id: SurfaceThreadId,
    pub thread_owner_epoch: ThreadOwnerEpoch,
    pub operation_id: SurfaceOperationId,
    pub terminal_commit_id: SurfaceCommitId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MutationAckRequirement {
    ThreadCursor(ThreadCursorAckRequirement),
    ThreadRemoteOwner {
        thread_id: SurfaceThreadId,
        thread_owner_epoch: ThreadOwnerEpoch,
        durable_revision: DurableRevision,
        commit_id: SurfaceCommitId,
    },
    HostReceipt(HostReceiptAckRequirement),
    GoalStoreReceipt {
        goal_id: SurfaceGoalId,
        store_commit_id: SurfaceCommitId,
        receipt_digest: Sha256Digest,
    },
    OperationTerminal(OperationTerminalAckRequirement),
    PolicyRevocationBarrier {
        plan: PolicyRevocationBarrierPlan,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FolderTrustLevel {
    Trusted,
    Untrusted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceMemoryReceipt {
    pub scope: MutationMemoryScope,
    pub record_id: SurfaceCatalogEntryId,
    pub memory_revision: MemoryRevision,
    pub display_path: CanonicalPath,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceFolderTrustReceipt {
    pub canonical_path: CanonicalPath,
    pub old_effective_level: FolderTrustLevel,
    pub new_effective_level: FolderTrustLevel,
    pub trust_revision: TrustRevision,
    pub policy_epoch: PolicyEpoch,
    pub reload_required: bool,
    pub reconciliation_proof: Option<Sha256Digest>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceRuntimeSettingsReceipt {
    pub host_revision: SettingsRevision,
    pub thread_revision: Option<SettingsRevision>,
    pub effective: SurfaceRuntimeSettings,
    pub pending: Option<SurfaceRuntimeSettings>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceSessionCatalogAction {
    Created,
    Opened,
    Loaded,
    Forked,
    Closed,
    Removed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceSessionCatalogReceipt {
    pub catalog_revision: SessionCatalogRevision,
    pub thread_id: Option<SurfaceThreadId>,
    pub action: SurfaceSessionCatalogAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceSessionMetadataReceipt {
    pub thread_id: SurfaceThreadId,
    pub metadata_revision: SessionMetadataRevision,
    pub title: DisplayText,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHostShutdownStage {
    Last,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceHostShutdownReceipt {
    pub host_incarnation: HostIncarnation,
    pub lifecycle_revision: HostLifecycleRevision,
    pub barrier_id: SurfaceSettlementId,
    pub shutdown_commit_id: SurfaceCommitId,
    pub stage: SurfaceHostShutdownStage,
    pub closed_at: UnixMillis,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostDomainReceipt {
    Memory(SurfaceMemoryReceipt),
    FolderTrust(SurfaceFolderTrustReceipt),
    RuntimeSettings(SurfaceRuntimeSettingsReceipt),
    SessionCatalog(SurfaceSessionCatalogReceipt),
    SessionMetadata(SurfaceSessionMetadataReceipt),
    HostLifecycle(SurfaceHostShutdownReceipt),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostReceiptIdentityPair {
    Memory {
        scope: MutationMemoryScope,
        revision: MemoryRevision,
        receipt: SurfaceMemoryReceipt,
    },
    FolderTrust {
        path: CanonicalPath,
        revision: TrustRevision,
        receipt: SurfaceFolderTrustReceipt,
    },
    RuntimeSettings {
        host_incarnation: HostIncarnation,
        thread_id: Option<SurfaceThreadId>,
        revision: SettingsRevision,
        receipt: SurfaceRuntimeSettingsReceipt,
    },
    SessionCatalog {
        thread_id: Option<SurfaceThreadId>,
        revision: SessionCatalogRevision,
        receipt: SurfaceSessionCatalogReceipt,
    },
    SessionMetadata {
        thread_id: SurfaceThreadId,
        revision: SessionMetadataRevision,
        receipt: SurfaceSessionMetadataReceipt,
    },
    HostLifecycle {
        host_incarnation: HostIncarnation,
        revision: HostLifecycleRevision,
        receipt: SurfaceHostShutdownReceipt,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MutationCommitAck {
    ThreadLocalCursor {
        cursor: SurfaceCursor,
        family: SurfaceFactFamily,
        event_id: SurfaceEventId,
        commit_class: CommitClass,
    },
    ThreadRemoteOwnerAck {
        thread_id: SurfaceThreadId,
        thread_owner_epoch: ThreadOwnerEpoch,
        durable_revision: DurableRevision,
        commit_id: SurfaceCommitId,
    },
    GoalStoreCommitAck {
        goal_id: SurfaceGoalId,
        receipt: SurfaceGoalStoreReceipt,
    },
    OperationTerminalAck {
        thread_id: SurfaceThreadId,
        thread_owner_epoch: ThreadOwnerEpoch,
        operation_id: SurfaceOperationId,
        value: OperationTerminalAtCursor,
    },
    PolicyRevocationBarrierAck {
        plan: PolicyRevocationBarrierPlan,
        settled_owner_leases: Vec<UuidV7>,
        settled_resources: Vec<NonEmptyText>,
        proof: Sha256Digest,
    },
    HostCommitAck {
        host_incarnation: HostIncarnation,
        identity: HostReceiptIdentityPair,
        commit_id: SurfaceCommitId,
        receipt_digest: Sha256Digest,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyRevocationSubject {
    OwnerLease(UuidV7),
    Resource(NonEmptyText),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownOperationSourcePhase {
    Requested,
    AdmittedReserved,
    AdmittedStarted,
    Suspended,
    BackgroundOwned,
    Finalizing,
    FinalizingDegraded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownRequestCause {
    HostShutdown,
    ThreadClose,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownSelectedCause {
    ExistingWinning { cause: OperationFinalizationCause },
    Requested { cause: ShutdownRequestCause },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownOperationPlan {
    ExistingTerminal {
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        terminal_commit_id: SurfaceCommitId,
        requirement: OperationTerminalAckRequirement,
    },
    PlannedFinalization {
        operation_id: SurfaceOperationId,
        source_phase: ShutdownOperationSourcePhase,
        finalize_intent_id: SurfaceFinalizeIntentId,
        terminal_commit_id: SurfaceCommitId,
        selected_cause: ShutdownSelectedCause,
        expected_settlements: Vec<SurfaceSettlementId>,
        requirement: OperationTerminalAckRequirement,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EphemeralThreadPersistence {
    EphemeralNonCataloguedOneShot {
        close_after: FirstOperationCompletionPolicy,
    },
    EphemeralAttached,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownThreadPlan {
    Recorded {
        thread_id: SurfaceThreadId,
        owner_epoch: ThreadOwnerEpoch,
        operations: Vec<ShutdownOperationPlan>,
        session_closed: ThreadCursorAckRequirement,
        catalog_closed: HostReceiptAckRequirement,
    },
    Ephemeral {
        thread_id: SurfaceThreadId,
        owner_epoch: ThreadOwnerEpoch,
        persistence: EphemeralThreadPersistence,
        operations: Vec<ShutdownOperationPlan>,
        session_closed: ThreadCursorAckRequirement,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownBarrierPlan {
    CloseThread {
        request_id: SurfaceRequestId,
        host_incarnation: HostIncarnation,
        thread: ShutdownThreadPlan,
        barrier_id: SurfaceSettlementId,
        closing_commit_id: SurfaceCommitId,
        plan_digest: Sha256Digest,
    },
    ShutdownHost {
        request_id: SurfaceRequestId,
        host_incarnation: HostIncarnation,
        threads: Vec<ShutdownThreadPlan>,
        barrier_id: SurfaceSettlementId,
        closing_commit_id: SurfaceCommitId,
        final_host_lifecycle: HostReceiptAckRequirement,
        plan_digest: Sha256Digest,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownThreadRequirement {
    OperationTerminal(OperationTerminalAckRequirement),
    SessionClosed(ThreadCursorAckRequirement),
    CatalogClosed(HostReceiptAckRequirement),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownMissing {
    Thread {
        thread_id: SurfaceThreadId,
        owner_epoch: ThreadOwnerEpoch,
        requirement: ShutdownThreadRequirement,
    },
    HostLifecycle {
        requirement: HostReceiptAckRequirement,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownScope {
    CloseThread {
        thread_id: SurfaceThreadId,
        owner_epoch: ThreadOwnerEpoch,
    },
    ShutdownHost {
        host_incarnation: HostIncarnation,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationDegradedState {
    pub settlement_id: SurfaceSettlementId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionDegradedState {
    pub durable_commit_id: SurfaceCommitId,
    pub fact_family: SurfaceFactFamily,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnerAckPendingState {
    pub thread_owner_epoch: ThreadOwnerEpoch,
    pub durable_revision: DurableRevision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartCommitDegradedState {
    pub generation_fence: SurfaceOperationFence,
    pub started_commit_id: SurfaceCommitId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingFinalizationDeferredState {
    pub operation_id: SurfaceOperationId,
    pub finalize_intent_id: SurfaceFinalizeIntentId,
    pub terminal_commit_id: SurfaceCommitId,
    pub missing_settlements: NonEmptyVec<SurfaceSettlementId>,
    pub missing_set_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalProjectionDeferredState {
    pub operation_id: SurfaceOperationId,
    pub finalize_intent_id: SurfaceFinalizeIntentId,
    pub terminal_commit_id: SurfaceCommitId,
    pub terminal_event_id: SurfaceEventId,
    pub durable_revision: DurableRevision,
    pub terminal_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FinalizingDegradedState {
    MissingFinalization(MissingFinalizationDeferredState),
    TerminalProjectionPending(TerminalProjectionDeferredState),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryPinPendingState {
    pub scope: MutationMemoryScope,
    pub record_id: SurfaceCatalogEntryId,
    pub memory_revision: MemoryRevision,
    pub thread_id: SurfaceThreadId,
    pub thread_owner_epoch: ThreadOwnerEpoch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyRevocationPendingState {
    pub plan: PolicyRevocationBarrierPlan,
    pub pending: NonEmptyVec<PolicyRevocationSubject>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownDeferredState {
    pub plan: ShutdownBarrierPlan,
    pub missing: NonEmptyVec<ShutdownMissing>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeferredMutationState {
    MutationDegraded(MutationDegradedState),
    ProjectionDegraded(ProjectionDegradedState),
    OwnerAckPending(OwnerAckPendingState),
    StartCommitDegraded(StartCommitDegradedState),
    FinalizingDegraded(FinalizingDegradedState),
    MemoryPinPending(MemoryPinPendingState),
    PolicyRevocationPending(PolicyRevocationPendingState),
    ShutdownDeferred(ShutdownDeferredState),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceMutationErrorCode {
    InvalidRequest,
    InvalidInput,
    CommitBatchTooLarge,
    InvalidContent,
    UnsupportedContent,
    UnsupportedOperation,
    CapabilityDenied,
    WrongHost,
    WrongThread,
    WrongAttachment,
    WrongOwnerEpoch,
    UnknownOperation,
    UnknownGeneration,
    UnknownInteraction,
    UnknownTask,
    UnknownWorkflow,
    UnknownGoal,
    NoActiveGoal,
    UnknownSession,
    StaleFence,
    StaleRevision,
    StaleLease,
    StaleResponseRoute,
    WrongInteractionKind,
    WrongResponseToken,
    WrongAuthorityFingerprint,
    IllegalState,
    OperationAlreadyTerminal,
    OperationActive,
    OperationNotInterrupted,
    OperationNotSteerable,
    AdmissionClosed,
    CapacityExceeded,
    ThreadOwnedElsewhere,
    ThreadClosed,
    HostShuttingDown,
    CommitFailed,
    StoreUnavailable,
    RuntimeUnavailable,
    StalePublisherPermit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceMutationRevision {
    Thread {
        cursor: SurfaceCursor,
    },
    Host {
        host_incarnation: HostIncarnation,
        revision: HostRevisionWitness,
    },
    SessionCatalog {
        revision: SessionCatalogRevision,
    },
    McpCatalog {
        thread_id: SurfaceThreadId,
        revision: McpCatalogRevision,
    },
    InputCatalog {
        revision: InputCatalogRevision,
    },
    WorkflowCatalog {
        revision: WorkflowCatalogRevision,
    },
    SessionMetadata {
        thread_id: SurfaceThreadId,
        revision: SessionMetadataRevision,
    },
    Settings {
        host_incarnation: HostIncarnation,
        thread_id: Option<SurfaceThreadId>,
        revision: SettingsRevision,
    },
    Trust {
        canonical_path: CanonicalPath,
        revision: TrustRevision,
        policy_epoch: PolicyEpoch,
    },
    Memory {
        scope: MutationMemoryScope,
        revision: MemoryRevision,
    },
    ProjectRootMemory {
        root: CanonicalPath,
        revision: ProjectRootMemoryRevision,
    },
    Plan {
        thread_id: SurfaceThreadId,
        revision: PlanRevision,
    },
    Usage {
        thread_id: SurfaceThreadId,
        revision: UsageRevision,
    },
    Context {
        thread_id: SurfaceThreadId,
        revision: ContextRevision,
    },
    Goal {
        goal_id: SurfaceGoalId,
        revision: GoalRevision,
        owner_epoch: GoalOwnerEpoch,
    },
    Task {
        thread_id: SurfaceThreadId,
        revision: TaskRevision,
    },
    Workflow {
        thread_id: SurfaceThreadId,
        workflow_run_id: SurfaceWorkflowRunId,
        revision: WorkflowRevision,
    },
    Interaction {
        thread_id: SurfaceThreadId,
        interaction_id: SurfaceInteractionId,
        revision: InteractionRevision,
        route_epoch: ResponseRouteEpoch,
    },
    PinnedContext {
        thread_id: SurfaceThreadId,
        revision: PinnedContextRevision,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceMutationError {
    pub code: SurfaceMutationErrorCode,
    pub message: DisplayText,
    pub winning_request_id: Option<SurfaceRequestId>,
    pub current_revision: Option<SurfaceMutationRevision>,
}

macro_rules! classified_mutation_error {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct $name(SurfaceMutationError);

        #[allow(dead_code)]
        impl $name {
            pub(crate) const fn new(error: SurfaceMutationError) -> Self {
                Self(error)
            }

            pub fn error(&self) -> &SurfaceMutationError {
                &self.0
            }
        }
    };
}

classified_mutation_error!(InvalidMutationError);
classified_mutation_error!(StaleMutationError);
classified_mutation_error!(UnavailableMutationError);
classified_mutation_error!(CommitFailedMutationError);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedMutation {
    pub request_id: SurfaceRequestId,
    pub target: MutationTarget,
    pub disposition: MutationDisposition,
    pub acknowledgements: NonEmptyVec<MutationCommitAck>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ReconcileMutationToken {
    request_id: SurfaceRequestId,
    target: MutationTarget,
    settlement_id: SurfaceSettlementId,
    expected_commit_id: SurfaceCommitId,
}

#[allow(dead_code)]
impl ReconcileMutationToken {
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        target: MutationTarget,
        settlement_id: SurfaceSettlementId,
        expected_commit_id: SurfaceCommitId,
    ) -> Self {
        Self {
            request_id,
            target,
            settlement_id,
            expected_commit_id,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RetryStartCommitToken {
    request_id: SurfaceRequestId,
    thread_owner_epoch: ThreadOwnerEpoch,
    fence: SurfaceOperationFence,
    started_commit_id: SurfaceCommitId,
}

#[allow(dead_code)]
impl RetryStartCommitToken {
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        thread_owner_epoch: ThreadOwnerEpoch,
        fence: SurfaceOperationFence,
        started_commit_id: SurfaceCommitId,
    ) -> Self {
        Self {
            request_id,
            thread_owner_epoch,
            fence,
            started_commit_id,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum RetryProjectionSelector {
    Local {
        fact_family: SurfaceFactFamily,
        event_id: SurfaceEventId,
    },
    Remote {
        durable_revision: DurableRevision,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct RetryProjectionToken {
    request_id: SurfaceRequestId,
    target: MutationTarget,
    durable_commit_id: SurfaceCommitId,
    expected_thread_owner_epoch: ThreadOwnerEpoch,
    selector: RetryProjectionSelector,
}

#[derive(Clone, Eq, PartialEq)]
pub struct RetryLocalProjectionToken(RetryProjectionToken);

#[derive(Clone, Eq, PartialEq)]
pub struct RetryRemoteProjectionToken(RetryProjectionToken);

#[allow(dead_code)]
impl RetryLocalProjectionToken {
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        target: MutationTarget,
        durable_commit_id: SurfaceCommitId,
        expected_thread_owner_epoch: ThreadOwnerEpoch,
        fact_family: SurfaceFactFamily,
        event_id: SurfaceEventId,
    ) -> Self {
        Self(RetryProjectionToken {
            request_id,
            target,
            durable_commit_id,
            expected_thread_owner_epoch,
            selector: RetryProjectionSelector::Local {
                fact_family,
                event_id,
            },
        })
    }

    pub(crate) fn as_token(&self) -> RetryProjectionToken {
        self.0.clone()
    }
}

#[allow(dead_code)]
impl RetryRemoteProjectionToken {
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        target: MutationTarget,
        durable_commit_id: SurfaceCommitId,
        expected_thread_owner_epoch: ThreadOwnerEpoch,
        durable_revision: DurableRevision,
    ) -> Self {
        Self(RetryProjectionToken {
            request_id,
            target,
            durable_commit_id,
            expected_thread_owner_epoch,
            selector: RetryProjectionSelector::Remote { durable_revision },
        })
    }

    pub(crate) fn as_token(&self) -> RetryProjectionToken {
        self.0.clone()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RetryFinalizationToken {
    request_id: SurfaceRequestId,
    thread_id: SurfaceThreadId,
    operation_id: SurfaceOperationId,
    finalize_intent_id: SurfaceFinalizeIntentId,
    terminal_commit_id: SurfaceCommitId,
    expected_thread_owner_epoch: ThreadOwnerEpoch,
    missing_set_digest: Sha256Digest,
}

#[allow(dead_code)]
impl RetryFinalizationToken {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        terminal_commit_id: SurfaceCommitId,
        expected_thread_owner_epoch: ThreadOwnerEpoch,
        missing_set_digest: Sha256Digest,
    ) -> Self {
        Self {
            request_id,
            thread_id,
            operation_id,
            finalize_intent_id,
            terminal_commit_id,
            expected_thread_owner_epoch,
            missing_set_digest,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Eq, PartialEq)]
enum ReconcileHostMutationTokenKind {
    Settlement {
        request_id: SurfaceRequestId,
        target: MutationTarget,
        settlement_id: SurfaceSettlementId,
        host_incarnation: HostIncarnation,
        expected_commit_id: SurfaceCommitId,
    },
    Shutdown {
        request_id: SurfaceRequestId,
        host_incarnation: HostIncarnation,
        scope: ShutdownScope,
        barrier_id: SurfaceSettlementId,
        closing_commit_id: SurfaceCommitId,
        barrier_plan_digest: Sha256Digest,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct ReconcileHostMutationToken(ReconcileHostMutationTokenKind);

#[derive(Clone, Eq, PartialEq)]
pub struct ReconcileHostSettlementToken(ReconcileHostMutationToken);

#[derive(Clone, Eq, PartialEq)]
pub struct ReconcileShutdownToken(ReconcileHostMutationToken);

#[allow(dead_code)]
impl ReconcileHostSettlementToken {
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        target: MutationTarget,
        settlement_id: SurfaceSettlementId,
        host_incarnation: HostIncarnation,
        expected_commit_id: SurfaceCommitId,
    ) -> Self {
        Self(ReconcileHostMutationToken(
            ReconcileHostMutationTokenKind::Settlement {
                request_id,
                target,
                settlement_id,
                host_incarnation,
                expected_commit_id,
            },
        ))
    }

    pub(crate) fn as_token(&self) -> ReconcileHostMutationToken {
        self.0.clone()
    }
}

#[allow(dead_code)]
impl ReconcileShutdownToken {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        host_incarnation: HostIncarnation,
        scope: ShutdownScope,
        barrier_id: SurfaceSettlementId,
        closing_commit_id: SurfaceCommitId,
        barrier_plan_digest: Sha256Digest,
    ) -> Self {
        Self(ReconcileHostMutationToken(
            ReconcileHostMutationTokenKind::Shutdown {
                request_id,
                host_incarnation,
                scope,
                barrier_id,
                closing_commit_id,
                barrier_plan_digest,
            },
        ))
    }

    pub(crate) fn as_token(&self) -> ReconcileHostMutationToken {
        self.0.clone()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ReconcileMemoryMutationToken {
    request_id: SurfaceRequestId,
    scope: MutationMemoryScope,
    memory_revision: MemoryRevision,
    record_id: SurfaceCatalogEntryId,
    pin_thread_id: SurfaceThreadId,
    expected_thread_owner_epoch: ThreadOwnerEpoch,
    expected_commit_id: SurfaceCommitId,
}

#[allow(dead_code)]
impl ReconcileMemoryMutationToken {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        scope: MutationMemoryScope,
        memory_revision: MemoryRevision,
        record_id: SurfaceCatalogEntryId,
        pin_thread_id: SurfaceThreadId,
        expected_thread_owner_epoch: ThreadOwnerEpoch,
        expected_commit_id: SurfaceCommitId,
    ) -> Self {
        Self {
            request_id,
            scope,
            memory_revision,
            record_id,
            pin_thread_id,
            expected_thread_owner_epoch,
            expected_commit_id,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ReconcileFolderTrustRevocationToken {
    request_id: SurfaceRequestId,
    expected_commit_id: SurfaceCommitId,
    plan: PolicyRevocationBarrierPlan,
}

#[allow(dead_code)]
impl ReconcileFolderTrustRevocationToken {
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        expected_commit_id: SurfaceCommitId,
        plan: PolicyRevocationBarrierPlan,
    ) -> Self {
        Self {
            request_id,
            expected_commit_id,
            plan,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum DeferredRepair {
    ThreadMutation {
        state: MutationDegradedState,
        token: ReconcileMutationToken,
    },
    HostMutation {
        state: MutationDegradedState,
        token: ReconcileHostSettlementToken,
    },
    Projection {
        state: ProjectionDegradedState,
        token: RetryLocalProjectionToken,
    },
    TerminalProjection {
        state: TerminalProjectionDeferredState,
        token: RetryLocalProjectionToken,
    },
    RemoteOwner {
        state: OwnerAckPendingState,
        token: RetryRemoteProjectionToken,
    },
    Start {
        state: StartCommitDegradedState,
        token: RetryStartCommitToken,
    },
    Finalization {
        state: MissingFinalizationDeferredState,
        token: RetryFinalizationToken,
    },
    MemoryPin {
        state: MemoryPinPendingState,
        token: ReconcileMemoryMutationToken,
    },
    Policy {
        state: PolicyRevocationPendingState,
        token: ReconcileFolderTrustRevocationToken,
    },
    Shutdown {
        state: ShutdownDeferredState,
        token: ReconcileShutdownToken,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct DeferredMutation {
    pub request_id: SurfaceRequestId,
    pub target: MutationTarget,
    pub commit_id: SurfaceCommitId,
    pub committed_acknowledgements: Vec<MutationCommitAck>,
    pub missing_acknowledgements: NonEmptyVec<MutationAckRequirement>,
    pub repair: DeferredRepair,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UncommittedMutation {
    Invalid {
        request_id: SurfaceRequestId,
        target: Option<MutationTarget>,
        error: InvalidMutationError,
    },
    Stale {
        request_id: SurfaceRequestId,
        target: Option<MutationTarget>,
        error: StaleMutationError,
    },
    Unavailable {
        request_id: SurfaceRequestId,
        target: Option<MutationTarget>,
        error: UnavailableMutationError,
    },
    CommitFailed {
        request_id: SurfaceRequestId,
        target: Option<MutationTarget>,
        error: CommitFailedMutationError,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub enum RuntimeSurfaceMutationResult {
    Committed(CommittedMutation),
    Deferred(DeferredMutation),
    Uncommitted(UncommittedMutation),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeferredCommandValue<T> {
    NoValue,
    Provisional { value: T },
}

#[derive(Clone, Eq, PartialEq)]
pub enum MutationReply<T> {
    Committed {
        mutation: CommittedMutation,
        value: T,
    },
    Deferred {
        mutation: DeferredMutation,
        partial: DeferredCommandValue<T>,
    },
    Uncommitted {
        mutation: UncommittedMutation,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedMutationReplay<T> {
    pub request_id: SurfaceRequestId,
    pub canonical_command_digest: Sha256Digest,
    pub target: MutationTarget,
    pub value: T,
    pub acknowledgements: NonEmptyVec<MutationCommitAck>,
}

#[derive(Clone, Eq, PartialEq)]
pub enum BackgroundTarget {
    ReservedOperation {
        operation_id: SurfaceOperationId,
        admission_lease_id: SurfaceAdmissionLeaseId,
    },
    ActiveGeneration {
        fence: SurfaceOperationFence,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResumeSourceWitness {
    DurableReplay { replayability_digest: Sha256Digest },
    LiveCapsule { incarnation: SurfaceIncarnation },
}

#[derive(Clone, PartialEq)]
pub enum InteractionSelector {
    Exact {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        kind: SurfaceInteractionKind,
        response_token: SurfaceResponseToken,
        response_route_epoch: ResponseRouteEpoch,
        response_grant_token: SurfaceResponseGrantToken,
        operation_fence: SurfaceOperationFence,
    },
    OpaqueRequestId {
        opaque_request_id: NonEmptyText,
        expected_kind: SurfaceInteractionKind,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub enum TaskControlAction {
    Stop { fence: SurfaceTaskFence },
    Foreground { fence: SurfaceTaskFence },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkflowControlAction {
    Launch {
        catalog_entry_id: SurfaceCatalogEntryId,
        observed_catalog_revision: WorkflowCatalogRevision,
        args: Vec<(NonEmptyText, DisplayText)>,
        parent: Option<SurfaceOperationFence>,
    },
    Pause {
        fence: SurfaceWorkflowFence,
    },
    Resume {
        fence: SurfaceWorkflowFence,
    },
    Stop {
        fence: SurfaceWorkflowFence,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalRunInput {
    Supplied {
        request: SurfaceInputRequest,
    },
    DerivedFromGoal {
        goal_id: SurfaceGoalId,
        objective_revision: GoalObjectiveRevision,
        goal_receipt_digest: Sha256Digest,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub enum ExpectedGoal {
    None,
    Exact(SurfaceGoalFence),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalTokenBudgetUpdate {
    Keep,
    Set(Option<i64>),
}

#[derive(Clone, Eq, PartialEq)]
pub enum GoalMutationAction {
    SetAndRun {
        expected_goal: ExpectedGoal,
        objective: NonEmptyText,
        token_budget: Option<i64>,
        input: GoalRunInput,
    },
    Edit {
        fence: SurfaceGoalFence,
        objective: NonEmptyText,
        token_budget: GoalTokenBudgetUpdate,
    },
    Clear {
        fence: SurfaceGoalFence,
    },
    ResumeAndRun {
        fence: SurfaceGoalFence,
        input: GoalRunInput,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PinnedContextAction {
    Add {
        expected_revision: PinnedContextRevision,
        entry: SurfacePinnedContextEntry,
        memory_receipt: Option<(SurfaceCatalogEntryId, MemoryRevision)>,
    },
    Remove {
        expected_revision: PinnedContextRevision,
        entry_id: SurfaceCatalogEntryId,
    },
    Clear {
        expected_revision: PinnedContextRevision,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpCatalogFamily {
    Tools,
    Resources,
    ResourceTemplates,
}

#[derive(Clone, Eq, PartialEq)]
pub struct McpCatalogCursor {
    pub thread_id: SurfaceThreadId,
    pub revision: McpCatalogRevision,
    pub family: McpCatalogFamily,
    pub offset: u64,
    pub cursor_authenticator: OpaqueToken,
}

#[derive(Clone, Eq, PartialEq)]
pub enum McpCatalogQuery {
    ListTools {
        cursor: Option<McpCatalogCursor>,
        limit: u32,
    },
    ListResources {
        cursor: Option<McpCatalogCursor>,
        limit: u32,
    },
    ListResourceTemplates {
        cursor: Option<McpCatalogCursor>,
        limit: u32,
    },
    Lookup {
        id: SurfaceCatalogEntryId,
    },
}

pub enum SurfaceCommand {
    ReserveOperation {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        intent: OperationRequestIntent,
    },
    AdmitReserved {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        operation_id: SurfaceOperationId,
        admission_lease_id: SurfaceAdmissionLeaseId,
    },
    CancelOperation {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        operation_id: SurfaceOperationId,
    },
    CancelSessionCurrent {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        legacy_rpc_id_digest: Sha256Digest,
    },
    InterruptGeneration {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        fence: SurfaceOperationFence,
    },
    PauseGoalOperation {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        goal_fence: SurfaceGoalFence,
    },
    ResumeOperation {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        operation_id: SurfaceOperationId,
        expected_last_generation: SurfaceGenerationId,
        resume_source: ResumeSourceWitness,
    },
    SteerOperation {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        fence: SurfaceOperationFence,
        input: SurfaceInputRequest,
    },
    TransferBackground {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        target: BackgroundTarget,
    },
    RespondInteraction {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        selector: InteractionSelector,
        response: BoundInteractionResponse,
    },
    ReconcileMutation {
        token: ReconcileMutationToken,
    },
    RetryStartCommit {
        token: RetryStartCommitToken,
    },
    RetryProjection {
        token: RetryProjectionToken,
    },
    RetryFinalization {
        token: RetryFinalizationToken,
    },
    ManualCompact {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        expected_context_revision: ContextRevision,
    },
    Backtrack {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        expected_cursor: SurfaceCursor,
        target: LastUserTurn,
    },
    TaskControl {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        action: TaskControlAction,
    },
    WorkflowControl {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        action: WorkflowControlAction,
    },
    GoalMutation {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        action: GoalMutationAction,
    },
    SettingsMutation {
        request_id: SurfaceRequestId,
        caller: SurfaceHostBoundCaller,
        host_incarnation: HostIncarnation,
        expected_thread_revision: SettingsRevision,
        patch: RuntimeSettingsPatch,
    },
    McpCatalogQuery {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        expected_revision: Option<McpCatalogRevision>,
        query: McpCatalogQuery,
    },
    PinnedContextMutation {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        action: PinnedContextAction,
    },
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct OperationWaiterHandle(Arc<()>);

#[allow(dead_code)]
impl OperationWaiterHandle {
    pub(crate) fn new() -> Self {
        Self(Arc::new(()))
    }
}

#[derive(Clone)]
pub struct ReservedOperationOutput {
    pub operation_id: SurfaceOperationId,
    pub lease: ReservationLease,
    pub requested_cursor: SurfaceCursor,
    pub waiter: OperationWaiterHandle,
}

#[derive(Clone)]
pub enum AdmissionOutput {
    Queued {
        operation_id: SurfaceOperationId,
        queue_position: u32,
        lease: ReservationLease,
        waiter: OperationWaiterHandle,
    },
    Admitted {
        operation_id: SurfaceOperationId,
        first_generation: SurfaceOperationFence,
        admitted_cursor: SurfaceCursor,
        waiter: OperationWaiterHandle,
    },
}

#[derive(Clone)]
pub enum CancelOperationOutput {
    CancelledBeforeAdmission {
        terminal: OperationTerminalAtCursor,
    },
    Accepted {
        operation_id: SurfaceOperationId,
        accepted_cursor: SurfaceCursor,
        waiter: OperationWaiterHandle,
    },
    AlreadyTerminal {
        terminal: OperationTerminalAtCursor,
    },
    FinalizationPending {
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        finalization_cursor: FinalizationStartedAtCursor,
        waiter: OperationWaiterHandle,
    },
}

#[derive(Clone)]
pub enum CancelSessionCurrentResult {
    NoCurrentOperation {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
    },
    Resolved {
        mutation: MutationReply<CancelOperationOutput>,
    },
}

#[derive(Clone)]
pub struct InterruptOutput {
    pub fence: SurfaceOperationFence,
    pub accepted_cursor: SurfaceCursor,
    pub settlement: InterruptSettlement,
    pub waiter: OperationWaiterHandle,
}

#[derive(Clone)]
pub enum PauseGoalOperationOutput {
    None,
    CancelledBeforeAdmission {
        terminal: OperationTerminalAtCursor,
    },
    Cancelling {
        operation_id: SurfaceOperationId,
        accepted_cursor: SurfaceCursor,
        waiter: OperationWaiterHandle,
    },
}

#[derive(Clone)]
pub struct PauseGoalOutput {
    pub goal: SurfaceGoal,
    pub goal_receipt: SurfaceGoalStoreReceipt,
    pub goal_cursor: SurfaceCursor,
    pub operation: PauseGoalOperationOutput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeTransitionRole {
    ResumeStarting,
    GenerationReserved,
    GenerationStarted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumeTransitionReceipt {
    pub role: ResumeTransitionRole,
    pub event_id: SurfaceEventId,
    pub cursor: SurfaceCursor,
    pub commit_class: CommitClass,
}

#[derive(Clone)]
pub struct ResumeOperationOutput {
    pub operation_id: SurfaceOperationId,
    pub generation: SurfaceOperationFence,
    pub resume_starting: ResumeTransitionReceipt,
    pub generation_reserved: ResumeTransitionReceipt,
    pub generation_started: ResumeTransitionReceipt,
    pub waiter: OperationWaiterHandle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SteerOutput {
    pub fence: SurfaceOperationFence,
    pub input_item_id: SurfaceItemId,
    pub committed_cursor: SurfaceCursor,
}

#[derive(Clone)]
pub enum TransferBackgroundOutput {
    QueuedOnStart {
        operation_id: SurfaceOperationId,
        intent_cursor: SurfaceCursor,
    },
    HandedOff {
        background_fence: SurfaceBackgroundFence,
        handoff_cursor: SurfaceCursor,
        waiter: OperationWaiterHandle,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RespondInteractionDisposition {
    Resolved {
        receipt: SurfaceInteractionResolutionReceipt,
    },
    AlreadyResolved {
        winning_receipt: SurfaceInteractionResolutionReceipt,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RespondInteractionOutput {
    pub interaction_id: SurfaceInteractionId,
    pub attempted_response_id: SurfaceResponseId,
    pub disposition: RespondInteractionDisposition,
    pub projected_cursor: Option<SurfaceCursor>,
}

#[derive(Clone)]
pub struct MaintenanceOperationOutput {
    pub operation_id: SurfaceOperationId,
    pub admitted_cursor: SurfaceCursor,
    pub waiter: OperationWaiterHandle,
}

#[derive(Clone, PartialEq)]
pub struct TaskControlOutput {
    pub task: SurfaceTask,
    pub cursor: SurfaceCursor,
}

#[derive(Clone)]
pub struct WorkflowControlOutput {
    pub workflow: SurfaceWorkflow,
    pub operation_id: Option<SurfaceOperationId>,
    pub cursor: SurfaceCursor,
    pub waiter: Option<OperationWaiterHandle>,
}

#[derive(Clone)]
pub struct GoalMutationOutput {
    pub goal: Option<SurfaceGoal>,
    pub goal_receipt: SurfaceGoalStoreReceipt,
    pub change_cursor: SurfaceCursor,
    pub operation_id: Option<SurfaceOperationId>,
    pub waiter: Option<OperationWaiterHandle>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingsMutationOutput {
    pub settings: SurfaceSettingsSnapshot,
    pub cursor: SurfaceCursor,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SurfaceCatalogEntry {
    McpTool(SurfaceMcpTool),
    McpResource(SurfaceMcpResource),
    McpResourceTemplate(SurfaceMcpResourceTemplate),
}

#[derive(Clone, Debug, PartialEq)]
pub enum McpCatalogPageValues {
    Tools(Vec<SurfaceMcpTool>),
    Resources(Vec<SurfaceMcpResource>),
    ResourceTemplates(Vec<SurfaceMcpResourceTemplate>),
    Entry(SurfaceCatalogEntry),
}

#[derive(Clone, PartialEq)]
pub struct McpCatalogPage {
    pub revision: McpCatalogRevision,
    pub values: McpCatalogPageValues,
    pub next_cursor: Option<McpCatalogCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinnedContextMutationOutput {
    pub snapshot: SurfacePinnedContextSnapshot,
    pub cursor: SurfaceCursor,
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceReadRevision {
    Host {
        host_incarnation: HostIncarnation,
        revision: HostRevisionWitness,
    },
    SessionCatalog {
        revision: SessionCatalogRevision,
    },
    McpCatalog {
        thread_id: SurfaceThreadId,
        revision: McpCatalogRevision,
    },
    InputCatalog {
        revision: InputCatalogRevision,
    },
    WorkflowCatalog {
        revision: WorkflowCatalogRevision,
    },
    Thread {
        cursor: SurfaceCursor,
    },
    Session {
        token: SessionReadToken,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceReadErrorCode {
    InvalidRequest,
    InvalidCursor,
    CapabilityDenied,
    NotFound,
    StaleRevision,
    ThreadOwnedElsewhere,
    ThreadClosed,
    StoreUnavailable,
    RuntimeUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceReadErrorClass {
    NotFound,
    Invalid,
    Stale,
    Unavailable,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SurfaceReadError {
    pub class: SurfaceReadErrorClass,
    pub code: SurfaceReadErrorCode,
    pub message: DisplayText,
    pub current_revision: Option<SurfaceReadRevision>,
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceReadResult<T> {
    Found {
        request_id: SurfaceRequestId,
        revision: SurfaceReadRevision,
        value: T,
    },
    NotFound {
        request_id: SurfaceRequestId,
        error: SurfaceReadError,
    },
    Invalid {
        request_id: SurfaceRequestId,
        error: SurfaceReadError,
    },
    Stale {
        request_id: SurfaceRequestId,
        error: SurfaceReadError,
    },
    Unavailable {
        request_id: SurfaceRequestId,
        error: SurfaceReadError,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSortKey {
    CreatedAt,
    UpdatedAt,
    RecencyAt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionListArchiveFilter {
    ActiveOnly,
    ArchivedOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSearchArchiveFilter {
    ActiveOnly,
    ActiveAndArchived,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionRelationFilter {
    DirectChildrenOf { parent_thread_id: SurfaceThreadId },
    DescendantsOf { ancestor_thread_id: SurfaceThreadId },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionSetFilter<T: Ord> {
    Any,
    Match(NonEmptySet<T>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfacePageLimit {
    ClientBounded {
        value: u32,
    },
    LegacyJsonl {
        wire_value: u64,
        effective: NonZeroU64,
    },
}

impl SurfacePageLimit {
    pub fn try_session_catalog(value: u32) -> Result<Self, SurfaceValueError> {
        Self::try_client_bounded(value, 100)
    }

    pub fn try_thread_page(value: u32) -> Result<Self, SurfaceValueError> {
        Self::try_client_bounded(value, 500)
    }

    pub fn legacy_jsonl(wire_value: u64) -> Self {
        Self::LegacyJsonl {
            wire_value,
            effective: NonZeroU64::new(wire_value).unwrap_or(NonZeroU64::MIN),
        }
    }

    fn try_client_bounded(value: u32, maximum: u32) -> Result<Self, SurfaceValueError> {
        if value == 0 {
            return Err(SurfaceValueError::Zero);
        }
        if value > maximum {
            return Err(SurfaceValueError::TooLong {
                maximum: maximum as usize,
                observed: value as usize,
            });
        }
        Ok(Self::ClientBounded { value })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionListFilter {
    pub cwd: Vec<CanonicalPath>,
    pub providers: SessionSetFilter<NonEmptyText>,
    pub models: SessionSetFilter<NonEmptyText>,
    pub relation: Option<SessionRelationFilter>,
    pub archived: SessionListArchiveFilter,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionCatalogCursor {
    pub catalog_revision: SessionCatalogRevision,
    pub sort_key: SessionSortKey,
    pub direction: SortDirection,
    pub query_digest: Sha256Digest,
    pub last_value_digest: Sha256Digest,
    pub last_thread_id: SurfaceThreadId,
    pub cursor_authenticator: OpaqueToken,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyJsonlPageCursor {
    pub wire_value: DisplayText,
    pub effective_offset: u64,
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceSessionPageCursor {
    Typed(SessionCatalogCursor),
    LegacyJsonl(LegacyJsonlPageCursor),
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionPageRequest {
    pub filters: SessionListFilter,
    pub search_term: Option<NonEmptyText>,
    pub sort_key: SessionSortKey,
    pub direction: SortDirection,
    pub cursor: Option<SurfaceSessionPageCursor>,
    pub limit: SurfacePageLimit,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionSearchRequest {
    pub query: NonEmptyText,
    pub archived: SessionSearchArchiveFilter,
    pub sort_key: SessionSortKey,
    pub direction: SortDirection,
    pub cursor: Option<SurfaceSessionPageCursor>,
    pub limit: SurfacePageLimit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceSessionSummary {
    pub thread_id: SurfaceThreadId,
    pub title: DisplayText,
    pub cwd: CanonicalPath,
    pub provider: NonEmptyText,
    pub model: Option<NonEmptyText>,
    pub created_at: Rfc3339Timestamp,
    pub updated_at: Rfc3339Timestamp,
    pub parent_thread_id: Option<SurfaceThreadId>,
    pub forked: bool,
    pub archived: bool,
    pub approval_mode: Option<SurfaceApprovalMode>,
    pub active_permission_profile: Option<SurfaceActivePermissionProfile>,
    pub permission_rule_count: u64,
    pub runtime_workspace_roots: Vec<CanonicalPath>,
    pub additional_working_directories: Vec<SurfaceAdditionalWorkingDirectory>,
    pub network_permissions: SurfaceNetworkPermissions,
    pub message_count: u64,
    pub turn_count: u64,
    pub metadata_revision: SessionMetadataRevision,
    pub running: bool,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SurfaceSessionSummaryPage {
    pub catalog_revision: SessionCatalogRevision,
    pub data: Vec<SurfaceSessionSummary>,
    pub next_cursor: Option<SurfaceSessionPageCursor>,
    pub backwards_cursor: Option<SurfaceSessionPageCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceSessionSearchHit {
    pub thread: SurfaceSessionSummary,
    pub snippet: DisplayText,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SurfaceSessionSearchPage {
    pub catalog_revision: SessionCatalogRevision,
    pub data: Vec<SurfaceSessionSearchHit>,
    pub next_cursor: Option<SurfaceSessionPageCursor>,
    pub backwards_cursor: Option<SurfaceSessionPageCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionReadToken {
    pub thread_id: SurfaceThreadId,
    pub durable_revision: DurableRevision,
    pub metadata_revision: SessionMetadataRevision,
    pub snapshot_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceSessionMetadata {
    pub summary: SurfaceSessionSummary,
    pub runtime_workspace_roots: Vec<CanonicalPath>,
    pub active_permission_profile: Option<SurfaceActivePermissionProfile>,
    pub permission_rules: SurfacePermissionRuleSet,
    pub additional_working_directories: Vec<SurfaceAdditionalWorkingDirectory>,
    pub network_permissions: SurfaceNetworkPermissions,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SurfaceHistoryId(NonEmptyText);

impl SurfaceHistoryId {
    pub fn try_new(value: impl Into<String>) -> Result<Self, SurfaceValueError> {
        NonEmptyText::try_new(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistorySystemRole {
    System,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryUserRole {
    User,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryAssistantRole {
    Assistant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryToolRole {
    Tool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryStatus {
    InProgressSnakeCase,
    InProgressCamelCase,
    Running,
    Completed,
    Failed,
    NotImplementedSnakeCase,
    Cancelled,
    Indeterminate,
    Denied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryTerminalStatus {
    Completed,
    Failed,
    NotImplementedSnakeCase,
    Cancelled,
    Indeterminate,
    Denied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryRunningStatus {
    Running,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryToolKind {
    Success,
    Empty,
    NoMatches,
    Truncated,
    PermissionDenied,
    InvalidInput,
    RuntimeError,
    Cancelled,
    Indeterminate,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SurfaceHistoryMessage {
    System {
        role: SurfaceHistorySystemRole,
        content: DisplayText,
    },
    User {
        role: SurfaceHistoryUserRole,
        content: DisplayText,
    },
    Assistant {
        role: SurfaceHistoryAssistantRole,
        content: Option<DisplayText>,
        reasoning_content: Option<DisplayText>,
        tool_calls: Vec<SurfaceDataValue>,
    },
    Tool {
        role: SurfaceHistoryToolRole,
        tool_call_id: SurfaceHistoryId,
        content: DisplayText,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileChangeKind {
    Edit,
    Write,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceHistoryFileChange {
    pub path: Option<DisplayText>,
    pub kind: FileChangeKind,
    pub diff: SurfaceDataValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SurfaceHistoryItem {
    PersistedMessage {
        message: SurfaceHistoryMessage,
    },
    UserMessage {
        content: DisplayText,
    },
    AgentMessage {
        id: SurfaceHistoryId,
        text: DisplayText,
    },
    Plan {
        id: SurfaceHistoryId,
        text: DisplayText,
    },
    Reasoning {
        id: SurfaceHistoryId,
        summary: DisplayText,
        content: DisplayText,
    },
    CommandExecution {
        id: SurfaceHistoryId,
        tool: NonEmptyText,
        command: Option<DisplayText>,
        cwd: Option<CanonicalPath>,
        process_id: Option<SurfaceHistoryId>,
        source: Option<NonEmptyText>,
        status: SurfaceHistoryStatus,
        command_actions: Vec<SurfaceDataValue>,
        aggregated_output: Option<DisplayText>,
        error: Option<SurfaceDataValue>,
        exit_code: Option<i32>,
        truncated: Option<bool>,
        duration_ms: Option<u64>,
        kind: Option<SurfaceHistoryToolKind>,
        terminal_source: Option<ToolTerminalSource>,
        invocation_started: Option<ToolInvocationStarted>,
    },
    ToolResult {
        tool_call_id: SurfaceHistoryId,
        content: DisplayText,
        status: Option<SurfaceHistoryStatus>,
        error: Option<SurfaceDataValue>,
        exit_code: Option<i32>,
        truncated: Option<bool>,
        kind: Option<SurfaceHistoryToolKind>,
        terminal_source: Option<ToolTerminalSource>,
        invocation_started: Option<ToolInvocationStarted>,
    },
    McpToolCall {
        id: SurfaceHistoryId,
        server: NonEmptyText,
        tool: NonEmptyText,
        status: SurfaceHistoryStatus,
        arguments: SurfaceDataValue,
        result: SurfaceDataValue,
        error: SurfaceDataValue,
        truncated: Option<bool>,
        kind: Option<SurfaceHistoryToolKind>,
        terminal_source: Option<ToolTerminalSource>,
        invocation_started: Option<ToolInvocationStarted>,
    },
    DynamicToolCall {
        id: SurfaceHistoryId,
        namespace: Option<NonEmptyText>,
        tool: NonEmptyText,
        status: SurfaceHistoryStatus,
        arguments: SurfaceDataValue,
        content_items: Option<Vec<SurfaceDataValue>>,
        success: Option<bool>,
        error: Option<SurfaceDataValue>,
        truncated: Option<bool>,
        kind: Option<SurfaceHistoryToolKind>,
        terminal_source: Option<ToolTerminalSource>,
        invocation_started: Option<ToolInvocationStarted>,
    },
    FileChange {
        id: SurfaceHistoryId,
        status: SurfaceHistoryStatus,
        changes: NonEmptyVec<SurfaceHistoryFileChange>,
        error: Option<SurfaceDataValue>,
        kind: Option<SurfaceHistoryToolKind>,
        terminal_source: Option<ToolTerminalSource>,
        invocation_started: Option<ToolInvocationStarted>,
    },
    WorkflowStarted {
        id: SurfaceHistoryId,
        workflow_name: NonEmptyText,
        task_id: SurfaceHistoryId,
        status: SurfaceHistoryRunningStatus,
        task: SurfaceDataValue,
    },
    WorkflowTerminal {
        id: SurfaceHistoryId,
        workflow_name: NonEmptyText,
        task_id: SurfaceHistoryId,
        status: SurfaceHistoryTerminalStatus,
        result: SurfaceDataValue,
        error: SurfaceDataValue,
        task: SurfaceDataValue,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnItemsView {
    NotLoaded,
    Summary,
    Full,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadItemTurnFilter {
    Any,
    Exact(SurfaceHistoryId),
    MatchNone,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadPageQuery {
    Messages {
        direction: SortDirection,
    },
    Turns {
        direction: SortDirection,
        items_view: TurnItemsView,
    },
    Items {
        turn: ThreadItemTurnFilter,
        direction: SortDirection,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct ThreadPageCursor {
    pub read_token: SessionReadToken,
    pub query_digest: Sha256Digest,
    pub next_ordinal: u64,
    pub cursor_authenticator: OpaqueToken,
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceThreadPageCursor {
    Typed(ThreadPageCursor),
    LegacyJsonl(LegacyJsonlPageCursor),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceHistoryTurn {
    pub thread_id: SurfaceHistoryId,
    pub turn_id: SurfaceHistoryId,
    pub index: u64,
    pub role: SurfaceHistoryRole,
    pub items_view: TurnItemsView,
    pub items: Vec<SurfaceHistoryItem>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceHistoryItemEntry {
    pub thread_id: SurfaceHistoryId,
    pub turn_id: SurfaceHistoryId,
    pub item_id: SurfaceHistoryId,
    pub index: u64,
    pub item: SurfaceHistoryItem,
}

#[derive(Clone, PartialEq)]
pub enum SurfaceThreadPage {
    Messages {
        read_token: SessionReadToken,
        data: Vec<SurfaceHistoryMessage>,
        next_cursor: Option<SurfaceThreadPageCursor>,
        backwards_cursor: Option<SurfaceThreadPageCursor>,
    },
    Turns {
        read_token: SessionReadToken,
        data: Vec<SurfaceHistoryTurn>,
        next_cursor: Option<SurfaceThreadPageCursor>,
        backwards_cursor: Option<SurfaceThreadPageCursor>,
    },
    Items {
        read_token: SessionReadToken,
        data: Vec<SurfaceHistoryItemEntry>,
        next_cursor: Option<SurfaceThreadPageCursor>,
        backwards_cursor: Option<SurfaceThreadPageCursor>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceSessionReadBundle {
    pub metadata: SurfaceSessionMetadata,
    pub read_token: SessionReadToken,
    pub messages: Vec<SurfaceHistoryMessage>,
    pub turns: Vec<SurfaceHistoryTurn>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadSessionMetadataOutput {
    pub metadata: SurfaceSessionMetadata,
    pub read_token: SessionReadToken,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretReference {
    Environment { name: NonEmptyText },
    HostSecretStore { key: NonEmptyText },
}

pub enum SurfaceMcpValue {
    LiteralNonSecret { value: DisplayText },
    Secret { reference: SecretReference },
    EphemeralSecret { value: ZeroizingProcessLocalSecret },
}

pub enum SurfaceMcpTransport {
    Stdio {
        command: NonEmptyText,
        args: Vec<SurfaceMcpValue>,
        env: Vec<(NonEmptyText, SurfaceMcpValue)>,
    },
    Sse {
        url: CanonicalUri,
        headers: Vec<(NonEmptyText, SurfaceMcpValue)>,
    },
}

pub struct SurfaceMcpServerDeclaration {
    pub name: NonEmptyText,
    pub transport: SurfaceMcpTransport,
    pub startup_timeout: DurationMillis,
    pub tool_timeout: DurationMillis,
    pub disabled: bool,
}

pub struct SurfaceThreadCreateSpec {
    pub title: DisplayText,
    pub persistence: ThreadPersistence,
    pub settings_overrides: Vec<RuntimeSettingsPatch>,
    pub mcp_servers: Vec<SurfaceMcpServerDeclaration>,
    pub parent_thread_id: Option<SurfaceThreadId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenThreadMode {
    LiveOnly,
    LiveOrMaterialize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveOnly {
    LiveOnly,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionMetadataPrecondition {
    Exact { revision: SessionMetadataRevision },
    LegacyLastWriteWins,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionMetadataPatch {
    SetTitle { title: DisplayText },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryScope {
    User {
        expected_memory_revision: Option<MemoryRevision>,
    },
    Project {
        canonical_root: CanonicalPath,
        expected_root_revision: ProjectRootMemoryRevision,
        expected_memory_revision: Option<MemoryRevision>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeSettingsTarget {
    HostDefaults,
    Thread { thread_id: SurfaceThreadId },
    HostDefaultsAndThread { thread_id: SurfaceThreadId },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSettingsExpectedRevision {
    pub host: SettingsRevision,
    pub thread: Option<SettingsRevision>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct InputCatalogCursor {
    pub revision: InputCatalogRevision,
    pub context_digest: Sha256Digest,
    pub query_digest: Sha256Digest,
    pub offset: u64,
    pub cursor_authenticator: OpaqueToken,
}

#[derive(Clone, Eq, PartialEq)]
pub enum InputCatalogQuery {
    Search {
        query: DisplayText,
        kinds: Set<SurfaceInputBindingKind>,
        cursor: Option<InputCatalogCursor>,
        limit: u32,
    },
    Lookup {
        id: SurfaceCatalogEntryId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputCatalogContext {
    HostDefaults {
        host_incarnation: HostIncarnation,
        settings_revision: SettingsRevision,
    },
    Thread {
        thread_id: SurfaceThreadId,
        settings_revision: SettingsRevision,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JsonlTurnControlAction {
    Interrupt,
    Resume,
    Steer { input: SurfaceInputRequest },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonlResolvedTurnControlStatus {
    Interrupted,
    Resumed,
    Steered,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonlTurnControlWireAction {
    Interrupt,
    Resume,
    Steer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonlIdleTurnControlStatus {
    Idle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonlResolvedTurnControlWireEcho {
    pub legacy_turn_id: LegacyTurnId,
    pub action: JsonlTurnControlWireAction,
    pub status: JsonlResolvedTurnControlStatus,
    pub legacy_input: Option<DisplayText>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonlIdleTurnControlWireEcho {
    pub legacy_turn_id: LegacyTurnId,
    pub action: JsonlTurnControlWireAction,
    pub status: JsonlIdleTurnControlStatus,
    pub legacy_input: Option<DisplayText>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonlTurnControlledOutput {
    pub operation_id: SurfaceOperationId,
    pub echo: JsonlResolvedTurnControlWireEcho,
    pub committed_cursor: SurfaceCursor,
    pub input_item_id: Option<SurfaceItemId>,
}

#[derive(Clone, Eq, PartialEq)]
pub enum JsonlTurnControlResult {
    Idle {
        request_id: SurfaceRequestId,
        echo: JsonlIdleTurnControlWireEcho,
    },
    Resolved {
        mutation: MutationReply<JsonlTurnControlledOutput>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceInputCatalogEntry {
    pub id: SurfaceCatalogEntryId,
    pub kind: SurfaceInputBindingKind,
    pub label: NonEmptyText,
    pub description: Option<DisplayText>,
    pub catalog_revision: InputCatalogRevision,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SurfaceInputCatalogPage {
    pub revision: InputCatalogRevision,
    pub data: Vec<SurfaceInputCatalogEntry>,
    pub next_cursor: Option<InputCatalogCursor>,
}

pub enum SurfaceHostCommand {
    ListSessions {
        request_id: SurfaceRequestId,
        page: SessionPageRequest,
    },
    SearchSessions {
        request_id: SurfaceRequestId,
        search: SessionSearchRequest,
    },
    ReadSessionMetadata {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
    },
    ReadSession {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        include_messages: bool,
        include_turns: bool,
    },
    ReadThreadPage {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        query: ThreadPageQuery,
        read_token: Option<SessionReadToken>,
        cursor: Option<SurfaceThreadPageCursor>,
        limit: SurfacePageLimit,
    },
    CreateThread {
        request_id: SurfaceRequestId,
        spec: SurfaceThreadCreateSpec,
    },
    OpenThread {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        mode: OpenThreadMode,
        expected_settings_digest: Option<Sha256Digest>,
    },
    LoadThread {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        expected_settings_digest: Option<Sha256Digest>,
        settings_overrides: Vec<RuntimeSettingsPatch>,
        mcp_servers: Vec<SurfaceMcpServerDeclaration>,
    },
    ForkThread {
        request_id: SurfaceRequestId,
        source_thread_id: SurfaceThreadId,
        source_read_token: SessionReadToken,
        title: Option<DisplayText>,
        settings_overrides: Vec<RuntimeSettingsPatch>,
    },
    ResolveRunningThread {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        mode: LiveOnly,
    },
    ResumeLatestActiveGoal {
        request_id: SurfaceRequestId,
        expected_goal_store_revision: Option<GoalCatalogRevision>,
    },
    UpdateSessionMetadata {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        precondition: SessionMetadataPrecondition,
        patch: SessionMetadataPatch,
    },
    QueryInputCatalog {
        request_id: SurfaceRequestId,
        context: InputCatalogContext,
        expected_revision: Option<InputCatalogRevision>,
        query: InputCatalogQuery,
    },
    ControlJsonlTurn {
        request_id: SurfaceRequestId,
        expected_thread_id: Option<SurfaceThreadId>,
        legacy_turn_id: LegacyTurnId,
        action: JsonlTurnControlAction,
    },
    RememberMemory {
        request_id: SurfaceRequestId,
        scope: MemoryScope,
        note: NonEmptyText,
        pin_to_thread: Option<SurfaceThreadId>,
    },
    ReconcileMemoryMutation {
        token: ReconcileMemoryMutationToken,
    },
    ReadFolderTrust {
        request_id: SurfaceRequestId,
        path: CanonicalPath,
    },
    SetFolderTrust {
        request_id: SurfaceRequestId,
        path: CanonicalPath,
        expected_trust_revision: TrustRevision,
        level: FolderTrustLevel,
    },
    ReconcileFolderTrustRevocation {
        token: ReconcileFolderTrustRevocationToken,
    },
    ReadRuntimeSettings {
        request_id: SurfaceRequestId,
        thread_id: Option<SurfaceThreadId>,
    },
    UpdateRuntimeSettings {
        request_id: SurfaceRequestId,
        target: RuntimeSettingsTarget,
        expected: RuntimeSettingsExpectedRevision,
        patch: NonEmptyVec<RuntimeSettingsPatch>,
    },
    ReconcileHostMutation {
        token: ReconcileHostMutationToken,
    },
    CloseThread {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        expected_owner_epoch: Option<ThreadOwnerEpoch>,
    },
    ShutdownHost {
        request_id: SurfaceRequestId,
        host_incarnation: HostIncarnation,
    },
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct RuntimeSurfaceHostHandle {
    host_incarnation: HostIncarnation,
    grant: NonEmptySet<SurfaceCapability>,
    connection_id: Option<SurfaceConnectionId>,
}

#[allow(dead_code)]
impl RuntimeSurfaceHostHandle {
    pub(crate) fn new(
        host_incarnation: HostIncarnation,
        grant: NonEmptySet<SurfaceCapability>,
        connection_id: Option<SurfaceConnectionId>,
    ) -> Self {
        Self {
            host_incarnation,
            grant,
            connection_id,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct RuntimeSurfaceHandle {
    host_incarnation: HostIncarnation,
    thread_id: SurfaceThreadId,
    authority: SurfaceAttachAuthority,
}

#[allow(dead_code)]
impl RuntimeSurfaceHandle {
    pub(crate) fn new(
        host_incarnation: HostIncarnation,
        thread_id: SurfaceThreadId,
        authority: SurfaceAttachAuthority,
    ) -> Self {
        Self {
            host_incarnation,
            thread_id,
            authority,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateThreadMaterialization {
    Created,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ForkThreadMaterialization {
    Forked { source_thread_id: SurfaceThreadId },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadSettingsReceipt {
    Unchanged {
        host_revision: SettingsRevision,
        thread_revision: Option<SettingsRevision>,
    },
    Committed {
        receipt: SurfaceRuntimeSettingsReceipt,
    },
}

#[derive(Clone)]
pub enum CreateThreadOutput {
    Recorded {
        surface: RuntimeSurfaceHandle,
        thread: SurfaceThreadSnapshot,
        materialization: CreateThreadMaterialization,
        catalog_receipt: SurfaceSessionCatalogReceipt,
        settings_receipt: ThreadSettingsReceipt,
    },
    Ephemeral {
        surface: RuntimeSurfaceHandle,
        thread: SurfaceThreadSnapshot,
        materialization: CreateThreadMaterialization,
        settings_receipt: ThreadSettingsReceipt,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenThreadMaterialization {
    AttachedLive,
    MaterializedLive,
}

#[derive(Clone)]
pub struct OpenThreadOutput {
    pub surface: RuntimeSurfaceHandle,
    pub thread: SurfaceThreadSnapshot,
    pub materialization: OpenThreadMaterialization,
    pub catalog_receipt: SurfaceSessionCatalogReceipt,
    pub settings_receipt: ThreadSettingsReceipt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadThreadRecovery {
    Clean,
    RecoveryRequired,
    FinalizationReconciled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadThreadMaterialization {
    LoadedCold { recovery: LoadThreadRecovery },
}

#[derive(Clone)]
pub struct LoadThreadOutput {
    pub surface: RuntimeSurfaceHandle,
    pub thread: SurfaceThreadSnapshot,
    pub materialization: LoadThreadMaterialization,
    pub catalog_receipt: SurfaceSessionCatalogReceipt,
    pub settings_receipt: ThreadSettingsReceipt,
}

#[derive(Clone)]
pub struct ForkThreadOutput {
    pub surface: RuntimeSurfaceHandle,
    pub thread: SurfaceThreadSnapshot,
    pub materialization: ForkThreadMaterialization,
    pub catalog_receipt: SurfaceSessionCatalogReceipt,
    pub settings_receipt: ThreadSettingsReceipt,
}

#[derive(Clone)]
pub struct ResolveRunningThreadOutput {
    pub surface: RuntimeSurfaceHandle,
    pub thread: SurfaceThreadSnapshot,
}

#[derive(Clone)]
pub struct ResumeLatestGoalOutput {
    pub surface: RuntimeSurfaceHandle,
    pub goal: SurfaceGoal,
    pub goal_receipt: SurfaceGoalStoreReceipt,
    pub goal_cursor: SurfaceCursor,
    pub operation_id: SurfaceOperationId,
    pub operation_cursor: SurfaceCursor,
    pub waiter: OperationWaiterHandle,
    pub catalog_receipt: SurfaceSessionCatalogReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryPinResult {
    NotRequested,
    Committed {
        thread_id: SurfaceThreadId,
        cursor: SurfaceCursor,
    },
    Pending {
        thread_id: SurfaceThreadId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryMutationOutput {
    pub memory_receipt: SurfaceMemoryReceipt,
    pub pin: MemoryPinResult,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderTrustRead {
    pub canonical_path: CanonicalPath,
    pub matched_ancestor: CanonicalPath,
    pub effective_level: FolderTrustLevel,
    pub trust_revision: TrustRevision,
    pub policy_epoch: PolicyEpoch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderTrustMutationOutput {
    pub receipt: SurfaceFolderTrustReceipt,
    pub barrier_plan: PolicyRevocationBarrierPlan,
    pub pending: Vec<PolicyRevocationSubject>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSettingsRead {
    pub host_revision: SettingsRevision,
    pub thread_revision: Option<SettingsRevision>,
    pub effective: SurfaceRuntimeSettings,
    pub pending: Option<SurfaceRuntimeSettings>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSettingsMutationOutput {
    pub receipt: SurfaceRuntimeSettingsReceipt,
    pub thread_cursor: Option<SurfaceCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionMetadataMutationOutput {
    pub metadata: SurfaceSessionMetadata,
    pub receipt: SurfaceSessionMetadataReceipt,
    pub thread_cursor: Option<SurfaceCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClosedThreadReceipt {
    Recorded {
        thread_id: SurfaceThreadId,
        operation_terminals: Vec<OperationTerminalAtCursor>,
        closed_cursor: SurfaceCursor,
        catalog_receipt: SurfaceSessionCatalogReceipt,
    },
    Ephemeral {
        thread_id: SurfaceThreadId,
        persistence: EphemeralThreadPersistence,
        operation_terminals: Vec<OperationTerminalAtCursor>,
        closed_cursor: SurfaceCursor,
    },
}

pub type CloseThreadOutput = ClosedThreadReceipt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownHostOutput {
    pub host_incarnation: HostIncarnation,
    pub host_receipt: SurfaceHostShutdownReceipt,
    pub closed_threads: Vec<ClosedThreadReceipt>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetainedShutdownOutput {
    CloseThread { output: CloseThreadOutput },
    ShutdownHost { output: ShutdownHostOutput },
}

#[derive(Clone, Eq, PartialEq)]
pub enum ReconcileHostMutationOutput {
    Settlement {
        result: RuntimeSurfaceMutationResult,
    },
    CloseThread {
        result: MutationReply<CloseThreadOutput>,
    },
    ShutdownHost {
        result: MutationReply<ShutdownHostOutput>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownBarrierState {
    Closing,
    Closed {
        retained_output: RetainedShutdownOutput,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownBarrierRecord {
    pub plan: ShutdownBarrierPlan,
    pub settled: Vec<MutationCommitAck>,
    pub state: ShutdownBarrierState,
}

pub struct StoreProviderCredential {
    pub request_id: SurfaceRequestId,
    pub provider: NonEmptyText,
    pub secret: ZeroizingProcessLocalSecret,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreProviderCredentialError {
    InvalidInput,
    StoreUnavailable,
    PermissionDenied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreProviderCredentialResult {
    Committed {
        credential_revision: BootstrapCredentialRevision,
        provider: NonEmptyText,
    },
    Uncommitted {
        error: StoreProviderCredentialError,
    },
}
