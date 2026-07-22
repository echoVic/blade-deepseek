use orca_core::thread_identity::{ConversationItemId, TurnId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, PathBuf};
use std::sync::Arc;

pub const SAFE_DIAGNOSTIC_TEXT_BYTE_LIMIT: usize = 4_096;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct NonEmptyText(String);

impl NonEmptyText {
    pub fn try_new(value: impl Into<String>) -> Result<Self, SurfaceValueError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(SurfaceValueError::Empty);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for NonEmptyText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DisplayText(String);

impl DisplayText {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SafeDiagnosticText(String);

impl SafeDiagnosticText {
    pub fn try_new(value: impl Into<String>) -> Result<Self, SurfaceValueError> {
        let value = value.into();
        if value.len() > SAFE_DIAGNOSTIC_TEXT_BYTE_LIMIT {
            return Err(SurfaceValueError::TooLong {
                maximum: SAFE_DIAGNOSTIC_TEXT_BYTE_LIMIT,
                observed: value.len(),
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SafeDiagnosticText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CanonicalPath(PathBuf);

impl CanonicalPath {
    pub fn try_new(value: PathBuf) -> Result<Self, SurfaceValueError> {
        if !value.is_absolute()
            || value
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(SurfaceValueError::NonCanonical);
        }
        Ok(Self(value))
    }

    pub fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CanonicalPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(PathBuf::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

macro_rules! canonical_string {
    ($name:ident, $validator:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn try_new(value: impl Into<String>) -> Result<Self, SurfaceValueError> {
                let value = value.into();
                $validator(&value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::try_new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

fn validate_uri(value: &str) -> Result<(), SurfaceValueError> {
    if value.chars().any(char::is_whitespace) {
        return Err(SurfaceValueError::InvalidFormat);
    }
    let (scheme, remainder) = value
        .split_once(':')
        .ok_or(SurfaceValueError::InvalidFormat)?;
    if scheme.is_empty()
        || !scheme.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || (index > 0 && (byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')))
        })
        || remainder.is_empty()
    {
        return Err(SurfaceValueError::NonCanonical);
    }
    if let Some(authority_and_path) = remainder.strip_prefix("//") {
        let authority = authority_and_path
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        if authority.is_empty() && scheme != "file" {
            return Err(SurfaceValueError::InvalidFormat);
        }
    }
    Ok(())
}

fn validate_mime(value: &str) -> Result<(), SurfaceValueError> {
    if value.contains(';') || value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(SurfaceValueError::NonCanonical);
    }
    let mut parts = value.split('/');
    let valid_part = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                    )
            })
    };
    if !valid_part(parts.next().unwrap_or_default())
        || !valid_part(parts.next().unwrap_or_default())
        || parts.next().is_some()
    {
        return Err(SurfaceValueError::InvalidFormat);
    }
    Ok(())
}

fn validate_domain(value: &str) -> Result<(), SurfaceValueError> {
    if value.is_empty()
        || value.len() > 253
        || value.bytes().any(|byte| byte.is_ascii_uppercase())
        || value.contains(['*', '/', ':'])
        || value.starts_with('.')
        || value.ends_with('.')
    {
        return Err(SurfaceValueError::NonCanonical);
    }
    if !value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    }) {
        return Err(SurfaceValueError::InvalidFormat);
    }
    Ok(())
}

fn validate_rfc3339_utc(value: &str) -> Result<(), SurfaceValueError> {
    if !value.ends_with('Z') || chrono::DateTime::parse_from_rfc3339(value).is_err() {
        return Err(SurfaceValueError::InvalidFormat);
    }
    Ok(())
}

canonical_string!(CanonicalUri, validate_uri);
canonical_string!(CanonicalMime, validate_mime);
canonical_string!(CanonicalDomainName, validate_domain);
canonical_string!(Rfc3339Timestamp, validate_rfc3339_utc);

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct FiniteF64(f64);

impl FiniteF64 {
    pub fn try_new(value: f64) -> Result<Self, SurfaceValueError> {
        if !value.is_finite() {
            return Err(SurfaceValueError::NonFinite);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for FiniteF64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(f64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

macro_rules! scalar_value {
    ($name:ident, $inner:ty) => {
        #[derive(
            Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name($inner);

        impl $name {
            pub const fn new(value: $inner) -> Self {
                Self(value)
            }

            pub const fn get(self) -> $inner {
                self.0
            }
        }
    };
}

scalar_value!(UnixMillis, i64);
scalar_value!(DurationMillis, u64);
scalar_value!(MonotonicTick, u64);
scalar_value!(ByteOffset, u64);
scalar_value!(ByteCount, u64);
scalar_value!(SequenceNumber, u64);
scalar_value!(ThreadOwnerEpoch, u64);
scalar_value!(GoalObjectiveRevision, u32);
scalar_value!(SurfaceGenerationId, u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Revision(u64);

impl Revision {
    pub fn try_new(value: u64) -> Result<Self, SurfaceValueError> {
        if value == 0 {
            return Err(SurfaceValueError::Zero);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for Revision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

macro_rules! revision_value {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Revision);

        impl $name {
            pub fn try_new(value: u64) -> Result<Self, SurfaceValueError> {
                Revision::try_new(value).map(Self)
            }

            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }
    )+};
}

revision_value!(
    DurableRevision,
    LiveRevision,
    SessionCatalogRevision,
    McpCatalogRevision,
    InputCatalogRevision,
    WorkflowCatalogRevision,
    SessionMetadataRevision,
    SettingsRevision,
    TrustRevision,
    PolicyEpoch,
    MemoryRevision,
    PinnedContextRevision,
    SessionHealthRevision,
    GoalRevision,
    GoalCatalogRevision,
    GoalOwnerEpoch,
    TaskRevision,
    WorkflowRevision,
    SubagentRevision,
    InteractionRevision,
    ResponseRouteEpoch,
    CapabilityRevision,
    PlanRevision,
    UsageRevision,
    ContextRevision,
    PinnedFileRevision,
    PinnedUserRevision,
    PinnedSystemRevision,
    ProjectRootMemoryRevision,
    BootstrapCredentialRevision,
    HostLifecycleRevision,
);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    pub const fn new(value: [u8; 32]) -> Self {
        Self(value)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        <[u8; 32]>::deserialize(deserializer).map(Self)
    }
}

#[derive(Clone)]
pub struct OpaqueToken([u8; 32]);

impl PartialEq for OpaqueToken {
    fn eq(&self, other: &Self) -> bool {
        self.0
            .iter()
            .zip(other.0.iter())
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
    }
}

impl Eq for OpaqueToken {}

#[allow(dead_code)]
impl OpaqueToken {
    pub(crate) const fn new(value: [u8; 32]) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Uuid([u8; 16]);

impl Uuid {
    pub fn try_from_bytes(value: [u8; 16]) -> Result<Self, SurfaceValueError> {
        Ok(Self(value))
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UuidV7([u8; 16]);

impl UuidV7 {
    pub fn try_from_bytes(value: [u8; 16]) -> Result<Self, SurfaceValueError> {
        let parsed = uuid::Uuid::from_bytes(value);
        if parsed.get_version_num() != 7 || parsed.get_variant() != uuid::Variant::RFC4122 {
            return Err(SurfaceValueError::WrongUuidKind);
        }
        Ok(Self(value))
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

macro_rules! uuid_serde {
    ($name:ident) => {
        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                uuid::Uuid::from_bytes(self.0)
                    .hyphenated()
                    .to_string()
                    .serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                let parsed = uuid::Uuid::parse_str(&value).map_err(serde::de::Error::custom)?;
                if value != parsed.hyphenated().to_string() {
                    return Err(serde::de::Error::custom("UUID is not canonical"));
                }
                Self::try_from_bytes(*parsed.as_bytes()).map_err(serde::de::Error::custom)
            }
        }
    };
}

uuid_serde!(Uuid);
uuid_serde!(UuidV7);

pub type Set<T> = BTreeSet<T>;
pub type Denied = ();
pub type Unit = ();

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NonEmptyVec<T>(Vec<T>);

impl<T> NonEmptyVec<T> {
    pub fn try_new(value: Vec<T>) -> Result<Self, SurfaceValueError> {
        if value.is_empty() {
            return Err(SurfaceValueError::Empty);
        }
        Ok(Self(value))
    }

    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for NonEmptyVec<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(Vec::<T>::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NonEmptySet<T: Ord>(BTreeSet<T>);

impl<T: Ord> NonEmptySet<T> {
    pub fn try_new(value: BTreeSet<T>) -> Result<Self, SurfaceValueError> {
        if value.is_empty() {
            return Err(SurfaceValueError::Empty);
        }
        Ok(Self(value))
    }

    pub fn as_set(&self) -> &BTreeSet<T> {
        &self.0
    }
}

impl<'de, T> Deserialize<'de> for NonEmptySet<T>
where
    T: Deserialize<'de> + Ord,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(BTreeSet::<T>::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

macro_rules! uuid_wrapper {
    ($name:ident, $inner:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name($inner);

        impl $name {
            pub fn try_from_bytes(value: [u8; 16]) -> Result<Self, SurfaceValueError> {
                $inner::try_from_bytes(value).map(Self)
            }
        }
    };
}

uuid_wrapper!(HostMonotonicClockId, UuidV7);
uuid_wrapper!(SurfaceThreadId, Uuid);
uuid_wrapper!(SurfaceOperationId, UuidV7);
uuid_wrapper!(SurfaceStreamId, UuidV7);
uuid_wrapper!(SurfaceInteractionId, UuidV7);
uuid_wrapper!(SurfaceAttachmentId, UuidV7);
uuid_wrapper!(SurfaceResponseId, UuidV7);
uuid_wrapper!(SurfaceResponseReceiptId, UuidV7);
uuid_wrapper!(SurfaceEventId, UuidV7);
uuid_wrapper!(SurfaceRequestId, UuidV7);
uuid_wrapper!(SurfaceCommitId, UuidV7);
uuid_wrapper!(SurfaceSettlementId, UuidV7);
uuid_wrapper!(SurfaceFinalizeIntentId, UuidV7);
uuid_wrapper!(SurfaceAdmissionLeaseId, UuidV7);
uuid_wrapper!(SurfaceInputCorrelationId, UuidV7);
uuid_wrapper!(SurfaceCapabilityCallId, UuidV7);
uuid_wrapper!(SurfaceConnectionId, UuidV7);
uuid_wrapper!(HostIncarnation, UuidV7);
uuid_wrapper!(SurfaceIncarnation, UuidV7);

macro_rules! text_id {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(NonEmptyText);

        impl $name {
            pub fn try_new(value: impl Into<String>) -> Result<Self, SurfaceValueError> {
                NonEmptyText::try_new(value).map(Self)
            }
        }
    )+};
}

text_id!(
    SurfaceToolCallId,
    SurfaceTaskId,
    SurfaceWorkflowRunId,
    SurfaceWorkflowResultId,
    SurfaceSubagentId,
    SurfaceGoalId,
    SurfaceGoalRunId,
    SurfaceGoalOuterTurnId,
    SurfaceGoalIntentId,
    SurfaceRemoteTerminalId,
    SurfaceCatalogEntryId,
);

pub type SurfaceTurnId = TurnId;
pub type SurfaceItemId = ConversationItemId;

macro_rules! opaque_token_wrapper {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Clone, Eq, PartialEq)]
        pub struct $name(OpaqueToken);

        #[allow(dead_code)]
        impl $name {
            pub(crate) const fn new(value: [u8; 32]) -> Self {
                Self(OpaqueToken::new(value))
            }
        }
    )+};
}

opaque_token_wrapper!(
    SurfaceResponseToken,
    SurfaceResponseGrantToken,
    SurfaceBackgroundOwnerToken,
    SurfacePublisherPermitId,
);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MonotonicInstant {
    pub clock_id: HostMonotonicClockId,
    pub tick: MonotonicTick,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PinnedContextSourceRevision {
    Memory(MemoryRevision),
    File(PinnedFileRevision),
    User(PinnedUserRevision),
    System(PinnedSystemRevision),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HostRevisionWitness {
    Memory(MemoryRevision),
    FolderTrust(TrustRevision),
    RuntimeSettings(SettingsRevision),
    SessionCatalog(SessionCatalogRevision),
    SessionMetadata(SessionMetadataRevision),
    HostLifecycle(HostLifecycleRevision),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceUnavailableReason {
    HostShuttingDown,
    ThreadClosing,
    ProjectionDegraded,
    CapacityExceeded,
    RuntimeUnavailable,
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct OptionalProcessLocalCancel(Arc<()>);

#[allow(dead_code)]
impl OptionalProcessLocalCancel {
    pub(crate) fn new() -> Self {
        Self(Arc::new(()))
    }
}

pub struct ZeroizingProcessLocalSecret(Vec<u8>);

#[allow(dead_code)]
impl ZeroizingProcessLocalSecret {
    pub(crate) fn new(value: Vec<u8>) -> Self {
        Self(value)
    }
}

impl Drop for ZeroizingProcessLocalSecret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct SurfaceBoundCaller {
    attachment_id: SurfaceAttachmentId,
    connection_id: Option<SurfaceConnectionId>,
}

#[allow(dead_code)]
impl SurfaceBoundCaller {
    pub(crate) fn new(
        attachment_id: SurfaceAttachmentId,
        connection_id: Option<SurfaceConnectionId>,
    ) -> Self {
        Self {
            attachment_id,
            connection_id,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct SurfaceHostBoundCaller {
    host_incarnation: HostIncarnation,
    connection_id: Option<SurfaceConnectionId>,
}

#[allow(dead_code)]
impl SurfaceHostBoundCaller {
    pub(crate) fn new(
        host_incarnation: HostIncarnation,
        connection_id: Option<SurfaceConnectionId>,
    ) -> Self {
        Self {
            host_incarnation,
            connection_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AcpRequestId {
    String(NonEmptyText),
    Integer(i64),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceOperationFence {
    pub thread_id: SurfaceThreadId,
    pub thread_owner_epoch: ThreadOwnerEpoch,
    pub operation_id: SurfaceOperationId,
    pub generation_id: SurfaceGenerationId,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SurfaceBackgroundFence {
    pub operation_fence: SurfaceOperationFence,
    pub background_owner_token: SurfaceBackgroundOwnerToken,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceGoalFence {
    pub goal_id: SurfaceGoalId,
    pub goal_revision: GoalRevision,
    pub goal_owner_epoch: GoalOwnerEpoch,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SurfaceTaskFence {
    pub task_id: SurfaceTaskId,
    pub task_revision: TaskRevision,
    pub background_owner: Option<SurfaceBackgroundFence>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceWorkflowFence {
    pub workflow_run_id: SurfaceWorkflowRunId,
    pub workflow_revision: WorkflowRevision,
    pub parent: Option<SurfaceOperationFence>,
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceScope {
    Thread,
    Operation {
        operation_id: SurfaceOperationId,
    },
    Generation {
        fence: SurfaceOperationFence,
    },
    Background {
        fence: SurfaceBackgroundFence,
    },
    Goal {
        goal_id: SurfaceGoalId,
        causative_generation: Option<SurfaceOperationFence>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CommitClass {
    Recorded {
        thread_owner_epoch: ThreadOwnerEpoch,
        durable_revision: DurableRevision,
        commit_id: SurfaceCommitId,
    },
    Ephemeral {
        incarnation: SurfaceIncarnation,
        live_revision: LiveRevision,
        commit_id: SurfaceCommitId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CursorSourceRevision {
    Recorded { durable_revision: DurableRevision },
    Ephemeral { live_revision: LiveRevision },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceCursor {
    pub thread_id: SurfaceThreadId,
    pub incarnation: SurfaceIncarnation,
    pub next_seq: SequenceNumber,
    pub source_revision: CursorSourceRevision,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum SurfaceCapability {
    ReadSnapshot,
    ReadCatalog,
    SubmitOperation,
    ControlBoundOperation,
    ControlAnyVisibleOperation,
    LegacyCancelCurrent,
    LegacyInterruptResume,
    LegacyJsonlControl,
    RespondGrantedInteraction,
    ManageTask,
    ManageWorkflow,
    ManageGoal,
    ManageThreadSettings,
    ManagePinnedContext,
    RepairThread,
    ReadSessionCatalog,
    ManageSessionCatalog,
    ManageSessionLifecycle,
    ManageMemory,
    ReadHostPolicy,
    ManageFolderTrust,
    ReadHostSettings,
    ManageHostSettings,
    ShutdownHost,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceAttachmentRole {
    Tui,
    Acp,
    Jsonl,
    InternalCompatibility,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceAttachmentGrant {
    pub attachment_id: SurfaceAttachmentId,
    pub host_incarnation: HostIncarnation,
    pub role: SurfaceAttachmentRole,
    pub capabilities: NonEmptySet<SurfaceCapability>,
    pub granted_at: SurfaceCursor,
    pub expires_at: Option<MonotonicInstant>,
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfacePublisherPermit {
    ActorControl {
        permit_id: SurfacePublisherPermitId,
        thread_id: SurfaceThreadId,
        owner_epoch: ThreadOwnerEpoch,
    },
    Generation {
        permit_id: SurfacePublisherPermitId,
        fence: SurfaceOperationFence,
    },
    Background {
        permit_id: SurfacePublisherPermitId,
        fence: SurfaceBackgroundFence,
    },
    Goal {
        permit_id: SurfacePublisherPermitId,
        goal_fence: SurfaceGoalFence,
        receipt_digest: Sha256Digest,
    },
    Finalizer {
        permit_id: SurfacePublisherPermitId,
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        owner_epoch: ThreadOwnerEpoch,
    },
    Recovery {
        permit_id: SurfacePublisherPermitId,
        current_owner_epoch: ThreadOwnerEpoch,
        historical_fence: SurfaceOperationFence,
    },
}

pub struct ProcessLeaseWitness(());

#[allow(dead_code)]
pub struct ThreadOwnershipLease {
    pub thread_id: SurfaceThreadId,
    pub host_incarnation: HostIncarnation,
    pub owner_epoch: ThreadOwnerEpoch,
    pub witness: ProcessLeaseWitness,
}

#[allow(dead_code)]
pub struct PolicyOwnerLease {
    pub lease_id: UuidV7,
    pub host_incarnation: HostIncarnation,
    pub observed_policy_epoch: PolicyEpoch,
    pub governed_roots: NonEmptySet<CanonicalPath>,
    pub witness: ProcessLeaseWitness,
    pub diagnostic_expires_at: UnixMillis,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceValueError {
    Empty,
    Zero,
    NonFinite,
    InvalidFormat,
    NonCanonical,
    WrongUuidKind,
    TooLong { maximum: usize, observed: usize },
}

impl fmt::Display for SurfaceValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for SurfaceValueError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_tokens_compare_without_exposing_bytes() {
        assert!(OpaqueToken::new([1; 32]) == OpaqueToken::new([1; 32]));
        assert!(OpaqueToken::new([1; 32]) != OpaqueToken::new([2; 32]));
    }
}
