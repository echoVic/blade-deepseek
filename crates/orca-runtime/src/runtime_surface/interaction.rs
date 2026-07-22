use super::*;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum SurfaceInteractionKind {
    ToolApproval,
    PermissionRequest,
    UserInput,
    McpElicitation,
    BackgroundApproval,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InteractionExpiryDeadline {
    pub issuing_host_incarnation: HostIncarnation,
    pub expires_at: MonotonicInstant,
    pub observed_expires_at: Option<UnixMillis>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InteractionExpiryAuthorityFailure {
    ClockIdMismatch {
        expected: HostMonotonicClockId,
        observed: HostMonotonicClockId,
    },
    TickArithmeticOverflow {
        clock_id: HostMonotonicClockId,
    },
    IssuingHostLost {
        clock_id: HostMonotonicClockId,
        issuing_host_incarnation: HostIncarnation,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InteractionUnavailableDisposition {
    FailOperation,
    AwaitCapableAttachment { deadline: InteractionExpiryDeadline },
}

#[derive(Clone, Eq, PartialEq)]
pub enum BrokerInteractionResponseRoute {
    Unassigned {
        epoch: ResponseRouteEpoch,
    },
    Exclusive {
        epoch: ResponseRouteEpoch,
        attachment_id: SurfaceAttachmentId,
        grant_token: SurfaceResponseGrantToken,
    },
    SharedFirstCommitWins {
        epoch: ResponseRouteEpoch,
        grants: NonEmptyVec<(SurfaceAttachmentId, SurfaceResponseGrantToken)>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceInteractionRoute {
    Unassigned {
        epoch: ResponseRouteEpoch,
    },
    Exclusive {
        epoch: ResponseRouteEpoch,
        attachment_id: SurfaceAttachmentId,
    },
    SharedFirstCommitWins {
        epoch: ResponseRouteEpoch,
        attachments: NonEmptySet<SurfaceAttachmentId>,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct AuthorityFingerprint {
    operation_id: SurfaceOperationId,
    request_digest: Sha256Digest,
    tool_digest: Sha256Digest,
    cwd: CanonicalPath,
    workspace_roots_digest: Sha256Digest,
    policy_epoch: PolicyEpoch,
    executable_generation: Sha256Digest,
    artifact_generation: Sha256Digest,
    capability_digest: Sha256Digest,
}

#[derive(Serialize)]
pub(super) struct CanonicalAuthorityFingerprintV1<'a> {
    operation_id: &'a SurfaceOperationId,
    request_digest: &'a Sha256Digest,
    tool_digest: &'a Sha256Digest,
    cwd: &'a CanonicalPath,
    workspace_roots_digest: &'a Sha256Digest,
    policy_epoch: PolicyEpoch,
    executable_generation: &'a Sha256Digest,
    artifact_generation: &'a Sha256Digest,
    capability_digest: &'a Sha256Digest,
}

fn canonical_authority_fingerprint_v1(
    authority: &AuthorityFingerprint,
) -> CanonicalAuthorityFingerprintV1<'_> {
    CanonicalAuthorityFingerprintV1 {
        operation_id: &authority.operation_id,
        request_digest: &authority.request_digest,
        tool_digest: &authority.tool_digest,
        cwd: &authority.cwd,
        workspace_roots_digest: &authority.workspace_roots_digest,
        policy_epoch: authority.policy_epoch,
        executable_generation: &authority.executable_generation,
        artifact_generation: &authority.artifact_generation,
        capability_digest: &authority.capability_digest,
    }
}

#[allow(dead_code)]
impl AuthorityFingerprint {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        operation_id: SurfaceOperationId,
        request_digest: Sha256Digest,
        tool_digest: Sha256Digest,
        cwd: CanonicalPath,
        workspace_roots_digest: Sha256Digest,
        policy_epoch: PolicyEpoch,
        executable_generation: Sha256Digest,
        artifact_generation: Sha256Digest,
        capability_digest: Sha256Digest,
    ) -> Self {
        Self {
            operation_id,
            request_digest,
            tool_digest,
            cwd,
            workspace_roots_digest,
            policy_epoch,
            executable_generation,
            artifact_generation,
            capability_digest,
        }
    }

    pub fn operation_id(&self) -> &SurfaceOperationId {
        &self.operation_id
    }

    pub fn request_digest(&self) -> &Sha256Digest {
        &self.request_digest
    }

    pub fn tool_digest(&self) -> &Sha256Digest {
        &self.tool_digest
    }

    pub fn cwd(&self) -> &CanonicalPath {
        &self.cwd
    }

    pub fn workspace_roots_digest(&self) -> &Sha256Digest {
        &self.workspace_roots_digest
    }

    pub const fn policy_epoch(&self) -> PolicyEpoch {
        self.policy_epoch
    }

    pub fn executable_generation(&self) -> &Sha256Digest {
        &self.executable_generation
    }

    pub fn artifact_generation(&self) -> &Sha256Digest {
        &self.artifact_generation
    }

    pub fn capability_digest(&self) -> &Sha256Digest {
        &self.capability_digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SurfacePermissionPathLabel(pub DisplayText);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SurfacePermissionDomainPattern(pub DisplayText);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceFileSystemPermissionProfile {
    pub read: Option<Vec<SurfacePermissionPathLabel>>,
    pub write: Option<Vec<SurfacePermissionPathLabel>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceShellPermissionProfile {
    pub unsandboxed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceAllowDeny {
    Allow,
    Deny,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfacePermissionNetworkProfile {
    pub enabled: Option<bool>,
    pub domains: Vec<(SurfacePermissionDomainPattern, SurfaceAllowDeny)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfacePermissionProfile {
    pub file_system: Option<SurfaceFileSystemPermissionProfile>,
    pub network: Option<SurfacePermissionNetworkProfile>,
    pub shell: Option<SurfaceShellPermissionProfile>,
}

impl SurfacePermissionProfile {
    pub const fn empty() -> Self {
        Self {
            file_system: None,
            network: None,
            shell: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PermissionGrantScope {
    Turn,
    Session,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NegativeI64(i64);

impl NegativeI64 {
    pub fn try_new(value: i64) -> Result<Self, SurfaceValueError> {
        if value >= 0 {
            return Err(SurfaceValueError::NonCanonical);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for NegativeI64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(i64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceSchemaInteger {
    Negative(NegativeI64),
    NonNegative(u64),
}

impl SurfaceSchemaInteger {
    pub fn try_negative(value: i64) -> Result<Self, SurfaceValueError> {
        NegativeI64::try_new(value).map(Self::Negative)
    }

    pub const fn non_negative(value: u64) -> Self {
        Self::NonNegative(value)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SurfaceSchema {
    String {
        title: Option<DisplayText>,
        description: Option<DisplayText>,
        enum_values: Vec<DisplayText>,
        min_length: Option<u64>,
        max_length: Option<u64>,
    },
    Integer {
        title: Option<DisplayText>,
        description: Option<DisplayText>,
        minimum: Option<SurfaceSchemaInteger>,
        maximum: Option<SurfaceSchemaInteger>,
        enum_values: Vec<SurfaceSchemaInteger>,
    },
    Number {
        title: Option<DisplayText>,
        description: Option<DisplayText>,
        minimum: Option<FiniteF64>,
        maximum: Option<FiniteF64>,
    },
    Boolean {
        title: Option<DisplayText>,
        description: Option<DisplayText>,
    },
    Array {
        title: Option<DisplayText>,
        description: Option<DisplayText>,
        items: Box<SurfaceSchema>,
        min_items: Option<u64>,
        max_items: Option<u64>,
    },
    Object {
        title: Option<DisplayText>,
        description: Option<DisplayText>,
        properties: Vec<SurfaceSchemaProperty>,
        additional_properties: Denied,
    },
    Unsupported {
        schema_digest: Sha256Digest,
        unsupported_keywords: NonEmptyVec<NonEmptyText>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SurfaceSchemaProperty {
    pub name: DisplayText,
    pub required: bool,
    pub schema: Box<SurfaceSchema>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SurfaceDataValue {
    Null,
    Boolean(bool),
    Integer(NegativeI64),
    Unsigned(u64),
    Number(FiniteF64),
    String(DisplayText),
    Array(Vec<SurfaceDataValue>),
    Object(Vec<SurfaceDataProperty>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SurfaceDataProperty {
    pub name: DisplayText,
    pub value: Box<SurfaceDataValue>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceToolAction {
    Read,
    Write,
    Network,
    Agent,
    Shell,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceToolRequest {
    pub tool_call_id: SurfaceToolCallId,
    pub source_response_id: Option<UuidV7>,
    pub turn_id: SurfaceTurnId,
    pub name: NonEmptyText,
    pub action: SurfaceToolAction,
    pub target: Option<DisplayText>,
    pub raw_arguments: DisplayText,
    pub arguments_digest: Sha256Digest,
}

#[derive(Clone, PartialEq)]
pub enum SurfaceInteractionRequest {
    ToolApproval {
        tool: SurfaceToolRequest,
        description: DisplayText,
        preview: Option<DisplayText>,
        authority: AuthorityFingerprint,
    },
    PermissionRequest {
        tool_call_id: SurfaceToolCallId,
        reason: Option<DisplayText>,
        permissions: SurfacePermissionProfile,
        authority: AuthorityFingerprint,
    },
    UserInput {
        question: NonEmptyText,
        suggestions: Vec<DisplayText>,
    },
    McpElicitation {
        server_name: NonEmptyText,
        server_request_id: NonEmptyText,
        message: DisplayText,
        request: SurfaceMcpElicitationRequest,
    },
    BackgroundApproval {
        task: SurfaceTaskFence,
        tool: SurfaceToolRequest,
        authority: AuthorityFingerprint,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SurfaceMcpElicitationRequest {
    Form {
        requested_schema: Option<SurfaceDataValue>,
        supported_schema: Option<SurfaceSchema>,
    },
    Url {
        raw_url: Option<DisplayText>,
        requested_schema: Option<SurfaceDataValue>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfacePermissionClientDecision {
    Allow {
        scope: PermissionGrantScope,
        permissions: SurfacePermissionProfile,
        strict_auto_review: bool,
    },
    Deny {
        scope: PermissionGrantScope,
        permissions: SurfacePermissionProfile,
        strict_auto_review: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceUserInputDecision {
    Answer(DisplayText),
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SurfaceMcpElicitationDecision {
    Accept { content: SurfaceDataValue },
    Decline,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SurfaceClientInteractionAnswer {
    ToolApproval {
        decision: SurfaceAllowDeny,
    },
    PermissionRequest {
        decision: SurfacePermissionClientDecision,
    },
    UserInput {
        decision: SurfaceUserInputDecision,
    },
    McpElicitation {
        decision: SurfaceMcpElicitationDecision,
    },
    BackgroundApproval {
        decision: SurfaceAllowDeny,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrokerInteractionAnswerPolicy {
    NativeStrict,
    LegacyJsonlV0250PermissionProfile {
        connection_id: SurfaceConnectionId,
        policy_epoch: PolicyEpoch,
    },
    LegacyJsonlV0250McpOpaqueContent {
        connection_id: SurfaceConnectionId,
    },
}

#[derive(Clone, Eq, PartialEq)]
enum ApplicableAuthorityFingerprintKind {
    NotApplicable,
    Persisted { authority: AuthorityFingerprint },
}

#[derive(Clone, Eq, PartialEq)]
pub struct ApplicableAuthorityFingerprint(ApplicableAuthorityFingerprintKind);

#[allow(dead_code)]
impl ApplicableAuthorityFingerprint {
    pub(crate) const fn not_applicable() -> Self {
        Self(ApplicableAuthorityFingerprintKind::NotApplicable)
    }

    pub(crate) fn persisted(authority: AuthorityFingerprint) -> Self {
        Self(ApplicableAuthorityFingerprintKind::Persisted { authority })
    }

    pub fn authority(&self) -> Option<&AuthorityFingerprint> {
        match &self.0 {
            ApplicableAuthorityFingerprintKind::NotApplicable => None,
            ApplicableAuthorityFingerprintKind::Persisted { authority } => Some(authority),
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct BoundInteractionResponse {
    response_id: SurfaceResponseId,
    answer: SurfaceClientInteractionAnswer,
    policy: BrokerInteractionAnswerPolicy,
    authority: ApplicableAuthorityFingerprint,
}

#[allow(dead_code)]
impl BoundInteractionResponse {
    pub(crate) fn new(
        response_id: SurfaceResponseId,
        answer: SurfaceClientInteractionAnswer,
        policy: BrokerInteractionAnswerPolicy,
        authority: ApplicableAuthorityFingerprint,
    ) -> Self {
        Self {
            response_id,
            answer,
            policy,
            authority,
        }
    }

    pub fn response_id(&self) -> &SurfaceResponseId {
        &self.response_id
    }

    pub fn answer(&self) -> &SurfaceClientInteractionAnswer {
        &self.answer
    }

    pub fn policy(&self) -> &BrokerInteractionAnswerPolicy {
        &self.policy
    }

    pub fn authority(&self) -> &ApplicableAuthorityFingerprint {
        &self.authority
    }
}

#[derive(Clone, PartialEq)]
pub struct ValidatedInteractionResponse {
    interaction_id: SurfaceInteractionId,
    response_id: SurfaceResponseId,
    answer: SurfaceClientInteractionAnswer,
    policy: BrokerInteractionAnswerPolicy,
    authority: ApplicableAuthorityFingerprint,
    route_epoch: ResponseRouteEpoch,
    operation_fence: SurfaceOperationFence,
}

#[allow(dead_code)]
impl ValidatedInteractionResponse {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        interaction_id: SurfaceInteractionId,
        response_id: SurfaceResponseId,
        answer: SurfaceClientInteractionAnswer,
        policy: BrokerInteractionAnswerPolicy,
        authority: ApplicableAuthorityFingerprint,
        route_epoch: ResponseRouteEpoch,
        operation_fence: SurfaceOperationFence,
    ) -> Self {
        Self {
            interaction_id,
            response_id,
            answer,
            policy,
            authority,
            route_epoch,
            operation_fence,
        }
    }

    pub fn interaction_id(&self) -> &SurfaceInteractionId {
        &self.interaction_id
    }

    pub fn response_id(&self) -> &SurfaceResponseId {
        &self.response_id
    }

    pub fn answer(&self) -> &SurfaceClientInteractionAnswer {
        &self.answer
    }

    pub fn policy(&self) -> &BrokerInteractionAnswerPolicy {
        &self.policy
    }

    pub fn authority(&self) -> &ApplicableAuthorityFingerprint {
        &self.authority
    }

    pub const fn route_epoch(&self) -> ResponseRouteEpoch {
        self.route_epoch
    }

    pub fn operation_fence(&self) -> &SurfaceOperationFence {
        &self.operation_fence
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceInteractionSafeProjection {
    ToolApproval {
        allowed: bool,
    },
    PermissionRequest {
        decision: SurfaceAllowDeny,
        scope: PermissionGrantScope,
        strict_auto_review: bool,
    },
    UserInput {
        answered: bool,
    },
    McpElicitation {
        accepted: bool,
    },
    BackgroundApproval {
        allowed: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceInteractionResolutionReceipt {
    pub response_id: SurfaceResponseId,
    pub receipt_id: SurfaceResponseReceiptId,
    pub kind: SurfaceInteractionKind,
    pub safe_projection: SurfaceInteractionSafeProjection,
}

#[derive(Clone, PartialEq)]
pub struct BrokerInteractionRequestRecord {
    pub thread_id: SurfaceThreadId,
    pub interaction_id: SurfaceInteractionId,
    pub fence: SurfaceOperationFence,
    pub kind: SurfaceInteractionKind,
    pub request: SurfaceInteractionRequest,
    pub response_token: SurfaceResponseToken,
    pub answer_policy: BrokerInteractionAnswerPolicy,
    pub recovery_disposition: InteractionUnavailableDisposition,
}

#[derive(Clone, Eq, PartialEq)]
pub enum BrokerResponsePayload {
    ReplayablePrivate { encrypted_reference: OpaqueToken },
    LiveOnly { incarnation: SurfaceIncarnation },
}

#[derive(Clone, Eq, PartialEq)]
pub struct BrokerInteractionResponseRecord {
    pub receipt: SurfaceInteractionResolutionReceipt,
    pub payload: BrokerResponsePayload,
    pub keyed_response_digest: OpaqueToken,
}

#[derive(Clone, Eq, PartialEq)]
pub enum BrokerInteractionWaitResult {
    Resolved {
        response: BrokerInteractionResponseRecord,
    },
    Cancelled {
        reason: InteractionCancelReason,
    },
    Expired {
        deadline: InteractionExpiryDeadline,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InteractionCancelReason {
    OperationCancelled {
        reason: CancelReason,
    },
    HostShutdown,
    ThreadClose,
    CapabilityUnavailable,
    ExpiryAuthorityUnavailable {
        deadline: InteractionExpiryDeadline,
        failure: InteractionExpiryAuthorityFailure,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceInteractionLifecycle {
    Requested,
    Resolved {
        receipt: SurfaceInteractionResolutionReceipt,
    },
    Cancelled {
        reason: InteractionCancelReason,
    },
    Expired {
        deadline: InteractionExpiryDeadline,
    },
    Transferred {
        background_fence: SurfaceBackgroundFence,
    },
}

#[derive(Clone, PartialEq)]
pub struct SurfaceInteractionView {
    pub interaction_id: SurfaceInteractionId,
    pub revision: InteractionRevision,
    pub fence: SurfaceOperationFence,
    pub kind: SurfaceInteractionKind,
    pub request: SurfaceInteractionRequest,
    pub route: SurfaceInteractionRoute,
    pub lifecycle: SurfaceInteractionLifecycle,
    pub recovery_disposition: InteractionUnavailableDisposition,
}

#[derive(Clone, PartialEq)]
pub enum InteractionPatch {
    Requested {
        interaction: SurfaceInteractionView,
    },
    RouteChanged {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        route: SurfaceInteractionRoute,
    },
    Resolved {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        receipt: SurfaceInteractionResolutionReceipt,
    },
    Cancelled {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        reason: InteractionCancelReason,
    },
    Expired {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        deadline: InteractionExpiryDeadline,
    },
    Transferred {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        background_fence: SurfaceBackgroundFence,
        route: SurfaceInteractionRoute,
    },
}

#[derive(Serialize)]
enum CanonicalInteractionRequestV1<'a> {
    ToolApproval {
        tool: &'a SurfaceToolRequest,
        description: &'a DisplayText,
        preview: &'a Option<DisplayText>,
        authority: CanonicalAuthorityFingerprintV1<'a>,
    },
    PermissionRequest {
        tool_call_id: &'a SurfaceToolCallId,
        reason: &'a Option<DisplayText>,
        permissions: &'a SurfacePermissionProfile,
        authority: CanonicalAuthorityFingerprintV1<'a>,
    },
    UserInput {
        question: &'a NonEmptyText,
        suggestions: &'a Vec<DisplayText>,
    },
    McpElicitation {
        server_name: &'a NonEmptyText,
        server_request_id: &'a NonEmptyText,
        message: &'a DisplayText,
        request: &'a SurfaceMcpElicitationRequest,
    },
    BackgroundApproval {
        task: CanonicalTaskFenceV1<'a>,
        tool: &'a SurfaceToolRequest,
        authority: CanonicalAuthorityFingerprintV1<'a>,
    },
}

fn canonical_interaction_request_v1(
    request: &SurfaceInteractionRequest,
) -> CanonicalInteractionRequestV1<'_> {
    match request {
        SurfaceInteractionRequest::ToolApproval {
            tool,
            description,
            preview,
            authority,
        } => CanonicalInteractionRequestV1::ToolApproval {
            tool,
            description,
            preview,
            authority: canonical_authority_fingerprint_v1(authority),
        },
        SurfaceInteractionRequest::PermissionRequest {
            tool_call_id,
            reason,
            permissions,
            authority,
        } => CanonicalInteractionRequestV1::PermissionRequest {
            tool_call_id,
            reason,
            permissions,
            authority: canonical_authority_fingerprint_v1(authority),
        },
        SurfaceInteractionRequest::UserInput {
            question,
            suggestions,
        } => CanonicalInteractionRequestV1::UserInput {
            question,
            suggestions,
        },
        SurfaceInteractionRequest::McpElicitation {
            server_name,
            server_request_id,
            message,
            request,
        } => CanonicalInteractionRequestV1::McpElicitation {
            server_name,
            server_request_id,
            message,
            request,
        },
        SurfaceInteractionRequest::BackgroundApproval {
            task,
            tool,
            authority,
        } => CanonicalInteractionRequestV1::BackgroundApproval {
            task: canonical_task_fence_v1(task),
            tool,
            authority: canonical_authority_fingerprint_v1(authority),
        },
    }
}

#[derive(Serialize)]
enum CanonicalInteractionLifecycleV1<'a> {
    Requested,
    Resolved {
        receipt: &'a SurfaceInteractionResolutionReceipt,
    },
    Cancelled {
        reason: &'a InteractionCancelReason,
    },
    Expired {
        deadline: &'a InteractionExpiryDeadline,
    },
    Transferred {
        background_fence: CanonicalBackgroundFenceV1<'a>,
    },
}

fn canonical_interaction_lifecycle_v1(
    lifecycle: &SurfaceInteractionLifecycle,
) -> CanonicalInteractionLifecycleV1<'_> {
    match lifecycle {
        SurfaceInteractionLifecycle::Requested => CanonicalInteractionLifecycleV1::Requested,
        SurfaceInteractionLifecycle::Resolved { receipt } => {
            CanonicalInteractionLifecycleV1::Resolved { receipt }
        }
        SurfaceInteractionLifecycle::Cancelled { reason } => {
            CanonicalInteractionLifecycleV1::Cancelled { reason }
        }
        SurfaceInteractionLifecycle::Expired { deadline } => {
            CanonicalInteractionLifecycleV1::Expired { deadline }
        }
        SurfaceInteractionLifecycle::Transferred { background_fence } => {
            CanonicalInteractionLifecycleV1::Transferred {
                background_fence: canonical_background_fence_v1(background_fence),
            }
        }
    }
}

#[derive(Serialize)]
pub(super) struct CanonicalInteractionViewV1<'a> {
    interaction_id: &'a SurfaceInteractionId,
    revision: InteractionRevision,
    fence: &'a SurfaceOperationFence,
    kind: SurfaceInteractionKind,
    request: CanonicalInteractionRequestV1<'a>,
    route: &'a SurfaceInteractionRoute,
    lifecycle: CanonicalInteractionLifecycleV1<'a>,
    recovery_disposition: &'a InteractionUnavailableDisposition,
}

fn canonical_interaction_view_v1(
    interaction: &SurfaceInteractionView,
) -> CanonicalInteractionViewV1<'_> {
    CanonicalInteractionViewV1 {
        interaction_id: &interaction.interaction_id,
        revision: interaction.revision,
        fence: &interaction.fence,
        kind: interaction.kind,
        request: canonical_interaction_request_v1(&interaction.request),
        route: &interaction.route,
        lifecycle: canonical_interaction_lifecycle_v1(&interaction.lifecycle),
        recovery_disposition: &interaction.recovery_disposition,
    }
}

#[derive(Serialize)]
pub(super) enum CanonicalInteractionPatchV1<'a> {
    Requested {
        interaction: CanonicalInteractionViewV1<'a>,
    },
    RouteChanged {
        interaction_id: &'a SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        route: &'a SurfaceInteractionRoute,
    },
    Resolved {
        interaction_id: &'a SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        receipt: &'a SurfaceInteractionResolutionReceipt,
    },
    Cancelled {
        interaction_id: &'a SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        reason: &'a InteractionCancelReason,
    },
    Expired {
        interaction_id: &'a SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        deadline: &'a InteractionExpiryDeadline,
    },
    Transferred {
        interaction_id: &'a SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        background_fence: CanonicalBackgroundFenceV1<'a>,
        route: &'a SurfaceInteractionRoute,
    },
}

pub(super) fn canonical_interaction_patch_v1(
    patch: &InteractionPatch,
) -> CanonicalInteractionPatchV1<'_> {
    match patch {
        InteractionPatch::Requested { interaction } => CanonicalInteractionPatchV1::Requested {
            interaction: canonical_interaction_view_v1(interaction),
        },
        InteractionPatch::RouteChanged {
            interaction_id,
            expected_revision,
            next_revision,
            route,
        } => CanonicalInteractionPatchV1::RouteChanged {
            interaction_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            route,
        },
        InteractionPatch::Resolved {
            interaction_id,
            expected_revision,
            next_revision,
            receipt,
        } => CanonicalInteractionPatchV1::Resolved {
            interaction_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            receipt,
        },
        InteractionPatch::Cancelled {
            interaction_id,
            expected_revision,
            next_revision,
            reason,
        } => CanonicalInteractionPatchV1::Cancelled {
            interaction_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            reason,
        },
        InteractionPatch::Expired {
            interaction_id,
            expected_revision,
            next_revision,
            deadline,
        } => CanonicalInteractionPatchV1::Expired {
            interaction_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            deadline,
        },
        InteractionPatch::Transferred {
            interaction_id,
            expected_revision,
            next_revision,
            background_fence,
            route,
        } => CanonicalInteractionPatchV1::Transferred {
            interaction_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            background_fence: canonical_background_fence_v1(background_fence),
            route,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid_v7_bytes(seed: u8) -> [u8; 16] {
        let mut bytes = [seed; 16];
        bytes[6] = 0x70 | (seed & 0x0f);
        bytes[8] = 0x80 | (seed & 0x3f);
        bytes
    }

    #[test]
    fn broker_only_route_and_transferred_patch_remain_constructible_in_runtime() {
        let operation_fence = SurfaceOperationFence {
            thread_id: SurfaceThreadId::try_from_bytes([1; 16]).unwrap(),
            thread_owner_epoch: ThreadOwnerEpoch::new(0),
            operation_id: SurfaceOperationId::try_from_bytes(uuid_v7_bytes(1)).unwrap(),
            generation_id: SurfaceGenerationId::new(0),
        };
        let background_fence = SurfaceBackgroundFence {
            operation_fence,
            background_owner_token: SurfaceBackgroundOwnerToken::new([1; 32]),
        };
        let _route = BrokerInteractionResponseRoute::Exclusive {
            epoch: ResponseRouteEpoch::try_new(1).unwrap(),
            attachment_id: SurfaceAttachmentId::try_from_bytes(uuid_v7_bytes(2)).unwrap(),
            grant_token: SurfaceResponseGrantToken::new([2; 32]),
        };
        let _patch = InteractionPatch::Transferred {
            interaction_id: SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(3)).unwrap(),
            expected_revision: InteractionRevision::try_new(1).unwrap(),
            next_revision: InteractionRevision::try_new(2).unwrap(),
            background_fence,
            route: SurfaceInteractionRoute::Unassigned {
                epoch: ResponseRouteEpoch::try_new(1).unwrap(),
            },
        };
    }
}
