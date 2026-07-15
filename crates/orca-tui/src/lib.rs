mod action_dispatcher;
mod agent_runtime;
pub mod app;
mod approval_actions;
mod approval_dialog_actions;
mod approval_mode_actions;
mod background_approval;
mod background_tasks;
pub mod bridge;
mod channels;
mod clipboard;
pub mod commands;
mod composer_input_actions;
mod composer_textarea;
pub mod diff;
mod display_text;
mod frame_scheduler;
mod global_actions;
mod hosted_runtime;
mod idle_key_actions;
mod idle_navigation_actions;
mod idle_submit_actions;
mod input_event_actions;
mod interaction_broker;
mod key_event_actions;
mod mention_menu_actions;
mod mention_search_manager;
mod operation_controller;
mod running_actions;
mod runtime_event_actions;
mod runtime_event_projection;
mod runtime_interaction_adapter;
mod selection;
mod session_picker_actions;
mod setup_actions;
pub mod shortcuts;
mod slash_command_actions;
mod slash_menu_actions;
mod status_key_actions;
mod submitted_turn;
mod terminal_lifecycle;
pub mod theme;
mod transcript_view;
pub mod types;
pub mod ui;
pub mod vim;
mod workflow_notifications;
mod workflow_panel_actions;

pub use app::run_tui;

#[cfg(test)]
pub(crate) mod test_support {
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::Duration;

    use orca_core::approval_types::ApprovalMode;
    use orca_core::cancel::CancelToken;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, ReasoningEffort, RunConfig,
        ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_core::model::ModelSelection;
    use orca_runtime::runtime_host::{
        GenerationContext, HostedTurnRequest, OperationHandle, RuntimeHost, RuntimeThreadHandle,
        ThreadOperationExecutor, ThreadOperationOutcome,
    };
    use orca_runtime::thread::RuntimeThread;

    use crate::interaction_broker::TuiInteractionBroker;
    use crate::operation_controller::{
        TuiOperationController, TuiOperationInterrupt, TuiTurnControl,
    };

    static PROCESS_ENV_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) fn lock_process_env() -> MutexGuard<'static, ()> {
        PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[derive(Clone, Default)]
    pub(crate) struct TestOperationInterrupt {
        calls: Arc<AtomicUsize>,
    }

    impl TestOperationInterrupt {
        pub(crate) fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl TuiOperationInterrupt for TestOperationInterrupt {
        fn interrupt_current(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct HostedControlExecutor {
        controller: TuiOperationController,
        control_tx: crossbeam_channel::Sender<(TuiTurnControl, CancelToken)>,
        release_rx: crossbeam_channel::Receiver<()>,
    }

    impl ThreadOperationExecutor for HostedControlExecutor {
        fn run_turn(
            &self,
            _thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let control = self
                .controller
                .wait_for_hosted(generation.fence().operation_id(), cancel)?;
            self.control_tx
                .send((control, cancel.clone()))
                .map_err(|_| io::Error::other("hosted test control receiver closed"))?;
            loop {
                if cancel.is_cancelled() {
                    return Ok(RunStatus::Cancelled.into());
                }
                match self.release_rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(()) => return Ok(RunStatus::Success.into()),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        return Ok(RunStatus::Cancelled.into());
                    }
                }
            }
        }
    }

    pub(crate) struct HostedOperationHarness {
        controller: TuiOperationController,
        control: TuiTurnControl,
        cancel: CancelToken,
        operation: Arc<OperationHandle>,
        thread: RuntimeThreadHandle,
        release_tx: crossbeam_channel::Sender<()>,
        host: Option<RuntimeHost>,
        completed: bool,
    }

    impl HostedOperationHarness {
        pub(crate) fn start() -> Self {
            let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
            let (control_tx, control_rx) = crossbeam_channel::bounded(1);
            let (release_tx, release_rx) = crossbeam_channel::bounded(1);
            let executor = Arc::new(HostedControlExecutor {
                controller: controller.clone(),
                control_tx,
                release_rx,
            });
            let host = RuntimeHost::start_with_executor(executor).expect("hosted test runtime");
            let thread = host
                .start_thread(test_run_config(), "hosted TUI test")
                .expect("hosted test thread");
            let operation = Arc::new(
                thread
                    .start_turn(HostedTurnRequest::new("hosted TUI test"), io::sink())
                    .expect("hosted test operation"),
            );
            controller
                .install_hosted(Arc::clone(&operation))
                .expect("install hosted test operation");
            let (control, cancel) = control_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("hosted test turn control");
            Self {
                controller,
                control,
                cancel,
                operation,
                thread,
                release_tx,
                host: Some(host),
                completed: false,
            }
        }

        pub(crate) fn controller(&self) -> &TuiOperationController {
            &self.controller
        }

        pub(crate) fn control(&self) -> TuiTurnControl {
            self.control.clone()
        }

        pub(crate) fn cancel_token(&self) -> &CancelToken {
            &self.cancel
        }

        pub(crate) fn operation(&self) -> &OperationHandle {
            &self.operation
        }

        pub(crate) fn operation_handle(&self) -> Arc<OperationHandle> {
            Arc::clone(&self.operation)
        }

        pub(crate) fn finish(&mut self) {
            if self.completed {
                return;
            }
            let _ = self.release_tx.try_send(());
            self.operation
                .wait_timeout(Duration::from_secs(2))
                .expect("hosted test operation completion");
            self.controller.complete_hosted(self.operation.id());
            self.completed = true;
        }
    }

    impl Drop for HostedOperationHarness {
        fn drop(&mut self) {
            if !self.completed {
                let _ = self.operation.interrupt();
                let _ = self.release_tx.try_send(());
                let _ = self.operation.wait_timeout(Duration::from_secs(2));
                self.controller.complete_hosted(self.operation.id());
            }
            let _ = self.thread.shutdown();
            if let Some(host) = self.host.take() {
                let _ = host.shutdown();
            }
        }
    }

    pub(crate) fn test_run_config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: std::env::current_dir().ok(),
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(Some("auto".to_string())),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            subagents: Default::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }
}
