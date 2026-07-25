use super::{
    CommitClass, CommitProbe, DurableBatchReceipt, ExclusiveOwnerLease, JsonlSurfaceCommitLedger,
    PreparedSurfaceCommit, RetryLocalProjectionToken, RetryProjectionToken, SurfaceCommitBatch,
    SurfaceCommitBatchPreflightResult, SurfaceCommitId, SurfaceCommitLedger, SurfaceFactFamily,
    SurfaceLedgerError, SurfacePublisherPermit, SurfaceReduceMode, SurfaceReduceResult,
    SurfaceReducerError, SurfaceReducerState, SurfaceScope, ThreadOwnerEpoch, preflight_batch,
    reduce_batch,
};
use std::collections::VecDeque;

#[derive(Clone, Debug, Eq, PartialEq)]
enum ManualCompactionItemKey {
    System {
        content: String,
        pinned: bool,
    },
    User {
        content: String,
        pinned: bool,
    },
    AssistantText {
        content: String,
        pinned: bool,
    },
    AssistantReasoning {
        content: String,
        pinned: bool,
    },
    Tool {
        tool_call_id: String,
        content: String,
        pinned: bool,
    },
}

fn manual_compaction_conversation_keys(
    conversation: &orca_core::conversation::Conversation,
) -> Vec<ManualCompactionItemKey> {
    let mut keys = Vec::new();
    for message in &conversation.messages {
        match message {
            orca_core::conversation::Message::System { content, pinned } => {
                keys.push(ManualCompactionItemKey::System {
                    content: content.clone(),
                    pinned: *pinned,
                });
            }
            orca_core::conversation::Message::User { content, pinned } => {
                keys.push(ManualCompactionItemKey::User {
                    content: content.clone(),
                    pinned: *pinned,
                });
            }
            orca_core::conversation::Message::Assistant {
                content,
                reasoning_content,
                pinned,
                ..
            } => {
                if let Some(content) = content {
                    keys.push(ManualCompactionItemKey::AssistantText {
                        content: content.clone(),
                        pinned: *pinned,
                    });
                }
                if let Some(content) = reasoning_content {
                    keys.push(ManualCompactionItemKey::AssistantReasoning {
                        content: content.clone(),
                        pinned: *pinned,
                    });
                }
            }
            orca_core::conversation::Message::Tool {
                tool_call_id,
                content,
                pinned,
                ..
            } => keys.push(ManualCompactionItemKey::Tool {
                tool_call_id: tool_call_id.clone(),
                content: content.clone(),
                pinned: *pinned,
            }),
        }
    }
    keys
}

fn manual_compaction_surface_item_key(
    item: &super::SurfaceItem,
) -> Option<ManualCompactionItemKey> {
    match item {
        super::SurfaceItem::SystemMessage {
            content, pinned, ..
        } => Some(ManualCompactionItemKey::System {
            content: content.as_str().to_string(),
            pinned: *pinned,
        }),
        super::SurfaceItem::UserMessage { input, pinned, .. } => {
            manual_compaction_surface_user_input(input).map(|content| {
                ManualCompactionItemKey::User {
                    content: content.to_string(),
                    pinned: *pinned,
                }
            })
        }
        super::SurfaceItem::AssistantMessage { text, pinned, .. }
        | super::SurfaceItem::AssistantPlan { text, pinned, .. } => {
            Some(ManualCompactionItemKey::AssistantText {
                content: text.as_str().to_string(),
                pinned: *pinned,
            })
        }
        super::SurfaceItem::AssistantReasoning {
            content, pinned, ..
        } => Some(ManualCompactionItemKey::AssistantReasoning {
            content: content.as_str().to_string(),
            pinned: *pinned,
        }),
        super::SurfaceItem::ToolResultMessage {
            tool_call_id,
            content,
            pinned,
            ..
        } => Some(ManualCompactionItemKey::Tool {
            tool_call_id: tool_call_id.as_str().to_string(),
            content: content.as_str().to_string(),
            pinned: *pinned,
        }),
    }
}

fn manual_compaction_key_matches(
    candidate: &ManualCompactionItemKey,
    target: &ManualCompactionItemKey,
) -> bool {
    if candidate == target {
        return true;
    }
    let redacted_matches = |candidate: &str, target: &str| {
        crate::thread_store::redact_sensitive_text(candidate) == target
    };
    match (candidate, target) {
        (
            ManualCompactionItemKey::System {
                content: candidate,
                pinned: candidate_pinned,
            },
            ManualCompactionItemKey::System {
                content: target,
                pinned: target_pinned,
            },
        )
        | (
            ManualCompactionItemKey::User {
                content: candidate,
                pinned: candidate_pinned,
            },
            ManualCompactionItemKey::User {
                content: target,
                pinned: target_pinned,
            },
        )
        | (
            ManualCompactionItemKey::AssistantText {
                content: candidate,
                pinned: candidate_pinned,
            },
            ManualCompactionItemKey::AssistantText {
                content: target,
                pinned: target_pinned,
            },
        )
        | (
            ManualCompactionItemKey::AssistantReasoning {
                content: candidate,
                pinned: candidate_pinned,
            },
            ManualCompactionItemKey::AssistantReasoning {
                content: target,
                pinned: target_pinned,
            },
        ) => candidate_pinned == target_pinned && redacted_matches(candidate, target),
        (
            ManualCompactionItemKey::Tool {
                tool_call_id: candidate_id,
                content: candidate,
                pinned: candidate_pinned,
            },
            ManualCompactionItemKey::Tool {
                tool_call_id: target_id,
                content: target,
                pinned: target_pinned,
            },
        ) => {
            candidate_id == target_id
                && candidate_pinned == target_pinned
                && redacted_matches(candidate, target)
        }
        _ => false,
    }
}

fn manual_compaction_surface_user_input(input: &super::SurfaceUserInputState) -> Option<&str> {
    match input {
        super::SurfaceUserInputState::Resolved {
            fact: super::SurfaceResolvedInputFact::Replayable { input, .. },
        } => Some(input.canonical_text.as_str()),
        super::SurfaceUserInputState::Resolved {
            fact:
                super::SurfaceResolvedInputFact::NonReplayable {
                    presentation: super::SurfaceInputPresentation::Visible { text },
                    ..
                },
        } => Some(text.as_str()),
        super::SurfaceUserInputState::Pending { .. }
        | super::SurfaceUserInputState::ResolutionFailed { .. }
        | super::SurfaceUserInputState::Resolved {
            fact:
                super::SurfaceResolvedInputFact::NonReplayable {
                    presentation: super::SurfaceInputPresentation::Redacted,
                    ..
                },
        } => None,
    }
}

fn manual_compaction_surface_item_id(item: &super::SurfaceItem) -> &super::SurfaceItemId {
    match item {
        super::SurfaceItem::UserMessage { id, .. }
        | super::SurfaceItem::SystemMessage { id, .. }
        | super::SurfaceItem::AssistantMessage { id, .. }
        | super::SurfaceItem::AssistantReasoning { id, .. }
        | super::SurfaceItem::AssistantPlan { id, .. }
        | super::SurfaceItem::ToolResultMessage { id, .. } => id,
    }
}

pub(crate) fn manual_compaction_item_patches(
    items: &[super::SurfaceItem],
    conversation: &orca_core::conversation::Conversation,
) -> Option<Vec<super::ItemPatch>> {
    let keys = manual_compaction_conversation_keys(conversation);
    let mut assignments = vec![None; keys.len()];
    let mut required = vec![false; keys.len()];
    let mut used = vec![false; items.len()];
    for (target_index, key) in keys.iter().enumerate().rev() {
        required[target_index] = match key {
            ManualCompactionItemKey::System { .. } => true,
            ManualCompactionItemKey::User { .. } => items
                .iter()
                .any(|item| matches!(item, super::SurfaceItem::UserMessage { .. })),
            ManualCompactionItemKey::AssistantText { .. } => items.iter().any(|item| {
                matches!(
                    item,
                    super::SurfaceItem::AssistantMessage { .. }
                        | super::SurfaceItem::AssistantPlan { .. }
                )
            }),
            ManualCompactionItemKey::AssistantReasoning { .. } => items
                .iter()
                .any(|item| matches!(item, super::SurfaceItem::AssistantReasoning { .. })),
            ManualCompactionItemKey::Tool { .. } => items
                .iter()
                .any(|item| matches!(item, super::SurfaceItem::ToolResultMessage { .. })),
        };
        if let Some(source_index) = items.iter().enumerate().rev().find_map(|(index, item)| {
            (!used[index]
                && manual_compaction_surface_item_key(item)
                    .as_ref()
                    .is_some_and(|candidate| manual_compaction_key_matches(candidate, key)))
            .then_some(index)
        }) {
            used[source_index] = true;
            assignments[target_index] = Some(items[source_index].clone());
            continue;
        }
        let replacement = match key {
            ManualCompactionItemKey::System { content, pinned } => {
                Some(super::SurfaceItem::SystemMessage {
                    id: super::SurfaceItemId::new(),
                    content: super::DisplayText::new(content.clone()),
                    pinned: *pinned,
                    origin: super::SurfaceItemOrigin::HistoryMaterialization,
                })
            }
            ManualCompactionItemKey::Tool {
                tool_call_id,
                content,
                pinned,
            } => items.iter().enumerate().rev().find_map(|(index, item)| {
                if used[index] {
                    return None;
                }
                let super::SurfaceItem::ToolResultMessage {
                    id,
                    turn_id,
                    tool_call_id: existing_id,
                    terminal,
                    ..
                } = item
                else {
                    return None;
                };
                (existing_id.as_str() == tool_call_id).then(|| {
                    used[index] = true;
                    super::SurfaceItem::ToolResultMessage {
                        id: id.clone(),
                        turn_id: turn_id.clone(),
                        tool_call_id: existing_id.clone(),
                        content: super::DisplayText::new(content.clone()),
                        terminal: terminal.clone(),
                        pinned: *pinned,
                    }
                })
            }),
            ManualCompactionItemKey::User { .. }
            | ManualCompactionItemKey::AssistantText { .. }
            | ManualCompactionItemKey::AssistantReasoning { .. } => None,
        };
        assignments[target_index] = replacement;
    }
    if assignments
        .iter()
        .zip(required.iter())
        .any(|(assignment, required)| *required && assignment.is_none())
    {
        return None;
    }
    let mut patches = items
        .iter()
        .map(|item| super::ItemPatch::Removed {
            item_id: manual_compaction_surface_item_id(item).clone(),
            reason: super::ItemRemovalReason::Compacted,
        })
        .collect::<Vec<_>>();
    patches.extend(
        assignments
            .into_iter()
            .flatten()
            .map(|item| super::ItemPatch::Added { item }),
    );
    Some(patches)
}

pub(super) fn manual_compaction_item_rebuild_paired(
    snapshot: &super::SurfaceSnapshot,
    batch: &super::SurfaceCommitBatch,
) -> bool {
    let completes_manual_compaction = batch.events.as_slice().iter().any(|event| {
        matches!(
            &event.event,
            super::SurfaceEvent::Context(super::SurfaceContextSnapshot {
                compaction: super::CompactionState::Completed {
                    reason: super::CompactionReason::Manual,
                    ..
                },
                ..
            })
        )
    });
    if !completes_manual_compaction {
        return false;
    }
    let removed = batch
        .events
        .as_slice()
        .iter()
        .filter_map(|event| match &event.event {
            super::SurfaceEvent::Item(super::ItemPatch::Removed {
                item_id,
                reason: super::ItemRemovalReason::Compacted,
            }) => Some(item_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    removed.len() == snapshot.items.len()
        && snapshot.items.iter().all(|item| {
            removed
                .iter()
                .filter(|item_id| **item_id == manual_compaction_surface_item_id(item))
                .count()
                == 1
        })
        && batch
            .events
            .as_slice()
            .iter()
            .all(|event| match &event.event {
                super::SurfaceEvent::Item(super::ItemPatch::Removed {
                    item_id,
                    reason: super::ItemRemovalReason::Compacted,
                }) => snapshot
                    .items
                    .iter()
                    .any(|item| manual_compaction_surface_item_id(item) == item_id),
                super::SurfaceEvent::Item(super::ItemPatch::Added { item }) => {
                    let item_id = manual_compaction_surface_item_id(item);
                    match snapshot
                        .items
                        .iter()
                        .find(|existing| manual_compaction_surface_item_id(existing) == item_id)
                    {
                        Some(existing) if existing == item => true,
                        Some(super::SurfaceItem::ToolResultMessage {
                            turn_id,
                            tool_call_id,
                            terminal,
                            ..
                        }) => matches!(
                            item,
                            super::SurfaceItem::ToolResultMessage {
                                turn_id: added_turn,
                                tool_call_id: added_tool,
                                terminal: added_terminal,
                                ..
                            } if added_turn == turn_id
                                && added_tool == tool_call_id
                                && added_terminal == terminal
                        ),
                        Some(_) => false,
                        None => matches!(
                            item,
                            super::SurfaceItem::SystemMessage {
                                origin: super::SurfaceItemOrigin::HistoryMaterialization,
                                ..
                            }
                        ),
                    }
                }
                super::SurfaceEvent::Item(_) => false,
                _ => true,
            })
        && {
            let added_ids = batch
                .events
                .as_slice()
                .iter()
                .filter_map(|event| match &event.event {
                    super::SurfaceEvent::Item(super::ItemPatch::Added { item }) => {
                        Some(manual_compaction_surface_item_id(item))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            added_ids.iter().enumerate().all(|(index, item_id)| {
                added_ids[index + 1..]
                    .iter()
                    .all(|other| *other != *item_id)
            })
        }
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceCommitError {
    OversizedBatch,
    InvalidBatch(SurfaceReducerError),
    StaleOwnerEpoch,
    StalePublisherPermit,
    CursorRangeAlreadyConsumed,
    Ledger(SurfaceLedgerError),
    Settlement(super::SettlementError),
    ProjectionPending { token: RetryProjectionToken },
}

impl std::fmt::Debug for SurfaceCommitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OversizedBatch => formatter.write_str("OversizedBatch"),
            Self::InvalidBatch(error) => {
                formatter.debug_tuple("InvalidBatch").field(error).finish()
            }
            Self::StaleOwnerEpoch => formatter.write_str("StaleOwnerEpoch"),
            Self::StalePublisherPermit => formatter.write_str("StalePublisherPermit"),
            Self::CursorRangeAlreadyConsumed => formatter.write_str("CursorRangeAlreadyConsumed"),
            Self::Ledger(error) => formatter.debug_tuple("Ledger").field(error).finish(),
            Self::Settlement(error) => formatter.debug_tuple("Settlement").field(error).finish(),
            Self::ProjectionPending { .. } => formatter.write_str("ProjectionPending"),
        }
    }
}

#[derive(Clone)]
pub struct SurfaceProjectionContext {
    pub request_id: super::SurfaceRequestId,
    pub target: super::MutationTarget,
    pub fact_family: SurfaceFactFamily,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceCommitApplied {
    pub receipt: DurableBatchReceipt,
}

struct ColdOwnerTakeoverAuthority {
    previous_owner_epoch: ThreadOwnerEpoch,
    previous_incarnation: super::SurfaceIncarnation,
    current_owner_epoch: ThreadOwnerEpoch,
    new_incarnation: Option<super::SurfaceIncarnation>,
    recoverable_operations: Vec<super::SurfaceOperationId>,
}

#[derive(Default)]
struct BoundedPublicationSuffix {
    batches: VecDeque<SurfaceCommitBatch>,
    encoded_bytes: VecDeque<u64>,
    events: u64,
    bytes: u64,
}

impl BoundedPublicationSuffix {
    fn from_committed(committed: Vec<SurfaceCommitBatch>) -> Self {
        let mut suffix = Self::default();
        let mut expected_after = None;
        for batch in committed.into_iter().rev() {
            if expected_after
                .as_ref()
                .is_some_and(|expected| expected != &batch.cursor_after)
            {
                break;
            }
            let batch_events = batch.event_count as u64;
            let batch_bytes = super::canonical_batch_encoded_bytes(&batch);
            if suffix.events.saturating_add(batch_events) > super::SURFACE_RETAINED_EVENT_LIMIT
                || suffix.bytes.saturating_add(batch_bytes) > super::SURFACE_RETAINED_BYTE_LIMIT
            {
                break;
            }
            expected_after = Some(batch.cursor_before.clone());
            suffix.events += batch_events;
            suffix.bytes += batch_bytes;
            suffix.batches.push_front(batch);
            suffix.encoded_bytes.push_front(batch_bytes);
        }
        suffix
    }

    fn push(&mut self, batch: &SurfaceCommitBatch) {
        if self
            .batches
            .back()
            .is_some_and(|previous| previous.cursor_after != batch.cursor_before)
        {
            self.clear();
        }
        let batch_bytes = super::canonical_batch_encoded_bytes(batch);
        self.events = self.events.saturating_add(batch.event_count as u64);
        self.bytes = self.bytes.saturating_add(batch_bytes);
        self.batches.push_back(batch.clone());
        self.encoded_bytes.push_back(batch_bytes);
        while self.events > super::SURFACE_RETAINED_EVENT_LIMIT
            || self.bytes > super::SURFACE_RETAINED_BYTE_LIMIT
        {
            let Some(expired) = self.batches.pop_front() else {
                break;
            };
            let expired_bytes = self
                .encoded_bytes
                .pop_front()
                .expect("publication bytes track every retained batch");
            self.events = self.events.saturating_sub(expired.event_count as u64);
            self.bytes = self.bytes.saturating_sub(expired_bytes);
        }
    }

    fn make_contiguous(&mut self) -> &[SurfaceCommitBatch] {
        self.batches.make_contiguous()
    }

    fn clear(&mut self) {
        self.batches.clear();
        self.encoded_bytes.clear();
        self.events = 0;
        self.bytes = 0;
    }
}

impl ColdOwnerTakeoverAuthority {
    fn authorizes_transition(
        &self,
        snapshot: &super::SurfaceSnapshot,
        new_incarnation: &super::SurfaceIncarnation,
        new_owner_epoch: &ThreadOwnerEpoch,
    ) -> bool {
        if new_owner_epoch != &self.current_owner_epoch {
            return false;
        }
        match &self.new_incarnation {
            Some(expected) => {
                new_incarnation == expected
                    && snapshot.thread.owner_epoch == self.current_owner_epoch
                    && snapshot.cursor.incarnation == *expected
            }
            None => {
                snapshot.thread.owner_epoch == self.previous_owner_epoch
                    && snapshot.cursor.incarnation == self.previous_incarnation
                    && new_incarnation != &self.previous_incarnation
            }
        }
    }

    fn authorizes(
        &self,
        operation_id: &super::SurfaceOperationId,
        snapshot: &super::SurfaceSnapshot,
        new_incarnation: &super::SurfaceIncarnation,
        new_owner_epoch: &ThreadOwnerEpoch,
    ) -> bool {
        self.recoverable_operations.contains(operation_id)
            && self.authorizes_transition(snapshot, new_incarnation, new_owner_epoch)
    }
}

enum OwnerLeaseAuthority<'owner> {
    Borrowed(&'owner ExclusiveOwnerLease),
    Owned(ExclusiveOwnerLease),
}

enum BatchCommitAuthority<'permit> {
    Single(&'permit SurfacePublisherPermit),
    ActorGenerationTerminalization {
        actor: &'permit SurfacePublisherPermit,
        generation: &'permit SurfacePublisherPermit,
    },
    #[allow(dead_code)]
    LiveGenerationSuspend {
        actor: &'permit SurfacePublisherPermit,
        generation: &'permit SurfacePublisherPermit,
    },
    LiveGenerationStop {
        generation: &'permit SurfacePublisherPermit,
        finalizer: &'permit SurfacePublisherPermit,
    },
}

enum RecoveredBatchAuthority {
    Single(SurfacePublisherPermit),
    ActorGenerationTerminalization {
        actor: SurfacePublisherPermit,
        generation: SurfacePublisherPermit,
    },
}

impl OwnerLeaseAuthority<'_> {
    fn lease(&self) -> &ExclusiveOwnerLease {
        match self {
            Self::Borrowed(lease) => lease,
            Self::Owned(lease) => lease,
        }
    }
}

pub struct RuntimeCommitCoordinator<'owner, L> {
    ledger: L,
    state: SurfaceReducerState,
    surface_hub: Option<super::SurfaceHub>,
    recovered_publications: BoundedPublicationSuffix,
    owner_lease: OwnerLeaseAuthority<'owner>,
    owner_epoch: ThreadOwnerEpoch,
    actor_control_permit: SurfacePublisherPermit,
    issued_permits: Vec<SurfacePublisherPermit>,
    next_sequence: u64,
    incomplete: Option<SurfaceCommitBatch>,
    recovered_prepared: Option<SurfaceCommitBatch>,
    cold_takeover_authority: Option<ColdOwnerTakeoverAuthority>,
    pending_projection: Option<(RetryProjectionToken, SurfaceCommitBatch)>,
    #[cfg(test)]
    projection_failure_injected: bool,
}

fn recovered_cold_takeover_authority(
    state: &SurfaceReducerState,
    current_owner_epoch: ThreadOwnerEpoch,
    materialized: Option<ColdOwnerTakeoverAuthority>,
) -> Option<ColdOwnerTakeoverAuthority> {
    let snapshot = state.snapshot();
    if snapshot.thread.owner_epoch < current_owner_epoch {
        return Some(ColdOwnerTakeoverAuthority {
            previous_owner_epoch: snapshot.thread.owner_epoch,
            previous_incarnation: snapshot.cursor.incarnation.clone(),
            current_owner_epoch,
            new_incarnation: None,
            recoverable_operations: snapshot_operation_ids(snapshot),
        });
    }
    (snapshot.thread.owner_epoch == current_owner_epoch)
        .then_some(materialized)
        .flatten()
}

fn snapshot_operation_ids(snapshot: &super::SurfaceSnapshot) -> Vec<super::SurfaceOperationId> {
    snapshot
        .foreground_operation
        .iter()
        .chain(snapshot.queued_operations.iter())
        .chain(snapshot.operation_history.iter())
        .map(|operation| operation.operation_id.clone())
        .collect()
}

fn materialized_takeover_authority(
    state: &SurfaceReducerState,
    batch: &SurfaceCommitBatch,
    current_owner_epoch: ThreadOwnerEpoch,
) -> Option<ColdOwnerTakeoverAuthority> {
    let transition = batch
        .events
        .as_slice()
        .iter()
        .find_map(|event| match &event.event {
            super::SurfaceEvent::Session(super::SessionPatch::OwnerEpochChanged {
                previous,
                next,
            }) if next == &current_owner_epoch => Some((*previous, *next)),
            _ => None,
        })?;
    (transition.0 < transition.1
        && state.snapshot().thread.owner_epoch == transition.0
        && batch.cursor_before.incarnation != batch.cursor_after.incarnation)
        .then(|| ColdOwnerTakeoverAuthority {
            previous_owner_epoch: transition.0,
            previous_incarnation: batch.cursor_before.incarnation.clone(),
            current_owner_epoch: transition.1,
            new_incarnation: Some(batch.cursor_after.incarnation.clone()),
            recoverable_operations: snapshot_operation_ids(state.snapshot()),
        })
}

impl<'owner, L: SurfaceCommitLedger> RuntimeCommitCoordinator<'owner, L> {
    pub fn new_with_owner_lease(
        ledger: L,
        state: SurfaceReducerState,
        owner_lease: &'owner ExclusiveOwnerLease,
    ) -> Result<Self, SurfaceCommitError> {
        Self::new_with_authority(ledger, state, OwnerLeaseAuthority::Borrowed(owner_lease))
    }

    fn new_with_authority(
        ledger: L,
        state: SurfaceReducerState,
        owner_lease: OwnerLeaseAuthority<'owner>,
    ) -> Result<Self, SurfaceCommitError> {
        let lease = owner_lease.lease();
        if !lease.authorizes_thread(&state.snapshot().thread.thread_id) {
            return Err(SurfaceCommitError::StaleOwnerEpoch);
        }
        let next_sequence = state.snapshot().cursor.next_seq.get();
        let owner_epoch = ThreadOwnerEpoch::new(lease.owner_epoch());
        let actor_control_permit = SurfacePublisherPermit::ActorControl {
            permit_id: next_permit_id(),
            thread_id: state.snapshot().thread.thread_id.clone(),
            owner_epoch,
        };
        Ok(Self {
            ledger,
            state,
            surface_hub: None,
            recovered_publications: BoundedPublicationSuffix::default(),
            owner_lease,
            owner_epoch,
            issued_permits: vec![actor_control_permit.clone()],
            actor_control_permit,
            next_sequence,
            incomplete: None,
            recovered_prepared: None,
            cold_takeover_authority: None,
            pending_projection: None,
            #[cfg(test)]
            projection_failure_injected: false,
        })
    }
}

impl<L: SurfaceCommitLedger> RuntimeCommitCoordinator<'static, L> {
    pub fn new_with_owned_lease(
        ledger: L,
        state: SurfaceReducerState,
        owner_lease: ExclusiveOwnerLease,
    ) -> Result<Self, SurfaceCommitError> {
        Self::new_with_authority(ledger, state, OwnerLeaseAuthority::Owned(owner_lease))
    }
}

impl<'owner> RuntimeCommitCoordinator<'owner, JsonlSurfaceCommitLedger> {
    pub fn recover(
        ledger: JsonlSurfaceCommitLedger,
        state: SurfaceReducerState,
        owner_lease: &'owner ExclusiveOwnerLease,
    ) -> Result<Self, SurfaceCommitError> {
        Self::recover_with_authority(ledger, state, OwnerLeaseAuthority::Borrowed(owner_lease))
    }

    fn recover_with_authority(
        ledger: JsonlSurfaceCommitLedger,
        mut state: SurfaceReducerState,
        owner_lease: OwnerLeaseAuthority<'owner>,
    ) -> Result<Self, SurfaceCommitError> {
        let lease = owner_lease.lease();
        if !lease.authorizes_thread(&state.snapshot().thread.thread_id) {
            return Err(SurfaceCommitError::StaleOwnerEpoch);
        }
        let recovered = ledger
            .recover_batches()
            .map_err(SurfaceCommitError::Ledger)?;
        let committed = recovered.committed;
        let prepared = recovered.prepared;
        let current_owner_epoch = ThreadOwnerEpoch::new(lease.owner_epoch());
        let mut materialized_takeover = None;
        for batch in &committed {
            let candidate_takeover =
                materialized_takeover_authority(&state, batch, current_owner_epoch);
            state = match reduce_batch(SurfaceReduceMode::Rematerialization, &state, batch) {
                SurfaceReduceResult::Applied { state } => state,
                SurfaceReduceResult::AlreadyApplied { .. } => state,
                SurfaceReduceResult::Rejected { error } => {
                    return Err(SurfaceCommitError::InvalidBatch(error));
                }
            };
            if candidate_takeover.is_some() {
                materialized_takeover = candidate_takeover;
            }
        }

        let cold_takeover_authority =
            recovered_cold_takeover_authority(&state, current_owner_epoch, materialized_takeover);
        let recovered_publications = BoundedPublicationSuffix::from_committed(committed);
        let mut coordinator = Self::new_with_authority(ledger, state, owner_lease)?;
        coordinator.recovered_publications = recovered_publications;
        coordinator.cold_takeover_authority = cold_takeover_authority;
        if let Some(batch) = prepared {
            match reduce_batch(
                SurfaceReduceMode::Rematerialization,
                &coordinator.state,
                &batch,
            ) {
                SurfaceReduceResult::Applied { .. } => {}
                SurfaceReduceResult::AlreadyApplied { .. } => {
                    return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
                }
                SurfaceReduceResult::Rejected { error } => {
                    return Err(SurfaceCommitError::InvalidBatch(error));
                }
            }
            coordinator.next_sequence = batch.cursor_after.next_seq.get();
            coordinator.incomplete = Some(batch.clone());
            coordinator.recovered_prepared = Some(batch.clone());
            match coordinator.issue_exact_recovered_authority(&batch)? {
                RecoveredBatchAuthority::Single(permit) => {
                    coordinator.commit_batch(&permit, &batch)?;
                }
                RecoveredBatchAuthority::ActorGenerationTerminalization { actor, generation } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::ActorGenerationTerminalization {
                            actor: &actor,
                            generation: &generation,
                        },
                        &batch,
                        None,
                    )?;
                }
            }
        }
        Ok(coordinator)
    }
}

impl RuntimeCommitCoordinator<'static, JsonlSurfaceCommitLedger> {
    pub fn recover_with_owned_lease(
        ledger: JsonlSurfaceCommitLedger,
        state: SurfaceReducerState,
        owner_lease: ExclusiveOwnerLease,
    ) -> Result<Self, SurfaceCommitError> {
        Self::recover_with_authority(ledger, state, OwnerLeaseAuthority::Owned(owner_lease))
    }
}

impl<'owner, L: SurfaceCommitLedger> RuntimeCommitCoordinator<'owner, L> {
    pub fn commit_actor_batch(
        &mut self,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        self.commit_batch(&self.actor_control_permit.clone(), batch)
    }

    pub fn commit_actor_batch_for_projection(
        &mut self,
        context: &SurfaceProjectionContext,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        self.commit_batch_inner(&self.actor_control_permit.clone(), batch, Some(context))
    }

    pub fn commit_generation_batch(
        &mut self,
        fence: super::SurfaceOperationFence,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let permit = self.register_permit(SurfacePublisherPermit::Generation {
            permit_id: next_permit_id(),
            fence,
        });
        self.commit_batch(&permit, batch)
    }

    pub(crate) fn commit_live_generation_stop_disposition_batch(
        &mut self,
        fence: super::SurfaceOperationFence,
        operation_id: super::SurfaceOperationId,
        finalize_intent_id: super::SurfaceFinalizeIntentId,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let generation = self.register_permit(SurfacePublisherPermit::Generation {
            permit_id: next_permit_id(),
            fence,
        });
        let finalizer = self.issue_finalizer_permit(operation_id, finalize_intent_id);
        self.commit_batch_with_authority(
            BatchCommitAuthority::LiveGenerationStop {
                generation: &generation,
                finalizer: &finalizer,
            },
            batch,
            None,
        )
    }

    #[allow(dead_code)]
    pub(crate) fn commit_live_generation_suspend_batch(
        &mut self,
        fence: super::SurfaceOperationFence,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let generation = self.register_permit(SurfacePublisherPermit::Generation {
            permit_id: next_permit_id(),
            fence,
        });
        self.commit_batch_with_authority(
            BatchCommitAuthority::LiveGenerationSuspend {
                actor: &self.actor_control_permit.clone(),
                generation: &generation,
            },
            batch,
            None,
        )
    }

    pub(crate) fn commit_actor_generation_terminalization_batch(
        &mut self,
        fence: super::SurfaceOperationFence,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        let generation = self.register_permit(SurfacePublisherPermit::Generation {
            permit_id: next_permit_id(),
            fence,
        });
        self.commit_batch_with_authority(
            BatchCommitAuthority::ActorGenerationTerminalization {
                actor: &actor,
                generation: &generation,
            },
            batch,
            None,
        )
    }

    pub fn commit_finalizer_batch(
        &mut self,
        operation_id: super::SurfaceOperationId,
        finalize_intent_id: super::SurfaceFinalizeIntentId,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let permit = self.issue_finalizer_permit(operation_id, finalize_intent_id);
        self.commit_batch(&permit, batch)
    }

    pub fn ledger(&self) -> &L {
        &self.ledger
    }

    pub fn ledger_mut(&mut self) -> &mut L {
        &mut self.ledger
    }

    pub fn state(&self) -> &SurfaceReducerState {
        &self.state
    }

    pub fn bind_surface_hub(
        &mut self,
        hub: super::SurfaceHub,
    ) -> Result<(), super::SurfaceHubBindError> {
        if self.surface_hub.is_some() {
            return Err(super::SurfaceHubBindError::AlreadyBound);
        }
        if hub.thread_id() != self.state.snapshot().thread.thread_id {
            return Err(super::SurfaceHubBindError::WrongThread);
        }
        let snapshot = std::sync::Arc::new(self.state.snapshot().clone());
        let publications = self.recovered_publications.make_contiguous();
        hub.repair_committed(snapshot, publications);
        self.surface_hub = Some(hub);
        self.recovered_publications.clear();
        Ok(())
    }

    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    pub fn recovery_action(
        &self,
        operation_id: &super::SurfaceOperationId,
        materialization: &super::MaterializationCause,
    ) -> Option<RecoveryAction> {
        let snapshot = self.state.snapshot();
        let materialization_class = match materialization {
            super::MaterializationCause::SameProcessProjectionReset {
                retained_incarnation,
            } if retained_incarnation == &snapshot.cursor.incarnation => {
                RecoveryMaterialization::SameProcessProjectionReset
            }
            super::MaterializationCause::ColdOwnerTakeover {
                new_incarnation,
                new_owner_epoch,
            } if self
                .cold_takeover_authority
                .as_ref()
                .is_some_and(|authority| {
                    authority.authorizes(operation_id, snapshot, new_incarnation, new_owner_epoch)
                }) =>
            {
                RecoveryMaterialization::ColdOwnerTakeover
            }
            _ => return None,
        };
        let operation = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .find(|operation| &operation.operation_id == operation_id)?;
        let phase = match &operation.phase {
            super::OperationPhase::Requested => RecoverySourcePhase::Requested,
            super::OperationPhase::Admitted => {
                let generation = operation.generations.last()?;
                match generation.phase {
                    super::GenerationPhase::Reserved => RecoverySourcePhase::Reserved,
                    super::GenerationPhase::Started | super::GenerationPhase::Transferred => {
                        RecoverySourcePhase::StartedOrTransferred {
                            exact_terminal_interaction_unavailable: snapshot
                                .interactions
                                .iter()
                                .any(|interaction| {
                                    interaction.fence == generation.fence
                                        && (matches!(
                                            &interaction.lifecycle,
                                            super::SurfaceInteractionLifecycle::Expired { .. }
                                                | super::SurfaceInteractionLifecycle::Cancelled {
                                                    reason:
                                                        super::InteractionCancelReason::CapabilityUnavailable
                                                        | super::InteractionCancelReason::ExpiryAuthorityUnavailable { .. },
                                                }
                                        ) || (matches!(
                                            materialization_class,
                                            RecoveryMaterialization::ColdOwnerTakeover
                                        ) && matches!(
                                            interaction.lifecycle,
                                            super::SurfaceInteractionLifecycle::Resolved { .. }
                                        ) && matches!(
                                            interaction.kind,
                                            super::SurfaceInteractionKind::ToolApproval
                                                | super::SurfaceInteractionKind::PermissionRequest
                                                | super::SurfaceInteractionKind::UserInput
                                                | super::SurfaceInteractionKind::McpElicitation
                                        ) && matches!(
                                            interaction.recovery_disposition,
                                            super::InteractionUnavailableDisposition::FailOperation
                                        )))
                                }),
                        }
                    }
                    super::GenerationPhase::Stopped => return None,
                }
            }
            super::OperationPhase::Suspended { .. } => {
                let resume_starting = matches!(
                    (&operation.pending_control, operation.generations.last()),
                    (
                        Some(super::PendingControlIntent::ResumeStarting { generation_fence }),
                        Some(generation),
                    ) if generation.phase == super::GenerationPhase::Reserved
                        && generation_fence == &generation.fence
                );
                if resume_starting {
                    RecoverySourcePhase::ResumeStartingReserved
                } else {
                    RecoverySourcePhase::Suspended
                }
            }
            super::OperationPhase::Finalizing { .. } => RecoverySourcePhase::Finalizing,
            super::OperationPhase::FinalizingDegraded { .. } => {
                let cause = match self.state.finalization_degraded_cause(operation_id)? {
                    super::FinalizationDegradedCause::MissingFinalization { .. } => {
                        RecoveryDegradedCause::MissingFinalization
                    }
                    super::FinalizationDegradedCause::TerminalProjectionPending { .. } => {
                        RecoveryDegradedCause::TerminalProjectionPending
                    }
                };
                RecoverySourcePhase::FinalizingDegraded { cause }
            }
            super::OperationPhase::Terminal => RecoverySourcePhase::Terminal,
        };
        let replayability = match phase {
            RecoverySourcePhase::Requested
            | RecoverySourcePhase::StartedOrTransferred { .. }
            | RecoverySourcePhase::Finalizing
            | RecoverySourcePhase::FinalizingDegraded { .. }
            | RecoverySourcePhase::Terminal => RecoveryReplayability::NotApplicable,
            RecoverySourcePhase::Reserved
            | RecoverySourcePhase::Suspended
            | RecoverySourcePhase::ResumeStartingReserved => {
                let replayability = &operation.generations.last()?.replayability;
                match replayability {
                    super::Replayability::Replayable { .. } => RecoveryReplayability::Replayable,
                    super::Replayability::NonReplayable { live_capsule, .. } => {
                        let current = matches!(
                            (live_capsule, materialization_class),
                            (
                                super::LiveOperationCapsule::Available { incarnation },
                                RecoveryMaterialization::SameProcessProjectionReset,
                            ) if incarnation == &snapshot.cursor.incarnation
                        );
                        if current {
                            RecoveryReplayability::NonReplayableCurrent
                        } else {
                            RecoveryReplayability::NonReplayableNotCurrent
                        }
                    }
                }
            }
        };
        Some(decide_post_materialization_recovery(
            phase,
            replayability,
            materialization_class,
        ))
    }

    pub fn recover_operation(
        &mut self,
        operation_id: &super::SurfaceOperationId,
        materialization: &super::MaterializationCause,
    ) -> Result<RecoveryAction, SurfaceCommitError> {
        self.recover_operation_inner(operation_id, materialization, None)
    }

    pub fn recover_unavailable_interactions(
        &mut self,
        operation_id: &super::SurfaceOperationId,
        materialization: &super::MaterializationCause,
    ) -> Result<(), SurfaceCommitError> {
        if self
            .recovery_action(operation_id, materialization)
            .is_none()
        {
            return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
        }
        self.materialize_cold_owner_takeover(materialization)?;
        let interactions = self
            .state
            .snapshot()
            .interactions
            .iter()
            .filter(|interaction| {
                interaction.fence.operation_id == *operation_id
                    && matches!(
                        interaction.lifecycle,
                        super::SurfaceInteractionLifecycle::Requested
                    )
                    && matches!(
                        interaction.recovery_disposition,
                        super::InteractionUnavailableDisposition::FailOperation
                    )
            })
            .cloned()
            .collect::<Vec<_>>();
        for interaction in interactions {
            let next_revision = super::InteractionRevision::try_new(
                interaction
                    .revision
                    .get()
                    .checked_add(1)
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
            )
            .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
            let batch = self.interaction_recovery_batch(
                interaction.fence.clone(),
                super::InteractionPatch::Cancelled {
                    interaction_id: interaction.interaction_id,
                    expected_revision: interaction.revision,
                    next_revision,
                    reason: super::InteractionCancelReason::CapabilityUnavailable,
                },
            )?;
            let permit = self.issue_recovery_permit(interaction.fence);
            self.commit_batch(&permit, &batch)?;
        }
        Ok(())
    }

    pub(crate) fn recover_interrupted_manual_compaction(
        &mut self,
        operation_id: &super::SurfaceOperationId,
        durable_snapshot: Option<&crate::thread_store::ManualCompactionDurableSnapshot>,
    ) -> Result<bool, SurfaceCommitError> {
        let snapshot = self.state.snapshot();
        let before_messages = match &snapshot.context.compaction {
            super::CompactionState::Running {
                operation_id: running,
                before_messages,
                ..
            } if running == operation_id => *before_messages,
            _ => return Ok(false),
        };
        let next_revision = super::ContextRevision::try_new(
            snapshot
                .context
                .revision
                .get()
                .checked_add(1)
                .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        )
        .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        let mut context = snapshot.context.clone();
        context.revision = next_revision;
        if let Some(durable_snapshot) = durable_snapshot {
            if durable_snapshot.operation_id != *operation_id
                || durable_snapshot.before_messages as u64 != before_messages
            {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
            let observed_message_count = durable_snapshot.conversation.messages.len() as u64;
            let fence = snapshot
                .foreground_operation
                .iter()
                .chain(snapshot.queued_operations.iter())
                .chain(snapshot.operation_history.iter())
                .find(|operation| &operation.operation_id == operation_id)
                .and_then(|operation| operation.generations.last())
                .map(|generation| generation.fence.clone())
                .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
            context.compaction = super::CompactionState::Completed {
                operation_id: operation_id.clone(),
                reason: super::CompactionReason::Manual,
                strategy: super::NonEmptyText::try_new(durable_snapshot.strategy.clone())
                    .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                before_messages,
                after_messages: observed_message_count,
                collapsed_messages: before_messages.saturating_sub(observed_message_count),
                status_text: super::DisplayText::new("recovered completed manual compaction"),
            };
            let item_patches =
                manual_compaction_item_patches(&snapshot.items, &durable_snapshot.conversation)
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
            let batch =
                self.manual_compaction_recovery_batch(fence.clone(), item_patches, context)?;
            let permit = self.issue_recovery_permit(fence);
            self.commit_batch(&permit, &batch)?;
        } else {
            context.compaction = super::CompactionState::Idle;
            let batch = self.thread_context_recovery_batch(context)?;
            self.commit_actor_batch(&batch)?;
        }
        Ok(true)
    }

    pub fn recover_operation_with_settlement_store<S: super::ExternalSettlementStore>(
        &mut self,
        operation_id: &super::SurfaceOperationId,
        materialization: &super::MaterializationCause,
        settlement_store: &mut S,
    ) -> Result<RecoveryAction, SurfaceCommitError> {
        self.recover_operation_inner(operation_id, materialization, Some(settlement_store))
    }

    fn recover_operation_inner(
        &mut self,
        operation_id: &super::SurfaceOperationId,
        materialization: &super::MaterializationCause,
        mut settlement_store: Option<&mut dyn super::ExternalSettlementStore>,
    ) -> Result<RecoveryAction, SurfaceCommitError> {
        let action = self
            .recovery_action(operation_id, materialization)
            .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        self.materialize_cold_owner_takeover(materialization)?;
        let operation = self
            .state
            .snapshot()
            .foreground_operation
            .iter()
            .chain(self.state.snapshot().queued_operations.iter())
            .chain(self.state.snapshot().operation_history.iter())
            .find(|operation| &operation.operation_id == operation_id)
            .cloned()
            .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        match action {
            RecoveryAction::FinalizeRequested => {
                let finalize_intent_id = super::SurfaceFinalizeIntentId::try_from_bytes(
                    *uuid::Uuid::now_v7().as_bytes(),
                )
                .expect("generated UUID is v7");
                let terminal_commit_id =
                    super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                        .expect("generated UUID is v7");
                let finalizing = self.operation_recovery_batch(
                    operation_id,
                    super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                        .expect("generated UUID is v7"),
                    vec![super::OperationPatch::FinalizationStarted {
                        operation_id: operation_id.clone(),
                        finalize_intent_id: finalize_intent_id.clone(),
                        terminal_commit_id: terminal_commit_id.clone(),
                        selected_cause: super::OperationFinalizationCause::Reservation(
                            super::ReservationFinalizerReason::RuntimeRestart,
                        ),
                        suspended_cause: None,
                        expected_settlements: Vec::new(),
                    }],
                )?;
                let finalizer_permit =
                    self.issue_finalizer_permit(operation_id.clone(), finalize_intent_id.clone());
                self.commit_batch(&finalizer_permit, &finalizing)?;

                let terminal = self.operation_recovery_batch(
                    operation_id,
                    terminal_commit_id,
                    vec![super::OperationPatch::Terminal {
                        record: super::OperationTerminalRecord {
                            operation_id: operation_id.clone(),
                            finalize_intent_id,
                            terminal: super::OperationTerminal::NotAdmitted {
                                reason: super::NotAdmittedReason::RuntimeRestart,
                            },
                            usage: super::UsageTotals {
                                input_tokens: 0,
                                output_tokens: 0,
                                cache_tokens: 0,
                                estimated_cost_usd_micros: 0,
                            },
                            source_diagnostic_digest: None,
                            settlement_receipts: Vec::new(),
                            committed_at: super::UnixMillis::new(0),
                        },
                    }],
                )?;
                self.commit_batch(&finalizer_permit, &terminal)?;
                Ok(action)
            }
            RecoveryAction::StopAndSuspend => {
                let generation = self
                    .recovery_generation(&operation)
                    .cloned()
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                let batch = self.operation_recovery_batch(
                    operation_id,
                    super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                        .expect("generated UUID is v7"),
                    vec![
                        super::OperationPatch::GenerationStopped {
                            fence: generation.fence.clone(),
                            reason: super::GenerationStopReason::NotStarted {
                                reason: super::NotStartedReason::RuntimeRestart,
                            },
                            usage_delta: super::UsageTotals {
                                input_tokens: 0,
                                output_tokens: 0,
                                cache_tokens: 0,
                                estimated_cost_usd_micros: 0,
                            },
                        },
                        super::OperationPatch::Suspended {
                            operation_id: operation_id.clone(),
                            cause: super::SuspensionCause::RecoveryRequired {
                                generation_id: generation.fence.generation_id,
                            },
                        },
                    ],
                )?;
                let recovery_permit = self.issue_recovery_permit(generation.fence);
                self.commit_batch(&recovery_permit, &batch)?;
                Ok(action)
            }
            RecoveryAction::StopAndFinalizeRuntimeRestart
            | RecoveryAction::StopAndFinalizeClientCapabilityUnavailable
            | RecoveryAction::StopAndFinalizeRecoveryAbort => {
                let generation = self
                    .recovery_generation(&operation)
                    .cloned()
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                let stop_reason = match action {
                    RecoveryAction::StopAndFinalizeRuntimeRestart => {
                        super::GenerationStopReason::RuntimeRestart
                    }
                    RecoveryAction::StopAndFinalizeClientCapabilityUnavailable => {
                        super::GenerationStopReason::ExecutionFailed {
                            class:
                                super::GenerationExecutionFailureClass::ClientCapabilityUnavailable,
                            message: super::SafeDiagnosticText::try_new(
                                "required client capability became unavailable",
                            )
                            .expect("static diagnostic is bounded"),
                        }
                    }
                    RecoveryAction::StopAndFinalizeRecoveryAbort => {
                        super::GenerationStopReason::NotStarted {
                            reason: super::NotStartedReason::RuntimeRestart,
                        }
                    }
                    _ => unreachable!(),
                };
                let recovery_abort =
                    super::SuspendedFinalizationCause::RecoveryAbortNonReplayable {
                        last_generation: generation.fence.generation_id,
                    };
                let (selected_cause, suspended_cause) =
                    if matches!(operation.phase, super::OperationPhase::Suspended { .. }) {
                        (
                            super::OperationFinalizationCause::Suspended(recovery_abort.clone()),
                            Some(recovery_abort),
                        )
                    } else {
                        (
                            super::OperationFinalizationCause::GenerationStop(stop_reason.clone()),
                            None,
                        )
                    };
                let batch = self.recovery_stop_and_finalize_batch(
                    operation_id,
                    generation.fence.clone(),
                    stop_reason,
                    selected_cause,
                    suspended_cause,
                )?;
                let recovery_permit = self.issue_recovery_permit(generation.fence);
                self.commit_batch(&recovery_permit, &batch)?;
                Ok(action)
            }
            RecoveryAction::FinalizeRecoveryAbort => {
                let generation = self
                    .recovery_generation(&operation)
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                let finalize_intent_id = super::SurfaceFinalizeIntentId::try_from_bytes(
                    *uuid::Uuid::now_v7().as_bytes(),
                )
                .expect("generated UUID is v7");
                let suspended_cause =
                    super::SuspendedFinalizationCause::RecoveryAbortNonReplayable {
                        last_generation: generation.fence.generation_id,
                    };
                let batch = self.recovery_finalization_batch(
                    operation_id,
                    finalize_intent_id.clone(),
                    super::OperationFinalizationCause::Suspended(suspended_cause.clone()),
                    Some(suspended_cause),
                )?;
                let finalizer_permit =
                    self.issue_finalizer_permit(operation_id.clone(), finalize_intent_id);
                self.commit_batch(&finalizer_permit, &batch)?;
                Ok(action)
            }
            RecoveryAction::StopAndRebaseSuspension => {
                let generation = self
                    .recovery_generation(&operation)
                    .cloned()
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                let super::OperationPhase::Suspended { cause } = operation.phase else {
                    return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
                };
                let batch = self.operation_recovery_batch(
                    operation_id,
                    super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                        .expect("generated UUID is v7"),
                    vec![
                        super::OperationPatch::GenerationStopped {
                            fence: generation.fence.clone(),
                            reason: super::GenerationStopReason::NotStarted {
                                reason: super::NotStartedReason::RuntimeRestart,
                            },
                            usage_delta: zero_usage(),
                        },
                        super::OperationPatch::SuspensionRebasedAfterUnstartedResume {
                            operation_id: operation_id.clone(),
                            previous_cause: cause,
                            replacement_fence: generation.fence.clone(),
                            rebased_cause: super::SuspensionCause::RecoveryRequired {
                                generation_id: generation.fence.generation_id,
                            },
                        },
                    ],
                )?;
                let recovery_permit = self.issue_recovery_permit(generation.fence);
                self.commit_batch(&recovery_permit, &batch)?;
                Ok(action)
            }
            RecoveryAction::ReconcileOriginalFinalizer => {
                let finalization = operation
                    .finalization
                    .as_ref()
                    .cloned()
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                if !finalization.expected_settlements.is_empty() {
                    let store =
                        settlement_store
                            .as_deref_mut()
                            .ok_or(SurfaceCommitError::Settlement(
                                super::SettlementError::StoreUnavailable,
                            ))?;
                    let intent = super::DurableFinalizeIntent::new(
                        finalization.finalize_intent_id.clone(),
                        finalization.expected_settlements.clone(),
                    )
                    .map_err(SurfaceCommitError::Settlement)?;
                    let receipts = super::reconcile_finalize_intent(&intent, store)
                        .map_err(SurfaceCommitError::Settlement)?;
                    let missing = receipts
                        .into_iter()
                        .filter(|receipt| {
                            !finalization
                                .settled
                                .iter()
                                .any(|settled| settled.settlement_id == receipt.settlement_id)
                        })
                        .map(
                            |receipt| super::OperationPatch::FinalizationSettlementRecorded {
                                operation_id: operation_id.clone(),
                                finalize_intent_id: finalization.finalize_intent_id.clone(),
                                receipt,
                            },
                        )
                        .collect::<Vec<_>>();
                    if !missing.is_empty() {
                        let settlement_batch = self.operation_recovery_batch(
                            operation_id,
                            super::SurfaceCommitId::try_from_bytes(
                                *uuid::Uuid::now_v7().as_bytes(),
                            )
                            .expect("generated UUID is v7"),
                            missing,
                        )?;
                        let finalizer_permit = self.issue_finalizer_permit(
                            operation_id.clone(),
                            finalization.finalize_intent_id.clone(),
                        );
                        self.commit_batch(&finalizer_permit, &settlement_batch)?;
                    }
                }
                let operation = self
                    .state
                    .snapshot()
                    .foreground_operation
                    .iter()
                    .chain(self.state.snapshot().queued_operations.iter())
                    .chain(self.state.snapshot().operation_history.iter())
                    .find(|operation| &operation.operation_id == operation_id)
                    .cloned()
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                let finalization = operation
                    .finalization
                    .as_ref()
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                let usage = recovered_operation_usage(self.state.snapshot(), operation_id);
                let terminal = terminal_from_finalization(&operation, finalization, &usage)?;
                let batch = self.operation_recovery_batch(
                    operation_id,
                    finalization.terminal_commit_id.clone(),
                    vec![super::OperationPatch::Terminal {
                        record: super::OperationTerminalRecord {
                            operation_id: operation_id.clone(),
                            finalize_intent_id: finalization.finalize_intent_id.clone(),
                            terminal,
                            usage,
                            source_diagnostic_digest: None,
                            settlement_receipts: finalization.settled.clone(),
                            committed_at: super::UnixMillis::new(0),
                        },
                    }],
                )?;
                let finalizer_permit = self.issue_finalizer_permit(
                    operation_id.clone(),
                    finalization.finalize_intent_id.clone(),
                );
                self.commit_batch(&finalizer_permit, &batch)?;
                Ok(action)
            }
            RecoveryAction::ExposeRecoveryRequired
            | RecoveryAction::ExposeRetryFinalization
            | RecoveryAction::ExposeRetryProjection
            | RecoveryAction::NoOp => Ok(action),
        }
    }

    pub(crate) fn materialize_cold_owner_takeover(
        &mut self,
        materialization: &super::MaterializationCause,
    ) -> Result<(), SurfaceCommitError> {
        let super::MaterializationCause::ColdOwnerTakeover {
            new_incarnation,
            new_owner_epoch,
        } = materialization
        else {
            return Ok(());
        };
        let snapshot = self.state.snapshot();
        if snapshot.cursor.incarnation == *new_incarnation
            && snapshot.thread.owner_epoch == *new_owner_epoch
        {
            return if self
                .cold_takeover_authority
                .as_ref()
                .is_some_and(|authority| {
                    authority.authorizes_transition(snapshot, new_incarnation, new_owner_epoch)
                }) {
                Ok(())
            } else {
                Err(SurfaceCommitError::StaleOwnerEpoch)
            };
        }
        if !self
            .cold_takeover_authority
            .as_ref()
            .is_some_and(|authority| {
                authority.authorizes_transition(snapshot, new_incarnation, new_owner_epoch)
            })
            || new_owner_epoch != &self.owner_epoch
            || snapshot.thread.owner_epoch >= *new_owner_epoch
            || snapshot.cursor.incarnation == *new_incarnation
        {
            return Err(SurfaceCommitError::StaleOwnerEpoch);
        }

        let cursor_before = snapshot.cursor.clone();
        let durable_revision = match cursor_before.source_revision {
            super::CursorSourceRevision::Recorded { durable_revision } => {
                super::DurableRevision::try_new(
                    durable_revision
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                )
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?
            }
            super::CursorSourceRevision::Ephemeral { .. } => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
        };
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: *new_owner_epoch,
            durable_revision,
            commit_id: super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
        };
        let event = super::SurfaceEventEnvelope {
            ordinal: 0,
            event_id: super::SurfaceEventId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Thread,
            event: super::SurfaceEvent::Session(super::SessionPatch::OwnerEpochChanged {
                previous: snapshot.thread.owner_epoch,
                next: *new_owner_epoch,
            }),
        };
        let mut batch = SurfaceCommitBatch {
            cursor_before: cursor_before.clone(),
            cursor_after: super::SurfaceCursor {
                incarnation: new_incarnation.clone(),
                next_seq: super::SequenceNumber::new(
                    cursor_before
                        .next_seq
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                ),
                source_revision: super::CursorSourceRevision::Recorded { durable_revision },
                ..cursor_before
            },
            commit_class,
            event_count: 1,
            batch_digest: super::Sha256Digest::new([0; 32]),
            events: super::NonEmptyVec::try_new(vec![event])
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        };
        batch.batch_digest = super::canonical_batch_digest(&batch);
        self.commit_actor_batch(&batch)?;
        if let Some(authority) = self.cold_takeover_authority.as_mut() {
            authority.new_incarnation = Some(new_incarnation.clone());
        }
        Ok(())
    }

    fn issue_finalizer_permit(
        &mut self,
        operation_id: super::SurfaceOperationId,
        finalize_intent_id: super::SurfaceFinalizeIntentId,
    ) -> SurfacePublisherPermit {
        self.register_permit(SurfacePublisherPermit::Finalizer {
            permit_id: next_permit_id(),
            operation_id,
            finalize_intent_id,
            owner_epoch: self.owner_epoch,
        })
    }

    fn issue_recovery_permit(
        &mut self,
        historical_fence: super::SurfaceOperationFence,
    ) -> SurfacePublisherPermit {
        self.register_permit(SurfacePublisherPermit::Recovery {
            permit_id: next_permit_id(),
            current_owner_epoch: self.owner_epoch,
            historical_fence,
        })
    }

    fn register_permit(&mut self, permit: SurfacePublisherPermit) -> SurfacePublisherPermit {
        self.issued_permits.push(permit.clone());
        permit
    }

    fn issue_exact_recovered_authority(
        &mut self,
        batch: &SurfaceCommitBatch,
    ) -> Result<RecoveredBatchAuthority, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        if permit_authorizes(&self.issued_permits, &actor, batch, self.owner_epoch)
            && finalizer_background_scope_matches_state(&self.state, &actor, batch)
            && recovery_manual_compaction_matches_state(&self.state, &actor, batch)
        {
            return Ok(RecoveredBatchAuthority::Single(actor));
        }

        let events = batch.events.as_slice();
        if let Some(SurfaceScope::Generation {
            fence: historical_fence,
        }) = events.get(1).map(|event| &event.scope)
        {
            let generation = SurfacePublisherPermit::Generation {
                permit_id: next_permit_id(),
                fence: historical_fence.clone(),
            };
            let mut issued = self.issued_permits.clone();
            issued.push(generation.clone());
            if actor_generation_terminalization_authorized(
                &issued,
                &actor,
                &generation,
                batch,
                self.owner_epoch,
            ) {
                let generation = self.register_permit(generation);
                return Ok(RecoveredBatchAuthority::ActorGenerationTerminalization {
                    actor,
                    generation,
                });
            }
        }
        let first = &events[0];
        let candidate = match (&first.scope, &first.event) {
            (
                _,
                super::SurfaceEvent::Operation(
                    super::OperationPatch::FinalizationStarted {
                        operation_id,
                        finalize_intent_id,
                        ..
                    }
                    | super::OperationPatch::FinalizationSettlementRecorded {
                        operation_id,
                        finalize_intent_id,
                        ..
                    }
                    | super::OperationPatch::FinalizationDegraded {
                        operation_id,
                        finalize_intent_id,
                        ..
                    }
                    | super::OperationPatch::Terminal {
                        record:
                            super::OperationTerminalRecord {
                                operation_id,
                                finalize_intent_id,
                                ..
                            },
                    },
                ),
            ) => SurfacePublisherPermit::Finalizer {
                permit_id: next_permit_id(),
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                owner_epoch: self.owner_epoch,
            },
            _ if events.iter().any(|event| {
                matches!(
                    &event.event,
                    super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped { .. })
                )
            }) =>
            {
                let historical_fence = events
                    .iter()
                    .find_map(|event| match &event.event {
                        super::SurfaceEvent::Operation(
                            super::OperationPatch::GenerationStopped { fence, .. },
                        ) => Some(fence.clone()),
                        _ => None,
                    })
                    .ok_or(SurfaceCommitError::StalePublisherPermit)?;
                SurfacePublisherPermit::Recovery {
                    permit_id: next_permit_id(),
                    current_owner_epoch: self.owner_epoch,
                    historical_fence,
                }
            }
            (SurfaceScope::Generation { fence }, _) => SurfacePublisherPermit::Generation {
                permit_id: next_permit_id(),
                fence: fence.clone(),
            },
            (SurfaceScope::Background { fence }, _) => SurfacePublisherPermit::Background {
                permit_id: next_permit_id(),
                fence: fence.clone(),
            },
            (
                SurfaceScope::Goal { .. },
                super::SurfaceEvent::Goal(super::GoalPatchEnvelope { receipt, .. }),
            ) => SurfacePublisherPermit::Goal {
                permit_id: next_permit_id(),
                goal_fence: super::SurfaceGoalFence {
                    goal_id: receipt.goal_id.clone(),
                    goal_revision: receipt.goal_revision,
                    goal_owner_epoch: receipt.goal_owner_epoch,
                },
                receipt_digest: receipt.receipt_digest.clone(),
            },
            _ => return Err(SurfaceCommitError::StalePublisherPermit),
        };
        let mut issued = self.issued_permits.clone();
        issued.push(candidate.clone());
        if !permit_authorizes(&issued, &candidate, batch, self.owner_epoch)
            || !finalizer_background_scope_matches_state(&self.state, &candidate, batch)
            || !recovery_manual_compaction_matches_state(&self.state, &candidate, batch)
        {
            return Err(SurfaceCommitError::StalePublisherPermit);
        }
        Ok(RecoveredBatchAuthority::Single(
            self.register_permit(candidate),
        ))
    }

    fn recovery_generation<'a>(
        &self,
        operation: &'a super::OperationRecord,
    ) -> Option<&'a super::GenerationRecord> {
        operation.generations.last()
    }

    fn recovery_stop_and_finalize_batch(
        &self,
        operation_id: &super::SurfaceOperationId,
        fence: super::SurfaceOperationFence,
        stop_reason: super::GenerationStopReason,
        selected_cause: super::OperationFinalizationCause,
        suspended_cause: Option<super::SuspendedFinalizationCause>,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        let discard_reason = match &stop_reason {
            super::GenerationStopReason::Cancelled { .. } => {
                super::AssistantDiscardReason::GenerationCancelled
            }
            super::GenerationStopReason::InterruptedResumable => {
                super::AssistantDiscardReason::GenerationInterrupted
            }
            super::GenerationStopReason::RuntimeRestart
            | super::GenerationStopReason::NotStarted {
                reason: super::NotStartedReason::RuntimeRestart,
            } => super::AssistantDiscardReason::RuntimeRestart,
            super::GenerationStopReason::ProjectionFailure { .. } => {
                super::AssistantDiscardReason::ProjectionRepair
            }
            _ => super::AssistantDiscardReason::ProviderFailed,
        };
        let open_streams = self
            .state
            .snapshot()
            .assistant_streams
            .iter()
            .filter(|stream| {
                stream.fence == fence && stream.state == super::SurfaceAssistantStreamState::Open
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut patches = vec![super::OperationPatch::GenerationStopped {
            fence: fence.clone(),
            reason: stop_reason,
            usage_delta: zero_usage(),
        }];
        let finalization =
            self.recovery_finalization_patch(operation_id, selected_cause, suspended_cause);
        patches.push(finalization);
        let mut batch = self.operation_recovery_batch(
            operation_id,
            super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            patches,
        )?;
        if open_streams.is_empty() {
            return Ok(batch);
        }

        let background_scope = self
            .state
            .snapshot()
            .background_operations
            .iter()
            .find(|operation| &operation.operation_id == operation_id)
            .map(|operation| SurfaceScope::Background {
                fence: operation.fence.clone(),
            });
        let stream_scope = background_scope.unwrap_or_else(|| SurfaceScope::Generation { fence });
        let mut events = open_streams
            .into_iter()
            .map(|stream| super::SurfaceEventEnvelope {
                ordinal: 0,
                event_id: super::SurfaceEventId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7"),
                commit_class: batch.commit_class.clone(),
                scope: stream_scope.clone(),
                event: super::SurfaceEvent::Assistant(super::AssistantPatch::StreamDiscarded {
                    stream_id: stream.stream_id,
                    reason: discard_reason,
                }),
            })
            .collect::<Vec<_>>();
        events.extend(batch.events.as_slice().iter().cloned());
        for (ordinal, event) in events.iter_mut().enumerate() {
            event.ordinal = ordinal as u32;
        }
        batch.event_count = u32::try_from(events.len())
            .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        batch.cursor_after.next_seq = super::SequenceNumber::new(
            batch
                .cursor_before
                .next_seq
                .get()
                .checked_add(batch.event_count as u64)
                .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        );
        batch.events = super::NonEmptyVec::try_new(events)
            .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        batch.batch_digest = super::canonical_batch_digest(&batch);
        Ok(batch)
    }

    fn recovery_finalization_batch(
        &self,
        operation_id: &super::SurfaceOperationId,
        finalize_intent_id: super::SurfaceFinalizeIntentId,
        selected_cause: super::OperationFinalizationCause,
        suspended_cause: Option<super::SuspendedFinalizationCause>,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        self.operation_recovery_batch(
            operation_id,
            super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            vec![self.recovery_finalization_patch_with_intent(
                operation_id,
                finalize_intent_id,
                selected_cause,
                suspended_cause,
            )],
        )
    }

    fn recovery_finalization_patch(
        &self,
        operation_id: &super::SurfaceOperationId,
        selected_cause: super::OperationFinalizationCause,
        suspended_cause: Option<super::SuspendedFinalizationCause>,
    ) -> super::OperationPatch {
        self.recovery_finalization_patch_with_intent(
            operation_id,
            super::SurfaceFinalizeIntentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            selected_cause,
            suspended_cause,
        )
    }

    fn recovery_finalization_patch_with_intent(
        &self,
        operation_id: &super::SurfaceOperationId,
        finalize_intent_id: super::SurfaceFinalizeIntentId,
        selected_cause: super::OperationFinalizationCause,
        suspended_cause: Option<super::SuspendedFinalizationCause>,
    ) -> super::OperationPatch {
        super::OperationPatch::FinalizationStarted {
            operation_id: operation_id.clone(),
            finalize_intent_id,
            terminal_commit_id: super::SurfaceCommitId::try_from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            )
            .expect("generated UUID is v7"),
            selected_cause,
            suspended_cause,
            expected_settlements: Vec::new(),
        }
    }

    fn operation_recovery_batch(
        &self,
        operation_id: &super::SurfaceOperationId,
        commit_id: super::SurfaceCommitId,
        patches: Vec<super::OperationPatch>,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        let cursor_before = self.state.snapshot().cursor.clone();
        let durable_revision = match cursor_before.source_revision {
            super::CursorSourceRevision::Recorded { durable_revision } => {
                super::DurableRevision::try_new(
                    durable_revision
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                )
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?
            }
            super::CursorSourceRevision::Ephemeral { .. } => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
        };
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: self.owner_epoch,
            durable_revision,
            commit_id,
        };
        let events = patches
            .into_iter()
            .enumerate()
            .map(|(ordinal, patch)| {
                let background_scope = self
                    .state
                    .snapshot()
                    .background_operations
                    .iter()
                    .find(|operation| &operation.operation_id == operation_id)
                    .map(|operation| SurfaceScope::Background {
                        fence: operation.fence.clone(),
                    });
                let scope = match &patch {
                    super::OperationPatch::GenerationStopped { fence, .. } => background_scope
                        .unwrap_or_else(|| SurfaceScope::Generation {
                            fence: fence.clone(),
                        }),
                    _ => background_scope.unwrap_or_else(|| SurfaceScope::Operation {
                        operation_id: operation_id.clone(),
                    }),
                };
                super::SurfaceEventEnvelope {
                    ordinal: ordinal as u32,
                    event_id: super::SurfaceEventId::try_from_bytes(
                        *uuid::Uuid::now_v7().as_bytes(),
                    )
                    .expect("generated UUID is v7"),
                    commit_class: commit_class.clone(),
                    scope,
                    event: super::SurfaceEvent::Operation(patch),
                }
            })
            .collect::<Vec<_>>();
        let event_count = u32::try_from(events.len())
            .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        let events = super::NonEmptyVec::try_new(events)
            .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        let mut batch = SurfaceCommitBatch {
            cursor_before: cursor_before.clone(),
            cursor_after: super::SurfaceCursor {
                next_seq: super::SequenceNumber::new(
                    cursor_before
                        .next_seq
                        .get()
                        .checked_add(event_count as u64)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                ),
                source_revision: super::CursorSourceRevision::Recorded { durable_revision },
                ..cursor_before
            },
            commit_class,
            event_count,
            batch_digest: super::Sha256Digest::new([0; 32]),
            events,
        };
        batch.batch_digest = super::canonical_batch_digest(&batch);
        Ok(batch)
    }

    fn thread_context_recovery_batch(
        &self,
        context: super::SurfaceContextSnapshot,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        let cursor_before = self.state.snapshot().cursor.clone();
        let durable_revision = match cursor_before.source_revision {
            super::CursorSourceRevision::Recorded { durable_revision } => {
                super::DurableRevision::try_new(
                    durable_revision
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                )
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?
            }
            super::CursorSourceRevision::Ephemeral { .. } => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
        };
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: self.owner_epoch,
            durable_revision,
            commit_id: super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
        };
        let event = super::SurfaceEventEnvelope {
            ordinal: 0,
            event_id: super::SurfaceEventId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Thread,
            event: super::SurfaceEvent::Context(context),
        };
        let mut batch = SurfaceCommitBatch {
            cursor_before: cursor_before.clone(),
            cursor_after: super::SurfaceCursor {
                next_seq: super::SequenceNumber::new(
                    cursor_before
                        .next_seq
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                ),
                source_revision: super::CursorSourceRevision::Recorded { durable_revision },
                ..cursor_before
            },
            commit_class,
            event_count: 1,
            batch_digest: super::Sha256Digest::new([0; 32]),
            events: super::NonEmptyVec::try_new(vec![event])
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        };
        batch.batch_digest = super::canonical_batch_digest(&batch);
        Ok(batch)
    }

    fn manual_compaction_recovery_batch(
        &self,
        fence: super::SurfaceOperationFence,
        item_patches: Vec<super::ItemPatch>,
        context: super::SurfaceContextSnapshot,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        let cursor_before = self.state.snapshot().cursor.clone();
        let durable_revision = match cursor_before.source_revision {
            super::CursorSourceRevision::Recorded { durable_revision } => {
                super::DurableRevision::try_new(
                    durable_revision
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                )
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?
            }
            super::CursorSourceRevision::Ephemeral { .. } => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
        };
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: self.owner_epoch,
            durable_revision,
            commit_id: super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
        };
        let scope = SurfaceScope::Generation {
            fence: fence.clone(),
        };
        let mut facts = item_patches
            .into_iter()
            .map(|patch| (scope.clone(), super::SurfaceEvent::Item(patch)))
            .collect::<Vec<_>>();
        facts.push((scope, super::SurfaceEvent::Context(context)));
        if facts.len() as u64 > super::SURFACE_COMMIT_BATCH_EVENT_LIMIT {
            return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
        }
        let event_count = u32::try_from(facts.len())
            .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        let events = facts
            .into_iter()
            .enumerate()
            .map(|(ordinal, (scope, event))| super::SurfaceEventEnvelope {
                ordinal: ordinal as u32,
                event_id: super::SurfaceEventId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7"),
                commit_class: commit_class.clone(),
                scope,
                event,
            })
            .collect::<Vec<_>>();
        let mut batch = SurfaceCommitBatch {
            cursor_before: cursor_before.clone(),
            cursor_after: super::SurfaceCursor {
                next_seq: super::SequenceNumber::new(
                    cursor_before
                        .next_seq
                        .get()
                        .checked_add(event_count as u64)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                ),
                source_revision: super::CursorSourceRevision::Recorded { durable_revision },
                ..cursor_before
            },
            commit_class,
            event_count,
            batch_digest: super::Sha256Digest::new([0; 32]),
            events: super::NonEmptyVec::try_new(events)
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        };
        batch.batch_digest = super::canonical_batch_digest(&batch);
        Ok(batch)
    }

    fn interaction_recovery_batch(
        &self,
        fence: super::SurfaceOperationFence,
        patch: super::InteractionPatch,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        let cursor_before = self.state.snapshot().cursor.clone();
        let durable_revision = match cursor_before.source_revision {
            super::CursorSourceRevision::Recorded { durable_revision } => {
                super::DurableRevision::try_new(
                    durable_revision
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                )
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?
            }
            super::CursorSourceRevision::Ephemeral { .. } => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
        };
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: self.owner_epoch,
            durable_revision,
            commit_id: super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
        };
        let event = super::SurfaceEventEnvelope {
            ordinal: 0,
            event_id: super::SurfaceEventId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Generation { fence },
            event: super::SurfaceEvent::Interaction(patch),
        };
        let mut batch = SurfaceCommitBatch {
            cursor_before: cursor_before.clone(),
            cursor_after: super::SurfaceCursor {
                next_seq: super::SequenceNumber::new(
                    cursor_before
                        .next_seq
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                ),
                source_revision: super::CursorSourceRevision::Recorded { durable_revision },
                ..cursor_before
            },
            commit_class,
            event_count: 1,
            batch_digest: super::Sha256Digest::new([0; 32]),
            events: super::NonEmptyVec::try_new(vec![event])
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        };
        batch.batch_digest = super::canonical_batch_digest(&batch);
        Ok(batch)
    }

    pub fn commit_batch(
        &mut self,
        permit: &SurfacePublisherPermit,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        self.commit_batch_inner(permit, batch, None)
    }

    pub fn commit_batch_for_projection(
        &mut self,
        permit: &SurfacePublisherPermit,
        context: &SurfaceProjectionContext,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        self.commit_batch_inner(permit, batch, Some(context))
    }

    fn commit_batch_inner(
        &mut self,
        permit: &SurfacePublisherPermit,
        batch: &SurfaceCommitBatch,
        projection_context: Option<&SurfaceProjectionContext>,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        self.commit_batch_with_authority(
            BatchCommitAuthority::Single(permit),
            batch,
            projection_context,
        )
    }

    fn commit_batch_with_authority(
        &mut self,
        authority: BatchCommitAuthority<'_>,
        batch: &SurfaceCommitBatch,
        projection_context: Option<&SurfaceProjectionContext>,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        if !self
            .owner_lease
            .lease()
            .authorizes_thread(&self.state.snapshot().thread.thread_id)
        {
            return Err(SurfaceCommitError::StaleOwnerEpoch);
        }
        if let Some((token, pending_batch)) = &self.pending_projection {
            return if prepared_identity(pending_batch) == prepared_identity(batch) {
                Err(SurfaceCommitError::ProjectionPending {
                    token: token.clone(),
                })
            } else {
                Err(SurfaceCommitError::CursorRangeAlreadyConsumed)
            };
        }
        let authorized = match authority {
            BatchCommitAuthority::Single(permit) => {
                permit_authorizes(&self.issued_permits, permit, batch, self.owner_epoch)
                    && finalizer_background_scope_matches_state(&self.state, permit, batch)
                    && recovery_stream_dispositions_match_state(&self.state, permit, batch)
                    && recovery_manual_compaction_matches_state(&self.state, permit, batch)
            }
            BatchCommitAuthority::ActorGenerationTerminalization { actor, generation } => {
                actor_generation_terminalization_authorized(
                    &self.issued_permits,
                    actor,
                    generation,
                    batch,
                    self.owner_epoch,
                )
            }
            BatchCommitAuthority::LiveGenerationSuspend { actor, generation } => {
                live_generation_suspend_authorized(
                    &self.state,
                    &self.issued_permits,
                    actor,
                    generation,
                    batch,
                    self.owner_epoch,
                )
            }
            BatchCommitAuthority::LiveGenerationStop {
                generation,
                finalizer,
            } => {
                live_generation_stop_disposition_authorized(
                    &self.state,
                    &self.issued_permits,
                    generation,
                    finalizer,
                    batch,
                    self.owner_epoch,
                ) && finalizer_background_scope_matches_state(&self.state, finalizer, batch)
            }
        };
        if !authorized {
            return Err(SurfaceCommitError::StalePublisherPermit);
        }
        if matches!(
            preflight_batch(batch),
            SurfaceCommitBatchPreflightResult::Rejected { .. }
        ) {
            return Err(SurfaceCommitError::OversizedBatch);
        }
        let batch_owner_epoch = match &batch.commit_class {
            CommitClass::Recorded {
                thread_owner_epoch, ..
            } => Some(thread_owner_epoch),
            CommitClass::Ephemeral { .. } => None,
        };
        if batch_owner_epoch.is_some_and(|epoch| {
            epoch != &self.owner_epoch
                && !self.recovered_prepared_authorizes_owner_transition(batch, epoch)
        }) {
            return Err(SurfaceCommitError::StaleOwnerEpoch);
        }
        match &self.incomplete {
            Some(incomplete) if incomplete != batch => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
            Some(_) => {}
            None if batch.cursor_before.next_seq.get() != self.next_sequence => {
                return match self
                    .ledger
                    .probe_commit(commit_id(&batch.commit_class), &batch.batch_digest)
                {
                    CommitProbe::Present(receipt) => Ok(SurfaceCommitApplied { receipt }),
                    _ => Err(SurfaceCommitError::CursorRangeAlreadyConsumed),
                };
            }
            None => {}
        }

        let candidate = match reduce_batch(SurfaceReduceMode::Live, &self.state, batch) {
            SurfaceReduceResult::Applied { state } => state,
            SurfaceReduceResult::AlreadyApplied { .. } => {
                let commit_id = commit_id(&batch.commit_class);
                return match self.ledger.probe_commit(commit_id, &batch.batch_digest) {
                    CommitProbe::Present(receipt) => Ok(SurfaceCommitApplied { receipt }),
                    _ => Err(SurfaceCommitError::CursorRangeAlreadyConsumed),
                };
            }
            SurfaceReduceResult::Rejected { error } => {
                return Err(SurfaceCommitError::InvalidBatch(error));
            }
        };

        let receipt = match self.ledger.append_complete_batch(batch) {
            Ok(receipt) => {
                self.next_sequence = batch.cursor_after.next_seq.get();
                self.incomplete = Some(batch.clone());
                receipt
            }
            Err(SurfaceLedgerError::PartialAppend) => {
                self.next_sequence = batch.cursor_after.next_seq.get();
                self.incomplete = Some(batch.clone());
                return Err(SurfaceCommitError::Ledger(
                    SurfaceLedgerError::PartialAppend,
                ));
            }
            Err(error) => return Err(SurfaceCommitError::Ledger(error)),
        };
        self.ledger
            .checkpoint(&receipt)
            .map_err(SurfaceCommitError::Ledger)?;
        let materialized = if projection_context.is_some() {
            self.materialize_projection(candidate)
        } else {
            Ok(candidate)
        };
        let materialized = match materialized {
            Ok(state) => state,
            Err(_) => {
                let context = projection_context.expect("projection context exists");
                let token = RetryLocalProjectionToken::new(
                    context.request_id.clone(),
                    context.target.clone(),
                    commit_id(&batch.commit_class).clone(),
                    self.owner_epoch,
                    context.fact_family,
                    batch.events.as_slice()[0].event_id.clone(),
                )
                .as_token();
                self.pending_projection = Some((token.clone(), batch.clone()));
                return Err(SurfaceCommitError::ProjectionPending { token });
            }
        };
        self.state = materialized;
        self.incomplete = None;
        if self.recovered_prepared.as_ref() == Some(batch) {
            self.recovered_prepared = None;
        }
        if let Some(hub) = &self.surface_hub {
            hub.apply_committed(std::sync::Arc::new(self.state.snapshot().clone()), batch);
        } else {
            self.recovered_publications.push(batch);
        }
        Ok(SurfaceCommitApplied { receipt })
    }

    fn recovered_prepared_authorizes_owner_transition(
        &self,
        batch: &SurfaceCommitBatch,
        historical_epoch: &ThreadOwnerEpoch,
    ) -> bool {
        self.recovered_prepared.as_ref() == Some(batch)
            && self.state.snapshot().thread.owner_epoch == *historical_epoch
            && historical_epoch.get().checked_add(1) == Some(self.owner_epoch.get())
    }

    pub fn retry_projection(
        &mut self,
        token: &RetryProjectionToken,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        if !self
            .owner_lease
            .lease()
            .authorizes_thread(&self.state.snapshot().thread.thread_id)
        {
            return Err(SurfaceCommitError::StaleOwnerEpoch);
        }
        let Some((expected, batch)) = self.pending_projection.clone() else {
            return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
        };
        if &expected != token {
            return Err(SurfaceCommitError::StaleOwnerEpoch);
        }
        let state = self.materialize_projection_batch(&batch).map_err(|_| {
            SurfaceCommitError::ProjectionPending {
                token: token.clone(),
            }
        })?;
        let receipt = match self
            .ledger
            .probe_commit(commit_id(&batch.commit_class), &batch.batch_digest)
        {
            CommitProbe::Present(receipt) => receipt,
            _ => {
                return Err(SurfaceCommitError::Ledger(
                    SurfaceLedgerError::CommitIdentityConflict,
                ));
            }
        };
        self.state = state;
        self.pending_projection = None;
        self.incomplete = None;
        if let Some(hub) = &self.surface_hub {
            hub.apply_committed(std::sync::Arc::new(self.state.snapshot().clone()), &batch);
        } else {
            self.recovered_publications.push(&batch);
        }
        Ok(SurfaceCommitApplied { receipt })
    }

    fn materialize_projection(
        &self,
        candidate: SurfaceReducerState,
    ) -> Result<SurfaceReducerState, ()> {
        #[cfg(test)]
        if self.projection_failure_injected {
            return Err(());
        }
        Ok(candidate)
    }

    fn materialize_projection_batch(
        &self,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceReducerState, ()> {
        let candidate = match reduce_batch(SurfaceReduceMode::Live, &self.state, batch) {
            SurfaceReduceResult::Applied { state } => state,
            _ => return Err(()),
        };
        self.materialize_projection(candidate)
    }

    #[cfg(test)]
    fn inject_projection_failure(&mut self, fail: bool) {
        self.projection_failure_injected = fail;
    }
}

fn permit_authorizes(
    issued_permits: &[SurfacePublisherPermit],
    permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(permit) {
        return false;
    }
    match permit {
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: permit_epoch,
            ..
        } => {
            thread_id == &batch.cursor_before.thread_id
                && *permit_epoch == owner_epoch
                && batch.cursor_after.thread_id == *thread_id
                && (batch.events.as_slice().iter().all(|event| {
                    !matches!(
                        &event.scope,
                        SurfaceScope::Generation { .. }
                            | SurfaceScope::Background { .. }
                            | SurfaceScope::Goal { .. }
                    ) && !matches!(&event.event, super::SurfaceEvent::Goal(_))
                        && !matches!(
                            &event.event,
                            super::SurfaceEvent::Operation(
                                super::OperationPatch::FinalizationStarted { .. }
                                    | super::OperationPatch::FinalizationSettlementRecorded { .. }
                                    | super::OperationPatch::FinalizationDegraded { .. }
                                    | super::OperationPatch::Terminal { .. }
                            )
                        )
                }) || actor_control_admission_pair_authorized(batch)
                    || actor_control_resume_pair_authorized(batch))
        }
        SurfacePublisherPermit::Generation { fence, .. } => batch
            .events
            .as_slice()
            .iter()
            .all(|event| matches!(&event.scope, SurfaceScope::Generation { fence: scope } if scope == fence)),
        SurfacePublisherPermit::Background { fence, .. } => batch
            .events
            .as_slice()
            .iter()
            .all(|event| matches!(&event.scope, SurfaceScope::Background { fence: scope } if scope == fence)),
        SurfacePublisherPermit::Goal {
            goal_fence,
            receipt_digest,
            ..
        } => batch.events.as_slice().iter().all(|event| {
            matches!(
                (&event.scope, &event.event),
                (
                    SurfaceScope::Goal { goal_id, .. },
                    super::SurfaceEvent::Goal(envelope),
                ) if goal_id == &goal_fence.goal_id
                    && envelope.receipt.goal_id == goal_fence.goal_id
                    && envelope.receipt.goal_revision == goal_fence.goal_revision
                    && envelope.receipt.goal_owner_epoch == goal_fence.goal_owner_epoch
                    && envelope.receipt.receipt_digest == *receipt_digest
            )
        }),
        SurfacePublisherPermit::Finalizer {
            operation_id,
            finalize_intent_id,
            owner_epoch: permit_epoch,
            ..
        } => {
            *permit_epoch == owner_epoch
                && batch.events.as_slice().iter().all(|event| {
                    finalizer_event_authorized(operation_id, finalize_intent_id, event)
                })
        }
        SurfacePublisherPermit::Recovery {
            current_owner_epoch,
            historical_fence,
            ..
        } => {
            *current_owner_epoch == owner_epoch
                && historical_fence.thread_id == batch.cursor_before.thread_id
                && batch.cursor_after.thread_id == historical_fence.thread_id
                && recovery_batch_authorized(historical_fence, batch)
        }
    }
}

fn actor_generation_terminalization_authorized(
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    generation_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(actor_permit) || !issued_permits.contains(generation_permit) {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_owner_epoch,
            ..
        },
        SurfacePublisherPermit::Generation { fence, .. },
    ) = (actor_permit, generation_permit)
    else {
        return false;
    };
    if *actor_owner_epoch != owner_epoch
        || thread_id != &fence.thread_id
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
    {
        return false;
    }
    let Some((intent, settlements)) = batch.events.as_slice().split_first() else {
        return false;
    };
    let cause = match (&intent.scope, &intent.event) {
        (
            SurfaceScope::Operation {
                operation_id: scoped_operation,
            },
            super::SurfaceEvent::Operation(super::OperationPatch::ControlIntentCommitted {
                operation_id,
                intent:
                    super::PendingControlIntent::Terminalize {
                        operation_id: intent_operation,
                        cause,
                    },
                ..
            }),
        ) if scoped_operation == &fence.operation_id
            && operation_id == &fence.operation_id
            && intent_operation == &fence.operation_id =>
        {
            *cause
        }
        _ => return false,
    };
    settlements
        .iter()
        .all(|event| match (&event.scope, &event.event) {
            (
                SurfaceScope::Generation { fence: scope },
                super::SurfaceEvent::Interaction(super::InteractionPatch::Cancelled {
                    reason, ..
                }),
            ) if scope == fence => matches!(
                (cause, reason),
                (
                    super::TerminalizationCause::HostShutdown,
                    super::InteractionCancelReason::HostShutdown,
                ) | (
                    super::TerminalizationCause::ThreadClose,
                    super::InteractionCancelReason::ThreadClose,
                ) | (
                    super::TerminalizationCause::UserCancel,
                    super::InteractionCancelReason::OperationCancelled {
                        reason: super::CancelReason::User,
                    },
                ) | (
                    super::TerminalizationCause::GoalPause,
                    super::InteractionCancelReason::OperationCancelled {
                        reason: super::CancelReason::GoalPause,
                    },
                )
            ),
            _ => false,
        })
}

fn live_generation_suspend_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    generation_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(actor_permit) || !issued_permits.contains(generation_permit) {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_owner_epoch,
            ..
        },
        SurfacePublisherPermit::Generation { fence, .. },
    ) = (actor_permit, generation_permit)
    else {
        return false;
    };
    if *actor_owner_epoch != owner_epoch
        || thread_id != &fence.thread_id
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
    {
        return false;
    }
    let Some((suspended, prefix)) = batch.events.as_slice().split_last() else {
        return false;
    };
    let Some((stopped, stream_discards)) = prefix.split_last() else {
        return false;
    };
    if !matches!(
        (&stopped.scope, &stopped.event),
        (
            SurfaceScope::Generation { fence: scope },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                fence: patch_fence,
                reason: super::GenerationStopReason::InterruptedResumable,
                ..
            }),
        ) if scope == fence && patch_fence == fence
    ) || !matches!(
        (&suspended.scope, &suspended.event),
        (
            SurfaceScope::Operation {
                operation_id: scoped_operation,
            },
            super::SurfaceEvent::Operation(super::OperationPatch::Suspended {
                operation_id,
                cause: super::SuspensionCause::Interrupted { generation_id },
            }),
        ) if scoped_operation == &fence.operation_id
            && operation_id == &fence.operation_id
            && generation_id == &fence.generation_id
    ) {
        return false;
    }
    stream_discards_cover_open_streams(
        state,
        fence,
        &SurfaceScope::Generation {
            fence: fence.clone(),
        },
        super::AssistantDiscardReason::GenerationInterrupted,
        stream_discards,
    )
}

fn live_generation_stop_disposition_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    generation_permit: &SurfacePublisherPermit,
    finalizer_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(generation_permit) || !issued_permits.contains(finalizer_permit) {
        return false;
    }
    let (
        SurfacePublisherPermit::Generation { fence, .. },
        SurfacePublisherPermit::Finalizer {
            operation_id,
            finalize_intent_id,
            owner_epoch: finalizer_owner_epoch,
            ..
        },
    ) = (generation_permit, finalizer_permit)
    else {
        return false;
    };
    if *finalizer_owner_epoch != owner_epoch || operation_id != &fence.operation_id {
        return false;
    }
    let Some((finalization, prefix)) = batch.events.as_slice().split_last() else {
        return false;
    };
    let Some((stop, stream_discards)) = prefix.split_last() else {
        return false;
    };
    let stop_reason = match (&stop.scope, &stop.event) {
        (
            SurfaceScope::Generation { fence: scope },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                fence: patch_fence,
                reason,
                ..
            }),
        ) if scope == fence && patch_fence == fence => reason,
        _ => return false,
    };
    let expected_discard_reason = match stop_reason {
        super::GenerationStopReason::Cancelled { .. } => {
            super::AssistantDiscardReason::GenerationCancelled
        }
        super::GenerationStopReason::InterruptedResumable => {
            super::AssistantDiscardReason::GenerationInterrupted
        }
        super::GenerationStopReason::RuntimeRestart
        | super::GenerationStopReason::NotStarted {
            reason: super::NotStartedReason::RuntimeRestart,
        } => super::AssistantDiscardReason::RuntimeRestart,
        super::GenerationStopReason::ProjectionFailure { .. } => {
            super::AssistantDiscardReason::ProjectionRepair
        }
        _ => super::AssistantDiscardReason::ProviderFailed,
    };
    let generation_scope = SurfaceScope::Generation {
        fence: fence.clone(),
    };
    if !stream_discards_cover_open_streams(
        state,
        fence,
        &generation_scope,
        expected_discard_reason,
        stream_discards,
    ) {
        return false;
    }
    matches!(
        (&finalization.scope, &finalization.event),
        (
            SurfaceScope::Operation {
                operation_id: scoped_operation,
            },
            super::SurfaceEvent::Operation(super::OperationPatch::FinalizationStarted {
                operation_id: patch_operation,
                finalize_intent_id: patch_intent,
                selected_cause: super::OperationFinalizationCause::GenerationStop(selected_reason),
                suspended_cause: None,
                ..
            }),
        ) if scoped_operation == operation_id
            && patch_operation == operation_id
            && patch_intent == finalize_intent_id
            && selected_reason == stop_reason
    )
}

fn actor_control_admission_pair_authorized(batch: &SurfaceCommitBatch) -> bool {
    let [admission, item] = batch.events.as_slice() else {
        return false;
    };
    let (
        SurfaceScope::Operation {
            operation_id: scoped_operation,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::Admitted {
            operation_id,
            logical_turn_id,
            input:
                super::AdmittedInput::PendingUser {
                    item_id,
                    presentation,
                    correlation_id,
                },
            first_generation,
        }),
    ) = (&admission.scope, &admission.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Generation { fence },
        super::SurfaceEvent::Item(super::ItemPatch::Added {
            item:
                super::SurfaceItem::UserMessage {
                    id,
                    turn_id,
                    input:
                        super::SurfaceUserInputState::Pending {
                            presentation: item_presentation,
                            correlation_id: item_correlation,
                        },
                    pinned: _,
                    origin: super::SurfaceItemOrigin::UserInput,
                },
        }),
    ) = (&item.scope, &item.event)
    else {
        return false;
    };
    scoped_operation == operation_id
        && fence == &first_generation.fence
        && id == item_id
        && turn_id == logical_turn_id
        && item_presentation == presentation
        && item_correlation == correlation_id
}

fn actor_control_resume_pair_authorized(batch: &SurfaceCommitBatch) -> bool {
    let [reserved, resume] = batch.events.as_slice() else {
        return false;
    };
    let (
        SurfaceScope::Generation { fence },
        super::SurfaceEvent::Operation(super::OperationPatch::GenerationReserved { generation }),
    ) = (&reserved.scope, &reserved.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Operation {
            operation_id: scoped_operation,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::ControlIntentCommitted {
            operation_id,
            intent: super::PendingControlIntent::ResumeStarting { generation_fence },
            ..
        }),
    ) = (&resume.scope, &resume.event)
    else {
        return false;
    };
    fence == &generation.fence
        && scoped_operation == operation_id
        && operation_id == &fence.operation_id
        && generation_fence == fence
}

fn finalizer_event_authorized(
    operation_id: &super::SurfaceOperationId,
    finalize_intent_id: &super::SurfaceFinalizeIntentId,
    event: &super::SurfaceEventEnvelope,
) -> bool {
    let scope_matches = match &event.scope {
        SurfaceScope::Operation {
            operation_id: scope,
        } => scope == operation_id,
        SurfaceScope::Background { fence } => fence.operation_fence.operation_id == *operation_id,
        _ => false,
    };
    scope_matches
        && matches!(
            &event.event,
            super::SurfaceEvent::Operation(
                super::OperationPatch::FinalizationStarted {
                    operation_id: patch_operation,
                    finalize_intent_id: patch_intent,
                    ..
                }
                    | super::OperationPatch::FinalizationSettlementRecorded {
                        operation_id: patch_operation,
                        finalize_intent_id: patch_intent,
                        ..
                    }
                    | super::OperationPatch::FinalizationDegraded {
                        operation_id: patch_operation,
                        finalize_intent_id: patch_intent,
                        ..
                    }
                    | super::OperationPatch::Terminal {
                        record: super::OperationTerminalRecord {
                            operation_id: patch_operation,
                            finalize_intent_id: patch_intent,
                            ..
                        },
                    }
            ) if patch_operation == operation_id && patch_intent == finalize_intent_id
        )
}

fn finalizer_background_scope_matches_state(
    state: &SurfaceReducerState,
    permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
) -> bool {
    let SurfacePublisherPermit::Finalizer { operation_id, .. } = permit else {
        return true;
    };
    let expected = state
        .snapshot()
        .background_operations
        .iter()
        .find(|operation| &operation.operation_id == operation_id)
        .map(|operation| &operation.fence);
    batch
        .events
        .as_slice()
        .iter()
        .all(|event| match &event.scope {
            SurfaceScope::Background { fence } => expected == Some(fence),
            _ => true,
        })
}

fn stream_discards_cover_open_streams<'a>(
    state: &SurfaceReducerState,
    fence: &super::SurfaceOperationFence,
    expected_scope: &SurfaceScope,
    expected_reason: super::AssistantDiscardReason,
    events: impl IntoIterator<Item = &'a super::SurfaceEventEnvelope>,
) -> bool {
    let expected = state
        .snapshot()
        .assistant_streams
        .iter()
        .filter(|stream| {
            stream.fence == *fence && stream.state == super::SurfaceAssistantStreamState::Open
        })
        .map(|stream| &stream.stream_id)
        .collect::<Vec<_>>();
    let actual = events.into_iter().collect::<Vec<_>>();
    actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(event, stream_id)| {
            event.scope == *expected_scope
                && matches!(
                    &event.event,
                    super::SurfaceEvent::Assistant(super::AssistantPatch::StreamDiscarded {
                        stream_id: discarded,
                        reason,
                    }) if discarded == stream_id && *reason == expected_reason
                )
        })
}

fn recovery_stream_dispositions_match_state(
    state: &SurfaceReducerState,
    permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
) -> bool {
    let SurfacePublisherPermit::Recovery {
        historical_fence, ..
    } = permit
    else {
        return true;
    };
    let Some(stop) = batch
        .events
        .as_slice()
        .iter()
        .find(|event| recovery_generation_stop_authorized(historical_fence, event))
    else {
        return true;
    };
    let stop_reason = match &stop.event {
        super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
            reason, ..
        }) => reason,
        _ => return false,
    };
    let expected_reason = match stop_reason {
        super::GenerationStopReason::Cancelled { .. } => {
            super::AssistantDiscardReason::GenerationCancelled
        }
        super::GenerationStopReason::InterruptedResumable => {
            super::AssistantDiscardReason::GenerationInterrupted
        }
        super::GenerationStopReason::RuntimeRestart
        | super::GenerationStopReason::NotStarted {
            reason: super::NotStartedReason::RuntimeRestart,
        } => super::AssistantDiscardReason::RuntimeRestart,
        super::GenerationStopReason::ProjectionFailure { .. } => {
            super::AssistantDiscardReason::ProjectionRepair
        }
        _ => super::AssistantDiscardReason::ProviderFailed,
    };
    let expected_scope = stop.scope.clone();
    stream_discards_cover_open_streams(
        state,
        historical_fence,
        &expected_scope,
        expected_reason,
        batch.events.as_slice().iter().filter(|event| {
            matches!(
                event.event,
                super::SurfaceEvent::Assistant(super::AssistantPatch::StreamDiscarded { .. })
            )
        }),
    )
}

fn recovery_batch_authorized(
    historical_fence: &super::SurfaceOperationFence,
    batch: &SurfaceCommitBatch,
) -> bool {
    let events = batch.events.as_slice();
    if recovery_manual_compaction_completion_authorized(historical_fence, events) {
        return true;
    }
    if let [event] = events {
        return matches!(
            (&event.scope, &event.event),
            (
                SurfaceScope::Generation { fence },
                super::SurfaceEvent::Interaction(super::InteractionPatch::Cancelled {
                    reason: super::InteractionCancelReason::CapabilityUnavailable,
                    ..
                }),
            ) if fence == historical_fence
        );
    }
    let stops = events
        .iter()
        .filter(|event| recovery_generation_stop_authorized(historical_fence, event))
        .collect::<Vec<_>>();
    if stops.len() != 1 {
        return false;
    }
    let background_fence = match &stops[0].scope {
        SurfaceScope::Background { fence } => Some(fence),
        _ => None,
    };
    let stop_reason = match &stops[0].event {
        super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
            reason, ..
        }) => reason,
        _ => return false,
    };
    let expected_discard_reason = match stop_reason {
        super::GenerationStopReason::Cancelled { .. } => {
            super::AssistantDiscardReason::GenerationCancelled
        }
        super::GenerationStopReason::InterruptedResumable => {
            super::AssistantDiscardReason::GenerationInterrupted
        }
        super::GenerationStopReason::RuntimeRestart
        | super::GenerationStopReason::NotStarted {
            reason: super::NotStartedReason::RuntimeRestart,
        } => super::AssistantDiscardReason::RuntimeRestart,
        super::GenerationStopReason::ProjectionFailure { .. } => {
            super::AssistantDiscardReason::ProjectionRepair
        }
        _ => super::AssistantDiscardReason::ProviderFailed,
    };
    let dispositions = events
        .iter()
        .filter(|event| !recovery_generation_stop_authorized(historical_fence, event))
        .collect::<Vec<_>>();
    let mut non_stream_dispositions = 0usize;
    dispositions.iter().all(|event| {
        let stream_discard = match (&event.scope, &event.event) {
            (
                SurfaceScope::Generation { fence },
                super::SurfaceEvent::Assistant(super::AssistantPatch::StreamDiscarded {
                    reason,
                    ..
                }),
            ) => {
                background_fence.is_none()
                    && fence == historical_fence
                    && *reason == expected_discard_reason
            }
            (
                SurfaceScope::Background { fence },
                super::SurfaceEvent::Assistant(super::AssistantPatch::StreamDiscarded {
                    reason,
                    ..
                }),
            ) => {
                background_fence == Some(fence)
                    && &fence.operation_fence == historical_fence
                    && *reason == expected_discard_reason
            }
            _ => false,
        };
        if stream_discard {
            true
        } else {
            non_stream_dispositions += 1;
            recovery_event_authorized(historical_fence, background_fence, event)
        }
    }) && non_stream_dispositions == 1
}

fn recovery_manual_compaction_completion_authorized(
    historical_fence: &super::SurfaceOperationFence,
    events: &[super::SurfaceEventEnvelope],
) -> bool {
    let mut completed = 0usize;
    events.iter().all(|event| {
        if !matches!(
            &event.scope,
            SurfaceScope::Generation { fence } if fence == historical_fence
        ) {
            return false;
        }
        match &event.event {
            super::SurfaceEvent::Item(super::ItemPatch::Removed {
                reason: super::ItemRemovalReason::Compacted,
                ..
            }) => true,
            super::SurfaceEvent::Item(super::ItemPatch::Added { .. }) => true,
            super::SurfaceEvent::Context(super::SurfaceContextSnapshot {
                compaction:
                    super::CompactionState::Completed {
                        operation_id,
                        reason: super::CompactionReason::Manual,
                        ..
                    },
                ..
            }) if operation_id == &historical_fence.operation_id => {
                completed += 1;
                true
            }
            _ => false,
        }
    }) && completed == 1
}

fn recovery_manual_compaction_matches_state(
    state: &SurfaceReducerState,
    permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
) -> bool {
    if !matches!(permit, SurfacePublisherPermit::Recovery { .. })
        || !batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                super::SurfaceEvent::Context(super::SurfaceContextSnapshot {
                    compaction: super::CompactionState::Completed {
                        reason: super::CompactionReason::Manual,
                        ..
                    },
                    ..
                })
            )
        })
    {
        return true;
    }
    manual_compaction_item_rebuild_paired(state.snapshot(), batch)
}

fn recovery_generation_stop_authorized(
    historical_fence: &super::SurfaceOperationFence,
    event: &super::SurfaceEventEnvelope,
) -> bool {
    let exact_scope = matches!(
        (&event.scope, &event.event),
        (
            SurfaceScope::Generation { fence },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                fence: patch_fence,
                ..
            }),
        ) if fence == historical_fence && patch_fence == historical_fence
    ) || matches!(
        (&event.scope, &event.event),
        (
            SurfaceScope::Background { fence },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                fence: patch_fence,
                ..
            }),
        ) if &fence.operation_fence == historical_fence && patch_fence == historical_fence
    );
    exact_scope
        && matches!(
            &event.event,
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                reason: super::GenerationStopReason::RuntimeRestart
                    | super::GenerationStopReason::NotStarted {
                        reason: super::NotStartedReason::RuntimeRestart,
                    }
                    | super::GenerationStopReason::ExecutionFailed {
                        class: super::GenerationExecutionFailureClass::ClientCapabilityUnavailable,
                        ..
                    },
                ..
            })
        )
}

fn recovery_event_authorized(
    historical_fence: &super::SurfaceOperationFence,
    background_fence: Option<&super::SurfaceBackgroundFence>,
    event: &super::SurfaceEventEnvelope,
) -> bool {
    match (&event.scope, &event.event) {
        (
            SurfaceScope::Generation { fence },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                fence: patch_fence,
                ..
            }),
        ) => {
            background_fence.is_none()
                && fence == historical_fence
                && patch_fence == historical_fence
        }
        (
            SurfaceScope::Background { fence },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                fence: patch_fence,
                ..
            }),
        ) => {
            background_fence == Some(fence)
                && &fence.operation_fence == historical_fence
                && patch_fence == historical_fence
        }
        (
            SurfaceScope::Operation { operation_id },
            super::SurfaceEvent::Operation(
                super::OperationPatch::Suspended {
                    operation_id: patch_operation,
                    ..
                }
                | super::OperationPatch::SuspensionRebasedAfterUnstartedResume {
                    operation_id: patch_operation,
                    ..
                }
                | super::OperationPatch::FinalizationStarted {
                    operation_id: patch_operation,
                    ..
                },
            ),
        ) => {
            background_fence.is_none()
                && operation_id == &historical_fence.operation_id
                && patch_operation == &historical_fence.operation_id
        }
        (
            SurfaceScope::Background { fence },
            super::SurfaceEvent::Operation(
                super::OperationPatch::Suspended {
                    operation_id: patch_operation,
                    ..
                }
                | super::OperationPatch::SuspensionRebasedAfterUnstartedResume {
                    operation_id: patch_operation,
                    ..
                }
                | super::OperationPatch::FinalizationStarted {
                    operation_id: patch_operation,
                    ..
                },
            ),
        ) => {
            background_fence == Some(fence)
                && &fence.operation_fence == historical_fence
                && patch_operation == &historical_fence.operation_id
        }
        _ => false,
    }
}

fn next_permit_id() -> super::SurfacePublisherPermitId {
    let first = uuid::Uuid::now_v7();
    let second = uuid::Uuid::now_v7();
    let mut bytes = [0; 32];
    bytes[..16].copy_from_slice(first.as_bytes());
    bytes[16..].copy_from_slice(second.as_bytes());
    super::SurfacePublisherPermitId::new(bytes)
}

fn recovered_operation_usage(
    snapshot: &super::SurfaceSnapshot,
    operation_id: &super::SurfaceOperationId,
) -> super::UsageTotals {
    snapshot
        .usage
        .active_operation
        .as_ref()
        .filter(|(active, _)| active == operation_id)
        .map(|(_, usage)| usage.clone())
        .unwrap_or_else(zero_usage)
}

fn terminal_from_terminalization(cause: super::TerminalizationCause) -> super::OperationTerminal {
    match cause {
        super::TerminalizationCause::UserCancel => super::OperationTerminal::Cancelled {
            reason: super::CancelReason::User,
        },
        super::TerminalizationCause::GoalPause => super::OperationTerminal::Cancelled {
            reason: super::CancelReason::GoalPause,
        },
        super::TerminalizationCause::HostShutdown => super::OperationTerminal::Shutdown {
            reason: super::SurfaceShutdownReason::HostShutdown,
        },
        super::TerminalizationCause::ThreadClose => super::OperationTerminal::Shutdown {
            reason: super::SurfaceShutdownReason::ThreadClose,
        },
    }
}

fn terminal_failure_class(class: super::GenerationExecutionFailureClass) -> super::FailureClass {
    match class {
        super::GenerationExecutionFailureClass::Provider => super::FailureClass::Provider,
        super::GenerationExecutionFailureClass::Tool => super::FailureClass::Tool,
        super::GenerationExecutionFailureClass::Hook => super::FailureClass::Hook,
        super::GenerationExecutionFailureClass::Workflow => super::FailureClass::Workflow,
        super::GenerationExecutionFailureClass::InputResolution => {
            super::FailureClass::InputResolution
        }
        super::GenerationExecutionFailureClass::ClientCapabilityUnavailable => {
            super::FailureClass::ClientCapabilityUnavailable
        }
        super::GenerationExecutionFailureClass::LegacyApprovalRequired => {
            super::FailureClass::LegacyApprovalRequired
        }
        super::GenerationExecutionFailureClass::RuntimeInvariant => {
            super::FailureClass::RuntimeInvariant
        }
        super::GenerationExecutionFailureClass::ExternalEffectAmbiguous => {
            super::FailureClass::ExternalEffectAmbiguous
        }
        super::GenerationExecutionFailureClass::RemoteResourceCleanupAmbiguous => {
            super::FailureClass::RemoteResourceCleanupAmbiguous
        }
    }
}

fn terminal_from_generation_stop(
    operation: &super::OperationRecord,
    reason: &super::GenerationStopReason,
    usage: &super::UsageTotals,
) -> Result<super::OperationTerminal, SurfaceCommitError> {
    let last_generation = || {
        operation
            .generations
            .last()
            .map(|generation| generation.fence.generation_id)
            .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)
    };
    Ok(match reason {
        super::GenerationStopReason::Completed { status } => match status {
            super::GenerationCompletionStatus::Success => super::OperationTerminal::Succeeded {
                usage: usage.clone(),
            },
            super::GenerationCompletionStatus::VerificationFailed { message } => {
                super::OperationTerminal::Failed {
                    class: super::FailureClass::Verification,
                    message: message.clone(),
                }
            }
            super::GenerationCompletionStatus::BudgetExhausted { budget } => {
                super::OperationTerminal::BudgetExhausted {
                    budget: budget.clone(),
                }
            }
        },
        super::GenerationStopReason::Cancelled { cause } => terminal_from_terminalization(*cause),
        super::GenerationStopReason::InterruptedResumable
        | super::GenerationStopReason::ProviderSuspended
        | super::GenerationStopReason::RuntimeRestart => {
            super::OperationTerminal::AbortedByRuntimeRestart {
                last_generation: last_generation()?,
            }
        }
        super::GenerationStopReason::ProjectionFailure { message } => {
            super::OperationTerminal::Failed {
                class: super::FailureClass::Persistence,
                message: message.clone(),
            }
        }
        super::GenerationStopReason::ExecutionFailed { class, message } => {
            super::OperationTerminal::Failed {
                class: terminal_failure_class(*class),
                message: message.clone(),
            }
        }
        super::GenerationStopReason::Panicked { message } => super::OperationTerminal::Panicked {
            message: message.clone(),
        },
        super::GenerationStopReason::NotStarted { reason } => match reason {
            super::NotStartedReason::ReservationExpired => super::OperationTerminal::NotAdmitted {
                reason: super::NotAdmittedReason::ReservationExpired,
            },
            super::NotStartedReason::Cancelled { cause } => terminal_from_terminalization(*cause),
            super::NotStartedReason::Interrupted | super::NotStartedReason::RuntimeRestart => {
                super::OperationTerminal::AbortedByRuntimeRestart {
                    last_generation: last_generation()?,
                }
            }
            super::NotStartedReason::StartCommitFailure { message } => {
                super::OperationTerminal::Failed {
                    class: super::FailureClass::Persistence,
                    message: message.clone(),
                }
            }
            super::NotStartedReason::MissingLiveInputCapsule => super::OperationTerminal::Failed {
                class: super::FailureClass::RuntimeInvariant,
                message: super::SafeDiagnosticText::try_new(
                    "non-replayable operation input capsule is unavailable before generation start",
                )
                .expect("static diagnostic is valid"),
            },
            super::NotStartedReason::AdmissionRejected { reason } => {
                super::OperationTerminal::NotAdmitted {
                    reason: match reason {
                        super::AdmissionRejectionReason::ConfigurationConflict => {
                            super::NotAdmittedReason::ConfigurationConflict
                        }
                        super::AdmissionRejectionReason::PolicyConflict => {
                            super::NotAdmittedReason::PolicyConflict
                        }
                    },
                }
            }
            super::NotStartedReason::Shutdown { reason } => {
                super::OperationTerminal::Shutdown { reason: *reason }
            }
        },
    })
}

fn terminal_from_finalization(
    operation: &super::OperationRecord,
    finalization: &super::OperationFinalizationRecord,
    usage: &super::UsageTotals,
) -> Result<super::OperationTerminal, SurfaceCommitError> {
    Ok(match &finalization.selected_cause {
        super::OperationFinalizationCause::Terminalization(cause) => {
            terminal_from_terminalization(*cause)
        }
        super::OperationFinalizationCause::GenerationStop(reason) => {
            terminal_from_generation_stop(operation, reason, usage)?
        }
        super::OperationFinalizationCause::Reservation(reason) => {
            super::OperationTerminal::NotAdmitted {
                reason: match reason {
                    super::ReservationFinalizerReason::ReservationExpired => {
                        super::NotAdmittedReason::ReservationExpired
                    }
                    super::ReservationFinalizerReason::AdmissionRejected { reason } => match reason
                    {
                        super::AdmissionRejectionReason::ConfigurationConflict => {
                            super::NotAdmittedReason::ConfigurationConflict
                        }
                        super::AdmissionRejectionReason::PolicyConflict => {
                            super::NotAdmittedReason::PolicyConflict
                        }
                    },
                    super::ReservationFinalizerReason::CancelledBeforeAdmission => {
                        super::NotAdmittedReason::CancelledBeforeAdmission
                    }
                    super::ReservationFinalizerReason::RuntimeRestart => {
                        super::NotAdmittedReason::RuntimeRestart
                    }
                    super::ReservationFinalizerReason::HostShutdown => {
                        super::NotAdmittedReason::HostShutdown
                    }
                    super::ReservationFinalizerReason::ThreadClose => {
                        super::NotAdmittedReason::ThreadClose
                    }
                },
            }
        }
        super::OperationFinalizationCause::OperationJoinSettlement(source) => {
            super::OperationTerminal::JoinFailed {
                message: source.message.clone(),
            }
        }
        super::OperationFinalizationCause::Suspended(cause) => match cause {
            super::SuspendedFinalizationCause::Terminalization(cause) => {
                terminal_from_terminalization(*cause)
            }
            super::SuspendedFinalizationCause::ResumeStartCommitFailure { message } => {
                super::OperationTerminal::Failed {
                    class: super::FailureClass::Persistence,
                    message: message.clone(),
                }
            }
            super::SuspendedFinalizationCause::RecoveryAbortNonReplayable { last_generation } => {
                super::OperationTerminal::AbortedByRuntimeRestart {
                    last_generation: *last_generation,
                }
            }
        },
    })
}

fn prepared_identity(batch: &SurfaceCommitBatch) -> PreparedSurfaceCommit {
    PreparedSurfaceCommit {
        commit_id: commit_id(&batch.commit_class).clone(),
        event_count: batch.event_count,
        batch_digest: batch.batch_digest.clone(),
        cursor_before: batch.cursor_before.clone(),
        cursor_after: batch.cursor_after.clone(),
    }
}

fn commit_id(class: &CommitClass) -> &SurfaceCommitId {
    match class {
        CommitClass::Recorded { commit_id, .. } | CommitClass::Ephemeral { commit_id, .. } => {
            commit_id
        }
    }
}

fn zero_usage() -> super::UsageTotals {
    super::UsageTotals {
        input_tokens: 0,
        output_tokens: 0,
        cache_tokens: 0,
        estimated_cost_usd_micros: 0,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryReplayability {
    NotApplicable,
    Replayable,
    NonReplayableCurrent,
    NonReplayableNotCurrent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryMaterialization {
    SameProcessProjectionReset,
    ColdOwnerTakeover,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryDegradedCause {
    MissingFinalization,
    TerminalProjectionPending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoverySourcePhase {
    Requested,
    Reserved,
    StartedOrTransferred {
        exact_terminal_interaction_unavailable: bool,
    },
    Suspended,
    ResumeStartingReserved,
    Finalizing,
    FinalizingDegraded {
        cause: RecoveryDegradedCause,
    },
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryAction {
    FinalizeRequested,
    StopAndSuspend,
    StopAndFinalizeRecoveryAbort,
    StopAndFinalizeClientCapabilityUnavailable,
    StopAndFinalizeRuntimeRestart,
    ExposeRecoveryRequired,
    FinalizeRecoveryAbort,
    StopAndRebaseSuspension,
    ReconcileOriginalFinalizer,
    ExposeRetryFinalization,
    ExposeRetryProjection,
    NoOp,
}

pub fn decide_post_materialization_recovery(
    phase: RecoverySourcePhase,
    replayability: RecoveryReplayability,
    materialization: RecoveryMaterialization,
) -> RecoveryAction {
    use RecoveryAction::*;
    use RecoveryReplayability::*;
    use RecoverySourcePhase::*;

    let current_live_capsule = matches!(
        (replayability, materialization),
        (Replayable, _)
            | (
                NonReplayableCurrent,
                RecoveryMaterialization::SameProcessProjectionReset
            )
    );
    match phase {
        Requested => FinalizeRequested,
        Reserved if current_live_capsule => StopAndSuspend,
        Reserved => StopAndFinalizeRecoveryAbort,
        StartedOrTransferred {
            exact_terminal_interaction_unavailable: true,
        } => StopAndFinalizeClientCapabilityUnavailable,
        StartedOrTransferred { .. } => StopAndFinalizeRuntimeRestart,
        Suspended if current_live_capsule => ExposeRecoveryRequired,
        Suspended => FinalizeRecoveryAbort,
        ResumeStartingReserved if current_live_capsule => StopAndRebaseSuspension,
        ResumeStartingReserved => StopAndFinalizeRecoveryAbort,
        Finalizing => ReconcileOriginalFinalizer,
        FinalizingDegraded {
            cause: RecoveryDegradedCause::MissingFinalization,
        } => ExposeRetryFinalization,
        FinalizingDegraded {
            cause: RecoveryDegradedCause::TerminalProjectionPending,
        } => ExposeRetryProjection,
        Terminal => NoOp,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownPlanError {
    ImmutableConflict,
    MissingDurableBarrier,
    IncompleteBarrier,
    OutputScopeMismatch,
}

#[derive(Clone, Debug, Default)]
pub struct ImmutableShutdownLedger {
    record: Option<super::ShutdownBarrierRecord>,
    durable_plan: bool,
}

impl ImmutableShutdownLedger {
    pub(crate) fn from_durable_record(
        record: super::ShutdownBarrierRecord,
    ) -> Result<Self, ShutdownPlanError> {
        validate_shutdown_plan(&record.plan)?;
        validate_shutdown_settlements(&record.plan, &record.settled)?;
        if let super::ShutdownBarrierState::Closed { retained_output } = &record.state {
            validate_closed_shutdown_record(&record.plan, &record.settled, retained_output)?;
        }
        Ok(Self {
            record: Some(record),
            durable_plan: true,
        })
    }

    pub(crate) fn durable_record(&self) -> Option<&super::ShutdownBarrierRecord> {
        self.record.as_ref()
    }

    pub(crate) fn mark_plan_durable(
        &mut self,
        expected: &super::ShutdownBarrierPlan,
    ) -> Result<(), ShutdownPlanError> {
        if self.record.as_ref().map(|record| &record.plan) != Some(expected) {
            return Err(ShutdownPlanError::ImmutableConflict);
        }
        self.durable_plan = true;
        Ok(())
    }

    pub fn record(
        &mut self,
        plan: super::ShutdownBarrierPlan,
    ) -> Result<&super::ShutdownBarrierPlan, ShutdownPlanError> {
        if self.record.is_some() {
            return if self.record.as_ref().map(|record| &record.plan) == Some(&plan) {
                Ok(&self.record.as_ref().expect("shutdown record exists").plan)
            } else {
                Err(ShutdownPlanError::ImmutableConflict)
            };
        }
        self.record = Some(super::ShutdownBarrierRecord {
            plan,
            settled: Vec::new(),
            state: super::ShutdownBarrierState::Closing,
        });
        Ok(&self
            .record
            .as_ref()
            .expect("shutdown record was inserted")
            .plan)
    }

    pub fn plan(&self) -> Option<&super::ShutdownBarrierPlan> {
        self.record.as_ref().map(|record| &record.plan)
    }

    pub fn signal_authorized(&self) -> bool {
        self.record.is_some() && self.durable_plan
    }

    pub fn settle(
        &mut self,
        acknowledgement: super::MutationCommitAck,
    ) -> Result<(), ShutdownPlanError> {
        if !self.durable_plan {
            return Err(ShutdownPlanError::MissingDurableBarrier);
        }
        let record = self
            .record
            .as_mut()
            .ok_or(ShutdownPlanError::MissingDurableBarrier)?;
        if matches!(record.state, super::ShutdownBarrierState::Closed { .. }) {
            return Err(ShutdownPlanError::ImmutableConflict);
        }
        if record.settled.contains(&acknowledgement) {
            return Ok(());
        }
        if !shutdown_ack_matches_plan(&record.plan, &acknowledgement)
            || record
                .settled
                .iter()
                .any(|existing| shutdown_acks_target_same_requirement(existing, &acknowledgement))
        {
            return Err(ShutdownPlanError::OutputScopeMismatch);
        }
        record.settled.push(acknowledgement);
        Ok(())
    }

    pub fn close(
        &mut self,
        output: super::RetainedShutdownOutput,
    ) -> Result<super::RetainedShutdownOutput, ShutdownPlanError> {
        if !self.durable_plan {
            return Err(ShutdownPlanError::MissingDurableBarrier);
        }
        let record = self
            .record
            .as_mut()
            .ok_or(ShutdownPlanError::MissingDurableBarrier)?;
        let matching_scope = match (&record.plan, &output) {
            (
                super::ShutdownBarrierPlan::CloseThread { thread, .. },
                super::RetainedShutdownOutput::CloseThread { output },
            ) => shutdown_thread_output_matches(thread, output),
            (
                super::ShutdownBarrierPlan::ShutdownHost {
                    host_incarnation,
                    threads,
                    ..
                },
                super::RetainedShutdownOutput::ShutdownHost { output },
            ) => {
                &output.host_incarnation == host_incarnation
                    && threads.len() == output.closed_threads.len()
                    && threads.iter().all(|plan| {
                        output
                            .closed_threads
                            .iter()
                            .any(|closed| shutdown_thread_output_matches(plan, closed))
                    })
            }
            _ => false,
        };
        if !matching_scope {
            return Err(ShutdownPlanError::OutputScopeMismatch);
        }
        let existing_output = match &record.state {
            super::ShutdownBarrierState::Closed { retained_output } => Some(retained_output),
            super::ShutdownBarrierState::Closing => None,
        };
        if let Some(existing_output) = existing_output {
            if existing_output != &output {
                return Err(ShutdownPlanError::ImmutableConflict);
            }
            return Ok(existing_output.clone());
        }
        validate_closed_shutdown_record(&record.plan, &record.settled, &output)?;
        record.state = super::ShutdownBarrierState::Closed {
            retained_output: output,
        };
        match &record.state {
            super::ShutdownBarrierState::Closed { retained_output } => Ok(retained_output.clone()),
            super::ShutdownBarrierState::Closing => unreachable!(),
        }
    }

    pub fn retained_output(&self) -> Option<&super::RetainedShutdownOutput> {
        match &self.record.as_ref()?.state {
            super::ShutdownBarrierState::Closed { retained_output } => Some(retained_output),
            super::ShutdownBarrierState::Closing => None,
        }
    }
}

fn validate_shutdown_plan(plan: &super::ShutdownBarrierPlan) -> Result<(), ShutdownPlanError> {
    let mut thread_ids = std::collections::BTreeSet::new();
    let validate_thread = |thread: &super::ShutdownThreadPlan| {
        let (thread_id, owner_epoch, operations, session_closed, catalog_closed) = match thread {
            super::ShutdownThreadPlan::Recorded {
                thread_id,
                owner_epoch,
                operations,
                session_closed,
                catalog_closed,
            } => (
                thread_id,
                owner_epoch,
                operations,
                session_closed,
                Some(catalog_closed),
            ),
            super::ShutdownThreadPlan::Ephemeral {
                thread_id,
                owner_epoch,
                operations,
                session_closed,
                ..
            } => (thread_id, owner_epoch, operations, session_closed, None),
        };
        if session_closed.thread_id != *thread_id
            || session_closed.family != SurfaceFactFamily::Session
        {
            return false;
        }
        if let Some(catalog) = catalog_closed {
            if !matches!(
                &catalog.identity,
                super::HostReceiptRequirementIdentity::SessionCatalog {
                    thread_id: Some(expected), ..
                } if expected == thread_id
            ) {
                return false;
            }
        }
        operations.iter().all(|operation| {
            let (operation_id, finalize_intent_id, terminal_commit_id, requirement) =
                match operation {
                    super::ShutdownOperationPlan::ExistingTerminal {
                        operation_id,
                        finalize_intent_id,
                        terminal_commit_id,
                        requirement,
                    }
                    | super::ShutdownOperationPlan::PlannedFinalization {
                        operation_id,
                        finalize_intent_id,
                        terminal_commit_id,
                        requirement,
                        ..
                    } => (
                        operation_id,
                        finalize_intent_id,
                        terminal_commit_id,
                        requirement,
                    ),
                };
            let _ = finalize_intent_id;
            requirement.thread_id == *thread_id
                && requirement.thread_owner_epoch == *owner_epoch
                && requirement.operation_id == *operation_id
                && requirement.terminal_commit_id == *terminal_commit_id
        })
    };

    match plan {
        super::ShutdownBarrierPlan::CloseThread { thread, .. } => {
            if validate_thread(thread) {
                Ok(())
            } else {
                Err(ShutdownPlanError::OutputScopeMismatch)
            }
        }
        super::ShutdownBarrierPlan::ShutdownHost {
            host_incarnation,
            threads,
            final_host_lifecycle,
            ..
        } => {
            let lifecycle_matches = final_host_lifecycle.host_incarnation == *host_incarnation
                && matches!(
                    &final_host_lifecycle.identity,
                    super::HostReceiptRequirementIdentity::HostLifecycle {
                        host_incarnation: identity_host, ..
                    } if identity_host == host_incarnation
                );
            if lifecycle_matches
                && threads.iter().all(|thread| {
                    let thread_id = shutdown_thread_id(thread);
                    thread_ids.insert(thread_id.clone()) && validate_thread(thread)
                })
            {
                Ok(())
            } else {
                Err(ShutdownPlanError::OutputScopeMismatch)
            }
        }
    }
}

fn validate_shutdown_settlements(
    plan: &super::ShutdownBarrierPlan,
    settled: &[super::MutationCommitAck],
) -> Result<(), ShutdownPlanError> {
    if settled
        .iter()
        .any(|ack| !shutdown_ack_matches_plan(plan, ack))
        || settled.iter().enumerate().any(|(index, ack)| {
            settled[..index]
                .iter()
                .any(|prior| shutdown_acks_target_same_requirement(prior, ack))
        })
    {
        Err(ShutdownPlanError::OutputScopeMismatch)
    } else {
        Ok(())
    }
}

fn validate_closed_shutdown_record(
    plan: &super::ShutdownBarrierPlan,
    settled: &[super::MutationCommitAck],
    output: &super::RetainedShutdownOutput,
) -> Result<(), ShutdownPlanError> {
    let outputs_match =
        match (plan, output) {
            (
                super::ShutdownBarrierPlan::CloseThread { thread, .. },
                super::RetainedShutdownOutput::CloseThread { output },
            ) => shutdown_thread_is_fully_settled(thread, output, settled),
            (
                super::ShutdownBarrierPlan::ShutdownHost {
                    host_incarnation,
                    threads,
                    final_host_lifecycle,
                    ..
                },
                super::RetainedShutdownOutput::ShutdownHost { output },
            ) => &output.host_incarnation == host_incarnation
                && threads.len() == output.closed_threads.len()
                && threads.iter().all(|thread| {
                    output
                        .closed_threads
                        .iter()
                        .any(|closed| shutdown_thread_is_fully_settled(thread, closed, settled))
                }) && settled.iter().any(|ack| {
                host_ack_matches_requirement(final_host_lifecycle, ack)
                    && matches!(
                        ack,
                        super::MutationCommitAck::HostCommitAck {
                            identity: super::HostReceiptIdentityPair::HostLifecycle { receipt, .. },
                            ..
                        } if receipt == &output.host_receipt
                    )
            }),
            _ => false,
        };
    if outputs_match {
        Ok(())
    } else {
        Err(ShutdownPlanError::IncompleteBarrier)
    }
}

fn shutdown_thread_is_fully_settled(
    plan: &super::ShutdownThreadPlan,
    output: &super::ClosedThreadReceipt,
    settled: &[super::MutationCommitAck],
) -> bool {
    if !shutdown_thread_output_matches(plan, output) {
        return false;
    }
    let (operations, session_closed, catalog_closed, closed_cursor, output_terminals) = match (
        plan, output,
    ) {
        (
            super::ShutdownThreadPlan::Recorded {
                operations,
                session_closed,
                catalog_closed,
                ..
            },
            super::ClosedThreadReceipt::Recorded {
                operation_terminals,
                closed_cursor,
                catalog_receipt,
                ..
            },
        ) => {
            if !settled.iter().any(|ack| {
                    host_ack_matches_requirement(catalog_closed, ack)
                        && matches!(
                            ack,
                            super::MutationCommitAck::HostCommitAck {
                                identity: super::HostReceiptIdentityPair::SessionCatalog { receipt, .. },
                                ..
                            } if receipt == catalog_receipt
                        )
                }) {
                    return false;
                }
            (
                operations,
                session_closed,
                Some(catalog_closed),
                closed_cursor,
                operation_terminals,
            )
        }
        (
            super::ShutdownThreadPlan::Ephemeral {
                operations,
                session_closed,
                ..
            },
            super::ClosedThreadReceipt::Ephemeral {
                operation_terminals,
                closed_cursor,
                ..
            },
        ) => (
            operations,
            session_closed,
            None,
            closed_cursor,
            operation_terminals,
        ),
        _ => return false,
    };
    let _ = catalog_closed;
    let session_matches = settled.iter().any(|ack| {
        thread_ack_matches_requirement(plan, session_closed, ack)
            && matches!(ack, super::MutationCommitAck::ThreadLocalCursor { cursor, .. } if cursor == closed_cursor)
    });
    let operations_match = operations.len() == output_terminals.len()
        && operations.iter().all(|operation| {
            let requirement = shutdown_operation_requirement(operation);
            settled.iter().any(|ack| {
                operation_ack_matches_requirement(requirement, ack)
                    && matches!(
                        ack,
                        super::MutationCommitAck::OperationTerminalAck { value, .. }
                            if output_terminals.contains(value)
                    )
            })
        });
    session_matches && operations_match
}

fn shutdown_thread_id(thread: &super::ShutdownThreadPlan) -> &super::SurfaceThreadId {
    match thread {
        super::ShutdownThreadPlan::Recorded { thread_id, .. }
        | super::ShutdownThreadPlan::Ephemeral { thread_id, .. } => thread_id,
    }
}

fn shutdown_operation_requirement(
    operation: &super::ShutdownOperationPlan,
) -> &super::OperationTerminalAckRequirement {
    match operation {
        super::ShutdownOperationPlan::ExistingTerminal { requirement, .. }
        | super::ShutdownOperationPlan::PlannedFinalization { requirement, .. } => requirement,
    }
}

fn shutdown_ack_matches_plan(
    plan: &super::ShutdownBarrierPlan,
    acknowledgement: &super::MutationCommitAck,
) -> bool {
    let thread_matches = |thread: &super::ShutdownThreadPlan| {
        let (operations, session_closed, catalog_closed) = match thread {
            super::ShutdownThreadPlan::Recorded {
                operations,
                session_closed,
                catalog_closed,
                ..
            } => (operations, session_closed, Some(catalog_closed)),
            super::ShutdownThreadPlan::Ephemeral {
                operations,
                session_closed,
                ..
            } => (operations, session_closed, None),
        };
        thread_ack_matches_requirement(thread, session_closed, acknowledgement)
            || catalog_closed.is_some_and(|requirement| {
                host_ack_matches_requirement(requirement, acknowledgement)
            })
            || operations.iter().any(|operation| {
                operation_ack_matches_requirement(
                    shutdown_operation_requirement(operation),
                    acknowledgement,
                )
            })
    };
    match plan {
        super::ShutdownBarrierPlan::CloseThread { thread, .. } => thread_matches(thread),
        super::ShutdownBarrierPlan::ShutdownHost {
            threads,
            final_host_lifecycle,
            ..
        } => {
            threads.iter().any(thread_matches)
                || host_ack_matches_requirement(final_host_lifecycle, acknowledgement)
        }
    }
}

fn thread_ack_matches_requirement(
    thread: &super::ShutdownThreadPlan,
    requirement: &super::ThreadCursorAckRequirement,
    acknowledgement: &super::MutationCommitAck,
) -> bool {
    let owner_epoch = match thread {
        super::ShutdownThreadPlan::Recorded { owner_epoch, .. }
        | super::ShutdownThreadPlan::Ephemeral { owner_epoch, .. } => owner_epoch,
    };
    matches!(
        acknowledgement,
        super::MutationCommitAck::ThreadLocalCursor {
            cursor,
            family,
            event_id,
            commit_class:
                CommitClass::Recorded {
                    thread_owner_epoch,
                    commit_id,
                    ..
                },
        } if cursor.thread_id == requirement.thread_id
            && family == &requirement.family
            && event_id == &requirement.event_id
            && commit_id == &requirement.commit_id
            && thread_owner_epoch == owner_epoch
    )
}

fn operation_ack_matches_requirement(
    requirement: &super::OperationTerminalAckRequirement,
    acknowledgement: &super::MutationCommitAck,
) -> bool {
    matches!(
        acknowledgement,
        super::MutationCommitAck::OperationTerminalAck {
            thread_id,
            thread_owner_epoch,
            operation_id,
            value,
        } if thread_id == &requirement.thread_id
            && thread_owner_epoch == &requirement.thread_owner_epoch
            && operation_id == &requirement.operation_id
            && value.operation_id == requirement.operation_id
            && value.cursor.thread_id == requirement.thread_id
            && matches!(
                &value.commit_class,
                CommitClass::Recorded {
                    thread_owner_epoch,
                    commit_id,
                    ..
                } if *thread_owner_epoch == requirement.thread_owner_epoch
                    && commit_id == &requirement.terminal_commit_id
            )
    )
}

fn host_ack_matches_requirement(
    requirement: &super::HostReceiptAckRequirement,
    acknowledgement: &super::MutationCommitAck,
) -> bool {
    let super::MutationCommitAck::HostCommitAck {
        host_incarnation,
        identity,
        commit_id,
        receipt_digest,
    } = acknowledgement
    else {
        return false;
    };
    if host_incarnation != &requirement.host_incarnation
        || commit_id != &requirement.commit_id
        || receipt_digest != &requirement.receipt_digest
    {
        return false;
    }
    match (&requirement.identity, identity) {
        (
            super::HostReceiptRequirementIdentity::SessionCatalog {
                thread_id,
                revision,
            },
            super::HostReceiptIdentityPair::SessionCatalog {
                thread_id: ack_thread,
                revision: ack_revision,
                receipt,
            },
        ) => {
            thread_id == ack_thread
                && revision == ack_revision
                && receipt.thread_id == *thread_id
                && receipt.catalog_revision == *revision
                && receipt.action == super::SurfaceSessionCatalogAction::Closed
        }
        (
            super::HostReceiptRequirementIdentity::HostLifecycle {
                host_incarnation,
                revision,
            },
            super::HostReceiptIdentityPair::HostLifecycle {
                host_incarnation: ack_host,
                revision: ack_revision,
                receipt,
            },
        ) => {
            host_incarnation == ack_host
                && revision == ack_revision
                && receipt.host_incarnation == *host_incarnation
                && receipt.lifecycle_revision == *revision
                && receipt.shutdown_commit_id == requirement.commit_id
                && receipt.stage == super::SurfaceHostShutdownStage::Last
        }
        _ => false,
    }
}

fn shutdown_acks_target_same_requirement(
    first: &super::MutationCommitAck,
    second: &super::MutationCommitAck,
) -> bool {
    match (first, second) {
        (
            super::MutationCommitAck::ThreadLocalCursor {
                cursor: first_cursor,
                family: first_family,
                ..
            },
            super::MutationCommitAck::ThreadLocalCursor {
                cursor: second_cursor,
                family: second_family,
                ..
            },
        ) => first_cursor.thread_id == second_cursor.thread_id && first_family == second_family,
        (
            super::MutationCommitAck::OperationTerminalAck {
                operation_id: first_operation,
                ..
            },
            super::MutationCommitAck::OperationTerminalAck {
                operation_id: second_operation,
                ..
            },
        ) => first_operation == second_operation,
        (
            super::MutationCommitAck::HostCommitAck {
                host_incarnation: first_host,
                commit_id: first_commit,
                ..
            },
            super::MutationCommitAck::HostCommitAck {
                host_incarnation: second_host,
                commit_id: second_commit,
                ..
            },
        ) => first_host == second_host && first_commit == second_commit,
        _ => false,
    }
}

fn shutdown_thread_output_matches(
    plan: &super::ShutdownThreadPlan,
    output: &super::ClosedThreadReceipt,
) -> bool {
    match (plan, output) {
        (
            super::ShutdownThreadPlan::Recorded {
                thread_id,
                operations,
                ..
            },
            super::ClosedThreadReceipt::Recorded {
                thread_id: output_thread_id,
                operation_terminals,
                ..
            },
        ) => {
            thread_id == output_thread_id
                && shutdown_operation_outputs_match(operations, operation_terminals)
        }
        (
            super::ShutdownThreadPlan::Ephemeral {
                thread_id,
                persistence,
                operations,
                ..
            },
            super::ClosedThreadReceipt::Ephemeral {
                thread_id: output_thread_id,
                persistence: output_persistence,
                operation_terminals,
                ..
            },
        ) => {
            thread_id == output_thread_id
                && persistence == output_persistence
                && shutdown_operation_outputs_match(operations, operation_terminals)
        }
        _ => false,
    }
}

fn shutdown_operation_outputs_match(
    plans: &[super::ShutdownOperationPlan],
    outputs: &[super::OperationTerminalAtCursor],
) -> bool {
    plans.len() == outputs.len()
        && plans.iter().all(|plan| {
            let requirement = match plan {
                super::ShutdownOperationPlan::ExistingTerminal { requirement, .. }
                | super::ShutdownOperationPlan::PlannedFinalization { requirement, .. } => {
                    requirement
                }
            };
            outputs.iter().any(|output| {
                output.operation_id == requirement.operation_id
                    && matches!(
                        &output.commit_class,
                        CommitClass::Recorded {
                            thread_owner_epoch,
                            commit_id,
                            ..
                        } if *thread_owner_epoch == requirement.thread_owner_epoch
                            && commit_id == &requirement.terminal_commit_id
                    )
                    && output.cursor.thread_id == requirement.thread_id
            })
        })
}

pub fn select_shutdown_cause(
    existing: Option<super::OperationFinalizationCause>,
    requested: super::ShutdownRequestCause,
) -> super::ShutdownSelectedCause {
    match existing {
        Some(cause) => super::ShutdownSelectedCause::ExistingWinning { cause },
        None => super::ShutdownSelectedCause::Requested { cause: requested },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_surface::reducer::tests::{
        digest, reducer_snapshot, thread_id, uuid_v7_bytes,
    };

    #[derive(Default)]
    struct TestLedger {
        writes: usize,
        receipt: Option<DurableBatchReceipt>,
    }

    impl SurfaceCommitLedger for TestLedger {
        fn append_complete_batch(
            &mut self,
            batch: &SurfaceCommitBatch,
        ) -> Result<DurableBatchReceipt, SurfaceLedgerError> {
            self.writes += 1;
            let CommitClass::Recorded {
                commit_id,
                durable_revision,
                ..
            } = &batch.commit_class
            else {
                unreachable!();
            };
            let receipt = DurableBatchReceipt {
                commit_id: commit_id.clone(),
                durable_revision: *durable_revision,
                event_count: batch.event_count,
                batch_digest: batch.batch_digest.clone(),
                cursor_after: batch.cursor_after.clone(),
            };
            self.receipt = Some(receipt.clone());
            Ok(receipt)
        }

        fn checkpoint(&mut self, _receipt: &DurableBatchReceipt) -> Result<(), SurfaceLedgerError> {
            self.writes += 1;
            Ok(())
        }

        fn probe_commit(
            &self,
            _id: &SurfaceCommitId,
            _digest: &super::super::Sha256Digest,
        ) -> CommitProbe {
            self.receipt
                .clone()
                .map(CommitProbe::Present)
                .unwrap_or(CommitProbe::Absent)
        }
    }

    struct TestClock;

    impl super::super::InjectedRuntimeClock for TestClock {
        fn clock_id(&self) -> super::super::HostMonotonicClockId {
            super::super::HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(90)).unwrap()
        }

        fn monotonic_tick(&self) -> u64 {
            1
        }

        fn wall_clock_ms(&self) -> i64 {
            1
        }
    }

    fn test_batch(state: &SurfaceReducerState) -> SurfaceCommitBatch {
        let durable_revision = super::super::DurableRevision::try_new(2).unwrap();
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            durable_revision,
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(91)).unwrap(),
        };
        let event = super::super::SurfaceEventEnvelope {
            ordinal: 0,
            event_id: super::super::SurfaceEventId::try_from_bytes(uuid_v7_bytes(92)).unwrap(),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Thread,
            event: super::super::SurfaceEvent::Session(super::super::SessionPatch::RuntimeFault {
                class: super::super::FailureClass::Persistence,
                message: super::super::DisplayText::new("projection test"),
                causative_generation: None,
            }),
        };
        let mut batch = SurfaceCommitBatch {
            cursor_before: state.snapshot().cursor.clone(),
            cursor_after: super::super::SurfaceCursor {
                next_seq: super::super::SequenceNumber::new(1),
                source_revision: super::super::CursorSourceRevision::Recorded { durable_revision },
                ..state.snapshot().cursor.clone()
            },
            commit_class,
            event_count: 1,
            batch_digest: digest(0),
            events: super::super::NonEmptyVec::try_new(vec![event]).unwrap(),
        };
        batch.batch_digest = super::super::canonical_batch_digest(&batch);
        batch
    }

    fn test_batch_with_events(
        state: &SurfaceReducerState,
        events: Vec<(SurfaceScope, super::super::SurfaceEvent)>,
    ) -> SurfaceCommitBatch {
        let mut batch = test_batch(state);
        let event_count = events.len() as u32;
        batch.events = super::super::NonEmptyVec::try_new(
            events
                .into_iter()
                .enumerate()
                .map(
                    |(ordinal, (scope, event))| super::super::SurfaceEventEnvelope {
                        ordinal: ordinal as u32,
                        event_id: super::super::SurfaceEventId::try_from_bytes(uuid_v7_bytes(
                            100 + ordinal as u8,
                        ))
                        .unwrap(),
                        commit_class: batch.commit_class.clone(),
                        scope,
                        event,
                    },
                )
                .collect(),
        )
        .unwrap();
        batch.event_count = event_count;
        batch.cursor_after.next_seq = super::super::SequenceNumber::new(event_count as u64);
        batch.batch_digest = super::super::canonical_batch_digest(&batch);
        batch
    }

    fn test_operation_fence(seed: u8) -> super::super::SurfaceOperationFence {
        super::super::SurfaceOperationFence {
            thread_id: thread_id(),
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            operation_id: super::super::SurfaceOperationId::try_from_bytes(uuid_v7_bytes(seed))
                .unwrap(),
            generation_id: super::super::SurfaceGenerationId::new(0),
        }
    }

    fn finalization_started(
        operation_id: super::super::SurfaceOperationId,
        finalize_intent_id: super::super::SurfaceFinalizeIntentId,
    ) -> super::super::OperationPatch {
        super::super::OperationPatch::FinalizationStarted {
            operation_id,
            finalize_intent_id,
            terminal_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(110)).unwrap(),
            selected_cause: super::super::OperationFinalizationCause::Reservation(
                super::super::ReservationFinalizerReason::RuntimeRestart,
            ),
            suspended_cause: None,
            expected_settlements: Vec::new(),
        }
    }

    #[test]
    fn actor_control_permit_cannot_publish_terminal() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let operation_id = test_operation_fence(111).operation_id;
        let finalize_intent_id =
            super::super::SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(112)).unwrap();
        let batch = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Operation {
                    operation_id: operation_id.clone(),
                },
                super::super::SurfaceEvent::Operation(super::super::OperationPatch::Terminal {
                    record: super::super::OperationTerminalRecord {
                        operation_id,
                        finalize_intent_id,
                        terminal: super::super::OperationTerminal::NotAdmitted {
                            reason: super::super::NotAdmittedReason::RuntimeRestart,
                        },
                        usage: zero_usage(),
                        source_diagnostic_digest: None,
                        settlement_receipts: Vec::new(),
                        committed_at: super::super::UnixMillis::new(0),
                    },
                }),
            )],
        );
        let permit = SurfacePublisherPermit::ActorControl {
            permit_id: super::super::SurfacePublisherPermitId::new([3; 32]),
            thread_id: thread_id(),
            owner_epoch: ThreadOwnerEpoch::new(1),
        };

        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn actor_control_permit_rejects_specialized_authority_classes() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let operation_fence = test_operation_fence(112);
        let operation_id = operation_fence.operation_id.clone();
        let permit = SurfacePublisherPermit::ActorControl {
            permit_id: super::super::SurfacePublisherPermitId::new([13; 32]),
            thread_id: thread_id(),
            owner_epoch: ThreadOwnerEpoch::new(1),
        };
        let finalizing = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Operation {
                    operation_id: operation_id.clone(),
                },
                super::super::SurfaceEvent::Operation(finalization_started(
                    operation_id,
                    super::super::SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(113))
                        .unwrap(),
                )),
            )],
        );
        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &finalizing,
            ThreadOwnerEpoch::new(1),
        ));

        let generation = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Generation {
                    fence: operation_fence,
                },
                super::super::SurfaceEvent::Session(super::super::SessionPatch::RuntimeFault {
                    class: super::super::FailureClass::Persistence,
                    message: super::super::DisplayText::new("generation authority"),
                    causative_generation: None,
                }),
            )],
        );
        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &generation,
            ThreadOwnerEpoch::new(1),
        ));

        let goal_id = super::super::SurfaceGoalId::try_new("actor-control-goal").unwrap();
        let goal = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Goal {
                    goal_id: goal_id.clone(),
                    causative_generation: None,
                },
                super::super::SurfaceEvent::Goal(super::super::GoalPatchEnvelope {
                    receipt: super::super::SurfaceGoalStoreReceipt {
                        goal_id: goal_id.clone(),
                        goal_revision: super::super::GoalRevision::try_new(2).unwrap(),
                        objective_revision: super::super::GoalObjectiveRevision::new(1),
                        catalog_revision: super::super::GoalCatalogRevision::try_new(1).unwrap(),
                        goal_owner_epoch: super::super::GoalOwnerEpoch::try_new(1).unwrap(),
                        row_state: super::super::SurfaceGoalReceiptState::Removed {
                            tombstone_revision: super::super::GoalRevision::try_new(2).unwrap(),
                        },
                        store_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(114))
                            .unwrap(),
                        receipt_digest: digest(114),
                    },
                    patch: super::super::GoalPatch::Removed {
                        goal_id,
                        previous_revision: super::super::GoalRevision::try_new(1).unwrap(),
                        tombstone_revision: super::super::GoalRevision::try_new(2).unwrap(),
                    },
                }),
            )],
        );
        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &goal,
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn actor_generation_terminalization_requires_matching_interaction_cancel_reason() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let fence = test_operation_fence(113);
        let actor = SurfacePublisherPermit::ActorControl {
            permit_id: super::super::SurfacePublisherPermitId::new([31; 32]),
            thread_id: thread_id(),
            owner_epoch: ThreadOwnerEpoch::new(1),
        };
        let generation = SurfacePublisherPermit::Generation {
            permit_id: super::super::SurfacePublisherPermitId::new([32; 32]),
            fence: fence.clone(),
        };
        let issued = vec![actor.clone(), generation.clone()];
        let cases = [
            (
                super::super::TerminalizationCause::UserCancel,
                super::super::InteractionCancelReason::OperationCancelled {
                    reason: super::super::CancelReason::User,
                },
            ),
            (
                super::super::TerminalizationCause::GoalPause,
                super::super::InteractionCancelReason::OperationCancelled {
                    reason: super::super::CancelReason::GoalPause,
                },
            ),
            (
                super::super::TerminalizationCause::HostShutdown,
                super::super::InteractionCancelReason::HostShutdown,
            ),
            (
                super::super::TerminalizationCause::ThreadClose,
                super::super::InteractionCancelReason::ThreadClose,
            ),
        ];

        for (cause_index, (cause, _)) in cases.iter().enumerate() {
            for (reason_index, (_, reason)) in cases.iter().enumerate() {
                let operation_id = fence.operation_id.clone();
                let batch = test_batch_with_events(
                    &state,
                    vec![
                        (
                            SurfaceScope::Operation {
                                operation_id: operation_id.clone(),
                            },
                            super::super::SurfaceEvent::Operation(
                                super::super::OperationPatch::ControlIntentCommitted {
                                    operation_id: operation_id.clone(),
                                    request_id: super::super::SurfaceRequestId::try_from_bytes(
                                        uuid_v7_bytes(130),
                                    )
                                    .unwrap(),
                                    intent: super::super::PendingControlIntent::Terminalize {
                                        operation_id: operation_id.clone(),
                                        cause: *cause,
                                    },
                                },
                            ),
                        ),
                        (
                            SurfaceScope::Generation {
                                fence: fence.clone(),
                            },
                            super::super::SurfaceEvent::Interaction(
                                super::super::InteractionPatch::Cancelled {
                                    interaction_id:
                                        super::super::SurfaceInteractionId::try_from_bytes(
                                            uuid_v7_bytes(131),
                                        )
                                        .unwrap(),
                                    expected_revision: super::super::InteractionRevision::try_new(
                                        1,
                                    )
                                    .unwrap(),
                                    next_revision: super::super::InteractionRevision::try_new(2)
                                        .unwrap(),
                                    reason: reason.clone(),
                                },
                            ),
                        ),
                    ],
                );

                assert_eq!(
                    actor_generation_terminalization_authorized(
                        &issued,
                        &actor,
                        &generation,
                        &batch,
                        ThreadOwnerEpoch::new(1),
                    ),
                    cause_index == reason_index,
                    "cause {cause:?} with cancellation {reason:?}",
                );
            }
        }
    }

    #[test]
    fn finalizer_permit_binds_operation_and_finalize_intent() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let operation_id = test_operation_fence(113).operation_id;
        let finalize_intent_id =
            super::super::SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(114)).unwrap();
        let batch = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Operation {
                    operation_id: operation_id.clone(),
                },
                super::super::SurfaceEvent::Operation(finalization_started(
                    operation_id.clone(),
                    finalize_intent_id.clone(),
                )),
            )],
        );
        let permit = SurfacePublisherPermit::Finalizer {
            permit_id: super::super::SurfacePublisherPermitId::new([4; 32]),
            operation_id: operation_id.clone(),
            finalize_intent_id,
            owner_epoch: ThreadOwnerEpoch::new(1),
        };
        let wrong_intent = SurfacePublisherPermit::Finalizer {
            permit_id: super::super::SurfacePublisherPermitId::new([5; 32]),
            operation_id,
            finalize_intent_id: super::super::SurfaceFinalizeIntentId::try_from_bytes(
                uuid_v7_bytes(115),
            )
            .unwrap(),
            owner_epoch: ThreadOwnerEpoch::new(1),
        };

        assert!(permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
        assert!(!permit_authorizes(
            std::slice::from_ref(&wrong_intent),
            &wrong_intent,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &test_batch(&state),
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn recovery_permit_binds_exact_historical_fence_and_disposition() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let historical_fence = test_operation_fence(116);
        let batch = test_batch_with_events(
            &state,
            vec![
                (
                    SurfaceScope::Generation {
                        fence: historical_fence.clone(),
                    },
                    super::super::SurfaceEvent::Operation(
                        super::super::OperationPatch::GenerationStopped {
                            fence: historical_fence.clone(),
                            reason: super::super::GenerationStopReason::RuntimeRestart,
                            usage_delta: zero_usage(),
                        },
                    ),
                ),
                (
                    SurfaceScope::Operation {
                        operation_id: historical_fence.operation_id.clone(),
                    },
                    super::super::SurfaceEvent::Operation(
                        super::super::OperationPatch::Suspended {
                            operation_id: historical_fence.operation_id.clone(),
                            cause: super::super::SuspensionCause::RecoveryRequired {
                                generation_id: historical_fence.generation_id,
                            },
                        },
                    ),
                ),
            ],
        );
        let permit = SurfacePublisherPermit::Recovery {
            permit_id: super::super::SurfacePublisherPermitId::new([6; 32]),
            current_owner_epoch: ThreadOwnerEpoch::new(1),
            historical_fence: historical_fence.clone(),
        };
        let wrong_fence = SurfacePublisherPermit::Recovery {
            permit_id: super::super::SurfacePublisherPermitId::new([7; 32]),
            current_owner_epoch: ThreadOwnerEpoch::new(1),
            historical_fence: test_operation_fence(117),
        };

        assert!(permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
        assert!(!permit_authorizes(
            std::slice::from_ref(&wrong_fence),
            &wrong_fence,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
        let stop_only = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Generation {
                    fence: historical_fence.clone(),
                },
                super::super::SurfaceEvent::Operation(
                    super::super::OperationPatch::GenerationStopped {
                        fence: historical_fence.clone(),
                        reason: super::super::GenerationStopReason::RuntimeRestart,
                        usage_delta: zero_usage(),
                    },
                ),
            )],
        );
        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &stop_only,
            ThreadOwnerEpoch::new(1),
        ));
        let two_dispositions = test_batch_with_events(
            &state,
            vec![
                (
                    SurfaceScope::Generation {
                        fence: historical_fence.clone(),
                    },
                    super::super::SurfaceEvent::Operation(
                        super::super::OperationPatch::GenerationStopped {
                            fence: historical_fence.clone(),
                            reason: super::super::GenerationStopReason::RuntimeRestart,
                            usage_delta: zero_usage(),
                        },
                    ),
                ),
                (
                    SurfaceScope::Operation {
                        operation_id: historical_fence.operation_id.clone(),
                    },
                    super::super::SurfaceEvent::Operation(
                        super::super::OperationPatch::Suspended {
                            operation_id: historical_fence.operation_id.clone(),
                            cause: super::super::SuspensionCause::RecoveryRequired {
                                generation_id: historical_fence.generation_id,
                            },
                        },
                    ),
                ),
                (
                    SurfaceScope::Operation {
                        operation_id: historical_fence.operation_id.clone(),
                    },
                    super::super::SurfaceEvent::Operation(finalization_started(
                        historical_fence.operation_id.clone(),
                        super::super::SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(118))
                            .unwrap(),
                    )),
                ),
            ],
        );
        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &two_dispositions,
            ThreadOwnerEpoch::new(1),
        ));
        let arbitrary = test_batch_with_events(
            &state,
            vec![
                (
                    SurfaceScope::Generation {
                        fence: historical_fence.clone(),
                    },
                    super::super::SurfaceEvent::Operation(
                        super::super::OperationPatch::GenerationStopped {
                            fence: historical_fence,
                            reason: super::super::GenerationStopReason::RuntimeRestart,
                            usage_delta: zero_usage(),
                        },
                    ),
                ),
                (
                    SurfaceScope::Thread,
                    super::super::SurfaceEvent::Session(super::super::SessionPatch::RuntimeFault {
                        class: super::super::FailureClass::Persistence,
                        message: super::super::DisplayText::new("arbitrary recovery write"),
                        causative_generation: None,
                    }),
                ),
            ],
        );
        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &arbitrary,
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn goal_permit_binds_exact_fence_and_receipt_digest() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let goal_id = super::super::SurfaceGoalId::try_new("goal-permit").unwrap();
        let goal_fence = super::super::SurfaceGoalFence {
            goal_id: goal_id.clone(),
            goal_revision: super::super::GoalRevision::try_new(2).unwrap(),
            goal_owner_epoch: super::super::GoalOwnerEpoch::try_new(3).unwrap(),
        };
        let receipt_digest = digest(118);
        let batch = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Goal {
                    goal_id: goal_id.clone(),
                    causative_generation: None,
                },
                super::super::SurfaceEvent::Goal(super::super::GoalPatchEnvelope {
                    receipt: super::super::SurfaceGoalStoreReceipt {
                        goal_id: goal_id.clone(),
                        goal_revision: goal_fence.goal_revision,
                        objective_revision: super::super::GoalObjectiveRevision::new(1),
                        catalog_revision: super::super::GoalCatalogRevision::try_new(1).unwrap(),
                        goal_owner_epoch: goal_fence.goal_owner_epoch,
                        row_state: super::super::SurfaceGoalReceiptState::Removed {
                            tombstone_revision: goal_fence.goal_revision,
                        },
                        store_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(119))
                            .unwrap(),
                        receipt_digest: receipt_digest.clone(),
                    },
                    patch: super::super::GoalPatch::Removed {
                        goal_id,
                        previous_revision: super::super::GoalRevision::try_new(1).unwrap(),
                        tombstone_revision: goal_fence.goal_revision,
                    },
                }),
            )],
        );
        let permit = SurfacePublisherPermit::Goal {
            permit_id: super::super::SurfacePublisherPermitId::new([8; 32]),
            goal_fence: goal_fence.clone(),
            receipt_digest,
        };
        let wrong_digest = SurfacePublisherPermit::Goal {
            permit_id: super::super::SurfacePublisherPermitId::new([9; 32]),
            goal_fence,
            receipt_digest: digest(120),
        };

        assert!(permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
        assert!(!permit_authorizes(
            std::slice::from_ref(&wrong_digest),
            &wrong_digest,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn recovery_permit_requires_exact_generation_stop_in_same_batch() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let mut batch = test_batch(&state);
        let operation_id =
            super::super::SurfaceOperationId::try_from_bytes(uuid_v7_bytes(94)).unwrap();
        let historical_fence = super::super::SurfaceOperationFence {
            thread_id: thread_id(),
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            operation_id: operation_id.clone(),
            generation_id: super::super::SurfaceGenerationId::new(0),
        };
        let permit = SurfacePublisherPermit::Recovery {
            permit_id: super::super::SurfacePublisherPermitId::new([2; 32]),
            current_owner_epoch: ThreadOwnerEpoch::new(1),
            historical_fence,
        };
        batch.events =
            super::super::NonEmptyVec::try_new(vec![super::super::SurfaceEventEnvelope {
                ordinal: 0,
                event_id: super::super::SurfaceEventId::try_from_bytes(uuid_v7_bytes(95)).unwrap(),
                commit_class: batch.commit_class.clone(),
                scope: SurfaceScope::Operation {
                    operation_id: operation_id.clone(),
                },
                event: super::super::SurfaceEvent::Operation(
                    super::super::OperationPatch::FinalizationStarted {
                        operation_id,
                        finalize_intent_id: super::super::SurfaceFinalizeIntentId::try_from_bytes(
                            uuid_v7_bytes(96),
                        )
                        .unwrap(),
                        terminal_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(97))
                            .unwrap(),
                        selected_cause: super::super::OperationFinalizationCause::Reservation(
                            super::super::ReservationFinalizerReason::RuntimeRestart,
                        ),
                        suspended_cause: None,
                        expected_settlements: Vec::new(),
                    },
                ),
            }])
            .unwrap();
        batch.batch_digest = super::super::canonical_batch_digest(&batch);

        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn terminal_stream_disposition_requires_exact_open_stream_coverage() {
        let fence = test_operation_fence(130);
        let mut snapshot = reducer_snapshot();
        let first = super::super::SurfaceAssistantStream {
            stream_id: super::super::SurfaceStreamId::try_from_bytes(uuid_v7_bytes(131)).unwrap(),
            fence: fence.clone(),
            turn_id: orca_core::thread_identity::TurnId::new(),
            item_id: orca_core::thread_identity::ConversationItemId::new(),
            channel: super::super::AssistantChannel::Message,
            next_offset: super::super::ByteOffset::new(5),
            text: super::super::DisplayText::new("first"),
            state: super::super::SurfaceAssistantStreamState::Open,
        };
        let second = super::super::SurfaceAssistantStream {
            stream_id: super::super::SurfaceStreamId::try_from_bytes(uuid_v7_bytes(132)).unwrap(),
            fence: fence.clone(),
            turn_id: first.turn_id.clone(),
            item_id: orca_core::thread_identity::ConversationItemId::new(),
            channel: super::super::AssistantChannel::Reasoning,
            next_offset: super::super::ByteOffset::new(6),
            text: super::super::DisplayText::new("second"),
            state: super::super::SurfaceAssistantStreamState::Open,
        };
        snapshot.assistant_streams = vec![first.clone(), second.clone()];
        let state = SurfaceReducerState::new(snapshot);
        let scope = SurfaceScope::Generation {
            fence: fence.clone(),
        };
        let discard = |stream_id| {
            (
                scope.clone(),
                super::super::SurfaceEvent::Assistant(
                    super::super::AssistantPatch::StreamDiscarded {
                        stream_id,
                        reason: super::super::AssistantDiscardReason::ProviderFailed,
                    },
                ),
            )
        };
        let exact = test_batch_with_events(
            &state,
            vec![
                discard(first.stream_id.clone()),
                discard(second.stream_id.clone()),
            ],
        );
        let missing = test_batch_with_events(&state, vec![discard(first.stream_id.clone())]);
        let reversed = test_batch_with_events(
            &state,
            vec![discard(second.stream_id), discard(first.stream_id)],
        );

        assert!(stream_discards_cover_open_streams(
            &state,
            &fence,
            &scope,
            super::super::AssistantDiscardReason::ProviderFailed,
            exact.events.as_slice(),
        ));
        assert!(!stream_discards_cover_open_streams(
            &state,
            &fence,
            &scope,
            super::super::AssistantDiscardReason::ProviderFailed,
            missing.events.as_slice(),
        ));
        assert!(!stream_discards_cover_open_streams(
            &state,
            &fence,
            &scope,
            super::super::AssistantDiscardReason::ProviderFailed,
            reversed.events.as_slice(),
        ));
    }

    #[test]
    fn recovery_permit_rejects_live_completed_stop_and_finalizer() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let fence = test_operation_fence(121);
        let finalize_intent_id =
            super::super::SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(122)).unwrap();
        let batch = test_batch_with_events(
            &state,
            vec![
                (
                    SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    super::super::SurfaceEvent::Operation(
                        super::super::OperationPatch::GenerationStopped {
                            fence: fence.clone(),
                            reason: super::super::GenerationStopReason::Completed {
                                status: super::super::GenerationCompletionStatus::Success,
                            },
                            usage_delta: zero_usage(),
                        },
                    ),
                ),
                (
                    SurfaceScope::Operation {
                        operation_id: fence.operation_id.clone(),
                    },
                    super::super::SurfaceEvent::Operation(finalization_started(
                        fence.operation_id.clone(),
                        finalize_intent_id,
                    )),
                ),
            ],
        );
        let permit = SurfacePublisherPermit::Recovery {
            permit_id: super::super::SurfacePublisherPermitId::new([10; 32]),
            current_owner_epoch: ThreadOwnerEpoch::new(1),
            historical_fence: fence,
        };

        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn projection_pending_requires_exact_retry_without_second_append() {
        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let state = SurfaceReducerState::new(reducer_snapshot());
        let batch = test_batch(&state);
        let context = SurfaceProjectionContext {
            request_id: super::super::SurfaceRequestId::try_from_bytes(uuid_v7_bytes(93)).unwrap(),
            target: super::super::MutationTarget::Thread {
                thread_id: thread_id(),
            },
            fact_family: SurfaceFactFamily::Session,
        };
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owner_lease(TestLedger::default(), state, &owner)
                .unwrap();
        coordinator.inject_projection_failure(true);

        let error = coordinator
            .commit_actor_batch_for_projection(&context, &batch)
            .unwrap_err();
        let SurfaceCommitError::ProjectionPending { token } = error else {
            panic!("expected projection retry token");
        };
        assert_eq!(coordinator.ledger().writes, 2);
        assert!(matches!(
            coordinator.commit_actor_batch(&batch),
            Err(SurfaceCommitError::ProjectionPending { token: pending }) if pending == token
        ));
        assert_eq!(coordinator.ledger().writes, 2);

        coordinator.inject_projection_failure(false);
        coordinator.retry_projection(&token).unwrap();
        assert_eq!(coordinator.ledger().writes, 2);
        assert_eq!(coordinator.state().snapshot().cursor.next_seq.get(), 1);
    }

    #[test]
    fn interrupted_manual_compaction_context_is_reset_before_operation_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let operation_id = test_operation_fence(132).operation_id;
        let mut snapshot = reducer_snapshot();
        snapshot.context.compaction = super::super::CompactionState::Running {
            operation_id: operation_id.clone(),
            reason: super::super::CompactionReason::Manual,
            before_messages: 4,
        };
        let state = SurfaceReducerState::new(snapshot);
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owner_lease(TestLedger::default(), state, &owner)
                .unwrap();
        assert!(
            coordinator
                .recover_interrupted_manual_compaction(&operation_id, None)
                .unwrap()
        );
        assert!(matches!(
            coordinator.state().snapshot().context.compaction,
            super::super::CompactionState::Idle
        ));
        assert_eq!(
            coordinator.state().snapshot().context.revision,
            super::super::ContextRevision::try_new(2).unwrap()
        );
        assert_eq!(coordinator.ledger().writes, 2);
        assert!(
            !coordinator
                .recover_interrupted_manual_compaction(&operation_id, None)
                .unwrap()
        );
        assert_eq!(coordinator.ledger().writes, 2);
    }

    #[test]
    fn manual_compaction_item_diff_preserves_the_latest_duplicate_identity() {
        let first = super::super::SurfaceItemId::new();
        let retained_tail = super::super::SurfaceItemId::new();
        let items = vec![
            super::super::SurfaceItem::SystemMessage {
                id: first.clone(),
                content: super::super::DisplayText::new("duplicate"),
                pinned: false,
                origin: super::super::SurfaceItemOrigin::HistoryMaterialization,
            },
            super::super::SurfaceItem::SystemMessage {
                id: retained_tail.clone(),
                content: super::super::DisplayText::new("duplicate"),
                pinned: false,
                origin: super::super::SurfaceItemOrigin::HistoryMaterialization,
            },
        ];
        let mut conversation = orca_core::conversation::Conversation::new();
        conversation.add_system("duplicate".to_string());

        let patches = manual_compaction_item_patches(&items, &conversation).unwrap();

        assert_eq!(patches.len(), 3);
        assert!(matches!(
            &patches[0],
            super::super::ItemPatch::Removed { item_id, .. } if item_id == &first
        ));
        assert!(matches!(
            &patches[1],
            super::super::ItemPatch::Removed { item_id, .. } if item_id == &retained_tail
        ));
        assert!(matches!(
            &patches[2],
            super::super::ItemPatch::Added {
                item: super::super::SurfaceItem::SystemMessage { id, .. },
            } if id == &retained_tail
        ));
    }

    #[test]
    fn manual_compaction_item_rebuild_keeps_rewritten_tool_in_durable_order() {
        let prefix_id = super::super::SurfaceItemId::new();
        let tool_id = super::super::SurfaceItemId::new();
        let tail_id = super::super::SurfaceItemId::new();
        let turn_id = super::super::SurfaceTurnId::new();
        let tool_call_id = super::super::SurfaceToolCallId::try_new("call-compact").unwrap();
        let items = vec![
            super::super::SurfaceItem::SystemMessage {
                id: prefix_id.clone(),
                content: super::super::DisplayText::new("system"),
                pinned: false,
                origin: super::super::SurfaceItemOrigin::HistoryMaterialization,
            },
            super::super::SurfaceItem::ToolResultMessage {
                id: tool_id.clone(),
                turn_id,
                tool_call_id: tool_call_id.clone(),
                content: super::super::DisplayText::new("large tool output"),
                terminal: super::super::SurfaceToolTerminal {
                    kind: super::super::SurfaceToolResultKind::Success,
                    source: super::super::ToolTerminalSource::Observed,
                    invocation_started: super::super::ToolInvocationStarted::Yes,
                },
                pinned: false,
            },
            super::super::SurfaceItem::SystemMessage {
                id: tail_id.clone(),
                content: super::super::DisplayText::new("tail"),
                pinned: true,
                origin: super::super::SurfaceItemOrigin::HistoryMaterialization,
            },
        ];
        let mut conversation = orca_core::conversation::Conversation::new();
        conversation.add_system("system".to_string());
        conversation.add_tool_result(
            tool_call_id.as_str().to_string(),
            "micro-compacted tool output".to_string(),
        );
        conversation.add_system_pinned("tail".to_string());

        let patches = manual_compaction_item_patches(&items, &conversation).unwrap();

        assert_eq!(patches.len(), 6);
        assert!(matches!(
            &patches[3],
            super::super::ItemPatch::Added {
                item: super::super::SurfaceItem::SystemMessage { id, content, .. },
            } if id == &prefix_id && content.as_str() == "system"
        ));
        assert!(matches!(
            &patches[4],
            super::super::ItemPatch::Added {
                item:
                    super::super::SurfaceItem::ToolResultMessage {
                        id,
                        tool_call_id: added_call,
                        content,
                        ..
                    },
            } if id == &tool_id
                && added_call == &tool_call_id
                && content.as_str() == "micro-compacted tool output"
        ));
        assert!(matches!(
            &patches[5],
            super::super::ItemPatch::Added {
                item: super::super::SurfaceItem::SystemMessage { id, content, .. },
            } if id == &tail_id && content.as_str() == "tail"
        ));
    }

    #[test]
    fn manual_compaction_item_rebuild_preserves_identity_across_durable_redaction() {
        let item_id = super::super::SurfaceItemId::new();
        let items = vec![super::super::SurfaceItem::SystemMessage {
            id: item_id.clone(),
            content: super::super::DisplayText::new(
                "provider api_key=sk-test-manual-compact-redaction-1234567890",
            ),
            pinned: false,
            origin: super::super::SurfaceItemOrigin::HistoryMaterialization,
        }];
        let mut conversation = orca_core::conversation::Conversation::new();
        conversation.add_system("provider api_key=<redacted>".to_string());

        let patches = manual_compaction_item_patches(&items, &conversation).unwrap();

        assert!(matches!(
            patches.as_slice(),
            [
                super::super::ItemPatch::Removed {
                    item_id: removed,
                    ..
                },
                super::super::ItemPatch::Added {
                    item: super::super::SurfaceItem::SystemMessage { id: added, .. },
                },
            ] if removed == &item_id && added == &item_id
        ));
    }

    #[test]
    fn manual_compaction_item_rebuild_does_not_fabricate_missing_assistant_authority() {
        let mut conversation = orca_core::conversation::Conversation::new();
        conversation.add_user("user".to_string());
        conversation.add_assistant(
            Some("assistant".to_string()),
            Some("reasoning".to_string()),
            Vec::new(),
        );

        let patches = manual_compaction_item_patches(&[], &conversation).unwrap();

        assert!(patches.is_empty());
    }

    #[test]
    fn manual_compaction_item_rebuild_excludes_audit_only_duplicate_user_input() {
        let resolved_id = super::super::SurfaceItemId::new();
        let failed_id = super::super::SurfaceItemId::new();
        let turn_id = super::super::SurfaceTurnId::new();
        let visible = super::super::SurfaceInputPresentation::Visible {
            text: super::super::DisplayText::new("same user text"),
        };
        let resolved = super::super::SurfaceItem::UserMessage {
            id: resolved_id.clone(),
            turn_id: turn_id.clone(),
            input: super::super::SurfaceUserInputState::Resolved {
                fact: super::super::SurfaceResolvedInputFact::NonReplayable {
                    presentation: visible.clone(),
                    live_capsule_incarnation: super::super::SurfaceIncarnation::try_from_bytes(
                        uuid_v7_bytes(145),
                    )
                    .unwrap(),
                },
            },
            pinned: false,
            origin: super::super::SurfaceItemOrigin::UserInput,
        };
        let failed = super::super::SurfaceItem::UserMessage {
            id: failed_id.clone(),
            turn_id,
            input: super::super::SurfaceUserInputState::ResolutionFailed {
                presentation: visible,
                correlation_id: super::super::SurfaceInputCorrelationId::try_from_bytes(
                    uuid_v7_bytes(146),
                )
                .unwrap(),
                code: super::super::InputResolutionErrorCode::RuntimeUnavailable,
                message: super::super::SafeDiagnosticText::try_new("resolution failed").unwrap(),
            },
            pinned: false,
            origin: super::super::SurfaceItemOrigin::UserInput,
        };
        let mut conversation = orca_core::conversation::Conversation::new();
        conversation.add_user("same user text".to_string());

        let patches =
            manual_compaction_item_patches(&[resolved.clone(), failed], &conversation).unwrap();

        assert!(matches!(
            patches.as_slice(),
            [
                super::super::ItemPatch::Removed {
                    item_id: removed_resolved,
                    ..
                },
                super::super::ItemPatch::Removed {
                    item_id: removed_failed,
                    ..
                },
                super::super::ItemPatch::Added {
                    item:
                        super::super::SurfaceItem::UserMessage {
                            id: added,
                            input: super::super::SurfaceUserInputState::Resolved { .. },
                            ..
                        },
                },
            ] if removed_resolved == &resolved_id
                && removed_failed == &failed_id
                && added == &resolved_id
        ));
    }

    #[test]
    fn interrupted_same_count_manual_compaction_recovers_from_operation_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let fence = test_operation_fence(133);
        let operation_id = fence.operation_id.clone();
        let mut snapshot = reducer_snapshot();
        let replayability = super::super::Replayability::NonReplayable {
            reason: super::super::NonReplayableReason::Missing,
            live_capsule: super::super::LiveOperationCapsule::Available {
                incarnation: snapshot.cursor.incarnation.clone(),
            },
        };
        let capability_fingerprint = digest(133);
        let logical_turn_id = super::super::SurfaceTurnId::new();
        snapshot.foreground_operation = Some(super::super::OperationRecord {
            operation_id: operation_id.clone(),
            request_id: super::super::SurfaceRequestId::try_from_bytes(uuid_v7_bytes(135)).unwrap(),
            intent: super::super::OperationIntent {
                origin: super::super::OperationOrigin::TuiUser,
                kind: super::super::OperationKind::ManualCompaction {
                    reason: super::super::ManualCompactionReason::Manual,
                },
                initial_replayability: replayability.clone(),
                busy_disposition: super::super::BusyDisposition::Queue,
                interrupt_settlement:
                    super::super::InterruptSettlement::SuspendUntilExplicitControl,
                legacy_visibility: super::super::LegacyVisibility::PublishAfterAdmitted,
                settings_revision: snapshot.settings.thread_revision,
                policy_epoch: snapshot.settings.effective.policy_epoch,
                required_capabilities: Default::default(),
                capability_fingerprint: capability_fingerprint.clone(),
                settings_receipt: super::super::OperationSettingsPreparationReceipt::Current {
                    settings_revision: snapshot.settings.thread_revision,
                    policy_epoch: snapshot.settings.effective.policy_epoch,
                },
            },
            phase: super::super::OperationPhase::Admitted,
            reservation: super::super::ReservationLease::new(
                super::super::SurfaceAdmissionLeaseId::try_from_bytes(uuid_v7_bytes(136)).unwrap(),
                operation_id.clone(),
                super::super::SequenceNumber::new(1),
                super::super::HostIncarnation::try_from_bytes(uuid_v7_bytes(137)).unwrap(),
                super::super::MonotonicInstant {
                    clock_id: super::super::HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(
                        138,
                    ))
                    .unwrap(),
                    tick: super::super::MonotonicTick::new(0),
                },
            ),
            ready_for_admission: false,
            initial_logical_turn_id: Some(logical_turn_id.clone()),
            initial_input_item_id: None,
            generations: vec![super::super::GenerationRecord {
                fence: fence.clone(),
                logical_turn_id,
                input: super::super::GenerationInputState::NotApplicable,
                predecessor: None,
                attempt: super::super::GenerationAttempt::Initial,
                goal_identity: None,
                replayability: replayability.clone(),
                required_capabilities: Default::default(),
                capability_fingerprint: capability_fingerprint.clone(),
                phase: super::super::GenerationPhase::Started,
                started_witness: Some(super::super::GenerationStartedWitness {
                    started_commit_id: super::super::SurfaceCommitId::try_from_bytes(
                        uuid_v7_bytes(139),
                    )
                    .unwrap(),
                    settings_revision: snapshot.settings.thread_revision,
                    policy_epoch: snapshot.settings.effective.policy_epoch,
                    durable_replayability_digest: super::super::canonical_replayability_digest(
                        &replayability,
                    ),
                    capability_fingerprint,
                }),
                stop_reason: None,
            }],
            agent_loop_turns: Vec::new(),
            pending_control: None,
            finalization: None,
            terminal: None,
        });
        let compacted_item_id = super::super::SurfaceItemId::new();
        let retained_user_id = super::super::SurfaceItemId::new();
        let retained_message_id = super::super::SurfaceItemId::new();
        let retained_reasoning_id = super::super::SurfaceItemId::new();
        let retained_plan_id = super::super::SurfaceItemId::new();
        let retained_turn_id = super::super::SurfaceTurnId::new();
        snapshot.items.extend([
            super::super::SurfaceItem::SystemMessage {
                id: compacted_item_id.clone(),
                content: super::super::DisplayText::new("old context"),
                pinned: false,
                origin: super::super::SurfaceItemOrigin::HistoryMaterialization,
            },
            super::super::SurfaceItem::UserMessage {
                id: retained_user_id.clone(),
                turn_id: retained_turn_id.clone(),
                input: super::super::SurfaceUserInputState::Resolved {
                    fact: super::super::SurfaceResolvedInputFact::NonReplayable {
                        presentation: super::super::SurfaceInputPresentation::Visible {
                            text: super::super::DisplayText::new("retained user"),
                        },
                        live_capsule_incarnation: snapshot.cursor.incarnation.clone(),
                    },
                },
                pinned: false,
                origin: super::super::SurfaceItemOrigin::UserInput,
            },
            super::super::SurfaceItem::AssistantMessage {
                id: retained_message_id.clone(),
                turn_id: retained_turn_id.clone(),
                text: super::super::DisplayText::new("retained assistant"),
                pinned: false,
            },
            super::super::SurfaceItem::AssistantReasoning {
                id: retained_reasoning_id.clone(),
                turn_id: retained_turn_id.clone(),
                summary: super::super::DisplayText::new("reasoning summary"),
                content: super::super::DisplayText::new("retained reasoning"),
                pinned: false,
            },
            super::super::SurfaceItem::AssistantPlan {
                id: retained_plan_id.clone(),
                turn_id: retained_turn_id,
                text: super::super::DisplayText::new("retained plan"),
                pinned: false,
            },
        ]);
        snapshot.context.compaction = super::super::CompactionState::Running {
            operation_id: operation_id.clone(),
            reason: super::super::CompactionReason::Manual,
            before_messages: 5,
        };
        let state = SurfaceReducerState::new(snapshot);
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owner_lease(TestLedger::default(), state, &owner)
                .unwrap();
        let mut observed = orca_core::conversation::Conversation::new();
        observed.add_system(
            "[Earlier conversation history was truncated to fit context window]".to_string(),
        );
        observed.add_user("retained user".to_string());
        observed.add_assistant(
            Some("retained assistant".to_string()),
            Some("retained reasoning".to_string()),
            Vec::new(),
        );
        observed.add_assistant(Some("retained plan".to_string()), None, Vec::new());
        let durable_snapshot = crate::thread_store::ManualCompactionDurableSnapshot {
            operation_id: operation_id.clone(),
            strategy: "local_truncation".to_string(),
            before_messages: 5,
            conversation: observed,
        };

        assert!(
            coordinator
                .recover_interrupted_manual_compaction(&operation_id, Some(&durable_snapshot),)
                .unwrap()
        );
        assert!(matches!(
            &coordinator.state().snapshot().context.compaction,
            super::super::CompactionState::Completed {
                operation_id: completed,
                before_messages: 5,
                after_messages: 4,
                collapsed_messages: 1,
                ..
            } if completed == &operation_id
        ));
        assert!(matches!(
            coordinator.state().snapshot().items.as_slice(),
            [
                super::super::SurfaceItem::SystemMessage {
                    content: marker,
                    ..
                },
                super::super::SurfaceItem::UserMessage {
                    id,
                    ..
                },
                super::super::SurfaceItem::AssistantMessage {
                    id: message_id,
                    ..
                },
                super::super::SurfaceItem::AssistantReasoning {
                    id: reasoning_id,
                    ..
                },
                super::super::SurfaceItem::AssistantPlan { id: plan_id, .. },
            ] if marker.as_str()
                == "[Earlier conversation history was truncated to fit context window]"
                && id == &retained_user_id
                && message_id == &retained_message_id
                && reasoning_id == &retained_reasoning_id
                && plan_id == &retained_plan_id
        ));
        assert!(!coordinator.state().snapshot().items.iter().any(|item| {
            matches!(
                item,
                super::super::SurfaceItem::SystemMessage { id, .. }
                    if id == &compacted_item_id
            )
        }));
        assert_eq!(coordinator.ledger().writes, 2);
    }

    #[test]
    fn unbound_publication_suffix_is_latest_contiguous_and_budget_bounded() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let template = test_batch(&state);
        let mut committed = Vec::new();
        let mut cursor = template.cursor_before.clone();
        for index in 0..10_u64 {
            let mut batch = template.clone();
            batch.cursor_before = cursor.clone();
            batch.cursor_after = super::super::SurfaceCursor {
                next_seq: super::super::SequenceNumber::new(
                    cursor.next_seq.get() + super::super::SURFACE_COMMIT_BATCH_EVENT_LIMIT,
                ),
                ..cursor.clone()
            };
            batch.event_count = super::super::SURFACE_COMMIT_BATCH_EVENT_LIMIT as u32;
            batch.batch_digest = digest(index as u8);
            cursor = batch.cursor_after.clone();
            committed.push(batch);
        }
        let expected_first = committed[2].batch_digest.clone();

        let suffix = BoundedPublicationSuffix::from_committed(committed);

        assert_eq!(suffix.batches.len(), 8);
        assert_eq!(suffix.events, super::super::SURFACE_RETAINED_EVENT_LIMIT);
        assert!(suffix.bytes <= super::super::SURFACE_RETAINED_BYTE_LIMIT);
        assert_eq!(suffix.batches.front().unwrap().batch_digest, expected_first);

        let mut disconnected = Vec::new();
        let mut first = template.clone();
        first.cursor_after.next_seq = super::super::SequenceNumber::new(1);
        let mut tail_first = template.clone();
        tail_first.cursor_before.next_seq = super::super::SequenceNumber::new(5);
        tail_first.cursor_after.next_seq = super::super::SequenceNumber::new(6);
        let mut tail_second = template;
        tail_second.cursor_before = tail_first.cursor_after.clone();
        tail_second.cursor_after.next_seq = super::super::SequenceNumber::new(7);
        disconnected.extend([first, tail_first, tail_second]);

        let suffix = BoundedPublicationSuffix::from_committed(disconnected);
        assert_eq!(suffix.batches.len(), 2);
        assert_eq!(
            suffix.batches.front().unwrap().cursor_before.next_seq.get(),
            5
        );
    }
}
