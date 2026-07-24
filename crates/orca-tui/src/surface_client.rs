use crossbeam_channel as mpsc;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io;
use std::time::Duration;

use orca_core::config::RunConfig;
use orca_runtime::runtime_host::HostedTurnRequest;
use orca_runtime::surface::{
    AttachResult, FreshAttachRequest, MutationReply, NonEmptyVec, OperationIngressCorrelation,
    OperationKind, OperationPatch, OperationRequestIntent, OperationSettingsPreparation,
    OperationTerminal, ReplayabilityRequest, RuntimeSettingsPatch, RuntimeSurfaceClientHandle,
    RuntimeSurfaceHandle, RuntimeSurfaceThreadHandle, SurfaceAttachmentRole, SurfaceCapability,
    SurfaceEvent, SurfaceInputRequest, SurfaceInputRequestBlock, SurfaceInteractionKind,
    SurfaceOperationId, SurfaceRequestId, SurfaceSettingsSnapshot, SurfaceSubscriptionItem,
    WaitOperationTerminalResult,
};

use crate::hosted_runtime::TuiHostedOperationOutcome;
use crate::operation_controller::TuiOperationController;
use crate::surface_projection::TuiSurfaceProjection;
use crate::types::TuiEvent;

#[derive(Debug)]
pub(crate) struct TerminalRecoveryRequired(&'static str);

#[cfg(test)]
pub(crate) fn terminal_recovery_error_for_test(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, TerminalRecoveryRequired(message))
}

impl fmt::Display for TerminalRecoveryRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for TerminalRecoveryRequired {}

pub(crate) fn is_terminal_recovery_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<TerminalRecoveryRequired>())
        .is_some()
}

struct SurfaceRunGuard<'a> {
    surface: &'a RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
    controller: &'a TuiOperationController,
    operation_id: Option<SurfaceOperationId>,
    controller_installed: bool,
    cancel_on_drop: bool,
}

struct SurfaceActivationGuard<'a> {
    controller: &'a TuiOperationController,
    pending: bool,
}

impl<'a> SurfaceActivationGuard<'a> {
    fn begin(controller: &'a TuiOperationController) -> io::Result<Self> {
        controller.begin_surface_activation()?;
        Ok(Self {
            controller,
            pending: true,
        })
    }

    fn disarm(&mut self) {
        self.pending = false;
    }
}

impl Drop for SurfaceActivationGuard<'_> {
    fn drop(&mut self) {
        if self.pending {
            self.controller.cancel_surface_activation();
        }
    }
}

impl<'a> SurfaceRunGuard<'a> {
    fn new(
        surface: &'a RuntimeSurfaceHandle,
        client: RuntimeSurfaceClientHandle,
        controller: &'a TuiOperationController,
    ) -> Self {
        Self {
            surface,
            client,
            controller,
            operation_id: None,
            controller_installed: false,
            cancel_on_drop: true,
        }
    }

    fn bind_operation(&mut self, operation_id: SurfaceOperationId) {
        self.operation_id = Some(operation_id);
    }

    fn controller_installed(&mut self) {
        self.controller_installed = true;
    }

    fn terminal_observed(&mut self) {
        self.cancel_on_drop = false;
    }
}

impl Drop for SurfaceRunGuard<'_> {
    fn drop(&mut self) {
        if self.cancel_on_drop {
            if let Some(operation_id) = self.operation_id.as_ref() {
                let _ = self
                    .client
                    .cancel_operation(SurfaceRequestId::new(), operation_id.clone());
            }
        }
        if self.controller_installed {
            if let Some(operation_id) = self.operation_id.as_ref() {
                self.controller.complete_surface(operation_id);
            }
        }
        detach(self.surface, &self.client);
    }
}

pub(crate) fn run(
    thread: &RuntimeSurfaceThreadHandle,
    request: HostedTurnRequest,
    config: RunConfig,
    controller: &TuiOperationController,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    run_typed_thread(thread, request, config, controller, event_tx)
}

pub(crate) fn update_settings(
    thread: &RuntimeSurfaceThreadHandle,
    patches: NonEmptyVec<RuntimeSettingsPatch>,
) -> io::Result<SurfaceSettingsSnapshot> {
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::ManageThreadSettings,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => {
            return Err(io::Error::other(
                "typed TUI settings attachment unavailable",
            ));
        }
    };
    let expected_revision = attachment.baseline.snapshot.settings.thread_revision;
    let result =
        attachment
            .client
            .update_settings(SurfaceRequestId::new(), expected_revision, patches);
    detach(&surface, &attachment.client);
    let result = result.map_err(|error| {
        io::Error::other(format!("typed TUI settings update failed: {error:?}"))
    })?;
    match result {
        MutationReply::Committed { value, .. } => Ok(value.settings),
        MutationReply::Uncommitted { mutation } => Err(io::Error::other(format!(
            "typed TUI settings update did not commit: {mutation:?}"
        ))),
        MutationReply::Deferred { .. } => {
            Err(io::Error::other("typed TUI settings update deferred"))
        }
    }
}

fn run_typed_thread(
    thread: &RuntimeSurfaceThreadHandle,
    request: HostedTurnRequest,
    config: RunConfig,
    controller: &TuiOperationController,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    let surface = thread.surface();
    run_typed_surface(&surface, request, config, controller, event_tx)
}

fn run_typed_surface(
    surface: &RuntimeSurfaceHandle,
    request: HostedTurnRequest,
    config: RunConfig,
    controller: &TuiOperationController,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    let mut activation = SurfaceActivationGuard::begin(controller)?;
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
            SurfaceCapability::RespondGrantedInteraction,
        ]),
        interaction_capabilities: BTreeSet::from([
            SurfaceInteractionKind::ToolApproval,
            SurfaceInteractionKind::PermissionRequest,
            SurfaceInteractionKind::UserInput,
            SurfaceInteractionKind::McpElicitation,
        ]),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        AttachResult::Denied { .. } => {
            return Err(io::Error::other("typed TUI surface attachment denied"));
        }
        AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. }
        | AttachResult::ThreadClosed { .. }
        | AttachResult::Unavailable { .. } => {
            return Err(io::Error::other("typed TUI surface unavailable"));
        }
    };
    let mut guard = SurfaceRunGuard::new(&surface, attachment.client.clone(), controller);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .ok_or_else(|| io::Error::other("typed TUI surface subscription unavailable"))?;
    let mut projection = TuiSurfaceProjection::from_surface_snapshot(&attachment.baseline.snapshot);
    if !typed_config_matches_surface(&config, &attachment.baseline.snapshot) {
        return Err(io::Error::other(
            "typed TUI config differs from runtime surface settings; update settings before submitting",
        ));
    }
    for event in projection.hydrate_open_streams() {
        let _ = event_tx.send(event);
    }
    let intent = OperationRequestIntent {
        correlation: OperationIngressCorrelation::TuiUser,
        kind: OperationKind::UserTurn,
        input: Some(SurfaceInputRequest {
            blocks: orca_runtime::surface::NonEmptyVec::try_new(vec![
                SurfaceInputRequestBlock::Text {
                    text: orca_runtime::surface::DisplayText::new(request.prompt()),
                },
            ])
            .map_err(|error| io::Error::other(error.to_string()))?,
        }),
        replayability: ReplayabilityRequest::CaptureReplayableCapsule,
        settings_preparation: OperationSettingsPreparation::UseCurrent {
            expected_settings_revision: attachment.baseline.snapshot.settings.thread_revision,
            expected_policy_epoch: attachment.baseline.snapshot.settings.effective.policy_epoch,
        },
    };
    let reserved = match attachment
        .client
        .reserve_operation(SurfaceRequestId::new(), intent)
        .map_err(|error| io::Error::other(format!("typed TUI reserve failed: {error:?}")))?
    {
        MutationReply::Committed { value, .. } => value,
        MutationReply::Deferred {
            mutation,
            partial: orca_runtime::surface::DeferredCommandValue::Provisional { value },
        } => {
            guard.bind_operation(value.operation_id.clone());
            return Err(io::Error::other(format!(
                "typed TUI reserve deferred and requires runtime reconciliation: request={:?} commit={:?}",
                mutation.request_id, mutation.commit_id
            )));
        }
        MutationReply::Deferred {
            mutation,
            partial: orca_runtime::surface::DeferredCommandValue::NoValue,
        } => {
            return Err(io::Error::other(format!(
                "typed TUI reserve deferred without provisional operation: request={:?} commit={:?}",
                mutation.request_id, mutation.commit_id
            )));
        }
        MutationReply::Uncommitted { mutation } => {
            return Err(io::Error::other(format!(
                "typed TUI reserve did not commit: {mutation:?}"
            )));
        }
    };
    let operation_id = reserved.operation_id.clone();
    guard.bind_operation(operation_id.clone());
    projection.focus_operation(operation_id.clone());
    match attachment
        .client
        .admit_reserved(
            SurfaceRequestId::new(),
            operation_id.clone(),
            reserved.lease.lease_id,
        )
        .map_err(|error| io::Error::other(format!("typed TUI admission failed: {error:?}")))?
    {
        MutationReply::Committed { .. } => {}
        MutationReply::Deferred { mutation, .. } => {
            return Err(io::Error::other(format!(
                "typed TUI admission deferred and requires runtime reconciliation: request={:?} commit={:?}",
                mutation.request_id, mutation.commit_id
            )));
        }
        MutationReply::Uncommitted { mutation } => {
            return Err(io::Error::other(format!(
                "typed TUI admission did not commit: {mutation:?}"
            )));
        }
    }
    controller.install_surface(attachment.client.clone(), operation_id.clone())?;
    activation.disarm();
    guard.controller_installed();

    let result = drain_operation(
        &attachment.client,
        &operation_id,
        &mut subscription,
        &mut projection,
        controller,
        event_tx,
    );
    if result.is_ok() {
        guard.terminal_observed();
    }
    result
}

fn typed_config_matches_surface(
    config: &RunConfig,
    snapshot: &orca_runtime::unstable_surface::SurfaceSnapshot,
) -> bool {
    let expected_cwd = config.cwd.clone().unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
    });
    let Ok(expected_cwd) = orca_runtime::unstable_surface::CanonicalPath::try_new(expected_cwd)
    else {
        return false;
    };
    let expected_roots = config
        .runtime_workspace_roots
        .clone()
        .unwrap_or_else(|| vec![expected_cwd.as_path().to_path_buf()]);
    let expected_roots = expected_roots
        .into_iter()
        .map(orca_runtime::unstable_surface::CanonicalPath::try_new)
        .collect::<Result<Vec<_>, _>>();
    let Ok(expected_roots) = expected_roots else {
        return false;
    };
    let expected_approval_mode = match config.approval_mode {
        orca_core::approval_types::ApprovalMode::Suggest => {
            orca_runtime::unstable_surface::SurfaceApprovalMode::Suggest
        }
        orca_core::approval_types::ApprovalMode::AutoEdit => {
            orca_runtime::unstable_surface::SurfaceApprovalMode::AutoEdit
        }
        orca_core::approval_types::ApprovalMode::FullAuto => {
            orca_runtime::unstable_surface::SurfaceApprovalMode::FullAuto
        }
        orca_core::approval_types::ApprovalMode::Plan => {
            orca_runtime::unstable_surface::SurfaceApprovalMode::Plan
        }
    };
    let expected_reasoning = match config.reasoning_effort {
        orca_core::config::ReasoningEffort::High => {
            orca_runtime::unstable_surface::SurfaceReasoningEffort::High
        }
        orca_core::config::ReasoningEffort::Max => {
            orca_runtime::unstable_surface::SurfaceReasoningEffort::Max
        }
    };
    snapshot.settings.effective.model.as_str() == config.model.display_name()
        && snapshot.settings.effective.cwd == expected_cwd
        && snapshot.settings.effective.workspace_roots == expected_roots
        && snapshot.settings.effective.approval_mode == expected_approval_mode
        && snapshot.settings.effective.reasoning_effort == expected_reasoning
}

fn drain_operation(
    client: &RuntimeSurfaceClientHandle,
    operation_id: &SurfaceOperationId,
    subscription: &mut orca_runtime::surface::SurfaceSubscriptionReceiver,
    projection: &mut TuiSurfaceProjection,
    controller: &TuiOperationController,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    let (wait_tx, wait_rx) = mpsc::bounded(1);
    let waiter_client = client.clone();
    let waiter_operation_id = operation_id.clone();
    let waiter = std::thread::spawn(move || {
        let result =
            waiter_client.wait_operation_terminal(SurfaceRequestId::new(), waiter_operation_id);
        let _ = wait_tx.send(result);
    });
    let mut terminal_seen = false;
    let mut sealed = false;
    let mut terminal_receipt = None;
    let mut failure: Option<io::Error> = None;
    let mut waiter_finished = false;
    let mut terminal_event_emitted = false;
    while (!terminal_seen || terminal_receipt.is_none()) && !sealed {
        let mut made_progress = false;
        while let Some(item) = subscription.try_recv() {
            made_progress = true;
            match item {
                SurfaceSubscriptionItem::Batch { batch } => {
                    terminal_seen |= batch.events.as_slice().iter().any(|envelope| {
                        matches!(
                            &envelope.event,
                            SurfaceEvent::Operation(OperationPatch::Terminal { record })
                                if &record.operation_id == operation_id
                        )
                    });
                    for envelope in batch.events.as_slice() {
                        if let SurfaceEvent::Interaction(
                            orca_runtime::unstable_surface::InteractionPatch::Requested {
                                interaction,
                            },
                        ) = &envelope.event
                        {
                            if let Some(event) =
                                controller.register_surface_interaction(interaction)
                            {
                                let _ = event_tx.send(event);
                            }
                        }
                    }
                    match projection.reduce_typed_batch(&batch) {
                        Ok(events) => {
                            for event in events {
                                if matches!(event, TuiEvent::SessionCompleted { .. }) {
                                    terminal_event_emitted = true;
                                }
                                let _ = event_tx.send(event);
                            }
                        }
                        Err(error) => {
                            failure = Some(io::Error::other(format!(
                                "typed TUI projection failed: {error:?}"
                            )));
                            sealed = true;
                            break;
                        }
                    }
                }
                SurfaceSubscriptionItem::Gap { required } => {
                    failure = Some(io::Error::other(format!(
                        "typed TUI subscription gap requires {:?}",
                        required.reason
                    )));
                    sealed = true;
                    break;
                }
                SurfaceSubscriptionItem::Sealed { .. } => sealed = true,
            }
        }
        if let Ok(result) = wait_rx.try_recv() {
            made_progress = true;
            waiter_finished = true;
            match result {
                Ok(WaitOperationTerminalResult::Terminal { value }) => {
                    if &value.operation_id == operation_id {
                        terminal_receipt = Some(value);
                    } else {
                        failure = Some(io::Error::other(
                            "typed TUI terminal waiter returned another operation",
                        ));
                        sealed = true;
                    }
                }
                Ok(other) => {
                    let message = terminal_wait_failure_message(&other);
                    failure = Some(
                        if matches!(
                            other,
                            WaitOperationTerminalResult::TerminalCommitFailure { .. }
                                | WaitOperationTerminalResult::TerminalProjectionFailure { .. }
                        ) {
                            io::Error::new(io::ErrorKind::Other, TerminalRecoveryRequired(message))
                        } else {
                            io::Error::other(message)
                        },
                    );
                    sealed = true;
                }
                Err(error) => {
                    failure = Some(io::Error::other(format!(
                        "typed TUI terminal wait failed: {error:?}"
                    )));
                    sealed = true;
                }
            }
        }
        if (!terminal_seen || terminal_receipt.is_none()) && !sealed {
            if controller.is_shutdown() {
                let _ = client.cancel_operation(SurfaceRequestId::new(), operation_id.clone());
            }
            if !made_progress {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    if let Some(terminal) = terminal_receipt {
        if !terminal_event_emitted {
            let _ = event_tx.send(TuiEvent::SessionCompleted {
                status: terminal_status(terminal.terminal.clone()).to_string(),
            });
        }
        let _ = waiter.join();
        return Ok(TuiHostedOperationOutcome::Turn {
            status: terminal_status(terminal.terminal).to_string(),
        });
    }
    if failure.is_none() && (sealed || !terminal_seen) {
        failure = Some(io::Error::other(
            "typed TUI surface closed before terminal reconciliation",
        ));
    }
    if let Some(error) = failure {
        let _ = client.cancel_operation(SurfaceRequestId::new(), operation_id.clone());
        if !waiter_finished {
            match wait_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(WaitOperationTerminalResult::Terminal { value }))
                    if &value.operation_id == operation_id =>
                {
                    if !terminal_event_emitted {
                        let _ = event_tx.send(TuiEvent::SessionCompleted {
                            status: terminal_status(value.terminal.clone()).to_string(),
                        });
                    }
                    let _ = waiter.join();
                    return Ok(TuiHostedOperationOutcome::Turn {
                        status: terminal_status(value.terminal).to_string(),
                    });
                }
                Ok(_) => waiter_finished = true,
                Err(_) => {}
            }
        }
        if waiter_finished {
            let _ = waiter.join();
        }
        return Err(error);
    }
    let _ = waiter.join();
    let terminal = terminal_receipt.expect("terminal receipt checked above");
    let status = terminal_status(terminal.terminal);
    Ok(TuiHostedOperationOutcome::Turn {
        status: status.to_string(),
    })
}

fn terminal_wait_failure_message(result: &WaitOperationTerminalResult) -> &'static str {
    match result {
        WaitOperationTerminalResult::Terminal { .. } => {
            "typed TUI terminal waiter returned terminal"
        }
        WaitOperationTerminalResult::TerminalCommitFailure { .. } => {
            "typed TUI terminal commit requires recovery"
        }
        WaitOperationTerminalResult::TerminalProjectionFailure { .. } => {
            "typed TUI terminal projection requires recovery"
        }
        WaitOperationTerminalResult::UnknownOperation { .. } => {
            "typed TUI terminal waiter lost operation"
        }
        WaitOperationTerminalResult::WrongThread { .. } => {
            "typed TUI terminal waiter used wrong thread"
        }
        WaitOperationTerminalResult::WaitCancelled { .. } => {
            "typed TUI terminal waiter was cancelled"
        }
    }
}

fn terminal_status(terminal: OperationTerminal) -> &'static str {
    match terminal {
        OperationTerminal::Succeeded { .. } => "success",
        OperationTerminal::Cancelled { .. } | OperationTerminal::Shutdown { .. } => "cancelled",
        OperationTerminal::BudgetExhausted { .. } => "budget_exhausted",
        OperationTerminal::NotAdmitted { .. } => "not_admitted",
        OperationTerminal::Failed { .. }
        | OperationTerminal::Panicked { .. }
        | OperationTerminal::JoinFailed { .. }
        | OperationTerminal::AbortedByRuntimeRestart { .. } => "failed",
    }
}

fn detach(surface: &RuntimeSurfaceHandle, client: &RuntimeSurfaceClientHandle) {
    let _ = surface.detach(
        client,
        orca_runtime::surface::DetachRequest {
            request_id: SurfaceRequestId::new(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::config::HistoryMode;
    use orca_runtime::runtime_host::{RuntimeHost, RuntimeThreadHandle};
    use std::sync::Mutex;
    use std::time::Instant;

    use crate::interaction_broker::TuiInteractionBroker;
    use crate::types::TuiTaskLifecycle;

    static ORCA_HOME_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn run_through_dispatch(
        thread: &RuntimeThreadHandle,
        request: HostedTurnRequest,
        config: RunConfig,
        controller: &TuiOperationController,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        let typed_thread = thread.typed_surface();
        run(&typed_thread, request, config, controller, event_tx)
    }

    #[test]
    fn typed_ordinary_turn_projects_terminal_and_assistant_output() {
        let _guard = ORCA_HOME_TEST_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed TUI turn")
            .expect("runtime thread");
        let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        let (event_tx, event_rx) = mpsc::unbounded();

        let outcome = run_through_dispatch(
            &thread,
            HostedTurnRequest::new("hello from typed TUI"),
            config,
            &controller,
            &event_tx,
        )
        .expect("typed operation");
        let events = event_rx.try_iter().collect::<Vec<_>>();

        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            TuiEvent::MessageDelta(_) | TuiEvent::AssistantResponseCompleted(_, _)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            TuiEvent::TurnStarted {
                task: Some(TuiTaskLifecycle { status, .. }),
                ..
            } if status == "running"
        )));
        assert!(events.iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));
        assert!(controller.current_id().is_none());
        assert!(!controller.has_surface_active());

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_ordinary_turn_interrupt_uses_surface_cancel() {
        let _guard = ORCA_HOME_TEST_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed TUI cancellation")
            .expect("runtime thread");
        let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        let (event_tx, _event_rx) = mpsc::unbounded();
        let (result_tx, result_rx) = mpsc::bounded(1);
        let run_thread = thread.clone();
        let run_controller = controller.clone();
        let run_config = config;
        let worker = std::thread::spawn(move || {
            let result = run_through_dispatch(
                &run_thread,
                HostedTurnRequest::new("mock_stream_delay_ms 5000"),
                run_config,
                &run_controller,
                &event_tx,
            );
            let _ = result_tx.send(result);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !controller.has_surface_active() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(controller.has_surface_active());

        controller.interrupt_current();
        let outcome = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("typed cancellation terminal")
            .expect("typed cancellation outcome");
        worker.join().expect("typed TUI worker");

        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "cancelled"
        ));
        assert!(!controller.has_surface_active());

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_late_interrupt_does_not_cancel_next_turn() {
        let _guard = ORCA_HOME_TEST_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed late interrupt")
            .expect("runtime thread");
        let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        let (first_event_tx, _first_event_rx) = mpsc::unbounded();
        let first = run_through_dispatch(
            &thread,
            HostedTurnRequest::new("first completed turn"),
            config.clone(),
            &controller,
            &first_event_tx,
        )
        .expect("first typed turn");
        assert!(matches!(
            first,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));

        controller.interrupt_current();

        let (second_event_tx, second_event_rx) = mpsc::unbounded();
        let second = run_through_dispatch(
            &thread,
            HostedTurnRequest::new("second turn after late interrupt"),
            config,
            &controller,
            &second_event_tx,
        )
        .expect("second typed turn");
        assert!(matches!(
            second,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(second_event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_failed_prearmed_activation_does_not_cancel_next_turn() {
        let _guard = ORCA_HOME_TEST_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed prearmed activation failure")
            .expect("runtime thread");
        let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        assert!(controller.begin_surface_activation().expect("prearm"));

        let mut mismatched = config.clone();
        mismatched.approval_mode = orca_core::approval_types::ApprovalMode::AutoEdit;
        let (failed_event_tx, _failed_event_rx) = mpsc::unbounded();
        let typed_thread = thread.typed_surface();
        let error = match run_typed_thread(
            &typed_thread,
            HostedTurnRequest::new("must fail before reservation"),
            mismatched,
            &controller,
            &failed_event_tx,
        ) {
            Ok(_) => panic!("mismatched settings must reject the activation"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("config differs"));

        controller.interrupt_current();

        let (event_tx, event_rx) = mpsc::unbounded();
        let typed_thread = thread.typed_surface();
        let outcome = run_typed_thread(
            &typed_thread,
            HostedTurnRequest::new("turn after failed activation"),
            config,
            &controller,
            &event_tx,
        )
        .expect("next typed turn");
        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_ordinary_turn_routes_tool_approval_through_runtime_surface() {
        let _guard = ORCA_HOME_TEST_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed TUI approval")
            .expect("runtime thread");
        let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        let (event_tx, event_rx) = mpsc::unbounded();
        let worker_controller = controller.clone();
        let worker_thread = thread.clone();
        let worker_config = config.clone();
        let worker_event_tx = event_tx.clone();
        let worker = std::thread::spawn(move || {
            run_through_dispatch(
                &worker_thread,
                HostedTurnRequest::new("bash printf canonical-approval"),
                worker_config,
                &worker_controller,
                &worker_event_tx,
            )
        });
        let key = loop {
            match event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("approval event")
            {
                TuiEvent::ApprovalNeeded { key, .. } => break key,
                _ => {}
            }
        };
        assert!(
            controller
                .respond_surface_interaction(
                    &key,
                    &crate::types::TuiInteractionResponse::Approval(true)
                )
                .expect("typed approval response")
        );
        let outcome = worker
            .join()
            .expect("typed approval worker")
            .expect("typed approval");
        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_ordinary_turn_routes_permission_through_runtime_surface() {
        let _guard = ORCA_HOME_TEST_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed TUI permission")
            .expect("runtime thread");
        let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        let (event_tx, event_rx) = mpsc::unbounded();
        let worker_controller = controller.clone();
        let worker_thread = thread.clone();
        let worker_config = config.clone();
        let worker_event_tx = event_tx.clone();
        let worker = std::thread::spawn(move || {
            run_through_dispatch(
                &worker_thread,
                HostedTurnRequest::new("request_network_permissions_then_done example.com"),
                worker_config,
                &worker_controller,
                &worker_event_tx,
            )
        });
        let key = loop {
            match event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("permission event")
            {
                TuiEvent::PermissionApprovalNeeded { key, .. } => break key,
                _ => {}
            }
        };
        assert!(
            controller
                .respond_surface_interaction(
                    &key,
                    &crate::types::TuiInteractionResponse::Permission(true)
                )
                .expect("typed permission response")
        );
        let outcome = worker
            .join()
            .expect("typed permission worker")
            .expect("typed permission");
        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_ordinary_turn_routes_user_input_through_runtime_surface() {
        let _guard = ORCA_HOME_TEST_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed TUI user input")
            .expect("runtime thread");
        let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        let (event_tx, event_rx) = mpsc::unbounded();
        let worker_controller = controller.clone();
        let worker_thread = thread.clone();
        let worker_config = config.clone();
        let worker_event_tx = event_tx.clone();
        let worker = std::thread::spawn(move || {
            run_through_dispatch(
                &worker_thread,
                HostedTurnRequest::new("ask continue?"),
                worker_config,
                &worker_controller,
                &worker_event_tx,
            )
        });
        let key = loop {
            match event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("user input event")
            {
                TuiEvent::UserInputRequested { key, .. } => break key,
                _ => {}
            }
        };
        assert!(
            controller
                .respond_surface_interaction(
                    &key,
                    &crate::types::TuiInteractionResponse::UserInput("yes".to_string()),
                )
                .expect("typed user input response")
        );
        let outcome = worker
            .join()
            .expect("typed user input worker")
            .expect("typed user input");
        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_ordinary_turn_reloads_and_runs_after_runtime_restart() {
        let _guard = ORCA_HOME_TEST_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;

        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed restart source")
            .expect("runtime thread");
        let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        let (event_tx, event_rx) = mpsc::unbounded();
        let first = run_through_dispatch(
            &thread,
            HostedTurnRequest::new("before runtime restart"),
            config.clone(),
            &controller,
            &event_tx,
        )
        .expect("first typed turn");
        assert!(matches!(
            first,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));
        let thread_id = thread.thread_id().to_string();
        thread.shutdown().expect("first thread shutdown");
        host.shutdown().expect("first host shutdown");

        let mut resumed_config = config;
        resumed_config.history_mode = HistoryMode::Resume(thread_id);
        let resumed_host = RuntimeHost::start().expect("resumed runtime host");
        let resumed_thread = resumed_host
            .start_thread(resumed_config.clone(), "typed restart resumed")
            .expect("resumed runtime thread");
        let resumed_controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        let (resumed_event_tx, resumed_event_rx) = mpsc::unbounded();
        let second = run_through_dispatch(
            &resumed_thread,
            HostedTurnRequest::new("after runtime restart"),
            resumed_config,
            &resumed_controller,
            &resumed_event_tx,
        )
        .expect("resumed typed turn");
        assert!(matches!(
            second,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        let resumed_events = resumed_event_rx.try_iter().collect::<Vec<_>>();
        assert!(resumed_events.iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));
        assert!(
            resumed_events
                .iter()
                .any(|event| matches!(event, TuiEvent::MessageDelta(text) if !text.is_empty()))
        );

        resumed_thread.shutdown().expect("resumed thread shutdown");
        resumed_host.shutdown().expect("resumed host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_cancelled_turn_restarts_and_next_turn_commits() {
        let _guard = ORCA_HOME_TEST_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;

        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed cancellation restart source")
            .expect("runtime thread");
        let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        let (event_tx, _event_rx) = mpsc::unbounded();
        let (result_tx, result_rx) = mpsc::bounded(1);
        let run_thread = thread.clone();
        let run_controller = controller.clone();
        let run_config = config.clone();
        let worker = std::thread::spawn(move || {
            let result = run_through_dispatch(
                &run_thread,
                HostedTurnRequest::new("mock_stream_delay_ms 5000"),
                run_config,
                &run_controller,
                &event_tx,
            );
            let _ = result_tx.send(result);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !controller.has_surface_active() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(controller.has_surface_active());
        controller.interrupt_current();

        let cancelled = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("cancelled terminal")
            .expect("cancelled typed turn");
        worker.join().expect("cancelled worker");
        assert!(matches!(
            cancelled,
            TuiHostedOperationOutcome::Turn { status } if status == "cancelled"
        ));
        let thread_id = thread.thread_id().to_string();
        thread.shutdown().expect("source thread shutdown");
        host.shutdown().expect("source host shutdown");

        let mut resumed_config = config;
        resumed_config.history_mode = HistoryMode::Resume(thread_id);
        let resumed_host = RuntimeHost::start().expect("resumed runtime host");
        let resumed_thread = resumed_host
            .start_thread(resumed_config.clone(), "typed cancellation restart resumed")
            .expect("resumed runtime thread");
        let resumed_controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        let (resumed_event_tx, resumed_event_rx) = mpsc::unbounded();
        let resumed = run_through_dispatch(
            &resumed_thread,
            HostedTurnRequest::new("after cancellation restart"),
            resumed_config,
            &resumed_controller,
            &resumed_event_tx,
        )
        .expect("resumed typed turn");
        assert!(matches!(
            resumed,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(resumed_event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));

        resumed_thread.shutdown().expect("resumed thread shutdown");
        resumed_host.shutdown().expect("resumed host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }
}
