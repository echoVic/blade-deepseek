use std::io;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::{EventFactory, RunStatus};

use crate::controller::{ControllerRunOptions, ThreadTurnExecutor, ThreadTurnRequest};
use crate::lifecycle::{RuntimeSessionLifecycle, RuntimeTaskKind};
use crate::session::{InteractiveSession, new_run_id};
use crate::thread_store::SessionTranscript;

pub struct RuntimeThread {
    thread_id: String,
    session: InteractiveSession,
    lifecycle: RuntimeSessionLifecycle,
}

impl RuntimeThread {
    pub fn start(config: &RunConfig, title: impl Into<String>) -> io::Result<Self> {
        let session = InteractiveSession::new_with_preloaded(config, &title.into(), None)?;
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
            thread_id,
            session,
            lifecycle,
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
        ThreadTurnExecutor::new(config, &mut self.session, &mut self.lifecycle)
            .run_request(request, writer)
    }

    pub fn run_request_with_cancel<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
        cancel: CancelToken,
    ) -> io::Result<RunStatus> {
        ThreadTurnExecutor::new(config, &mut self.session, &mut self.lifecycle)
            .run_request_with_cancel(request, writer, cancel)
    }

    pub fn run_request_with_event_factory<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
        events: &mut EventFactory,
    ) -> io::Result<RunStatus> {
        ThreadTurnExecutor::new(config, &mut self.session, &mut self.lifecycle)
            .run_request_with_event_factory(request, writer, events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ApprovalMode;
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
}
