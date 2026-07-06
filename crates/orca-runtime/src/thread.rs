use std::io;
use std::sync::Arc;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::{EventFactory, RunStatus};

use crate::controller::{ControllerRunOptions, ThreadTurnExecutor, ThreadTurnRequest};
use crate::extension::ExtensionData;
use crate::lifecycle::{RuntimeSessionLifecycle, RuntimeTaskKind};
use crate::session::{InteractiveSession, new_run_id};
use crate::thread_store::SessionTranscript;

pub struct RuntimeThread {
    thread_id: String,
    session: InteractiveSession,
    lifecycle: RuntimeSessionLifecycle,
    thread_extensions: Arc<ExtensionData>,
    next_extension_turn: u64,
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

    pub(crate) fn resume_same_thread(
        config: &RunConfig,
        transcript: SessionTranscript,
    ) -> io::Result<Self> {
        let session = InteractiveSession::resume_same_thread(config, transcript)?;
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
        }
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

    pub(crate) fn thread_extensions_handle(&self) -> Arc<ExtensionData> {
        Arc::clone(&self.thread_extensions)
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
        let thread_extensions = self.thread_extensions_handle();
        let turn_extension_id = self.next_turn_extension_id();
        ThreadTurnExecutor::new_with_thread_extensions(
            config,
            &mut self.session,
            &mut self.lifecycle,
            thread_extensions,
            turn_extension_id,
        )
        .run_request(request, writer)
    }

    pub fn run_request_with_cancel<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
        cancel: CancelToken,
    ) -> io::Result<RunStatus> {
        let thread_extensions = self.thread_extensions_handle();
        let turn_extension_id = self.next_turn_extension_id();
        ThreadTurnExecutor::new_with_thread_extensions(
            config,
            &mut self.session,
            &mut self.lifecycle,
            thread_extensions,
            turn_extension_id,
        )
        .run_request_with_cancel(request, writer, cancel)
    }

    pub fn run_request_with_event_factory<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
        events: &mut EventFactory,
    ) -> io::Result<RunStatus> {
        let thread_extensions = self.thread_extensions_handle();
        let turn_extension_id = self.next_turn_extension_id();
        ThreadTurnExecutor::new_with_thread_extensions(
            config,
            &mut self.session,
            &mut self.lifecycle,
            thread_extensions,
            turn_extension_id,
        )
        .run_request_with_event_factory(request, writer, events)
    }

    fn next_turn_extension_id(&mut self) -> String {
        self.next_extension_turn = self.next_extension_turn.saturating_add(1);
        format!("{}:turn-{}", self.thread_id, self.next_extension_turn)
    }
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
