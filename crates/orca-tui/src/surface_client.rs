use crossbeam_channel as mpsc;
use std::collections::BTreeSet;
use std::io;
use std::time::Duration;

use orca_core::config::RunConfig;
#[cfg(not(test))]
use orca_runtime::runtime_host::HostedOperationKind;
use orca_runtime::runtime_host::{HostedTurnRequest, RuntimeThreadHandle};
use orca_runtime::surface::{
    AttachResult, FreshAttachRequest, MutationReply, OperationIngressCorrelation, OperationKind,
    OperationPatch, OperationRequestIntent, OperationSettingsPreparation, OperationTerminal,
    ReplayabilityRequest, RuntimeSurfaceClientHandle, RuntimeSurfaceHandle, SurfaceAttachmentRole,
    SurfaceCapability, SurfaceEvent, SurfaceInputRequest, SurfaceInputRequestBlock,
    SurfaceInteractionKind, SurfaceOperationId, SurfaceRequestId, SurfaceSubscriptionItem,
    WaitOperationTerminalResult,
};

use crate::hosted_runtime::TuiHostedOperationOutcome;
use crate::operation_controller::TuiOperationController;
use crate::surface_projection::TuiSurfaceProjection;
use crate::types::TuiEvent;

#[cfg(test)]
thread_local! {
    static FORCE_TYPED_SURFACE_TEST_PATH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct SurfaceRunGuard<'a> {
    surface: &'a RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
    controller: &'a TuiOperationController,
    operation_id: Option<SurfaceOperationId>,
    controller_installed: bool,
    cancel_on_drop: bool,
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
    thread: &RuntimeThreadHandle,
    request: HostedTurnRequest,
    config: RunConfig,
    controller: &TuiOperationController,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    #[cfg(test)]
    {
        if FORCE_TYPED_SURFACE_TEST_PATH.with(std::cell::Cell::get) {
            return run_typed(thread, request, controller, event_tx);
        }
        return crate::app::run_hosted_operation(thread, request, config, controller, event_tx);
    }
    #[cfg(not(test))]
    if matches!(request.operation_kind(), HostedOperationKind::Turn) {
        run_typed(thread, request, controller, event_tx)
    } else {
        crate::app::run_hosted_operation(thread, request, config, controller, event_tx)
    }
}

fn run_typed(
    thread: &RuntimeThreadHandle,
    request: HostedTurnRequest,
    controller: &TuiOperationController,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    let surface = thread.surface();
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
                    failure = Some(io::Error::other(terminal_wait_failure_message(&other)));
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
    use orca_runtime::runtime_host::RuntimeHost;
    use std::sync::Mutex;
    use std::time::Instant;

    use crate::interaction_broker::TuiInteractionBroker;

    static ORCA_HOME_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn run_through_dispatch(
        thread: &RuntimeThreadHandle,
        request: HostedTurnRequest,
        config: RunConfig,
        controller: &TuiOperationController,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        FORCE_TYPED_SURFACE_TEST_PATH.with(|enabled| {
            let previous = enabled.replace(true);
            let result = run(thread, request, config, controller, event_tx);
            enabled.set(previous);
            result
        })
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
}
