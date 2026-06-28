use std::time::{SystemTime, UNIX_EPOCH};

use std::io;
use std::path::Path;

use orca_core::config::{HistoryMode, RunConfig};
use orca_core::conversation::Conversation;
use orca_core::cost_types::UsageTotals;
use orca_core::hook_types::HookEvent;
use orca_core::subagent_types::SubagentType;
use orca_core::task_types::TaskStatus;
use orca_core::tool_types::ToolResult;
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;

use crate::agent_common;
use crate::cost::CostTracker;
use crate::hooks::{HookContext, HookRunner, conversation_with_hook_context};
use crate::instructions::{self, ProjectInstructions};
use crate::memory::{self, MemoryBlock};
use crate::tasks::TaskRegistry;
use crate::thread_store::{
    SessionMeta, SessionStore, SessionTranscript, SessionWriter, ThreadStore,
};

pub fn new_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("run-{nanos}")
}

pub struct InteractiveSession {
    store: SessionStore,
    conversation: Conversation,
    writer: Option<SessionWriter>,
    session_id: Option<String>,
    instructions: ProjectInstructions,
    cost_tracker: CostTracker,
    mcp_registry: McpRegistry,
    hooks: HookRunner,
    memory: MemoryBlock,
    task_registry: TaskRegistry,
}

pub(crate) struct InteractiveSessionRuntimeParts<'a> {
    pub conversation: &'a mut Conversation,
    pub writer: Option<&'a mut SessionWriter>,
    pub instructions: &'a ProjectInstructions,
    pub cost_tracker: &'a mut CostTracker,
    pub mcp_registry: &'a McpRegistry,
    pub hooks: &'a HookRunner,
    pub memory: &'a MemoryBlock,
    pub task_registry: &'a TaskRegistry,
}

pub(crate) struct AgentConversationContext<'a> {
    pub(crate) resumed: Option<&'a SessionTranscript>,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
    pub(crate) conversation: Option<&'a mut Conversation>,
}

impl<'a> AgentConversationContext<'a> {
    pub(crate) fn new() -> Self {
        Self {
            resumed: None,
            history_writer: None,
            conversation: None,
        }
    }

    pub(crate) fn with_resumed(mut self, resumed: Option<&'a SessionTranscript>) -> Self {
        self.resumed = resumed;
        self
    }

    pub(crate) fn with_history_writer(
        mut self,
        history_writer: Option<&'a mut SessionWriter>,
    ) -> Self {
        self.history_writer = history_writer;
        self
    }

    pub(crate) fn with_conversation(mut self, conversation: Option<&'a mut Conversation>) -> Self {
        self.conversation = conversation;
        self
    }

    #[cfg(test)]
    pub(crate) fn resumed(&self) -> Option<&SessionTranscript> {
        self.resumed
    }

    #[cfg(test)]
    pub(crate) fn history_writer(&self) -> Option<&SessionWriter> {
        self.history_writer.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn conversation(&self) -> Option<&Conversation> {
        self.conversation.as_deref()
    }
}

pub(crate) fn record_tool_result_for_agent(
    conversation: &mut Conversation,
    history_writer: Option<&mut SessionWriter>,
    result: &ToolResult,
    emit_deltas: bool,
) -> io::Result<String> {
    let result_content = agent_common::format_tool_result_for_model(result);
    conversation.add_tool_result(result.id.clone(), result_content.clone());
    if emit_deltas && let Some(writer) = history_writer {
        writer.append_tool_result_message(result, result_content.clone(), false)?;
    }
    Ok(result_content)
}

impl InteractiveSession {
    pub fn new_with_preloaded(
        config: &RunConfig,
        prompt_for_title: &str,
        preloaded: Option<SessionTranscript>,
    ) -> io::Result<Self> {
        let cwd = config
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let store = SessionStore::new();
        let instructions = instructions::load_for_cwd_or_default(&cwd);
        let memory = memory::load_for_cwd(&cwd);
        let mcp_registry = orca_mcp::initialize_registry(&config.mcp_servers);
        let hooks = HookRunner::new(config.hooks.clone());
        let system_prompt = agent_common::build_agent_system_prompt(
            &cwd,
            0,
            &SubagentType::General,
            Some(&instructions),
            config.approval_mode,
            Some(&memory),
        );
        let (conversation, loaded_transcript) = match &config.history_mode {
            HistoryMode::Resume(selector) | HistoryMode::Fork(selector) => {
                let transcript = match preloaded {
                    Some(t) => t,
                    None => store.load_session(selector)?,
                };
                let mut conv = store.resume_conversation(&transcript, system_prompt);
                conv.strip_legacy_pinned_volatile();
                conv.strip_legacy_summary_messages();
                (conv, Some(transcript))
            }
            HistoryMode::Record | HistoryMode::Disabled => {
                let mut conversation = Conversation::new();
                conversation.add_system(system_prompt);
                (conversation, None)
            }
        };

        let mut session_id = None;
        let writer = match &config.history_mode {
            HistoryMode::Disabled => None,
            HistoryMode::Record | HistoryMode::Resume(_) => {
                match store.create_live_thread_with_permissions(
                    &cwd,
                    config.provider.as_str(),
                    config.model.as_history_value(),
                    prompt_for_title,
                    config.active_permission_profile.clone(),
                    config.approval_mode,
                    config.permission_rules.clone(),
                    config.additional_working_directories.clone(),
                ) {
                    Ok(mut thread) => {
                        if let Err(error) = thread.append_items(&conversation.messages) {
                            eprintln!("orca: warning: history write failed: {error}");
                            None
                        } else {
                            let (thread_id, writer) = thread.into_thread_id_and_writer();
                            session_id = Some(thread_id);
                            Some(writer)
                        }
                    }
                    Err(error) => {
                        eprintln!("orca: warning: failed to initialize history: {error}");
                        None
                    }
                }
            }
            HistoryMode::Fork(_) => {
                let parent_id = loaded_transcript
                    .map(|transcript| transcript.meta.session_id)
                    .unwrap_or_default();
                let meta = store.create_fork_meta(
                    &cwd,
                    config.provider.as_str(),
                    config.model.as_history_value(),
                    prompt_for_title,
                    parent_id,
                );
                let mut meta = meta;
                meta.active_permission_profile = config.active_permission_profile.clone();
                meta.approval_mode = Some(config.approval_mode);
                meta.permission_rules = config.permission_rules.clone();
                meta.additional_working_directories = config.additional_working_directories.clone();
                session_id = Some(meta.session_id.clone());
                start_writer_with_messages(&store, meta, &conversation)
            }
        };

        let task_session_id = session_id.clone().unwrap_or_else(new_run_id);

        Ok(Self {
            store,
            conversation,
            writer,
            session_id,
            instructions,
            cost_tracker: CostTracker::new(None),
            mcp_registry,
            hooks,
            memory,
            task_registry: TaskRegistry::new_for_cwd(task_session_id, &cwd),
        })
    }

    pub(crate) fn resume_same_thread(
        config: &RunConfig,
        transcript: SessionTranscript,
    ) -> io::Result<Self> {
        let cwd = config
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let store = SessionStore::new();
        let instructions = instructions::load_for_cwd_or_default(&cwd);
        let memory = memory::load_for_cwd(&cwd);
        let mcp_registry = orca_mcp::initialize_registry(&config.mcp_servers);
        let hooks = HookRunner::new(config.hooks.clone());
        let system_prompt = agent_common::build_agent_system_prompt(
            &cwd,
            0,
            &SubagentType::General,
            Some(&instructions),
            config.approval_mode,
            Some(&memory),
        );
        let mut conversation = store.resume_conversation(&transcript, system_prompt);
        conversation.strip_legacy_pinned_volatile();
        conversation.strip_legacy_summary_messages();
        let session_id = transcript.meta.session_id.clone();
        let writer = Some(SessionWriter::append_to_existing(transcript.path)?);

        Ok(Self {
            store,
            conversation,
            writer,
            session_id: Some(session_id.clone()),
            instructions,
            cost_tracker: CostTracker::new(None),
            mcp_registry,
            hooks,
            memory,
            task_registry: TaskRegistry::new_for_cwd(session_id, &cwd),
        })
    }

    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    pub fn conversation_mut(&mut self) -> &mut Conversation {
        &mut self.conversation
    }

    pub fn writer_mut(&mut self) -> Option<&mut SessionWriter> {
        self.writer.as_mut()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    pub fn instructions(&self) -> &ProjectInstructions {
        &self.instructions
    }

    pub fn cost_tracker(&self) -> &CostTracker {
        &self.cost_tracker
    }

    pub fn cost_tracker_mut(&mut self) -> &mut CostTracker {
        &mut self.cost_tracker
    }

    pub fn usage_totals(&self) -> UsageTotals {
        self.cost_tracker.totals()
    }

    pub fn mcp_registry(&self) -> &McpRegistry {
        &self.mcp_registry
    }

    pub fn hooks(&self) -> &HookRunner {
        &self.hooks
    }

    pub fn memory(&self) -> &MemoryBlock {
        &self.memory
    }

    pub fn task_registry(&self) -> &TaskRegistry {
        &self.task_registry
    }

    pub(crate) fn runtime_parts(&mut self) -> InteractiveSessionRuntimeParts<'_> {
        InteractiveSessionRuntimeParts {
            conversation: &mut self.conversation,
            writer: self.writer.as_mut(),
            instructions: &self.instructions,
            cost_tracker: &mut self.cost_tracker,
            mcp_registry: &self.mcp_registry,
            hooks: &self.hooks,
            memory: &self.memory,
            task_registry: &self.task_registry,
        }
    }

    pub fn has_active_workflows(&self) -> bool {
        self.task_registry.list().iter().any(|task| {
            matches!(
                task.status,
                TaskStatus::Queued
                    | TaskStatus::Running
                    | TaskStatus::Paused
                    | TaskStatus::Stopping
            )
        })
    }

    pub fn append_message(&mut self, message: &orca_core::conversation::Message) {
        if let Some(writer) = &mut self.writer {
            if let Err(error) = writer.append_message(message) {
                eprintln!("orca: warning: history write failed: {error}");
                self.writer = None;
            }
        }
    }

    pub fn complete(&mut self, status: &str) {
        if let Some(writer) = &mut self.writer
            && let Err(error) = writer.complete(status)
        {
            eprintln!("orca: warning: history completion write failed: {error}");
        }
    }

    pub fn backtrack_last_user(&mut self) -> Option<String> {
        self.conversation.backtrack_last_user()
    }

    pub fn set_model(&mut self, model: Option<&str>) {
        self.cost_tracker.set_model(model);
    }

    pub fn add_pinned_context(&mut self, content: String) {
        self.conversation.add_user_pinned(content);
        if let Some(message) = self.conversation.messages.last().cloned() {
            self.append_message(&message);
        }
    }

    pub fn replace_goal_context(&mut self, content: String) {
        self.conversation.replace_goal_state(content);
    }

    pub fn replace_skill_context(&mut self, content: Option<String>) {
        self.conversation.replace_skill_context(content);
    }

    pub fn compact(&mut self, config: &RunConfig, cwd: &Path) -> (usize, usize) {
        let before_messages = self.conversation.messages.len();
        if let Ok(outcome) = self.hooks.run(
            HookEvent::OnBudgetWarning,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: Some(before_messages),
                after_messages: None,
                usage: None,
            },
        ) && !outcome.injected_context.is_empty()
        {
            self.conversation = conversation_with_hook_context(&self.conversation, &outcome);
        }
        let _ = self.hooks.run(
            HookEvent::PreCompact,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: Some(before_messages),
                after_messages: None,
                usage: None,
            },
        );
        let provider_config = ProviderConfig {
            api_key: config.api_key.clone(),
            base_url: config.base_url.clone(),
            model: Some(orca_core::model::auxiliary_model().to_string()),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let compaction = orca_provider::context::compact_with_summary(
            config.provider,
            &self.conversation,
            &orca_provider::context::ContextConfig::for_model_with_runtime(
                config.model.as_option().as_deref(),
                &config.model_runtime,
            ),
            &provider_config,
        );
        self.conversation = compaction.conversation;
        let after_messages = self.conversation.messages.len();
        if let Some(writer) = &mut self.writer {
            let _ = writer.append_compaction(before_messages, after_messages);
            if let orca_provider::context::CompactionKind::RemoteSummary(summary) = compaction.kind
            {
                let _ = writer.append_summary_state(
                    before_messages,
                    after_messages,
                    summary,
                    &self.conversation.summary,
                );
            }
        }
        let _ = self.hooks.run(
            HookEvent::PostCompact,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: Some(before_messages),
                after_messages: Some(after_messages),
                usage: None,
            },
        );
        (before_messages, after_messages)
    }
}

fn start_writer_with_messages(
    store: &SessionStore,
    meta: SessionMeta,
    conversation: &Conversation,
) -> Option<SessionWriter> {
    match store.start_writer_from_meta(meta) {
        Ok(mut writer) => {
            for message in &conversation.messages {
                if let Err(error) = writer.append_message(message) {
                    eprintln!("orca: warning: history write failed: {error}");
                    return None;
                }
            }
            if !conversation.summary.is_empty() {
                let inherited_marker = conversation
                    .summary
                    .latest_rolling()
                    .map(|text| text.to_string())
                    .unwrap_or_default();
                let count = conversation.messages.len();
                if let Err(error) = writer.append_summary_state(
                    count,
                    count,
                    inherited_marker,
                    &conversation.summary,
                ) {
                    eprintln!("orca: warning: history write failed: {error}");
                    return None;
                }
            }
            Some(writer)
        }
        Err(error) => {
            eprintln!("orca: warning: failed to initialize history: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::task_types::TaskStatus;
    use tempfile::tempdir;

    use super::*;
    use crate::history;

    fn config(cwd: PathBuf, history_mode: HistoryMode) -> RunConfig {
        RunConfig {
            app_version: "test".to_string(),
            prompt: String::new(),
            cwd: Some(cwd),
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).expect("model"),
            model_runtime: ModelRuntimeConfig::default(),
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    fn with_orca_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = history::TEST_ENV_LOCK.lock().expect("env lock");
        let home = tempdir().expect("temp home");
        let previous = std::env::var_os("ORCA_HOME");
        unsafe {
            std::env::set_var("ORCA_HOME", home.path());
        }
        let result = f(home.path());
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("ORCA_HOME", previous);
            } else {
                std::env::remove_var("ORCA_HOME");
            }
        }
        result
    }

    #[test]
    fn interactive_session_records_initial_conversation_and_backtracks_user() {
        with_orca_home(|home| {
            let cfg = config(home.to_path_buf(), HistoryMode::Record);
            let mut session = InteractiveSession::new_with_preloaded(&cfg, "first prompt", None)
                .expect("session");

            assert!(session.session_id().is_some());
            assert_eq!(session.conversation().messages.len(), 1);

            session
                .conversation_mut()
                .add_user("first prompt".to_string());
            let last = session
                .conversation()
                .messages
                .last()
                .cloned()
                .expect("user message");
            session.append_message(&last);

            assert_eq!(
                session.backtrack_last_user(),
                Some("first prompt".to_string())
            );
            assert_eq!(session.conversation().messages.len(), 1);
        });
    }

    #[test]
    fn interactive_session_resume_replays_preloaded_transcript() {
        with_orca_home(|home| {
            let mut writer =
                history::SessionWriter::start(home, "mock", Some("auto".to_string()), "resume")
                    .expect("writer");
            writer
                .append_message(&orca_core::conversation::Message::User {
                    content: "previous".to_string(),
                    pinned: false,
                })
                .expect("message");
            writer.complete("success").expect("complete");
            let transcript = history::load_session("latest").expect("transcript");
            let cfg = config(
                home.to_path_buf(),
                HistoryMode::Resume(transcript.meta.session_id.clone()),
            );

            let session =
                InteractiveSession::new_with_preloaded(&cfg, "resumed prompt", Some(transcript))
                    .expect("session");

            assert!(
                session
                    .conversation()
                    .messages
                    .iter()
                    .any(|message| matches!(
                        message,
                        orca_core::conversation::Message::User { content, .. }
                            if content == "previous"
                    ))
            );
        });
    }

    #[test]
    fn interactive_session_reports_active_workflows_from_runtime_registry() {
        with_orca_home(|home| {
            let cfg = config(home.to_path_buf(), HistoryMode::Record);
            let session =
                InteractiveSession::new_with_preloaded(&cfg, "workflow", None).expect("session");

            assert!(!session.has_active_workflows());
            assert!(session.store().list_sessions(10).is_ok());
            let handle = session.task_registry().create_workflow(
                "run-1".to_string(),
                "demo".to_string(),
                "demo workflow".to_string(),
                1,
            );
            session
                .task_registry()
                .mark_running(&handle.id)
                .expect("running");

            assert!(session.has_active_workflows());
            assert_eq!(
                session
                    .task_registry()
                    .get(&handle.id)
                    .expect("task")
                    .status,
                TaskStatus::Running
            );
        });
    }
}
