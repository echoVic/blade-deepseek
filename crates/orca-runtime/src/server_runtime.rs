use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Arc;

use orca_core::cancel::CancelToken;
use orca_core::event_schema::RunStatus;
use orca_core::{
    approval_rules::{PermissionRule, PermissionRules},
    approval_types::{ApprovalMode, Decision},
};
use serde_json::{Value, json};

use crate::agent_loop::ThreadSteerHandle;
use crate::controller::{ThreadTurnExecutor, ThreadTurnRequest};
use crate::lifecycle::{RuntimePermissionRequestHandler, RuntimeSessionLifecycle, RuntimeTaskKind};
use crate::protocol;
use crate::session::{InteractiveSession, new_run_id};
use crate::thread_store::{
    SessionStore, StoredThreadItem, StoredThreadProjection, StoredThreadTurn, ThreadMetadataPatch,
    ThreadStore, TurnItemsView,
};
pub use orca_core::config::{ActivePermissionProfile, AdditionalWorkingDirectory};
use orca_core::config::{HistoryMode, OutputFormat, RunConfig};

#[derive(Default)]
pub struct ServerThreadRuntime {
    threads: HashMap<String, ServerThread>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PermissionProfileOverride {
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub approval_mode: Option<ApprovalMode>,
    pub runtime_workspace_roots: Option<Vec<std::path::PathBuf>>,
    pub permission_rules: Option<PermissionRules>,
    pub permission_updates: Vec<PermissionUpdate>,
}

impl PermissionProfileOverride {
    pub fn is_empty(&self) -> bool {
        self.active_permission_profile.is_none()
            && self.approval_mode.is_none()
            && self.runtime_workspace_roots.is_none()
            && self.permission_rules.is_none()
            && self.permission_updates.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionUpdate {
    AddRules {
        destination: String,
        behavior: Decision,
        rules: Vec<PermissionRuleValue>,
    },
    ReplaceRules {
        destination: String,
        behavior: Decision,
        rules: Vec<PermissionRuleValue>,
    },
    RemoveRules {
        destination: String,
        behavior: Decision,
        rules: Vec<PermissionRuleValue>,
    },
    SetMode {
        destination: String,
        mode: ApprovalMode,
    },
    AddDirectories {
        directories: Vec<AdditionalWorkingDirectory>,
    },
    RemoveDirectories {
        destination: String,
        directories: Vec<std::path::PathBuf>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionRuleValue {
    pub tool: String,
    pub pattern: Option<String>,
}

impl PermissionRuleValue {
    pub fn new(tool: impl Into<String>, pattern: Option<impl Into<String>>) -> Self {
        Self {
            tool: tool.into(),
            pattern: pattern.map(Into::into),
        }
    }

    fn into_rule(self, behavior: Decision) -> PermissionRule {
        PermissionRule::new(
            self.tool,
            self.pattern.unwrap_or_else(|| "*".to_string()),
            behavior,
        )
    }

    fn matches_rule(&self, rule: &PermissionRule, behavior: Decision) -> bool {
        rule.decision == behavior
            && rule.tool == self.tool
            && self
                .pattern
                .as_deref()
                .map(|pattern| pattern == rule.pattern)
                .unwrap_or(true)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerThreadTurn {
    prompt: String,
}

pub struct ServerThread {
    thread_id: String,
    session: InteractiveSession,
    lifecycle: RuntimeSessionLifecycle,
    title: String,
    cwd: String,
    runtime_workspace_roots: Vec<std::path::PathBuf>,
    active_permission_profile: Option<ActivePermissionProfile>,
    additional_working_directories: Vec<AdditionalWorkingDirectory>,
}

impl ServerThreadTurn {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
        }
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }
}

impl ServerThread {
    pub fn start(config: &RunConfig) -> io::Result<Self> {
        let run_config = thread_run_config(config);
        Self::start_with_config(&run_config)
    }

    fn start_with_config(run_config: &RunConfig) -> io::Result<Self> {
        let cwd = run_config
            .cwd
            .clone()
            .unwrap_or(std::env::current_dir()?)
            .display()
            .to_string();
        let session = InteractiveSession::new_with_preloaded(&run_config, "", None)?;
        let thread_id = session
            .session_id()
            .map(ToString::to_string)
            .unwrap_or_else(new_run_id);
        let mut lifecycle = RuntimeSessionLifecycle::new(thread_id.clone());
        lifecycle.start_task(RuntimeTaskKind::Agent);
        Ok(Self {
            thread_id,
            session,
            lifecycle,
            title: "(empty prompt)".to_string(),
            runtime_workspace_roots: run_config
                .runtime_workspace_roots
                .clone()
                .unwrap_or_else(|| vec![std::path::PathBuf::from(&cwd)]),
            cwd,
            active_permission_profile: run_config.active_permission_profile.clone(),
            additional_working_directories: run_config.additional_working_directories.clone(),
        })
    }

    fn resume_same_thread(run_config: &RunConfig, thread_id: &str) -> io::Result<Self> {
        let cwd = run_config
            .cwd
            .clone()
            .unwrap_or(std::env::current_dir()?)
            .display()
            .to_string();
        let transcript = SessionStore::new().load_session(thread_id)?;
        let session = InteractiveSession::resume_same_thread(run_config, transcript)?;
        let thread_id = session
            .session_id()
            .map(ToString::to_string)
            .unwrap_or_else(|| thread_id.to_string());
        let mut lifecycle = RuntimeSessionLifecycle::new(thread_id.clone());
        lifecycle.start_task(RuntimeTaskKind::Agent);
        Ok(Self {
            thread_id,
            session,
            lifecycle,
            title: "(resumed prompt)".to_string(),
            runtime_workspace_roots: run_config
                .runtime_workspace_roots
                .clone()
                .unwrap_or_else(|| vec![std::path::PathBuf::from(&cwd)]),
            cwd,
            active_permission_profile: run_config.active_permission_profile.clone(),
            additional_working_directories: run_config.additional_working_directories.clone(),
        })
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn active_task_id(&self) -> Option<String> {
        self.lifecycle
            .active_task()
            .map(|task| task.id().to_string())
    }

    pub fn next_persisted_turn_id(&self) -> String {
        crate::history::next_turn_id_for_messages(
            &self.thread_id,
            &self.session.conversation().messages,
        )
    }

    pub fn run_turn<W: Write>(
        &mut self,
        config: &RunConfig,
        prompt: &str,
        writer: W,
    ) -> io::Result<()> {
        self.run_turn_request(config, &ServerThreadTurn::new(prompt), writer)
    }

    pub fn run_turn_request<W: Write>(
        &mut self,
        config: &RunConfig,
        turn: &ServerThreadTurn,
        writer: W,
    ) -> io::Result<()> {
        let mut run_config = thread_run_config(config);
        run_config.prompt = turn.prompt().to_string();
        run_config.additional_working_directories = self.additional_working_directories.clone();
        if run_config.runtime_workspace_roots.is_none() {
            run_config.runtime_workspace_roots = Some(self.runtime_workspace_roots.clone());
        }
        self.active_permission_profile = run_config.active_permission_profile.clone();
        self.runtime_workspace_roots = run_config
            .runtime_workspace_roots
            .clone()
            .unwrap_or_else(|| vec![std::path::PathBuf::from(&self.cwd)]);
        self.start_persisted_turn_task();

        let request =
            ThreadTurnRequest::new(turn.prompt()).with_wait_for_background_workflows(false);
        let status = ThreadTurnExecutor::new(&run_config, &mut self.session, &mut self.lifecycle)
            .run_request(&request, writer)?;
        let _ = status;
        Ok(())
    }

    pub fn run_turn_with_cancel<W: Write>(
        &mut self,
        config: &RunConfig,
        prompt: &str,
        writer: W,
        cancel: CancelToken,
        steer_handle: ThreadSteerHandle,
    ) -> io::Result<RunStatus> {
        let mut run_config = thread_run_config(config);
        run_config.prompt = prompt.to_string();
        run_config.additional_working_directories = self.additional_working_directories.clone();
        if run_config.runtime_workspace_roots.is_none() {
            run_config.runtime_workspace_roots = Some(self.runtime_workspace_roots.clone());
        }
        self.active_permission_profile = run_config.active_permission_profile.clone();
        self.runtime_workspace_roots = run_config
            .runtime_workspace_roots
            .clone()
            .unwrap_or_else(|| vec![std::path::PathBuf::from(&self.cwd)]);
        self.start_persisted_turn_task();
        let request = ThreadTurnRequest::new(prompt)
            .with_wait_for_background_workflows(false)
            .with_steer_handle(steer_handle);
        ThreadTurnExecutor::new(&run_config, &mut self.session, &mut self.lifecycle)
            .run_request_with_cancel(&request, writer, cancel)
    }

    pub fn run_turn_with_permissions_and_cancel<W: Write>(
        &mut self,
        config: &RunConfig,
        prompt: &str,
        permissions: PermissionProfileOverride,
        writer: W,
        cancel: CancelToken,
        steer_handle: ThreadSteerHandle,
    ) -> io::Result<RunStatus> {
        if permissions.is_empty() {
            return self.run_turn_with_cancel(config, prompt, writer, cancel, steer_handle);
        }
        let mut run_config = config.clone();
        apply_permission_override(&mut run_config, permissions);
        persist_permission_profile(&run_config, &self.thread_id)?;
        self.active_permission_profile = run_config.active_permission_profile.clone();
        self.runtime_workspace_roots = run_config
            .runtime_workspace_roots
            .clone()
            .unwrap_or_else(|| vec![std::path::PathBuf::from(&self.cwd)]);
        self.run_turn_with_cancel(&run_config, prompt, writer, cancel, steer_handle)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run_turn_with_permissions_cancel_and_permission_handler<W: Write>(
        &mut self,
        config: &RunConfig,
        prompt: &str,
        permissions: PermissionProfileOverride,
        writer: W,
        cancel: CancelToken,
        steer_handle: ThreadSteerHandle,
        permission_handler: Arc<dyn RuntimePermissionRequestHandler + Send + Sync>,
    ) -> io::Result<RunStatus> {
        let mut run_config = thread_run_config(config);
        run_config.prompt = prompt.to_string();
        run_config.additional_working_directories = self.additional_working_directories.clone();
        if run_config.runtime_workspace_roots.is_none() {
            run_config.runtime_workspace_roots = Some(self.runtime_workspace_roots.clone());
        }
        if !permissions.is_empty() {
            apply_permission_override(&mut run_config, permissions);
            persist_permission_profile(&run_config, &self.thread_id)?;
        }
        self.active_permission_profile = run_config.active_permission_profile.clone();
        self.runtime_workspace_roots = run_config
            .runtime_workspace_roots
            .clone()
            .unwrap_or_else(|| vec![std::path::PathBuf::from(&self.cwd)]);
        self.additional_working_directories = run_config.additional_working_directories.clone();
        self.start_persisted_turn_task();
        let request = ThreadTurnRequest::new(prompt)
            .with_wait_for_background_workflows(false)
            .with_steer_handle(steer_handle)
            .with_permission_handler(permission_handler);
        ThreadTurnExecutor::new(&run_config, &mut self.session, &mut self.lifecycle)
            .run_request_with_cancel(&request, writer, cancel)
    }

    fn start_persisted_turn_task(&mut self) {
        let turn_id = self.next_persisted_turn_id();
        self.lifecycle
            .start_task_with_id(RuntimeTaskKind::Agent, turn_id);
    }

    pub fn read_projection(
        &self,
        include_messages: bool,
        include_turns: bool,
    ) -> StoredThreadProjection {
        let messages = if include_messages {
            self.session
                .conversation()
                .messages
                .iter()
                .map(crate::thread_store::message_to_thread_json)
                .collect()
        } else {
            Vec::new()
        };
        let turns = if include_turns {
            crate::thread_store::messages_to_thread_turns(
                &self.thread_id,
                &self.session.conversation().messages,
                usize::MAX,
                TurnItemsView::Full,
            )
        } else {
            Vec::new()
        };
        StoredThreadProjection {
            thread_id: self.thread_id.clone(),
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            runtime_workspace_roots: self.runtime_workspace_roots.clone(),
            active_permission_profile: self.active_permission_profile.clone(),
            additional_working_directories: self.additional_working_directories.clone(),
            message_count: self.session.conversation().messages.len(),
            messages,
            turns,
        }
    }

    pub fn list_turns(
        &self,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: crate::thread_store::SortDirection,
        items_view: TurnItemsView,
    ) -> crate::thread_store::StoredThreadTurnPage {
        crate::thread_store::page_thread_turns(
            crate::thread_store::messages_to_thread_turns(
                &self.thread_id,
                &self.session.conversation().messages,
                usize::MAX,
                items_view,
            ),
            cursor,
            limit,
            sort_direction,
        )
    }

    pub fn list_items(
        &self,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: crate::thread_store::SortDirection,
    ) -> crate::thread_store::StoredThreadItemPage {
        crate::thread_store::page_thread_items(
            crate::thread_store::messages_to_thread_items(
                &self.thread_id,
                &self.session.conversation().messages,
                turn_id,
                usize::MAX,
            ),
            cursor,
            limit,
            sort_direction,
        )
    }

    pub fn update_metadata(&mut self, patch: ThreadMetadataPatch) {
        if let Some(title) = patch.title {
            self.title = title;
        }
        if let Some(active_permission_profile) = patch.active_permission_profile {
            self.active_permission_profile = Some(active_permission_profile);
        }
        if let Some(runtime_workspace_roots) = patch.runtime_workspace_roots {
            self.runtime_workspace_roots = runtime_workspace_roots;
        }
        if let Some(additional_working_directories) = patch.additional_working_directories {
            self.additional_working_directories = additional_working_directories;
        }
    }

    pub fn task_registry(&self) -> crate::tasks::TaskRegistry {
        self.session.task_registry().clone()
    }

    pub fn additional_working_directories(&self) -> &[AdditionalWorkingDirectory] {
        &self.additional_working_directories
    }

    pub fn runtime_workspace_roots(&self) -> &[std::path::PathBuf] {
        &self.runtime_workspace_roots
    }

    pub fn active_permission_profile(&self) -> Option<&ActivePermissionProfile> {
        self.active_permission_profile.as_ref()
    }
}

impl ServerThreadRuntime {
    pub fn start_thread(&mut self, config: &RunConfig) -> io::Result<String> {
        let thread = ServerThread::start(config)?;
        let thread_id = thread.thread_id().to_string();
        self.threads.insert(thread_id.clone(), thread);
        Ok(thread_id)
    }

    pub fn resume_thread(&mut self, config: &RunConfig, thread_id: &str) -> io::Result<String> {
        self.resume_thread_with_permissions(config, thread_id, PermissionProfileOverride::default())
    }

    pub fn resume_thread_with_permissions(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        permissions: PermissionProfileOverride,
    ) -> io::Result<String> {
        let mut run_config = config.clone();
        run_config.output_format = OutputFormat::Jsonl;
        run_config.history_mode = HistoryMode::Resume(thread_id.to_string());
        run_config.show_session_picker = false;
        run_config.desktop_notifications = false;
        merge_stored_permission_profile(&mut run_config, thread_id)?;
        apply_permission_override(&mut run_config, permissions);
        persist_permission_profile(&run_config, thread_id)?;
        let thread = ServerThread::resume_same_thread(&run_config, thread_id)?;
        let resumed_thread_id = thread.thread_id().to_string();
        self.threads.insert(resumed_thread_id.clone(), thread);
        Ok(resumed_thread_id)
    }

    pub fn fork_thread(&mut self, config: &RunConfig, thread_id: &str) -> io::Result<String> {
        self.fork_thread_with_permissions(config, thread_id, PermissionProfileOverride::default())
    }

    pub fn fork_thread_with_permissions(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        permissions: PermissionProfileOverride,
    ) -> io::Result<String> {
        let mut run_config = config.clone();
        run_config.output_format = OutputFormat::Jsonl;
        run_config.history_mode = HistoryMode::Fork(thread_id.to_string());
        run_config.show_session_picker = false;
        run_config.desktop_notifications = false;
        merge_stored_permission_profile(&mut run_config, thread_id)?;
        apply_permission_override(&mut run_config, permissions);
        let thread = ServerThread::start_with_config(&run_config)?;
        let forked_thread_id = thread.thread_id().to_string();
        self.threads.insert(forked_thread_id.clone(), thread);
        Ok(forked_thread_id)
    }

    pub fn has_thread(&self, thread_id: &str) -> bool {
        self.threads.contains_key(thread_id)
    }

    pub fn task_registry(&self, thread_id: &str) -> Option<crate::tasks::TaskRegistry> {
        self.threads.get(thread_id).map(ServerThread::task_registry)
    }

    pub fn additional_working_directories(
        &self,
        thread_id: &str,
    ) -> Option<Vec<std::path::PathBuf>> {
        self.threads.get(thread_id).map(|thread| {
            thread
                .additional_working_directories()
                .iter()
                .map(|directory| directory.path.clone())
                .collect()
        })
    }

    pub fn active_permission_profile(&self, thread_id: &str) -> Option<ActivePermissionProfile> {
        self.threads
            .get(thread_id)
            .and_then(|thread| thread.active_permission_profile.clone())
    }

    pub fn thread(&self, thread_id: &str) -> Option<&ServerThread> {
        self.threads.get(thread_id)
    }

    pub fn take_thread(&mut self, thread_id: &str) -> Option<ServerThread> {
        self.threads.remove(thread_id)
    }

    pub fn put_thread(&mut self, thread: ServerThread) {
        self.threads.insert(thread.thread_id().to_string(), thread);
    }

    pub fn run_turn<W: Write>(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        prompt: &str,
        writer: W,
    ) -> io::Result<()> {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown thread: {thread_id}"),
            ));
        };
        thread.run_turn(config, prompt, writer)
    }

    pub fn run_turn_with_permissions<W: Write>(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        prompt: &str,
        permissions: PermissionProfileOverride,
        writer: W,
    ) -> io::Result<()> {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown thread: {thread_id}"),
            ));
        };
        if permissions.is_empty() {
            return thread.run_turn(config, prompt, writer);
        }
        let mut run_config = config.clone();
        apply_permission_override(&mut run_config, permissions);
        persist_permission_profile(&run_config, thread_id)?;
        thread.run_turn(&run_config, prompt, writer)
    }

    pub fn read_thread(
        &self,
        thread_id: &str,
        include_messages: bool,
        include_turns: bool,
    ) -> Option<StoredThreadProjection> {
        let thread = self.threads.get(thread_id)?;
        Some(thread.read_projection(include_messages, include_turns))
    }

    pub fn list_thread_turns(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: crate::thread_store::SortDirection,
        items_view: TurnItemsView,
    ) -> Option<crate::thread_store::StoredThreadTurnPage> {
        let thread = self.threads.get(thread_id)?;
        Some(thread.list_turns(cursor, limit, sort_direction, items_view))
    }

    pub fn list_thread_items(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: crate::thread_store::SortDirection,
    ) -> Option<crate::thread_store::StoredThreadItemPage> {
        let thread = self.threads.get(thread_id)?;
        Some(thread.list_items(turn_id, cursor, limit, sort_direction))
    }

    pub fn update_thread_metadata(&mut self, thread_id: &str, patch: ThreadMetadataPatch) -> bool {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return false;
        };
        thread.update_metadata(patch);
        true
    }

    pub fn has_completed_turn(&self, turn_id: &str) -> bool {
        self.completed_turn_thread_id(turn_id).is_some()
    }

    pub fn completed_turn_thread_id(&self, turn_id: &str) -> Option<String> {
        self.threads.values().find_map(|thread| {
            thread
                .list_turns(
                    None,
                    usize::MAX,
                    crate::thread_store::SortDirection::Asc,
                    TurnItemsView::Full,
                )
                .data
                .into_iter()
                .find(|turn| turn.turn_id == turn_id)
                .map(|turn| turn.thread_id)
        })
    }

    pub fn next_persisted_turn_id(&self, thread_id: &str) -> Option<String> {
        self.threads
            .get(thread_id)
            .map(ServerThread::next_persisted_turn_id)
    }
}

fn merge_stored_permission_profile(config: &mut RunConfig, thread_id: &str) -> io::Result<()> {
    let transcript = SessionStore::new().load_session(thread_id)?;
    if let Some(approval_mode) = transcript.meta.approval_mode {
        config.approval_mode = approval_mode;
    }
    if !transcript.meta.runtime_workspace_roots.is_empty() {
        config.runtime_workspace_roots = Some(transcript.meta.runtime_workspace_roots);
    }
    if let Some(active_permission_profile) = transcript.meta.active_permission_profile {
        config.active_permission_profile = Some(active_permission_profile);
    }
    if !transcript.meta.permission_rules.rules.is_empty() {
        config.permission_rules = transcript.meta.permission_rules;
    }
    Ok(())
}

fn apply_permission_override(config: &mut RunConfig, permissions: PermissionProfileOverride) {
    if let Some(active_permission_profile) = permissions.active_permission_profile {
        config.active_permission_profile = Some(active_permission_profile);
    }
    if let Some(approval_mode) = permissions.approval_mode {
        config.approval_mode = approval_mode;
    }
    if let Some(runtime_workspace_roots) = permissions.runtime_workspace_roots {
        config.runtime_workspace_roots = Some(runtime_workspace_roots);
    }
    if let Some(permission_rules) = permissions.permission_rules {
        config.permission_rules = permission_rules;
    }
    apply_permission_updates(config, permissions.permission_updates);
}

fn apply_permission_updates(config: &mut RunConfig, updates: Vec<PermissionUpdate>) {
    for update in updates {
        match update {
            PermissionUpdate::SetMode { mode, .. } => {
                config.approval_mode = mode;
            }
            PermissionUpdate::AddRules {
                behavior, rules, ..
            } => {
                config
                    .permission_rules
                    .rules
                    .extend(rules.into_iter().map(|rule| rule.into_rule(behavior)));
            }
            PermissionUpdate::ReplaceRules {
                behavior, rules, ..
            } => {
                config
                    .permission_rules
                    .rules
                    .retain(|rule| rule.decision != behavior);
                config
                    .permission_rules
                    .rules
                    .extend(rules.into_iter().map(|rule| rule.into_rule(behavior)));
            }
            PermissionUpdate::RemoveRules {
                behavior, rules, ..
            } => {
                config.permission_rules.rules.retain(|rule| {
                    !rules
                        .iter()
                        .any(|remove| remove.matches_rule(rule, behavior))
                });
            }
            PermissionUpdate::AddDirectories { directories } => {
                for directory in directories {
                    if let Some(existing) = config
                        .additional_working_directories
                        .iter()
                        .find(|existing| existing.path == directory.path)
                    {
                        let mut existing = existing.clone();
                        existing.source = directory.source;
                        if let Some(slot) = config
                            .additional_working_directories
                            .iter_mut()
                            .find(|slot| slot.path == existing.path)
                        {
                            *slot = existing;
                        }
                    } else {
                        config.additional_working_directories.push(directory);
                    }
                }
            }
            PermissionUpdate::RemoveDirectories {
                destination,
                directories,
            } => {
                config.additional_working_directories.retain(|directory| {
                    directory.source != destination
                        || !directories.iter().any(|remove| remove == &directory.path)
                });
            }
        }
    }
}

fn persist_permission_profile(config: &RunConfig, thread_id: &str) -> io::Result<()> {
    SessionStore::new().update_thread_metadata(
        thread_id,
        ThreadMetadataPatch {
            title: None,
            active_permission_profile: config.active_permission_profile.clone(),
            approval_mode: Some(config.approval_mode),
            runtime_workspace_roots: config.runtime_workspace_roots.clone(),
            permission_rules: Some(config.permission_rules.clone()),
            additional_working_directories: Some(config.additional_working_directories.clone()),
        },
    )?;
    Ok(())
}

pub struct ServerRequestWriter<W: Write> {
    id: Value,
    inner: W,
    buffer: Vec<u8>,
    agent_message: Option<AgentMessageItem>,
    plan: Option<PlanItem>,
    plan_parser: ProposedPlanStreamParser,
    reasoning: Option<ReasoningItem>,
    tool_items: HashMap<String, ToolCallItem>,
    file_change_items: HashMap<String, FileChangeItem>,
    workflow_items: HashMap<String, WorkflowItem>,
}

const PROPOSED_PLAN_OPEN: &str = "<proposed_plan>";
const PROPOSED_PLAN_CLOSE: &str = "</proposed_plan>";

#[derive(Clone, Debug, Default)]
struct ProposedPlanStreamParser {
    buffer: String,
    in_plan: bool,
    plan_buffer: String,
    drop_leading_plan_newline: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProposedPlanSegment {
    Agent(String),
    Plan(String),
}

#[derive(Clone, Debug)]
struct AgentMessageItem {
    id: String,
    text: String,
}

#[derive(Clone, Debug)]
struct PlanItem {
    id: String,
    text: String,
}

#[derive(Clone, Debug)]
struct ReasoningItem {
    id: String,
    summary: String,
}

#[derive(Clone, Debug)]
struct ToolCallItem {
    id: String,
    tool: String,
    command: Option<String>,
}

#[derive(Clone, Debug)]
struct FileChangeItem {
    id: String,
    path: Option<String>,
    kind: String,
}

#[derive(Clone, Debug)]
struct WorkflowItem {
    id: String,
    task_id: String,
    workflow_name: String,
    task: Value,
    status: String,
    result: Value,
}

impl<W: Write> ServerRequestWriter<W> {
    pub fn new(id: Value, inner: W) -> Self {
        Self {
            id,
            inner,
            buffer: Vec::new(),
            agent_message: None,
            plan: None,
            plan_parser: ProposedPlanStreamParser::default(),
            reasoning: None,
            tool_items: HashMap::new(),
            file_change_items: HashMap::new(),
            workflow_items: HashMap::new(),
        }
    }

    pub fn flush_remaining(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&self.buffer).to_string();
            self.buffer.clear();
            self.write_runtime_line(&line)?;
        }
        Ok(())
    }

    fn write_runtime_line(&mut self, line: &str) -> io::Result<()> {
        let runtime_event: Value = serde_json::from_str(line).unwrap_or(Value::Null);
        let event_type = runtime_event["type"].as_str().unwrap_or_default();
        if event_type == "assistant.message.delta" {
            let delta = runtime_event["payload"]["text"]
                .as_str()
                .unwrap_or_default();
            self.write_assistant_message_delta(delta)?;
        }
        if event_type == "assistant.reasoning.delta" {
            let delta = runtime_event["payload"]["text"]
                .as_str()
                .unwrap_or_default();
            if self.reasoning.is_none() {
                self.reasoning = Some(ReasoningItem {
                    id: "item-reasoning-1".to_string(),
                    summary: String::new(),
                });
                protocol::write_server_event(
                    &mut self.inner,
                    &self.id,
                    protocol::ServerEvent::ItemStarted {
                        thread_id: Value::Null,
                        turn_id: Value::Null,
                        item: json!({
                            "id": "item-reasoning-1",
                            "type": "reasoning",
                            "summary": "",
                            "content": "",
                        }),
                    },
                )?;
            }
            if let Some(item) = &mut self.reasoning {
                item.summary.push_str(delta);
                protocol::write_server_event(
                    &mut self.inner,
                    &self.id,
                    protocol::ServerEvent::ItemReasoningDelta {
                        item_id: Value::from(item.id.clone()),
                        delta: Value::from(delta.to_string()),
                    },
                )?;
            }
        }
        if event_type == "tool.call.requested" {
            self.write_tool_item_started(&runtime_event)?;
            self.write_file_change_item_started(&runtime_event)?;
        }
        if event_type == "workflow.started" {
            self.write_workflow_item_started(&runtime_event)?;
        }
        if let Some(event) = protocol::map_runtime_event_line(line) {
            protocol::write_server_event(&mut self.inner, &self.id, event)?;
        }
        if event_type == "tool.call.completed" {
            self.write_tool_item_completed(&runtime_event)?;
            self.write_file_change_item_completed(&runtime_event)?;
        }
        if event_type == "workflow.completed" {
            self.record_workflow_completed(&runtime_event);
        }
        if event_type == "workflow.result.available" {
            self.record_workflow_result(&runtime_event);
            self.write_workflow_item_completed(&runtime_event, "completed")?;
        }
        if event_type == "workflow.failed" {
            self.write_workflow_item_completed(&runtime_event, "failed")?;
        }
        if event_type == "session.completed" {
            self.flush_assistant_message_parser()?;
        }
        if event_type == "session.completed"
            && let Some(item) = self.agent_message.take()
        {
            protocol::write_server_event(
                &mut self.inner,
                &self.id,
                protocol::ServerEvent::ItemCompleted {
                    thread_id: Value::Null,
                    turn_id: Value::Null,
                    item: serde_json::json!({
                        "id": item.id,
                        "type": "agent_message",
                        "text": item.text,
                    }),
                },
            )?;
        }
        if event_type == "session.completed"
            && let Some(item) = self.plan.take()
        {
            protocol::write_server_event(
                &mut self.inner,
                &self.id,
                protocol::ServerEvent::ItemCompleted {
                    thread_id: Value::Null,
                    turn_id: Value::Null,
                    item: json!({
                        "id": item.id,
                        "type": "plan",
                        "text": item.text,
                    }),
                },
            )?;
        }
        if event_type == "session.completed"
            && let Some(item) = self.reasoning.take()
        {
            protocol::write_server_event(
                &mut self.inner,
                &self.id,
                protocol::ServerEvent::ItemCompleted {
                    thread_id: Value::Null,
                    turn_id: Value::Null,
                    item: json!({
                        "id": item.id,
                        "type": "reasoning",
                        "summary": item.summary,
                        "content": "",
                    }),
                },
            )?;
        }
        Ok(())
    }

    fn write_assistant_message_delta(&mut self, delta: &str) -> io::Result<()> {
        for segment in self.plan_parser.push(delta) {
            match segment {
                ProposedPlanSegment::Agent(text) => self.write_agent_message_delta(&text)?,
                ProposedPlanSegment::Plan(text) => self.write_plan_delta(&text)?,
            }
        }
        Ok(())
    }

    fn flush_assistant_message_parser(&mut self) -> io::Result<()> {
        for segment in self.plan_parser.finish() {
            match segment {
                ProposedPlanSegment::Agent(text) => self.write_agent_message_delta(&text)?,
                ProposedPlanSegment::Plan(text) => self.write_plan_delta(&text)?,
            }
        }
        Ok(())
    }

    fn write_agent_message_delta(&mut self, delta: &str) -> io::Result<()> {
        if delta.is_empty() {
            return Ok(());
        }
        if self.agent_message.is_none() {
            self.agent_message = Some(AgentMessageItem {
                id: "item-agent-message-1".to_string(),
                text: String::new(),
            });
            protocol::write_server_event(
                &mut self.inner,
                &self.id,
                protocol::ServerEvent::ItemStarted {
                    thread_id: Value::Null,
                    turn_id: Value::Null,
                    item: json!({
                        "id": "item-agent-message-1",
                        "type": "agent_message",
                        "text": "",
                    }),
                },
            )?;
        }
        if let Some(item) = &mut self.agent_message {
            item.text.push_str(delta);
            protocol::write_server_event(
                &mut self.inner,
                &self.id,
                protocol::ServerEvent::ItemMessageDelta {
                    item_id: Value::from(item.id.clone()),
                    delta: Value::from(delta.to_string()),
                },
            )?;
        }
        Ok(())
    }

    fn write_plan_delta(&mut self, delta: &str) -> io::Result<()> {
        if delta.is_empty() {
            return Ok(());
        }
        if self.plan.is_none() {
            self.plan = Some(PlanItem {
                id: "item-plan-1".to_string(),
                text: String::new(),
            });
            protocol::write_server_event(
                &mut self.inner,
                &self.id,
                protocol::ServerEvent::ItemStarted {
                    thread_id: Value::Null,
                    turn_id: Value::Null,
                    item: json!({
                        "id": "item-plan-1",
                        "type": "plan",
                        "text": "",
                    }),
                },
            )?;
        }
        if let Some(item) = &mut self.plan {
            item.text.push_str(delta);
            protocol::write_server_event(
                &mut self.inner,
                &self.id,
                protocol::ServerEvent::ItemPlanDelta {
                    item_id: Value::from(item.id.clone()),
                    delta: Value::from(delta.to_string()),
                },
            )?;
        }
        Ok(())
    }

    fn write_tool_item_started(&mut self, runtime_event: &Value) -> io::Result<()> {
        let payload = &runtime_event["payload"];
        let tool_id = payload["id"].as_str().unwrap_or("tool-call").to_string();
        let tool = payload["name"].as_str().unwrap_or_default().to_string();
        if let Some((server, local_tool)) = mcp_tool_parts(&tool) {
            let item = ToolCallItem {
                id: tool_id.clone(),
                tool: tool.clone(),
                command: None,
            };
            self.tool_items.insert(tool_id.clone(), item);
            return protocol::write_server_event(
                &mut self.inner,
                &self.id,
                protocol::ServerEvent::ItemStarted {
                    thread_id: Value::Null,
                    turn_id: Value::Null,
                    item: json!({
                        "id": tool_id,
                        "type": "mcpToolCall",
                        "server": server,
                        "tool": local_tool,
                        "status": "in_progress",
                        "arguments": mcp_tool_arguments(payload),
                        "result": Value::Null,
                        "error": Value::Null,
                    }),
                },
            );
        }
        if is_dynamic_tool(&tool) {
            let item = ToolCallItem {
                id: tool_id.clone(),
                tool: tool.clone(),
                command: None,
            };
            self.tool_items.insert(tool_id.clone(), item);
            return protocol::write_server_event(
                &mut self.inner,
                &self.id,
                protocol::ServerEvent::ItemStarted {
                    thread_id: Value::Null,
                    turn_id: Value::Null,
                    item: json!({
                        "id": tool_id,
                        "type": "dynamicToolCall",
                        "namespace": Value::Null,
                        "tool": tool,
                        "status": "in_progress",
                        "arguments": tool_arguments(payload),
                        "contentItems": Value::Null,
                        "success": Value::Null,
                        "error": Value::Null,
                    }),
                },
            );
        }
        let command = payload["target"].as_str().map(ToString::to_string);
        let item = ToolCallItem {
            id: tool_id.clone(),
            tool: tool.clone(),
            command: command.clone(),
        };
        self.tool_items.insert(tool_id.clone(), item);
        protocol::write_server_event(
            &mut self.inner,
            &self.id,
            protocol::ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: json!({
                    "id": tool_id,
                    "type": "commandExecution",
                    "tool": tool,
                    "command": command,
                    "status": "in_progress",
                }),
            },
        )
    }

    fn write_tool_item_completed(&mut self, runtime_event: &Value) -> io::Result<()> {
        let payload = &runtime_event["payload"];
        let tool_id = payload["id"].as_str().unwrap_or("tool-call").to_string();
        let item = self.tool_items.remove(&tool_id).unwrap_or(ToolCallItem {
            id: tool_id,
            tool: payload["name"].as_str().unwrap_or_default().to_string(),
            command: None,
        });
        if let Some((server, local_tool)) = mcp_tool_parts(&item.tool) {
            return protocol::write_server_event(
                &mut self.inner,
                &self.id,
                protocol::ServerEvent::ItemCompleted {
                    thread_id: Value::Null,
                    turn_id: Value::Null,
                    item: json!({
                        "id": item.id,
                        "type": "mcpToolCall",
                        "server": server,
                        "tool": local_tool,
                        "status": payload["status"].clone(),
                        "arguments": mcp_tool_arguments(payload),
                        "result": mcp_tool_result(payload),
                        "error": mcp_tool_error(payload),
                    }),
                },
            );
        }
        if is_dynamic_tool(&item.tool) {
            return protocol::write_server_event(
                &mut self.inner,
                &self.id,
                protocol::ServerEvent::ItemCompleted {
                    thread_id: Value::Null,
                    turn_id: Value::Null,
                    item: json!({
                        "id": item.id,
                        "type": "dynamicToolCall",
                        "namespace": Value::Null,
                        "tool": item.tool,
                        "status": payload["status"].clone(),
                        "arguments": tool_arguments(payload),
                        "contentItems": dynamic_tool_content_items(payload),
                        "success": payload["status"].as_str() == Some("completed"),
                        "error": dynamic_tool_error(payload),
                    }),
                },
            );
        }
        protocol::write_server_event(
            &mut self.inner,
            &self.id,
            protocol::ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: json!({
                    "id": item.id,
                    "type": "commandExecution",
                    "tool": item.tool,
                    "command": item.command,
                    "status": payload["status"].clone(),
                    "aggregatedOutput": payload["output"].clone(),
                    "error": payload["error"].clone(),
                    "exitCode": payload["exit_code"].clone(),
                    "truncated": payload["truncated"].clone(),
                }),
            },
        )
    }

    fn write_file_change_item_started(&mut self, runtime_event: &Value) -> io::Result<()> {
        let payload = &runtime_event["payload"];
        let tool = payload["name"].as_str().unwrap_or_default().to_string();
        let Some(kind) = file_change_kind(&tool) else {
            return Ok(());
        };
        let tool_id = payload["id"].as_str().unwrap_or("tool-call").to_string();
        let target = payload["target"].as_str();
        let path = file_change_path(&tool, target);
        let item = FileChangeItem {
            id: format!("{tool_id}:file-change"),
            path: path.clone(),
            kind: kind.to_string(),
        };
        self.file_change_items.insert(tool_id, item.clone());
        protocol::write_server_event(
            &mut self.inner,
            &self.id,
            protocol::ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: json!({
                    "id": item.id,
                    "type": "fileChange",
                    "status": "inProgress",
                    "changes": [{
                        "path": item.path,
                        "kind": item.kind,
                        "diff": file_change_diff(payload),
                    }],
                }),
            },
        )
    }

    fn write_file_change_item_completed(&mut self, runtime_event: &Value) -> io::Result<()> {
        let payload = &runtime_event["payload"];
        let tool_id = payload["id"].as_str().unwrap_or("tool-call").to_string();
        let Some(item) = self.file_change_items.remove(&tool_id) else {
            return Ok(());
        };
        protocol::write_server_event(
            &mut self.inner,
            &self.id,
            protocol::ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: json!({
                    "id": item.id,
                    "type": "fileChange",
                    "status": file_change_status(payload),
                    "changes": [{
                        "path": item.path,
                        "kind": item.kind,
                        "diff": file_change_diff(payload),
                    }],
                }),
            },
        )
    }

    fn write_workflow_item_started(&mut self, runtime_event: &Value) -> io::Result<()> {
        let payload = &runtime_event["payload"];
        let run_id = payload["runId"]
            .as_str()
            .unwrap_or("workflow-run")
            .to_string();
        let task_id = payload["taskId"].as_str().unwrap_or_default().to_string();
        let workflow_name = payload["workflowName"]
            .as_str()
            .unwrap_or("workflow")
            .to_string();
        let item = WorkflowItem {
            id: run_id.clone(),
            task_id: task_id.clone(),
            workflow_name: workflow_name.clone(),
            task: payload["task"].clone(),
            status: "running".to_string(),
            result: Value::Null,
        };
        self.workflow_items.insert(run_id.clone(), item);
        protocol::write_server_event(
            &mut self.inner,
            &self.id,
            protocol::ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: json!({
                    "id": run_id,
                    "type": "workflow",
                    "workflowName": workflow_name,
                    "taskId": task_id,
                    "status": "running",
                    "task": payload["task"].clone(),
                }),
            },
        )
    }

    fn record_workflow_result(&mut self, runtime_event: &Value) {
        let payload = &runtime_event["payload"];
        let run_id = payload["runId"].as_str().unwrap_or("workflow-run");
        if let Some(item) = self.workflow_items.get_mut(run_id) {
            item.result = payload["result"].clone();
            item.task = payload["task"].clone();
            item.status = "completed".to_string();
        }
    }

    fn record_workflow_completed(&mut self, runtime_event: &Value) {
        let payload = &runtime_event["payload"];
        let run_id = payload["runId"].as_str().unwrap_or("workflow-run");
        if let Some(item) = self.workflow_items.get_mut(run_id) {
            item.task = payload["task"].clone();
            item.status = "completed".to_string();
        }
    }

    fn write_workflow_item_completed(
        &mut self,
        runtime_event: &Value,
        status: &str,
    ) -> io::Result<()> {
        let payload = &runtime_event["payload"];
        let run_id = payload["runId"]
            .as_str()
            .unwrap_or("workflow-run")
            .to_string();
        let fallback = WorkflowItem {
            id: run_id,
            task_id: payload["taskId"].as_str().unwrap_or_default().to_string(),
            workflow_name: payload["workflowName"]
                .as_str()
                .unwrap_or("workflow")
                .to_string(),
            task: payload["task"].clone(),
            status: status.to_string(),
            result: Value::Null,
        };
        let mut item = self.workflow_items.remove(&fallback.id).unwrap_or(fallback);
        if item.task.is_null() {
            item.task = payload["task"].clone();
        }
        item.status = status.to_string();
        protocol::write_server_event(
            &mut self.inner,
            &self.id,
            protocol::ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: json!({
                    "id": item.id,
                    "type": "workflow",
                    "workflowName": item.workflow_name,
                    "taskId": item.task_id,
                    "status": item.status,
                    "result": item.result,
                    "error": payload["error"].clone(),
                    "task": item.task,
                }),
            },
        )
    }
}

impl ProposedPlanStreamParser {
    fn push(&mut self, delta: &str) -> Vec<ProposedPlanSegment> {
        self.buffer.push_str(delta);
        self.drain(false)
    }

    fn finish(&mut self) -> Vec<ProposedPlanSegment> {
        self.drain(true)
    }

    fn drain(&mut self, finish: bool) -> Vec<ProposedPlanSegment> {
        let mut out = Vec::new();
        loop {
            if self.in_plan {
                if let Some(end) = self.buffer.find(PROPOSED_PLAN_CLOSE) {
                    let plan_and_close: String = self
                        .buffer
                        .drain(..end + PROPOSED_PLAN_CLOSE.len())
                        .collect();
                    self.plan_buffer.push_str(&plan_and_close[..end]);
                    let text = self.normalize_plan_text();
                    if !text.is_empty() {
                        out.push(ProposedPlanSegment::Plan(text));
                    }
                    self.in_plan = false;
                    self.drop_leading_plan_newline = false;
                    continue;
                }
                if finish {
                    let text = format!("{PROPOSED_PLAN_OPEN}{}{}", self.plan_buffer, self.buffer);
                    self.plan_buffer.clear();
                    self.buffer.clear();
                    self.in_plan = false;
                    self.drop_leading_plan_newline = false;
                    if !text.is_empty() {
                        out.push(ProposedPlanSegment::Agent(text));
                    }
                } else if !self.buffer.is_empty() {
                    self.plan_buffer.push_str(&self.buffer);
                    self.buffer.clear();
                }
                break;
            }

            if let Some(start) = self.buffer.find(PROPOSED_PLAN_OPEN) {
                if start > 0 {
                    out.push(ProposedPlanSegment::Agent(self.buffer[..start].to_string()));
                }
                self.buffer.drain(..start + PROPOSED_PLAN_OPEN.len());
                self.in_plan = true;
                self.drop_leading_plan_newline = true;
                continue;
            }
            let keep = if finish {
                0
            } else {
                pending_open_tag_prefix_len(&self.buffer)
            };
            if self.buffer.len() > keep {
                let take = self.buffer.len() - keep;
                out.push(ProposedPlanSegment::Agent(
                    self.buffer.drain(..take).collect(),
                ));
            }
            break;
        }
        out
    }

    fn normalize_plan_text(&mut self) -> String {
        let mut text = std::mem::take(&mut self.plan_buffer);
        if self.drop_leading_plan_newline {
            if let Some(stripped) = text.strip_prefix('\n') {
                text = stripped.to_string();
            }
            self.drop_leading_plan_newline = false;
        }
        text
    }
}

fn pending_open_tag_prefix_len(text: &str) -> usize {
    let max = text.len().min(PROPOSED_PLAN_OPEN.len().saturating_sub(1));
    (1..=max)
        .rev()
        .find(|&len| PROPOSED_PLAN_OPEN.starts_with(&text[text.len() - len..]))
        .unwrap_or(0)
}

fn file_change_kind(tool: &str) -> Option<&'static str> {
    match tool {
        "edit" => Some("edit"),
        "write_file" => Some("write"),
        _ => None,
    }
}

fn file_change_path(tool: &str, target: Option<&str>) -> Option<String> {
    let target = target?.trim();
    if target.is_empty() {
        return None;
    }
    match tool {
        "edit" => Some(
            target
                .split_once("::")
                .map(|(path, _)| path)
                .unwrap_or(target)
                .trim()
                .to_string(),
        ),
        "write_file" => Some(target.to_string()),
        _ => None,
    }
}

fn file_change_status(payload: &Value) -> Value {
    match payload["status"].as_str() {
        Some("in_progress") => Value::from("inProgress"),
        Some(status) => Value::from(status.to_string()),
        None => Value::Null,
    }
}

fn file_change_diff(_payload: &Value) -> Value {
    Value::from(String::new())
}

fn is_dynamic_tool(tool: &str) -> bool {
    !is_builtin_tool(tool) && mcp_tool_parts(tool).is_none()
}

fn is_builtin_tool(tool: &str) -> bool {
    matches!(
        tool,
        "read_file"
            | "list_files"
            | "glob"
            | "grep"
            | "bash"
            | "edit"
            | "write_file"
            | "git_status"
            | "subagent"
            | "subagent_status"
            | "task_list"
            | "task_stop"
            | "WorkflowDraft"
            | "workflow_draft"
            | "WorkflowDraftAction"
            | "workflow_draft_action"
            | "Workflow"
            | "workflow"
            | "workflow_send_message"
            | "workflow_read_messages"
            | "workflow_clear_messages"
            | "workflow_create_task_list"
            | "workflow_claim_task"
            | "workflow_complete_task"
            | "workflow_list_tasks"
            | "web_search"
            | "get_goal"
            | "create_goal"
            | "update_goal"
            | "update_plan"
            | "request_user_input"
            | "list_skills"
            | "read_skill"
    )
}

fn mcp_tool_parts(tool: &str) -> Option<(String, String)> {
    let rest = tool.strip_prefix("mcp__")?;
    let (server, local_tool) = rest.rsplit_once("__")?;
    Some((server.to_string(), local_tool.to_string()))
}

fn tool_arguments(payload: &Value) -> Value {
    payload["raw_arguments"]
        .as_str()
        .or_else(|| payload["target"].as_str())
        .and_then(|arguments| serde_json::from_str(arguments).ok())
        .unwrap_or(Value::Null)
}

fn mcp_tool_arguments(payload: &Value) -> Value {
    tool_arguments(payload)
}

fn mcp_tool_result(payload: &Value) -> Value {
    if !tool_status_is_completed(payload) || !payload["error"].is_null() {
        return Value::Null;
    }
    let Some(output) = payload["output"].as_str() else {
        return Value::Null;
    };
    match serde_json::from_str::<Value>(output) {
        Ok(value) if value.is_object() => json!({
            "content": value.get("content").cloned().unwrap_or_else(|| {
                json!([{ "type": "text", "text": output }])
            }),
            "structuredContent": value.get("structuredContent").cloned().unwrap_or(Value::Null),
            "_meta": value.get("_meta").cloned().unwrap_or(Value::Null),
        }),
        _ => json!({
            "content": [{ "type": "text", "text": output }],
            "structuredContent": Value::Null,
            "_meta": Value::Null,
        }),
    }
}

fn mcp_tool_error(payload: &Value) -> Value {
    if let Some(error) = payload["error"].as_str() {
        return json!({ "message": error });
    }
    if payload["status"].as_str() == Some("failed") {
        if let Some(output) = payload["output"].as_str() {
            return json!({ "message": output });
        }
        return json!({ "message": "MCP tool call failed" });
    }
    Value::Null
}

fn dynamic_tool_content_items(payload: &Value) -> Value {
    if !tool_status_is_completed(payload) {
        return Value::Null;
    }
    match payload["output"].as_str() {
        Some(output) => json!([{ "type": "text", "text": output }]),
        None => Value::Null,
    }
}

fn dynamic_tool_error(payload: &Value) -> Value {
    match tool_error_detail(payload) {
        Value::String(message) => json!({ "message": message }),
        Value::Null => Value::Null,
        other => other,
    }
}

fn tool_error_detail(payload: &Value) -> Value {
    if let Some(error) = payload["error"].as_str() {
        return Value::from(error.to_string());
    }
    if !tool_status_is_completed(payload) {
        if let Some(output) = payload["output"].as_str() {
            return Value::from(output.to_string());
        }
        return Value::from("tool call failed");
    }
    Value::Null
}

fn tool_status_is_completed(payload: &Value) -> bool {
    payload["status"].as_str() == Some("completed")
}

impl<W: Write> Write for ServerRequestWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.buffer[..pos]).to_string();
            self.buffer.drain(..=pos);
            self.write_runtime_line(&line)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub fn thread_run_config(config: &RunConfig) -> RunConfig {
    let mut run_config = config.clone();
    run_config.output_format = OutputFormat::Jsonl;
    run_config.history_mode = match run_config.history_mode {
        HistoryMode::Record => HistoryMode::Record,
        HistoryMode::Disabled | HistoryMode::Resume(_) | HistoryMode::Fork(_) => {
            HistoryMode::Disabled
        }
    };
    run_config.show_session_picker = false;
    run_config.desktop_notifications = false;
    run_config
}

pub fn thread_turn_to_json(turn: StoredThreadTurn) -> Value {
    serde_json::json!({
        "threadId": turn.thread_id,
        "turnId": turn.turn_id,
        "index": turn.index,
        "role": turn.role,
        "itemsView": turn_items_view_to_json(turn.items_view),
        "items": turn.items,
    })
}

pub fn thread_item_to_json(item: StoredThreadItem) -> Value {
    serde_json::json!({
        "threadId": item.thread_id,
        "turnId": item.turn_id,
        "itemId": item.item_id,
        "index": item.index,
        "item": item.item,
    })
}

fn turn_items_view_to_json(items_view: TurnItemsView) -> &'static str {
    match items_view {
        TurnItemsView::NotLoaded => "notLoaded",
        TurnItemsView::Summary => "summary",
        TurnItemsView::Full => "full",
    }
}
