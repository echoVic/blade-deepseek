use std::collections::HashMap;
use std::io;
use std::sync::mpsc::SyncSender;

use crate::runtime_actor::RuntimeActorEffect;
use crate::runtime_host::{
    RuntimeAcpTerminalExitStatus, RuntimeAcpTerminalObservation, RuntimeAcpTerminalOutput,
};
use crate::runtime_surface as surface;

pub(crate) enum CapabilityReply {
    ReadTextFile {
        reply: SyncSender<io::Result<String>>,
        result: io::Result<String>,
    },
    WriteTextFile {
        reply: SyncSender<io::Result<()>>,
        result: io::Result<()>,
    },
    TerminalCreate {
        reply: SyncSender<io::Result<String>>,
        result: io::Result<String>,
    },
    TerminalObservation {
        reply: SyncSender<io::Result<RuntimeAcpTerminalObservation>>,
        result: io::Result<RuntimeAcpTerminalObservation>,
    },
    TerminalCleanup {
        reply: SyncSender<io::Result<()>>,
        result: io::Result<()>,
    },
}

pub(crate) struct ResidentSurfaceCapabilityCall {
    attachment_id: surface::SurfaceAttachmentId,
    capability_revision: surface::CapabilityRevision,
    write_claimed: bool,
    terminal_cleanup_lease: Option<ResidentTerminalCleanupLease>,
    waiter: Option<ResidentSurfaceCapabilityWaiter>,
}

impl ResidentSurfaceCapabilityCall {
    pub(crate) fn new(
        attachment_id: surface::SurfaceAttachmentId,
        capability_revision: surface::CapabilityRevision,
        write_claimed: bool,
        terminal_cleanup_lease: Option<ResidentTerminalCleanupLease>,
        waiter: Option<ResidentSurfaceCapabilityWaiter>,
    ) -> Self {
        Self {
            attachment_id,
            capability_revision,
            write_claimed,
            terminal_cleanup_lease,
            waiter,
        }
    }

    pub(crate) fn attachment_id(&self) -> &surface::SurfaceAttachmentId {
        &self.attachment_id
    }

    pub(crate) fn capability_revision(&self) -> surface::CapabilityRevision {
        self.capability_revision
    }

    pub(crate) fn write_claimed(&self) -> bool {
        self.write_claimed
    }

    pub(crate) fn claim_write(&mut self) -> bool {
        if self.write_claimed {
            return false;
        }
        self.write_claimed = true;
        true
    }

    pub(crate) fn release_write(&mut self) -> bool {
        if !self.write_claimed {
            return false;
        }
        self.write_claimed = false;
        true
    }

    pub(crate) fn terminal_cleanup_lease(&self) -> Option<ResidentTerminalCleanupLease> {
        self.terminal_cleanup_lease.clone()
    }

    pub(crate) fn take_waiter(&mut self) -> Option<ResidentSurfaceCapabilityWaiter> {
        self.waiter.take()
    }
}

#[derive(Clone)]
pub(crate) struct ResidentTerminalCleanupLease {
    pub(crate) lease_id: surface::UuidV7,
    pub(crate) terminal_id: surface::SurfaceRemoteTerminalId,
}

pub(crate) enum ResidentSurfaceCapabilityWaiter {
    ReadTextFile(SyncSender<io::Result<String>>),
    WriteTextFile(SyncSender<io::Result<()>>),
    TerminalCreate(SyncSender<io::Result<String>>),
    TerminalObservation(SyncSender<io::Result<RuntimeAcpTerminalObservation>>),
    TerminalCleanup(SyncSender<io::Result<()>>),
}

pub(crate) struct PendingSurfaceCapabilityTransition {
    fence: surface::SurfaceOperationFence,
    batch: surface::SurfaceCommitBatch,
    waiter_outcome: Option<PendingSurfaceCapabilityWaiterOutcome>,
    deferred_settlement: Option<PendingSurfaceCapabilitySettlement>,
    retry_at: tokio::time::Instant,
}

impl PendingSurfaceCapabilityTransition {
    pub(crate) fn retry_at(&self) -> tokio::time::Instant {
        self.retry_at
    }

    pub(crate) fn waits_for_written(&self) -> bool {
        self.waiter_outcome.is_none()
    }

    pub(crate) fn set_deferred_settlement(
        &mut self,
        settlement: PendingSurfaceCapabilitySettlement,
    ) -> Result<(), PendingSurfaceCapabilitySettlement> {
        if self.deferred_settlement.is_some() {
            return Err(settlement);
        }
        self.deferred_settlement = Some(settlement);
        Ok(())
    }
}

pub(crate) enum PendingSurfaceCapabilityWaiterOutcome {
    ReadTextFileCompleted(String),
    WriteTextFileCompleted,
    TerminalCreated(String),
    TerminalOutputObserved(RuntimeAcpTerminalOutput),
    TerminalExitObserved(RuntimeAcpTerminalExitStatus),
    TerminalCleanupCompleted,
    Failed {
        kind: io::ErrorKind,
        message: String,
    },
}

pub(crate) enum PendingSurfaceCapabilitySettlement {
    ReadTextFile {
        client: surface::RuntimeSurfaceClientHandle,
        capability_revision: surface::CapabilityRevision,
        settlement: surface::AcpReadTextFileSettlement,
    },
    WriteTextFile {
        client: surface::RuntimeSurfaceClientHandle,
        capability_revision: surface::CapabilityRevision,
        settlement: surface::AcpWriteTextFileSettlement,
    },
    TerminalCreate {
        client: surface::RuntimeSurfaceClientHandle,
        capability_revision: surface::CapabilityRevision,
        settlement: surface::AcpTerminalCreateSettlement,
    },
    TerminalObservation {
        client: surface::RuntimeSurfaceClientHandle,
        capability_revision: surface::CapabilityRevision,
        settlement: surface::AcpTerminalObservationSettlement,
    },
    TerminalCleanup {
        client: surface::RuntimeSurfaceClientHandle,
        capability_revision: surface::CapabilityRevision,
        settlement: surface::AcpTerminalCleanupSettlement,
    },
    DispatchTerminalCleanup {
        route: surface::AcpCapabilityAttachmentRoute,
        dispatch: surface::AcpTerminalCleanupDispatch,
    },
    BeginTerminalRelease {
        kill_call: surface::SurfaceCapabilityCall,
        lease_id: surface::UuidV7,
        terminal_id: surface::SurfaceRemoteTerminalId,
    },
}

pub(crate) struct CapabilityCommitEffect {
    call_id: surface::SurfaceCapabilityCallId,
    pending: PendingSurfaceCapabilityTransition,
    physical_write_confirmed: bool,
}

impl CapabilityCommitEffect {
    pub(crate) fn call_id(&self) -> &surface::SurfaceCapabilityCallId {
        &self.call_id
    }

    pub(crate) fn fence(&self) -> &surface::SurfaceOperationFence {
        &self.pending.fence
    }

    pub(crate) fn batch(&self) -> &surface::SurfaceCommitBatch {
        &self.pending.batch
    }
}

pub(crate) struct CapabilityCommitResolution {
    pub(crate) reply: Option<RuntimeActorEffect>,
    pub(crate) deferred_settlement: Option<PendingSurfaceCapabilitySettlement>,
    pub(crate) retained_for_retry: bool,
}

pub(crate) struct RuntimeCapabilityController<Call, Transition> {
    capability_calls: HashMap<surface::SurfaceCapabilityCallId, Call>,
    pending_capability_transitions: HashMap<surface::SurfaceCapabilityCallId, Transition>,
}

impl<Call, Transition> RuntimeCapabilityController<Call, Transition> {
    pub(crate) fn new() -> Self {
        Self {
            capability_calls: HashMap::new(),
            pending_capability_transitions: HashMap::new(),
        }
    }

    pub(crate) fn register_call(&mut self, call_id: surface::SurfaceCapabilityCallId, call: Call) {
        self.capability_calls.insert(call_id, call);
    }

    pub(crate) fn discard_call(
        &mut self,
        call_id: &surface::SurfaceCapabilityCallId,
    ) -> Option<Call> {
        self.capability_calls.remove(call_id)
    }

    pub(crate) fn has_transition(&self, call_id: &surface::SurfaceCapabilityCallId) -> bool {
        self.pending_capability_transitions.contains_key(call_id)
    }

    pub(crate) fn transitions_empty(&self) -> bool {
        self.pending_capability_transitions.is_empty()
    }

    #[cfg(test)]
    fn trace(&self) -> CapabilityControllerTrace {
        CapabilityControllerTrace::new(
            self.capability_calls.len(),
            self.pending_capability_transitions.len(),
        )
    }
}

impl
    RuntimeCapabilityController<ResidentSurfaceCapabilityCall, PendingSurfaceCapabilityTransition>
{
    pub(crate) fn durable_calls<'a>(
        &'a self,
        snapshot: &'a surface::SurfaceSnapshot,
    ) -> impl Iterator<
        Item = (
            surface::SurfaceCapabilityCall,
            Option<ResidentTerminalCleanupLease>,
            bool,
        ),
    > + 'a {
        self.capability_calls
            .iter()
            .filter_map(move |(call_id, resident)| {
                Self::surface_call(snapshot, call_id).map(|call| {
                    (
                        call,
                        resident.terminal_cleanup_lease(),
                        resident.write_claimed(),
                    )
                })
            })
    }

    pub(crate) fn any_claimed_durable_call(
        &self,
        snapshot: &surface::SurfaceSnapshot,
        predicate: impl Fn(&surface::SurfaceCapabilityCall) -> bool,
    ) -> bool {
        self.capability_calls.iter().any(|(call_id, resident)| {
            resident.write_claimed()
                && Self::surface_call(snapshot, call_id).is_some_and(|call| predicate(&call))
        })
    }

    pub(crate) fn pending_transition_ids(
        &self,
    ) -> impl Iterator<Item = &surface::SurfaceCapabilityCallId> {
        self.pending_capability_transitions.keys()
    }

    pub(crate) fn pending_transition_retries(
        &self,
    ) -> impl Iterator<Item = (surface::SurfaceCapabilityCallId, tokio::time::Instant)> + '_ {
        self.pending_capability_transitions
            .iter()
            .map(|(call_id, pending)| (call_id.clone(), pending.retry_at()))
    }

    pub(crate) fn pending_transition_retry_times(
        &self,
    ) -> impl Iterator<Item = tokio::time::Instant> + '_ {
        self.pending_capability_transitions
            .values()
            .map(PendingSurfaceCapabilityTransition::retry_at)
    }

    pub(crate) fn authorize_call(
        &self,
        call_id: &surface::SurfaceCapabilityCallId,
        attachment_id: &surface::SurfaceAttachmentId,
        capability_revision: surface::CapabilityRevision,
    ) -> bool {
        self.capability_calls.get(call_id).is_some_and(|resident| {
            resident.attachment_id == *attachment_id
                && resident.capability_revision == capability_revision
        })
    }

    pub(crate) fn call_write_claimed(&self, call_id: &surface::SurfaceCapabilityCallId) -> bool {
        self.capability_calls
            .get(call_id)
            .is_some_and(ResidentSurfaceCapabilityCall::write_claimed)
    }

    pub(crate) fn try_claim_write(&mut self, call_id: &surface::SurfaceCapabilityCallId) -> bool {
        self.capability_calls
            .get_mut(call_id)
            .is_some_and(ResidentSurfaceCapabilityCall::claim_write)
    }

    pub(crate) fn release_write(&mut self, call_id: &surface::SurfaceCapabilityCallId) -> bool {
        self.capability_calls
            .get_mut(call_id)
            .is_some_and(ResidentSurfaceCapabilityCall::release_write)
    }

    pub(crate) fn terminal_cleanup_lease(
        &self,
        call_id: &surface::SurfaceCapabilityCallId,
    ) -> Option<ResidentTerminalCleanupLease> {
        self.capability_calls
            .get(call_id)
            .and_then(ResidentSurfaceCapabilityCall::terminal_cleanup_lease)
    }

    pub(crate) fn set_deferred_settlement(
        &mut self,
        call_id: &surface::SurfaceCapabilityCallId,
        settlement: PendingSurfaceCapabilitySettlement,
    ) -> Result<(), PendingSurfaceCapabilitySettlement> {
        let Some(pending) = self.pending_capability_transitions.get_mut(call_id) else {
            return Err(settlement);
        };
        pending.set_deferred_settlement(settlement)
    }

    pub(crate) fn transition_waits_for_written(
        &self,
        call_id: &surface::SurfaceCapabilityCallId,
    ) -> bool {
        self.pending_capability_transitions
            .get(call_id)
            .is_some_and(PendingSurfaceCapabilityTransition::waits_for_written)
    }

    pub(crate) fn retry_transition_effect(
        &mut self,
        call_id: &surface::SurfaceCapabilityCallId,
        physical_write_confirmed: bool,
    ) -> Option<RuntimeActorEffect> {
        self.pending_capability_transitions
            .remove(call_id)
            .map(|pending| {
                RuntimeActorEffect::CommitCapability(CapabilityCommitEffect {
                    call_id: call_id.clone(),
                    pending,
                    physical_write_confirmed,
                })
            })
    }

    pub(crate) fn take_call_with_waiter(
        &mut self,
        call_id: &surface::SurfaceCapabilityCallId,
    ) -> Option<(
        ResidentSurfaceCapabilityCall,
        ResidentSurfaceCapabilityWaiter,
    )> {
        let mut call = self.capability_calls.remove(call_id)?;
        let waiter = call.take_waiter()?;
        Some((call, waiter))
    }

    pub(crate) fn resolve_commit_effect(
        &mut self,
        mut effect: CapabilityCommitEffect,
        committed: bool,
    ) -> CapabilityCommitResolution {
        if !committed {
            effect.pending.retry_at =
                tokio::time::Instant::now() + std::time::Duration::from_millis(100);
            self.pending_capability_transitions
                .insert(effect.call_id, effect.pending);
            return CapabilityCommitResolution {
                reply: None,
                deferred_settlement: None,
                retained_for_retry: true,
            };
        }
        let deferred_settlement = effect.pending.deferred_settlement.take();
        let reply = self.apply_committed_transition(
            &effect.call_id,
            effect.pending.waiter_outcome,
            effect.physical_write_confirmed,
        );
        CapabilityCommitResolution {
            reply,
            deferred_settlement,
            retained_for_retry: false,
        }
    }

    pub(crate) fn surface_call(
        snapshot: &surface::SurfaceSnapshot,
        call_id: &surface::SurfaceCapabilityCallId,
    ) -> Option<surface::SurfaceCapabilityCall> {
        snapshot
            .tools
            .iter()
            .flat_map(|tool| tool.capability_calls.iter())
            .find(|call| call.call_id == *call_id)
            .cloned()
    }

    pub(crate) fn terminal_create_lease_id(
        call_id: &surface::SurfaceCapabilityCallId,
    ) -> surface::UuidV7 {
        surface::UuidV7::try_from_bytes(*call_id.as_bytes())
            .expect("capability call id is a UUIDv7")
    }

    pub(crate) fn read_waiter_outcome(
        result: io::Result<String>,
    ) -> PendingSurfaceCapabilityWaiterOutcome {
        match result {
            Ok(content) => PendingSurfaceCapabilityWaiterOutcome::ReadTextFileCompleted(content),
            Err(error) => PendingSurfaceCapabilityWaiterOutcome::Failed {
                kind: error.kind(),
                message: error.to_string(),
            },
        }
    }

    pub(crate) fn write_waiter_outcome(
        result: io::Result<()>,
    ) -> PendingSurfaceCapabilityWaiterOutcome {
        match result {
            Ok(()) => PendingSurfaceCapabilityWaiterOutcome::WriteTextFileCompleted,
            Err(error) => PendingSurfaceCapabilityWaiterOutcome::Failed {
                kind: error.kind(),
                message: error.to_string(),
            },
        }
    }

    pub(crate) fn terminal_create_waiter_outcome(
        result: io::Result<String>,
    ) -> PendingSurfaceCapabilityWaiterOutcome {
        match result {
            Ok(terminal_id) => PendingSurfaceCapabilityWaiterOutcome::TerminalCreated(terminal_id),
            Err(error) => PendingSurfaceCapabilityWaiterOutcome::Failed {
                kind: error.kind(),
                message: error.to_string(),
            },
        }
    }

    pub(crate) fn terminal_observation_waiter_outcome(
        result: io::Result<RuntimeAcpTerminalObservation>,
    ) -> PendingSurfaceCapabilityWaiterOutcome {
        match result {
            Ok(RuntimeAcpTerminalObservation::Output(output)) => {
                PendingSurfaceCapabilityWaiterOutcome::TerminalOutputObserved(output)
            }
            Ok(RuntimeAcpTerminalObservation::Exit(status)) => {
                PendingSurfaceCapabilityWaiterOutcome::TerminalExitObserved(status)
            }
            Err(error) => PendingSurfaceCapabilityWaiterOutcome::Failed {
                kind: error.kind(),
                message: error.to_string(),
            },
        }
    }

    pub(crate) fn retain_transition(
        &mut self,
        call_id: surface::SurfaceCapabilityCallId,
        fence: surface::SurfaceOperationFence,
        batch: surface::SurfaceCommitBatch,
        waiter_outcome: Option<PendingSurfaceCapabilityWaiterOutcome>,
    ) {
        self.pending_capability_transitions.insert(
            call_id,
            PendingSurfaceCapabilityTransition {
                fence,
                batch,
                waiter_outcome,
                deferred_settlement: None,
                retry_at: tokio::time::Instant::now(),
            },
        );
    }

    pub(crate) fn apply_committed_transition(
        &mut self,
        call_id: &surface::SurfaceCapabilityCallId,
        waiter_outcome: Option<PendingSurfaceCapabilityWaiterOutcome>,
        physical_write_confirmed: bool,
    ) -> Option<RuntimeActorEffect> {
        let Some(waiter_outcome) = waiter_outcome else {
            if physical_write_confirmed
                && let Some(resident) = self.capability_calls.get_mut(call_id)
            {
                resident.write_claimed = false;
            }
            return None;
        };
        if let Some(mut resident) = self.capability_calls.remove(call_id)
            && let Some(waiter) = resident.waiter.take()
        {
            let reply = match (waiter, waiter_outcome) {
                (
                    ResidentSurfaceCapabilityWaiter::ReadTextFile(waiter),
                    PendingSurfaceCapabilityWaiterOutcome::ReadTextFileCompleted(content),
                ) => Some(CapabilityReply::ReadTextFile {
                    reply: waiter,
                    result: Ok(content),
                }),
                (
                    ResidentSurfaceCapabilityWaiter::WriteTextFile(waiter),
                    PendingSurfaceCapabilityWaiterOutcome::WriteTextFileCompleted,
                ) => Some(CapabilityReply::WriteTextFile {
                    reply: waiter,
                    result: Ok(()),
                }),
                (
                    ResidentSurfaceCapabilityWaiter::TerminalCreate(waiter),
                    PendingSurfaceCapabilityWaiterOutcome::TerminalCreated(terminal_id),
                ) => Some(CapabilityReply::TerminalCreate {
                    reply: waiter,
                    result: Ok(terminal_id),
                }),
                (
                    ResidentSurfaceCapabilityWaiter::TerminalObservation(waiter),
                    PendingSurfaceCapabilityWaiterOutcome::TerminalOutputObserved(output),
                ) => Some(CapabilityReply::TerminalObservation {
                    reply: waiter,
                    result: Ok(RuntimeAcpTerminalObservation::Output(output)),
                }),
                (
                    ResidentSurfaceCapabilityWaiter::TerminalObservation(waiter),
                    PendingSurfaceCapabilityWaiterOutcome::TerminalExitObserved(status),
                ) => Some(CapabilityReply::TerminalObservation {
                    reply: waiter,
                    result: Ok(RuntimeAcpTerminalObservation::Exit(status)),
                }),
                (
                    ResidentSurfaceCapabilityWaiter::TerminalCleanup(waiter),
                    PendingSurfaceCapabilityWaiterOutcome::TerminalCleanupCompleted,
                ) => Some(CapabilityReply::TerminalCleanup {
                    reply: waiter,
                    result: Ok(()),
                }),
                (
                    ResidentSurfaceCapabilityWaiter::ReadTextFile(waiter),
                    PendingSurfaceCapabilityWaiterOutcome::Failed { kind, message },
                ) => Some(CapabilityReply::ReadTextFile {
                    reply: waiter,
                    result: Err(io::Error::new(kind, message)),
                }),
                (
                    ResidentSurfaceCapabilityWaiter::WriteTextFile(waiter),
                    PendingSurfaceCapabilityWaiterOutcome::Failed { kind, message },
                ) => Some(CapabilityReply::WriteTextFile {
                    reply: waiter,
                    result: Err(io::Error::new(kind, message)),
                }),
                (
                    ResidentSurfaceCapabilityWaiter::TerminalCreate(waiter),
                    PendingSurfaceCapabilityWaiterOutcome::Failed { kind, message },
                ) => Some(CapabilityReply::TerminalCreate {
                    reply: waiter,
                    result: Err(io::Error::new(kind, message)),
                }),
                (
                    ResidentSurfaceCapabilityWaiter::TerminalCleanup(waiter),
                    PendingSurfaceCapabilityWaiterOutcome::Failed { kind, message },
                ) => Some(CapabilityReply::TerminalCleanup {
                    reply: waiter,
                    result: Err(io::Error::new(kind, message)),
                }),
                (
                    ResidentSurfaceCapabilityWaiter::TerminalObservation(waiter),
                    PendingSurfaceCapabilityWaiterOutcome::Failed { kind, message },
                ) => Some(CapabilityReply::TerminalObservation {
                    reply: waiter,
                    result: Err(io::Error::new(kind, message)),
                }),
                _ => None,
            };
            return reply.map(RuntimeActorEffect::ReplyCapability);
        }
        None
    }

    pub(crate) fn cancel_calls(
        &mut self,
        call_ids: &[surface::SurfaceCapabilityCallId],
    ) -> Vec<RuntimeActorEffect> {
        call_ids
            .iter()
            .filter_map(|call_id| self.discard_call(call_id))
            .filter_map(|mut call| call.waiter.take())
            .filter_map(|waiter| {
                let kind = io::ErrorKind::Interrupted;
                let message = "ACP capability was cancelled before settlement".to_string();
                let reply = match waiter {
                    ResidentSurfaceCapabilityWaiter::ReadTextFile(reply) => {
                        CapabilityReply::ReadTextFile {
                            reply,
                            result: Err(io::Error::new(kind, message)),
                        }
                    }
                    ResidentSurfaceCapabilityWaiter::WriteTextFile(reply) => {
                        CapabilityReply::WriteTextFile {
                            reply,
                            result: Err(io::Error::new(kind, message)),
                        }
                    }
                    ResidentSurfaceCapabilityWaiter::TerminalCreate(reply) => {
                        CapabilityReply::TerminalCreate {
                            reply,
                            result: Err(io::Error::new(kind, message)),
                        }
                    }
                    ResidentSurfaceCapabilityWaiter::TerminalObservation(reply) => {
                        CapabilityReply::TerminalObservation {
                            reply,
                            result: Err(io::Error::new(kind, message)),
                        }
                    }
                    ResidentSurfaceCapabilityWaiter::TerminalCleanup(reply) => {
                        CapabilityReply::TerminalCleanup {
                            reply,
                            result: Err(io::Error::new(kind, message)),
                        }
                    }
                };
                Some(RuntimeActorEffect::ReplyCapability(reply))
            })
            .collect()
    }

    pub(crate) fn abandon_call_waiters(&mut self) {
        for call in self.capability_calls.values_mut() {
            drop(call.waiter.take());
        }
    }
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct CapabilityControllerTrace {
    capability_calls: usize,
    pending_capability_transitions: usize,
}

#[cfg(test)]
impl CapabilityControllerTrace {
    const fn new(capability_calls: usize, pending_capability_transitions: usize) -> Self {
        Self {
            capability_calls,
            pending_capability_transitions,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::runtime_actor::RuntimeActorEffect;
    use crate::surface;

    use super::{
        CapabilityControllerTrace, CapabilityReply, PendingSurfaceCapabilityTransition,
        ResidentSurfaceCapabilityCall, ResidentSurfaceCapabilityWaiter,
        RuntimeCapabilityController,
    };

    #[test]
    fn capability_controller_trace_equivalence() {
        let mut controller = RuntimeCapabilityController::<u8, u8>::new();
        let call_id =
            surface::SurfaceCapabilityCallId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .unwrap();
        let second_call_id =
            surface::SurfaceCapabilityCallId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .unwrap();

        let mut trace = vec![controller.trace()];
        controller.capability_calls.insert(call_id.clone(), 1);
        trace.push(controller.trace());
        controller
            .pending_capability_transitions
            .insert(call_id.clone(), 2);
        trace.push(controller.trace());
        controller
            .capability_calls
            .insert(second_call_id.clone(), 3);
        controller.pending_capability_transitions.remove(&call_id);
        trace.push(controller.trace());
        controller.capability_calls.remove(&call_id);
        trace.push(controller.trace());

        assert_eq!(
            trace,
            vec![
                CapabilityControllerTrace::new(0, 0),
                CapabilityControllerTrace::new(1, 0),
                CapabilityControllerTrace::new(1, 1),
                CapabilityControllerTrace::new(2, 0),
                CapabilityControllerTrace::new(1, 0),
            ]
        );

        let attachment_id =
            surface::SurfaceAttachmentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).unwrap();
        let revision = surface::CapabilityRevision::try_new(1).unwrap();
        let (waiter_tx, waiter_rx) = std::sync::mpsc::sync_channel(1);
        let mut controller = RuntimeCapabilityController::<
            ResidentSurfaceCapabilityCall,
            PendingSurfaceCapabilityTransition,
        >::new();
        controller.capability_calls.insert(
            call_id.clone(),
            ResidentSurfaceCapabilityCall {
                attachment_id: attachment_id.clone(),
                capability_revision: revision,
                write_claimed: false,
                terminal_cleanup_lease: None,
                waiter: Some(ResidentSurfaceCapabilityWaiter::ReadTextFile(waiter_tx)),
            },
        );
        let effect = controller.apply_committed_transition(
            &call_id,
            Some(RuntimeCapabilityController::<
                ResidentSurfaceCapabilityCall,
                PendingSurfaceCapabilityTransition,
            >::read_waiter_outcome(Ok("settled".to_string()))),
            false,
        );
        match effect {
            Some(RuntimeActorEffect::ReplyCapability(CapabilityReply::ReadTextFile {
                reply,
                result,
            })) => reply.send(result).unwrap(),
            _ => panic!("read settlement must return one actor-applied reply effect"),
        }
        assert_eq!(waiter_rx.recv().unwrap().unwrap(), "settled");
        assert!(!controller.capability_calls.contains_key(&call_id));

        let terminal_call_id =
            surface::SurfaceCapabilityCallId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .unwrap();
        let (terminal_tx, terminal_rx) = std::sync::mpsc::sync_channel(1);
        controller.register_call(
            terminal_call_id.clone(),
            ResidentSurfaceCapabilityCall {
                attachment_id: attachment_id.clone(),
                capability_revision: surface::CapabilityRevision::try_new(2).unwrap(),
                write_claimed: false,
                terminal_cleanup_lease: None,
                waiter: Some(ResidentSurfaceCapabilityWaiter::TerminalCreate(terminal_tx)),
            },
        );
        let effect = controller.apply_committed_transition(
            &terminal_call_id,
            Some(
                super::PendingSurfaceCapabilityWaiterOutcome::TerminalCreated(
                    "terminal-1".to_string(),
                ),
            ),
            false,
        );
        match effect {
            Some(RuntimeActorEffect::ReplyCapability(CapabilityReply::TerminalCreate {
                reply,
                result,
            })) => reply.send(result).unwrap(),
            _ => panic!("terminal creation must return one actor-applied reply effect"),
        }
        assert_eq!(terminal_rx.recv().unwrap().unwrap(), "terminal-1");

        let cleanup_call_id =
            surface::SurfaceCapabilityCallId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .unwrap();
        let (cleanup_tx, cleanup_rx) = std::sync::mpsc::sync_channel(1);
        controller.register_call(
            cleanup_call_id.clone(),
            ResidentSurfaceCapabilityCall {
                attachment_id: attachment_id.clone(),
                capability_revision: surface::CapabilityRevision::try_new(3).unwrap(),
                write_claimed: false,
                terminal_cleanup_lease: None,
                waiter: Some(ResidentSurfaceCapabilityWaiter::TerminalCleanup(cleanup_tx)),
            },
        );
        let effect = controller.apply_committed_transition(
            &cleanup_call_id,
            Some(super::PendingSurfaceCapabilityWaiterOutcome::TerminalCleanupCompleted),
            false,
        );
        match effect {
            Some(RuntimeActorEffect::ReplyCapability(CapabilityReply::TerminalCleanup {
                reply,
                result,
            })) => reply.send(result).unwrap(),
            _ => panic!("terminal cleanup must return one actor-applied reply effect"),
        }
        cleanup_rx.recv().unwrap().unwrap();

        controller.capability_calls.insert(
            second_call_id.clone(),
            ResidentSurfaceCapabilityCall {
                attachment_id,
                capability_revision: surface::CapabilityRevision::try_new(2).unwrap(),
                write_claimed: true,
                terminal_cleanup_lease: None,
                waiter: None,
            },
        );
        assert!(
            controller
                .apply_committed_transition(&second_call_id, None, true)
                .is_none()
        );
        assert!(
            !controller
                .capability_calls
                .get(&second_call_id)
                .unwrap()
                .write_claimed
        );
    }
}
