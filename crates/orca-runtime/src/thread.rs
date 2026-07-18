use std::io;
use std::sync::Arc;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::{EventFactory, EventPublicationStore, RunStatus};
use orca_mcp::McpRegistry;

use crate::controller::{
    ControllerRunOptions, ThreadTurnExecutor, ThreadTurnOutcome, ThreadTurnRequest,
};
use crate::extension::ExtensionData;
use crate::goal_actor::{GoalRuntimeBinding, GoalRuntimeHandle};
use crate::lifecycle::{RuntimeSessionLifecycle, RuntimeTaskKind};
use crate::session::{InteractiveSession, new_run_id};
use crate::thread_store::SessionTranscript;

pub struct RuntimeThread {
    thread_id: String,
    session: InteractiveSession,
    lifecycle: RuntimeSessionLifecycle,
    thread_extensions: Arc<ExtensionData>,
    next_extension_turn: u64,
    goal_runtime: Option<GoalRuntimeHandle>,
    goal_actor_join: Option<std::thread::JoinHandle<()>>,
}

impl RuntimeThread {
    pub fn start(config: &RunConfig, title: impl Into<String>) -> io::Result<Self> {
        let session = InteractiveSession::new_with_preloaded(config, &title.into(), None)?;
        Ok(Self::from_session(session))
    }

    pub fn start_with_preloaded(
        config: &RunConfig,
        title: impl Into<String>,
        preloaded: Option<SessionTranscript>,
    ) -> io::Result<Self> {
        let session = InteractiveSession::new_with_preloaded(config, &title.into(), preloaded)?;
        Ok(Self::from_session(session))
    }

    pub fn start_with_preloaded_and_mcp_registry(
        config: &RunConfig,
        title: impl Into<String>,
        preloaded: Option<SessionTranscript>,
        mcp_registry: McpRegistry,
    ) -> io::Result<Self> {
        let session = InteractiveSession::new_with_preloaded_and_mcp_registry(
            config,
            &title.into(),
            preloaded,
            mcp_registry,
        )?;
        Ok(Self::from_session(session))
    }

    fn from_session(session: InteractiveSession) -> Self {
        let thread_id = session
            .session_id()
            .map(ToString::to_string)
            .unwrap_or_else(new_run_id);
        let mut lifecycle = RuntimeSessionLifecycle::new(thread_id.clone());
        lifecycle.start_task(RuntimeTaskKind::Agent);

        Self {
            thread_extensions: Arc::new(ExtensionData::new(thread_id.clone())),
            thread_id,
            session,
            lifecycle,
            next_extension_turn: 0,
            goal_runtime: None,
            goal_actor_join: None,
        }
    }

    fn begin_goal_turn(
        &mut self,
        request: &ThreadTurnRequest,
    ) -> io::Result<Option<GoalRuntimeBinding>> {
        if request.tool_mode() != crate::controller::ThreadTurnToolMode::Goal {
            return Ok(None);
        }
        let Some(session_id) = self.session().session_id().map(str::to_string) else {
            return Ok(None);
        };
        let handle = match self.goal_runtime_handle() {
            Ok(handle) => handle,
            Err(_) => return Ok(None),
        };
        let origin = if request.continuation().is_some() {
            orca_core::goal_runtime::GoalTurnOrigin::Continuation
        } else {
            orca_core::goal_runtime::GoalTurnOrigin::User
        };
        let turn = handle
            .begin_outer_turn(
                &session_id,
                origin,
                request.turn_id().to_string(),
                now_timestamp(),
            )
            .ok();
        let binding = GoalRuntimeBinding { handle, turn };
        self.thread_extensions.insert(binding.clone());
        Ok(Some(binding))
    }

    pub(crate) fn goal_runtime_handle(&mut self) -> io::Result<GoalRuntimeHandle> {
        if self.goal_runtime.is_none() {
            let (handle, join) = GoalRuntimeHandle::open_default().map_err(io::Error::other)?;
            self.goal_runtime = Some(handle);
            self.goal_actor_join = Some(join);
        }
        Ok(self
            .goal_runtime
            .as_ref()
            .expect("goal runtime initialized")
            .clone())
    }

    fn finish_goal_turn(&mut self, binding: Option<&GoalRuntimeBinding>, status: RunStatus) {
        let Some(binding) = binding else {
            return;
        };
        let Some(turn) = binding.turn.as_ref() else {
            self.thread_extensions.remove::<GoalRuntimeBinding>();
            return;
        };
        let goal_status = match status {
            RunStatus::Success => orca_core::goal_runtime::GoalTurnStatus::Success,
            RunStatus::Cancelled => orca_core::goal_runtime::GoalTurnStatus::Cancelled,
            RunStatus::ApprovalRequired => {
                orca_core::goal_runtime::GoalTurnStatus::ApprovalRequired
            }
            RunStatus::BudgetExhausted => orca_core::goal_runtime::GoalTurnStatus::BudgetExhausted,
            RunStatus::Failed | RunStatus::VerificationFailed => {
                orca_core::goal_runtime::GoalTurnStatus::Failed
            }
        };
        let _ = binding.handle.finish_outer_turn(
            &turn.session_id,
            goal_status,
            orca_core::goal_runtime::GoalUsage::default(),
            0,
            0,
            None,
            now_timestamp(),
        );
        self.thread_extensions.remove::<GoalRuntimeBinding>();
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn session(&self) -> &InteractiveSession {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut InteractiveSession {
        &mut self.session
    }

    pub fn lifecycle(&self) -> &RuntimeSessionLifecycle {
        &self.lifecycle
    }

    pub fn lifecycle_mut(&mut self) -> &mut RuntimeSessionLifecycle {
        &mut self.lifecycle
    }

    pub fn thread_extensions(&self) -> &ExtensionData {
        self.thread_extensions.as_ref()
    }

    pub fn thread_extensions_handle(&self) -> Arc<ExtensionData> {
        Arc::clone(&self.thread_extensions)
    }

    pub(crate) fn event_factory(&self) -> EventFactory {
        let run_id = self.thread_id.clone();
        let Some((next_seq, writer)) = self.session.event_publication_store() else {
            return EventFactory::new(run_id);
        };
        let store: Arc<dyn EventPublicationStore> = Arc::new(writer);
        EventFactory::with_publication_store(run_id, next_seq, store)
    }

    pub fn run_turn_to_writer<W: io::Write>(
        &mut self,
        config: &RunConfig,
        prompt: &str,
        writer: W,
        options: ControllerRunOptions,
    ) -> io::Result<RunStatus> {
        self.run_request(
            config,
            &ThreadTurnRequest::new(prompt).with_options(options),
            writer,
        )
    }

    pub fn run_request<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
    ) -> io::Result<RunStatus> {
        let binding = self.begin_goal_turn(request)?;
        let thread_extensions = self.thread_extensions_handle();
        let turn_extension_id = self.next_turn_extension_id();
        let result = ThreadTurnExecutor::new_with_thread_extensions(
            config,
            &mut self.session,
            &mut self.lifecycle,
            thread_extensions,
            turn_extension_id,
        )
        .run_request(request, writer);
        if let Ok(status) = result {
            self.finish_goal_turn(binding.as_ref(), status);
        }
        result
    }

    pub fn run_request_with_cancel<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
        cancel: CancelToken,
    ) -> io::Result<RunStatus> {
        let binding = self.begin_goal_turn(request)?;
        let thread_extensions = self.thread_extensions_handle();
        let turn_extension_id = self.next_turn_extension_id();
        let result = ThreadTurnExecutor::new_with_thread_extensions(
            config,
            &mut self.session,
            &mut self.lifecycle,
            thread_extensions,
            turn_extension_id,
        )
        .run_request_with_cancel(request, writer, cancel);
        if let Ok(status) = result {
            self.finish_goal_turn(binding.as_ref(), status);
        }
        result
    }

    pub fn run_request_with_event_factory<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
        events: &mut EventFactory,
    ) -> io::Result<RunStatus> {
        self.run_request_with_event_factory_and_cancel(
            config,
            request,
            writer,
            events,
            CancelToken::new(),
        )
    }

    pub fn run_request_with_event_factory_and_cancel<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
        events: &mut EventFactory,
        cancel: CancelToken,
    ) -> io::Result<RunStatus> {
        let binding = self.begin_goal_turn(request)?;
        let thread_extensions = self.thread_extensions_handle();
        let turn_extension_id = self.next_turn_extension_id();
        let result = ThreadTurnExecutor::new_with_thread_extensions(
            config,
            &mut self.session,
            &mut self.lifecycle,
            thread_extensions,
            turn_extension_id,
        )
        .run_request_with_event_factory_and_cancel(request, writer, events, cancel);
        if let Ok(status) = result {
            self.finish_goal_turn(binding.as_ref(), status);
        }
        result
    }

    pub fn run_request_with_event_factory_and_cancel_outcome<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
        events: &mut EventFactory,
        cancel: CancelToken,
    ) -> io::Result<ThreadTurnOutcome> {
        let binding = self.begin_goal_turn(request)?;
        let thread_extensions = self.thread_extensions_handle();
        let turn_extension_id = self.next_turn_extension_id();
        let result = ThreadTurnExecutor::new_with_thread_extensions(
            config,
            &mut self.session,
            &mut self.lifecycle,
            thread_extensions,
            turn_extension_id,
        )
        .run_request_with_event_factory_and_cancel_outcome(request, writer, events, cancel);
        if let Ok(ThreadTurnOutcome::Completed { status, .. }) = &result {
            self.finish_goal_turn(binding.as_ref(), *status);
        }
        result
    }

    fn next_turn_extension_id(&mut self) -> String {
        self.next_extension_turn = self.next_extension_turn.saturating_add(1);
        format!("{}:turn-{}", self.thread_id, self.next_extension_turn)
    }
}

impl Drop for RuntimeThread {
    fn drop(&mut self) {
        if let Some(handle) = self.goal_runtime.as_ref() {
            let _ = handle.shutdown();
        }
        if let Some(join) = self.goal_actor_join.take() {
            let _ = join.join();
        }
    }
}

fn now_timestamp() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::CostTracker;
    use crate::lifecycle::RuntimeTurnState;
    use crate::tasks::TaskRegistry;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::cancel::CancelToken;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn test_config(cwd: PathBuf) -> RunConfig {
        RunConfig {
            app_version: "test".to_string(),
            prompt: String::new(),
            cwd: Some(cwd),
            output_format: OutputFormat::Jsonl,
            approval_mode: ApprovalMode::Suggest,
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
            history_mode: HistoryMode::Disabled,
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

    #[test]
    fn runtime_thread_starts_with_runtime_owned_session_and_lifecycle() {
        let cwd = tempfile::tempdir().unwrap();
        let config = test_config(cwd.path().to_path_buf());

        let thread = RuntimeThread::start(&config, "inspect repo").unwrap();

        assert!(thread.thread_id().starts_with("run-"));
        assert_eq!(thread.session().conversation().messages.len(), 1);
        assert_eq!(thread.lifecycle().run_id(), thread.thread_id());
    }

    #[test]
    fn runtime_thread_exposes_session_mutation_through_boundary() {
        let cwd = tempfile::tempdir().unwrap();
        let config = test_config(cwd.path().to_path_buf());
        let mut thread = RuntimeThread::start(&config, "inspect repo").unwrap();

        thread
            .session_mut()
            .replace_skill_context(Some("thread skill marker".to_string()));

        let skill_context = thread
            .session()
            .conversation()
            .volatile
            .skill
            .as_deref()
            .unwrap_or_default();
        assert!(skill_context.contains("thread skill marker"));
    }

    #[derive(Debug)]
    struct ThreadExtensionMarker(&'static str);

    #[derive(Debug)]
    struct TurnExtensionMarker(&'static str);

    #[test]
    fn runtime_thread_reuses_thread_extensions_across_turn_states() {
        let cwd = tempfile::tempdir().unwrap();
        let config = test_config(cwd.path().to_path_buf());
        let mut thread = RuntimeThread::start(&config, "inspect repo").unwrap();
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new(thread.thread_id().to_string());
        let first_turn_id = thread.next_turn_extension_id();
        let second_turn_id = thread.next_turn_extension_id();

        assert_eq!(thread.thread_extensions().level_id(), thread.thread_id());
        assert_eq!(first_turn_id, format!("{}:turn-1", thread.thread_id()));
        assert_eq!(second_turn_id, format!("{}:turn-2", thread.thread_id()));
        thread
            .thread_extensions()
            .insert(ThreadExtensionMarker("thread-scoped"));

        {
            let mut cost_tracker = CostTracker::new(None);
            let first_turn_state = RuntimeTurnState::new_with_thread_extensions(
                &mut cost_tracker,
                &cancel,
                &task_registry,
                thread.thread_extensions_handle(),
                first_turn_id,
            );

            first_turn_state
                .turn_extensions()
                .insert(TurnExtensionMarker("turn-scoped"));
            assert_eq!(
                first_turn_state
                    .thread_extensions()
                    .get::<ThreadExtensionMarker>()
                    .expect("thread marker should persist")
                    .0,
                "thread-scoped"
            );
            assert_eq!(
                first_turn_state
                    .turn_extensions()
                    .get::<TurnExtensionMarker>()
                    .expect("turn marker should exist in first turn")
                    .0,
                "turn-scoped"
            );
        }

        let mut cost_tracker = CostTracker::new(None);
        let second_turn_state = RuntimeTurnState::new_with_thread_extensions(
            &mut cost_tracker,
            &cancel,
            &task_registry,
            thread.thread_extensions_handle(),
            second_turn_id.clone(),
        );

        assert_eq!(
            second_turn_state.turn_extensions().level_id(),
            second_turn_id
        );
        assert_eq!(
            second_turn_state
                .thread_extensions()
                .get::<ThreadExtensionMarker>()
                .expect("thread marker should survive the next turn")
                .0,
            "thread-scoped"
        );
        assert!(
            second_turn_state
                .turn_extensions()
                .get::<TurnExtensionMarker>()
                .is_none(),
            "turn-scoped marker must not leak into later turns"
        );
    }
}
