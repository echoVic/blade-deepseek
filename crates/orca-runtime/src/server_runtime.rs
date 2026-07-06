use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Arc;

use orca_core::cancel::CancelToken;
use orca_core::event_schema::RunStatus;
use orca_core::{
    approval_rules::{PermissionRule, PermissionRules},
    approval_types::{ApprovalMode, Decision},
};
use serde_json::Value;

use crate::controller::ThreadTurnRequest;
use crate::lifecycle::{RuntimePermissionRequestHandler, RuntimeTaskKind, ThreadSteerHandle};
use crate::protocol;
use crate::runtime_event_projector::RuntimeEventProjector;
use crate::thread::RuntimeThread;
use crate::thread_store::{
    SessionStore, StoredThreadItem, StoredThreadProjection, StoredThreadTurn, ThreadMetadataPatch,
    ThreadStore, TurnItemsView,
};
pub use orca_core::config::{
    ActivePermissionProfile, AdditionalWorkingDirectory, PermissionProfileNetworkAccess,
};
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
    thread: RuntimeThread,
    title: String,
    cwd: String,
    runtime_workspace_roots: Vec<std::path::PathBuf>,
    active_permission_profile: Option<ActivePermissionProfile>,
    additional_working_directories: Vec<AdditionalWorkingDirectory>,
    network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
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
        let thread = RuntimeThread::start(run_config, "")?;
        Ok(Self {
            thread,
            title: "(empty prompt)".to_string(),
            runtime_workspace_roots: run_config
                .runtime_workspace_roots
                .clone()
                .unwrap_or_else(|| vec![std::path::PathBuf::from(&cwd)]),
            cwd,
            active_permission_profile: run_config.active_permission_profile.clone(),
            additional_working_directories: run_config.additional_working_directories.clone(),
            network_domain_permissions: HashMap::new(),
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
        let network_domain_permissions = transcript.meta.network_domain_permissions.clone();
        let thread = RuntimeThread::resume_same_thread(run_config, transcript)?;
        Ok(Self {
            thread,
            title: "(resumed prompt)".to_string(),
            runtime_workspace_roots: run_config
                .runtime_workspace_roots
                .clone()
                .unwrap_or_else(|| vec![std::path::PathBuf::from(&cwd)]),
            cwd,
            active_permission_profile: run_config.active_permission_profile.clone(),
            additional_working_directories: run_config.additional_working_directories.clone(),
            network_domain_permissions,
        })
    }

    pub fn thread_id(&self) -> &str {
        self.thread.thread_id()
    }

    pub fn active_task_id(&self) -> Option<String> {
        self.thread
            .lifecycle()
            .active_task()
            .map(|task| task.id().to_string())
    }

    pub fn next_persisted_turn_id(&self) -> String {
        crate::thread_store::next_turn_id_for_messages(
            self.thread.thread_id(),
            &self.thread.session().conversation().messages,
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
        let status = self.thread.run_request(&run_config, &request, writer)?;
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
        self.thread
            .run_request_with_cancel(&run_config, &request, writer, cancel)
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
        persist_permission_profile(&run_config, self.thread.thread_id())?;
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
            persist_permission_profile(&run_config, self.thread.thread_id())?;
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
        self.thread
            .run_request_with_cancel(&run_config, &request, writer, cancel)
    }

    fn start_persisted_turn_task(&mut self) {
        let turn_id = self.next_persisted_turn_id();
        self.thread
            .lifecycle_mut()
            .start_task_with_id(RuntimeTaskKind::Agent, turn_id);
    }

    pub fn read_projection(
        &self,
        include_messages: bool,
        include_turns: bool,
    ) -> StoredThreadProjection {
        let messages = if include_messages {
            self.thread
                .session()
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
                self.thread.thread_id(),
                &self.thread.session().conversation().messages,
                usize::MAX,
                TurnItemsView::Full,
            )
        } else {
            Vec::new()
        };
        StoredThreadProjection {
            thread_id: self.thread.thread_id().to_string(),
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            runtime_workspace_roots: self.runtime_workspace_roots.clone(),
            active_permission_profile: self.active_permission_profile.clone(),
            additional_working_directories: self.additional_working_directories.clone(),
            network_domain_permissions: self.network_domain_permissions.clone(),
            message_count: self.thread.session().conversation().messages.len(),
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
                self.thread.thread_id(),
                &self.thread.session().conversation().messages,
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
                self.thread.thread_id(),
                &self.thread.session().conversation().messages,
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
        if let Some(network_domain_permissions) = patch.network_domain_permissions {
            self.network_domain_permissions = network_domain_permissions;
        }
    }

    pub fn task_registry(&self) -> crate::tasks::TaskRegistry {
        self.thread.session().task_registry().clone()
    }

    pub fn additional_working_directories(&self) -> &[AdditionalWorkingDirectory] {
        &self.additional_working_directories
    }

    pub fn network_domain_permissions(&self) -> &HashMap<String, PermissionProfileNetworkAccess> {
        &self.network_domain_permissions
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
            network_domain_permissions: None,
        },
    )?;
    Ok(())
}

pub struct ServerRequestWriter<W: Write> {
    id: Value,
    inner: W,
    buffer: Vec<u8>,
    projector: RuntimeEventProjector,
}

impl<W: Write> ServerRequestWriter<W> {
    pub fn new(id: Value, inner: W) -> Self {
        Self {
            id,
            inner,
            buffer: Vec::new(),
            projector: RuntimeEventProjector::default(),
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
        for event in self.projector.project_line(line) {
            protocol::write_server_event(&mut self.inner, &self.id, event)?;
        }
        Ok(())
    }
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
