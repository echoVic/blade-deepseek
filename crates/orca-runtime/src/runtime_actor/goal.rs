use std::collections::VecDeque;
use std::sync::mpsc::SyncSender;

use tokio::sync::mpsc;

use orca_core::cancel::OperationId;

use crate::goal_actor::{GoalRuntimeHandle, GoalTurnContext};
use crate::goal_store::{GoalRecoveryRecord, GoalSurfaceMutationRecord};
use crate::runtime_host::RuntimeHostError;

pub(crate) enum GoalBlockingCompletion<Settlement> {
    RuntimeOpened {
        reply: SyncSender<Result<GoalRuntimeHandle, RuntimeHostError>>,
        result: Result<OpenedGoalRuntime, RuntimeHostError>,
    },
    SetGoal {
        reply: SyncSender<Result<orca_core::goal_types::ThreadGoal, RuntimeHostError>>,
        result: Result<GoalSurfaceWorkerResult, RuntimeHostError>,
    },
    EditGoal {
        reply: SyncSender<Result<Option<orca_core::goal_types::ThreadGoal>, RuntimeHostError>>,
        result: Result<GoalSurfaceWorkerResult, RuntimeHostError>,
    },
    ClearGoal {
        reply: SyncSender<Result<(), RuntimeHostError>>,
        result: Result<GoalSurfaceWorkerResult, RuntimeHostError>,
    },
    Pause {
        operation_id: OperationId,
        result: Result<Option<PendingGoalPauseEvent>, RuntimeHostError>,
    },
    SurfaceMutation {
        settlement: Settlement,
    },
    PauseResume {
        settlement: Settlement,
    },
    PreviewCommit {
        settlement: Settlement,
    },
    FinishVerify {
        settlement: Settlement,
    },
    Recovery {
        settlement: Settlement,
    },
}

pub(crate) struct GoalSurfaceWorkerResult {
    pub(crate) runtime: GoalRuntimeHandle,
    pub(crate) mutations: Vec<GoalSurfaceMutationRecord>,
    pub(crate) projected_goal: Option<orca_core::goal_types::ThreadGoal>,
}

pub(crate) struct OpenedGoalRuntime {
    pub(crate) handle: GoalRuntimeHandle,
    pub(crate) join: Option<std::thread::JoinHandle<()>>,
    pub(crate) recoveries: Vec<GoalRecoveryRecord>,
}

#[derive(Clone)]
pub(crate) struct ActiveGoalControl {
    pub(crate) session_id: String,
    pub(crate) runtime: GoalRuntimeHandle,
}

pub(crate) struct PendingGoalPauseEvent {
    pub(crate) goal_id: orca_core::goal_runtime::GoalId,
    pub(crate) goal_run_id: Option<orca_core::goal_runtime::GoalRunId>,
    pub(crate) outer_turn_id: Option<orca_core::goal_runtime::GoalOuterTurnId>,
    pub(crate) previous_state: orca_core::goal_runtime::GoalState,
    pub(crate) next_state: orca_core::goal_runtime::GoalState,
    pub(crate) reason: orca_core::goal_runtime::GoalPauseReason,
    pub(crate) message: String,
    pub(crate) reason_code: String,
}

pub(crate) struct GoalOperationController<
    Command,
    PendingRecovery,
    Completion,
    ActiveControl,
    PauseEvent,
> {
    completion_tx: mpsc::Sender<Completion>,
    completion_rx: mpsc::Receiver<Completion>,
    blocking_in_flight: bool,
    deferred_commands: VecDeque<Command>,
    pending_recovery: Option<PendingRecovery>,
    active_control: Option<(OperationId, ActiveControl, Option<GoalTurnContext>)>,
    pending_pause_event: Option<(OperationId, PauseEvent)>,
}

impl<Command, PendingRecovery, Completion, ActiveControl, PauseEvent>
    GoalOperationController<Command, PendingRecovery, Completion, ActiveControl, PauseEvent>
{
    pub(crate) fn new(completion_capacity: usize) -> Self {
        let (completion_tx, completion_rx) = mpsc::channel(completion_capacity);
        Self {
            completion_tx,
            completion_rx,
            blocking_in_flight: false,
            deferred_commands: VecDeque::new(),
            pending_recovery: None,
            active_control: None,
            pending_pause_event: None,
        }
    }

    pub(crate) fn begin_blocking(&mut self) -> bool {
        if self.blocking_in_flight {
            return false;
        }
        self.blocking_in_flight = true;
        true
    }

    pub(crate) fn completion_sender(&self) -> mpsc::Sender<Completion> {
        self.completion_tx.clone()
    }

    pub(crate) fn completion_receiver(&mut self) -> &mut mpsc::Receiver<Completion> {
        &mut self.completion_rx
    }

    pub(crate) fn finish_blocking(&mut self) {
        self.blocking_in_flight = false;
    }

    pub(crate) fn is_blocking(&self) -> bool {
        self.blocking_in_flight
    }

    pub(crate) fn defer(&mut self, command: Command) {
        self.deferred_commands.push_back(command);
    }

    pub(crate) fn take_deferred(&mut self) -> Option<Command> {
        self.deferred_commands.pop_front()
    }

    pub(crate) fn set_pending_recovery(&mut self, pending: PendingRecovery) {
        debug_assert!(self.pending_recovery.is_none());
        self.pending_recovery = Some(pending);
    }

    pub(crate) fn pending_recovery(&self) -> Option<&PendingRecovery> {
        self.pending_recovery.as_ref()
    }

    pub(crate) fn take_pending_recovery(&mut self) -> Option<PendingRecovery> {
        self.pending_recovery.take()
    }

    pub(crate) fn bind_active(
        &mut self,
        operation_id: OperationId,
        control: Option<ActiveControl>,
        turn: Option<GoalTurnContext>,
    ) {
        debug_assert!(control.is_some() || turn.is_none());
        self.active_control = control.map(|control| (operation_id, control, turn));
        self.pending_pause_event = None;
    }

    pub(crate) fn active_control(&self, operation_id: OperationId) -> Option<&ActiveControl> {
        self.active_control
            .as_ref()
            .filter(|(active_id, _, _)| *active_id == operation_id)
            .map(|(_, control, _)| control)
    }

    pub(crate) fn active_turn(&self, operation_id: OperationId) -> Option<&GoalTurnContext> {
        self.active_control
            .as_ref()
            .filter(|(active_id, _, _)| *active_id == operation_id)
            .and_then(|(_, _, turn)| turn.as_ref())
    }

    pub(crate) fn replace_active_turn(
        &mut self,
        operation_id: OperationId,
        turn: GoalTurnContext,
    ) -> bool {
        let Some((active_id, _, active_turn)) = self.active_control.as_mut() else {
            return false;
        };
        if *active_id != operation_id {
            return false;
        }
        *active_turn = Some(turn);
        true
    }

    pub(crate) fn has_active_control(&self, operation_id: OperationId) -> bool {
        self.active_control(operation_id).is_some()
    }

    pub(crate) fn schedule_pause_event(&mut self, operation_id: OperationId, event: PauseEvent) {
        if self.pending_pause_event.is_none() && self.has_active_control(operation_id) {
            self.pending_pause_event = Some((operation_id, event));
        }
    }

    pub(crate) fn has_pending_pause_event(&self, operation_id: OperationId) -> bool {
        self.pending_pause_event
            .as_ref()
            .is_some_and(|(active_id, _)| *active_id == operation_id)
    }

    pub(crate) fn take_pause_event(&mut self, operation_id: OperationId) -> Option<PauseEvent> {
        if !self.has_pending_pause_event(operation_id) {
            return None;
        }
        self.pending_pause_event.take().map(|(_, event)| event)
    }

    pub(crate) fn clear_active(&mut self, operation_id: OperationId) {
        if self
            .active_control
            .as_ref()
            .is_some_and(|(active_id, _, _)| *active_id == operation_id)
        {
            self.active_control = None;
        }
        if self.has_pending_pause_event(operation_id) {
            self.pending_pause_event = None;
        }
    }

    #[cfg(test)]
    fn trace(&self) -> GoalControllerTrace {
        GoalControllerTrace::new(
            self.blocking_in_flight,
            self.deferred_commands.len(),
            self.pending_recovery.is_some(),
            self.active_control.is_some(),
            self.pending_pause_event.is_some(),
        )
    }
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct GoalControllerTrace {
    blocking_in_flight: bool,
    deferred_commands: usize,
    pending_recovery: bool,
    active_control: bool,
    pending_pause_event: bool,
}

#[cfg(test)]
impl GoalControllerTrace {
    const fn new(
        blocking_in_flight: bool,
        deferred_commands: usize,
        pending_recovery: bool,
        active_control: bool,
        pending_pause_event: bool,
    ) -> Self {
        Self {
            blocking_in_flight,
            deferred_commands,
            pending_recovery,
            active_control,
            pending_pause_event,
        }
    }
}

#[cfg(test)]
mod tests {
    use orca_core::cancel::OperationIdAllocator;

    use super::{GoalControllerTrace, GoalOperationController};

    #[test]
    fn goal_controller_trace_equivalence() {
        let mut controller = GoalOperationController::<u8, u8, u8, u8, u8>::new(2);
        let mut trace = vec![controller.trace()];

        assert!(controller.begin_blocking());
        controller.defer(10);
        controller.defer(20);
        trace.push(controller.trace());

        controller.set_pending_recovery(30);
        trace.push(controller.trace());
        controller.finish_blocking();
        assert_eq!(controller.take_deferred(), Some(10));
        trace.push(controller.trace());
        assert_eq!(controller.take_pending_recovery(), Some(30));
        assert_eq!(controller.take_deferred(), Some(20));
        trace.push(controller.trace());

        assert_eq!(
            trace,
            vec![
                GoalControllerTrace::new(false, 0, false, false, false),
                GoalControllerTrace::new(true, 2, false, false, false),
                GoalControllerTrace::new(true, 2, true, false, false),
                GoalControllerTrace::new(false, 1, true, false, false),
                GoalControllerTrace::new(false, 0, false, false, false),
            ]
        );

        controller.completion_tx.try_send(40).unwrap();
        assert_eq!(controller.completion_rx.try_recv(), Ok(40));

        let allocator = OperationIdAllocator::new();
        let first = allocator.allocate();
        let second = allocator.allocate();
        controller.bind_active(first, Some(50), None);
        assert_eq!(controller.active_control(first), Some(&50));
        assert_eq!(controller.active_control(second), None);
        controller.schedule_pause_event(second, 60);
        assert!(!controller.has_pending_pause_event(first));
        controller.schedule_pause_event(first, 70);
        assert_eq!(controller.take_pause_event(second), None);
        assert_eq!(controller.take_pause_event(first), Some(70));
        controller.clear_active(first);
        assert_eq!(controller.active_control(first), None);
    }
}
