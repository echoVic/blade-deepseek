use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use orca_core::{
    approval_rules::{PermissionRule, PermissionRules},
    approval_types::{ApprovalMode, Decision},
};
use serde_json::Value;

use crate::protocol;
use crate::runtime_event_projector::RuntimeEventProjector;
use crate::runtime_host::{
    HostedOperationWriter, HostedTurnRequest, OperationHandle, OperationOutcome, RuntimeHost,
    RuntimeHostError, RuntimeThreadHandle,
};
use crate::thread_store::{
    SessionStore, StoredThreadItem, StoredThreadProjection, StoredThreadTurn, ThreadMetadataPatch,
    ThreadStore, TurnItemsView,
};
pub use orca_core::config::{
    ActivePermissionProfile, AdditionalWorkingDirectory, PermissionProfileNetworkAccess,
};
use orca_core::config::{HistoryMode, OutputFormat, RunConfig};
use orca_core::thread_identity::TurnId;
use orca_mcp::McpRegistry;

pub struct ServerThreadRuntime {
    host: Option<RuntimeHost>,
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
    handle: RuntimeThreadHandle,
    title: String,
    cwd: String,
    runtime_workspace_roots: Vec<std::path::PathBuf>,
    active_permission_profile: Option<ActivePermissionProfile>,
    additional_working_directories: Vec<AdditionalWorkingDirectory>,
    network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
}

pub(crate) struct ServerThreadSubmissionContext {
    pub(crate) cwd: String,
    pub(crate) runtime_workspace_roots: Vec<std::path::PathBuf>,
    pub(crate) mcp_registry: McpRegistry,
}

pub(crate) struct PreparedServerTurn {
    thread_id: String,
    turn_id: TurnId,
    config: RunConfig,
    handle: RuntimeThreadHandle,
}

#[derive(Clone, Default)]
struct SharedTurnOutput {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedTurnOutput {
    fn bytes(&self) -> Vec<u8> {
        self.bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Write for SharedTurnOutput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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
    fn from_handle(
        handle: RuntimeThreadHandle,
        run_config: &RunConfig,
        title: impl Into<String>,
        network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
    ) -> io::Result<Self> {
        let cwd = run_config
            .cwd
            .clone()
            .unwrap_or(std::env::current_dir()?)
            .display()
            .to_string();
        Ok(Self {
            handle,
            title: title.into(),
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
        self.handle.thread_id()
    }

    pub fn active_task_id(&self) -> Option<String> {
        self.handle
            .snapshot()
            .ok()
            .and_then(|snapshot| snapshot.active_task_id().map(ToString::to_string))
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
        mut writer: W,
    ) -> io::Result<()> {
        let prepared =
            self.prepare_turn(config, turn.prompt(), PermissionProfileOverride::default())?;
        let output = SharedTurnOutput::default();
        let operation = prepared.start(
            HostedTurnRequest::new(turn.prompt()).with_wait_for_background_workflows(false),
            output.clone(),
        )?;
        let terminal = operation.wait();
        writer.write_all(&output.bytes())?;
        operation_outcome_result(terminal.outcome())
    }

    fn prepare_turn(
        &mut self,
        config: &RunConfig,
        prompt: &str,
        permissions: PermissionProfileOverride,
    ) -> io::Result<PreparedServerTurn> {
        if self.handle.state().map_err(runtime_host_error)?
            != crate::runtime_host::RuntimeThreadState::Idle
        {
            return Err(io::Error::other(format!(
                "thread is not idle: {}",
                self.thread_id()
            )));
        }
        let turn_id = TurnId::new();
        let mut run_config = thread_run_config(config);
        run_config.prompt = prompt.to_string();
        run_config.additional_working_directories = self.additional_working_directories.clone();
        if run_config.runtime_workspace_roots.is_none() {
            run_config.runtime_workspace_roots = Some(self.runtime_workspace_roots.clone());
        }
        if !permissions.is_empty() {
            apply_permission_override(&mut run_config, permissions);
            persist_permission_profile(&run_config, self.thread_id())?;
        }
        self.active_permission_profile = run_config.active_permission_profile.clone();
        self.runtime_workspace_roots = run_config
            .runtime_workspace_roots
            .clone()
            .unwrap_or_else(|| vec![std::path::PathBuf::from(&self.cwd)]);
        self.additional_working_directories = run_config.additional_working_directories.clone();
        Ok(PreparedServerTurn {
            thread_id: self.thread_id().to_string(),
            turn_id,
            config: run_config,
            handle: self.handle.clone(),
        })
    }

    pub fn read_projection(
        &self,
        include_messages: bool,
        include_turns: bool,
    ) -> Option<StoredThreadProjection> {
        let snapshot = self.handle.snapshot().ok()?;
        let messages = if include_messages {
            snapshot
                .messages()
                .iter()
                .map(crate::thread_store::message_to_thread_json)
                .collect()
        } else {
            Vec::new()
        };
        let turns = if include_turns {
            if let Some(records) = snapshot.conversation_records() {
                crate::thread_store::conversation_records_to_thread_turns(
                    self.thread_id(),
                    records,
                    usize::MAX,
                    TurnItemsView::Full,
                )
                .ok()?
            } else {
                crate::thread_store::messages_to_thread_turns(
                    self.thread_id(),
                    snapshot.messages(),
                    usize::MAX,
                    TurnItemsView::Full,
                )
            }
        } else {
            Vec::new()
        };
        Some(StoredThreadProjection {
            thread_id: self.thread_id().to_string(),
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            runtime_workspace_roots: self.runtime_workspace_roots.clone(),
            active_permission_profile: self.active_permission_profile.clone(),
            additional_working_directories: self.additional_working_directories.clone(),
            network_domain_permissions: self.network_domain_permissions.clone(),
            message_count: snapshot.messages().len(),
            messages,
            turns,
        })
    }

    pub fn list_turns(
        &self,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: crate::thread_store::SortDirection,
        items_view: TurnItemsView,
    ) -> Option<crate::thread_store::StoredThreadTurnPage> {
        let snapshot = self.handle.snapshot().ok()?;
        let turns = if let Some(records) = snapshot.conversation_records() {
            crate::thread_store::conversation_records_to_thread_turns(
                self.thread_id(),
                records,
                usize::MAX,
                items_view,
            )
            .ok()?
        } else {
            crate::thread_store::messages_to_thread_turns(
                self.thread_id(),
                snapshot.messages(),
                usize::MAX,
                items_view,
            )
        };
        Some(crate::thread_store::page_thread_turns(
            turns,
            cursor,
            limit,
            sort_direction,
        ))
    }

    pub fn list_items(
        &self,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: crate::thread_store::SortDirection,
    ) -> Option<crate::thread_store::StoredThreadItemPage> {
        let snapshot = self.handle.snapshot().ok()?;
        let items = if let Some(records) = snapshot.conversation_records() {
            crate::thread_store::conversation_records_to_thread_items(
                self.thread_id(),
                records,
                turn_id,
                usize::MAX,
            )
            .ok()?
        } else {
            crate::thread_store::messages_to_thread_items(
                self.thread_id(),
                snapshot.messages(),
                turn_id,
                usize::MAX,
            )
        };
        Some(crate::thread_store::page_thread_items(
            items,
            cursor,
            limit,
            sort_direction,
        ))
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
        self.handle.task_registry()
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

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn mcp_registry(&self) -> McpRegistry {
        self.handle.mcp_registry()
    }

    pub fn active_permission_profile(&self) -> Option<&ActivePermissionProfile> {
        self.active_permission_profile.as_ref()
    }

    fn submission_context(
        &self,
        permissions: &PermissionProfileOverride,
    ) -> ServerThreadSubmissionContext {
        ServerThreadSubmissionContext {
            cwd: self.cwd.clone(),
            runtime_workspace_roots: permissions
                .runtime_workspace_roots
                .clone()
                .unwrap_or_else(|| self.runtime_workspace_roots.clone()),
            mcp_registry: self.handle.mcp_registry(),
        }
    }
}

impl PreparedServerTurn {
    pub(crate) fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub(crate) fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub(crate) fn start<W>(
        self,
        request: HostedTurnRequest,
        writer: W,
    ) -> io::Result<OperationHandle>
    where
        W: Write + Send + 'static,
    {
        self.handle
            .start_turn_with_config(request.with_turn_id(self.turn_id), writer, self.config)
            .map_err(runtime_host_error)
    }

    pub(crate) fn start_with_output<W>(
        self,
        request: HostedTurnRequest,
        writer: W,
    ) -> io::Result<OperationHandle>
    where
        W: HostedOperationWriter,
    {
        self.handle
            .start_turn_with_config_and_output(
                request.with_turn_id(self.turn_id),
                writer,
                self.config,
            )
            .map_err(runtime_host_error)
    }
}

fn operation_outcome_result(outcome: &OperationOutcome) -> io::Result<()> {
    match outcome {
        OperationOutcome::Completed(_) => Ok(()),
        OperationOutcome::Backgrounded { task_id } => Err(io::Error::other(format!(
            "server operation backgrounded unexpectedly as task {task_id}"
        ))),
        OperationOutcome::ExecutionFailed { kind, message } => {
            Err(io::Error::new(*kind, message.clone()))
        }
        OperationOutcome::Panicked { message } => Err(io::Error::other(message.clone())),
    }
}

fn runtime_host_error(error: RuntimeHostError) -> io::Error {
    io::Error::other(error.to_string())
}

impl ServerThreadRuntime {
    pub fn start() -> io::Result<Self> {
        Ok(Self {
            host: Some(RuntimeHost::start().map_err(runtime_host_error)?),
            threads: HashMap::new(),
        })
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        let Some(host) = self.host.take() else {
            return Ok(());
        };
        host.shutdown().map_err(runtime_host_error)
    }

    fn start_record(
        &mut self,
        run_config: RunConfig,
        title: impl Into<String>,
        network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
    ) -> io::Result<String> {
        let handle = self
            .host
            .as_ref()
            .ok_or_else(|| io::Error::other("server runtime host is shut down"))?
            .start_thread(run_config.clone(), "")
            .map_err(runtime_host_error)?;
        let thread =
            ServerThread::from_handle(handle, &run_config, title, network_domain_permissions)?;
        let thread_id = thread.thread_id().to_string();
        self.threads.insert(thread_id.clone(), thread);
        Ok(thread_id)
    }

    pub fn start_thread(&mut self, config: &RunConfig) -> io::Result<String> {
        self.start_record(thread_run_config(config), "(empty prompt)", HashMap::new())
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
        if let Some(thread) = self.threads.get_mut(thread_id) {
            thread.update_metadata(ThreadMetadataPatch {
                title: None,
                active_permission_profile: run_config.active_permission_profile.clone(),
                approval_mode: Some(run_config.approval_mode),
                runtime_workspace_roots: run_config.runtime_workspace_roots.clone(),
                permission_rules: Some(run_config.permission_rules.clone()),
                additional_working_directories: Some(
                    run_config.additional_working_directories.clone(),
                ),
                network_domain_permissions: None,
            });
            return Ok(thread_id.to_string());
        }
        let transcript = SessionStore::new().load_session(thread_id)?;
        self.start_record(
            run_config,
            "(resumed prompt)",
            transcript.meta.network_domain_permissions,
        )
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
        self.start_record(run_config, "(empty prompt)", HashMap::new())
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
        let prepared = thread.prepare_turn(config, prompt, permissions)?;
        let output = SharedTurnOutput::default();
        let operation = prepared.start(
            HostedTurnRequest::new(prompt).with_wait_for_background_workflows(false),
            output.clone(),
        )?;
        let terminal = operation.wait();
        let mut writer = writer;
        writer.write_all(&output.bytes())?;
        operation_outcome_result(terminal.outcome())
    }

    pub fn read_thread(
        &self,
        thread_id: &str,
        include_messages: bool,
        include_turns: bool,
    ) -> Option<StoredThreadProjection> {
        self.threads
            .get(thread_id)?
            .read_projection(include_messages, include_turns)
    }

    pub fn list_thread_turns(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: crate::thread_store::SortDirection,
        items_view: TurnItemsView,
    ) -> Option<crate::thread_store::StoredThreadTurnPage> {
        self.threads
            .get(thread_id)?
            .list_turns(cursor, limit, sort_direction, items_view)
    }

    pub fn list_thread_items(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: crate::thread_store::SortDirection,
    ) -> Option<crate::thread_store::StoredThreadItemPage> {
        self.threads
            .get(thread_id)?
            .list_items(turn_id, cursor, limit, sort_direction)
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
                )?
                .data
                .into_iter()
                .find(|turn| turn.turn_id == turn_id)
                .map(|turn| turn.thread_id)
        })
    }

    pub(crate) fn submission_context(
        &self,
        thread_id: &str,
        permissions: &PermissionProfileOverride,
    ) -> Option<ServerThreadSubmissionContext> {
        self.threads
            .get(thread_id)
            .map(|thread| thread.submission_context(permissions))
    }

    pub(crate) fn prepare_turn(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        prompt: &str,
        permissions: PermissionProfileOverride,
    ) -> io::Result<PreparedServerTurn> {
        let thread = self.threads.get_mut(thread_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown thread: {thread_id}"),
            )
        })?;
        thread.prepare_turn(config, prompt, permissions)
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
