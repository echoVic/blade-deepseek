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
use crate::runtime_host::HostedOperationWriter;
use crate::server::surface_adapter::{
    JsonlInteractionTransport, JsonlSurfaceAdapter, JsonlTransportTurn, PreparedJsonlTurn,
};
use crate::thread_store::{
    SortDirection, StoredThreadItem, StoredThreadItemPage, StoredThreadProjection,
    StoredThreadSearchPage, StoredThreadSummaryPage, StoredThreadTurn, StoredThreadTurnPage,
    ThreadListFilters, ThreadMetadataPatch, ThreadSortKey, TurnItemsView,
};
pub use orca_core::config::{
    ActivePermissionProfile, AdditionalWorkingDirectory, PermissionProfileNetworkAccess,
};
use orca_core::config::{HistoryMode, OutputFormat, RunConfig};
use orca_core::thread_identity::TurnId;
use orca_mcp::McpRegistry;

pub struct ServerThreadRuntime {
    adapter: JsonlSurfaceAdapter,
    transport_turns: Vec<JsonlTransportTurn>,
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

pub(crate) struct ServerThreadSubmissionContext {
    pub(crate) cwd: String,
    pub(crate) runtime_workspace_roots: Vec<std::path::PathBuf>,
    pub(crate) mcp_registry: McpRegistry,
}

pub struct ServerThreadView {
    cwd: String,
    runtime_workspace_roots: Vec<std::path::PathBuf>,
    active_permission_profile: Option<ActivePermissionProfile>,
    additional_working_directories: Vec<AdditionalWorkingDirectory>,
    network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
    mcp_registry: McpRegistry,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerThreadTurn {
    prompt: String,
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

impl ServerThreadView {
    pub fn additional_working_directories(&self) -> &[AdditionalWorkingDirectory] {
        &self.additional_working_directories
    }

    pub fn active_permission_profile(&self) -> Option<&ActivePermissionProfile> {
        self.active_permission_profile.as_ref()
    }

    pub fn runtime_workspace_roots(&self) -> &[std::path::PathBuf] {
        &self.runtime_workspace_roots
    }

    pub fn network_domain_permissions(&self) -> &HashMap<String, PermissionProfileNetworkAccess> {
        &self.network_domain_permissions
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn mcp_registry(&self) -> &McpRegistry {
        &self.mcp_registry
    }
}

pub(crate) struct PreparedServerTurn {
    inner: PreparedJsonlTurn,
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

impl HostedOperationWriter for SharedTurnOutput {
    fn finish_generation(&mut self, _commit_terminal: bool) -> io::Result<()> {
        self.flush()
    }
}

impl PreparedServerTurn {
    pub(crate) fn thread_id(&self) -> &str {
        self.inner.thread_id()
    }

    pub(crate) fn turn_id(&self) -> &TurnId {
        self.inner.turn_id()
    }

    pub(crate) fn start_with_output<W>(self, writer: W) -> io::Result<JsonlTransportTurn>
    where
        W: HostedOperationWriter + Send + 'static,
    {
        self.inner.start(writer)
    }
}

impl ServerThreadRuntime {
    pub fn start() -> io::Result<Self> {
        Ok(Self {
            adapter: JsonlSurfaceAdapter::start()?,
            transport_turns: Vec::new(),
        })
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        let result = self.adapter.shutdown();
        for turn in &mut self.transport_turns {
            let _ = turn.wait_terminal();
        }
        self.transport_turns.clear();
        result
    }

    pub fn start_thread(&mut self, config: &RunConfig) -> io::Result<String> {
        self.adapter.start_thread(config)
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
        self.adapter.resume_thread(config, thread_id, permissions)
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
        self.adapter.fork_thread(config, thread_id, permissions)
    }

    pub fn has_thread(&self, thread_id: &str) -> bool {
        self.adapter.has_thread(thread_id)
    }

    pub fn task_registry(&self, thread_id: &str) -> Option<crate::tasks::TaskRegistry> {
        self.adapter.task_registry(thread_id)
    }

    pub fn additional_working_directories(
        &self,
        thread_id: &str,
    ) -> Option<Vec<std::path::PathBuf>> {
        self.adapter
            .read_session(thread_id, false, false)
            .ok()
            .map(|thread| {
                thread
                    .additional_working_directories
                    .into_iter()
                    .map(|directory| directory.path)
                    .collect()
            })
    }

    pub fn active_permission_profile(&self, thread_id: &str) -> Option<ActivePermissionProfile> {
        self.adapter
            .read_session(thread_id, false, false)
            .ok()
            .and_then(|thread| thread.active_permission_profile)
    }

    #[allow(dead_code)]
    pub(crate) fn jsonl_surface(
        &self,
        thread_id: &str,
    ) -> Option<crate::surface::RuntimeSurfaceHandle> {
        self.adapter.jsonl_surface(thread_id)
    }

    pub fn thread(&self, thread_id: &str) -> Option<ServerThreadView> {
        let thread = self.adapter.read_session(thread_id, false, false).ok()?;
        Some(ServerThreadView {
            cwd: thread.cwd,
            runtime_workspace_roots: thread.runtime_workspace_roots,
            active_permission_profile: thread.active_permission_profile,
            additional_working_directories: thread.additional_working_directories,
            network_domain_permissions: thread.network_domain_permissions,
            mcp_registry: self.adapter.mcp_registry(thread_id)?,
        })
    }

    pub fn run_turn<W: Write>(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        prompt: &str,
        writer: W,
    ) -> io::Result<()> {
        self.run_turn_with_permissions(
            config,
            thread_id,
            prompt,
            PermissionProfileOverride::default(),
            writer,
        )
    }

    pub fn run_turn_with_permissions<W: Write>(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        prompt: &str,
        permissions: PermissionProfileOverride,
        writer: W,
    ) -> io::Result<()> {
        let prepared = self.prepare_turn(
            config,
            thread_id,
            prompt,
            permissions,
            &Value::from("synchronous-jsonl-turn"),
        )?;
        let output = SharedTurnOutput::default();
        let mut operation = prepared.start_with_output(output.clone())?;
        operation.wait_terminal()?;
        let mut writer = writer;
        writer.write_all(&output.bytes())?;
        Ok(())
    }

    pub fn read_thread(
        &self,
        thread_id: &str,
        include_messages: bool,
        include_turns: bool,
    ) -> Option<StoredThreadProjection> {
        self.adapter
            .read_session(thread_id, include_messages, include_turns)
            .ok()
    }

    pub fn list_thread_turns(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: crate::thread_store::SortDirection,
        items_view: TurnItemsView,
    ) -> Option<StoredThreadTurnPage> {
        self.adapter
            .list_turns(thread_id, cursor, limit, sort_direction, items_view)
            .ok()
    }

    pub fn list_thread_items(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: crate::thread_store::SortDirection,
    ) -> Option<StoredThreadItemPage> {
        self.adapter
            .list_items(thread_id, turn_id, cursor, limit, sort_direction)
            .ok()
    }

    pub fn update_thread_metadata(&mut self, thread_id: &str, patch: ThreadMetadataPatch) -> bool {
        self.adapter.update_metadata(thread_id, patch).is_ok()
    }

    pub fn has_completed_turn(&self, turn_id: &str) -> bool {
        self.completed_turn_thread_id(turn_id).is_some()
    }

    pub fn completed_turn_thread_id(&self, turn_id: &str) -> Option<String> {
        self.adapter
            .list_sessions(
                None,
                usize::MAX,
                ThreadListFilters::active(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                None,
            )
            .ok()?
            .data
            .into_iter()
            .find_map(|thread| {
                self.adapter
                    .list_turns(
                        &thread.thread_id,
                        None,
                        usize::MAX,
                        SortDirection::Asc,
                        TurnItemsView::Full,
                    )
                    .ok()?
                    .data
                    .into_iter()
                    .any(|turn| turn.turn_id == turn_id)
                    .then_some(thread.thread_id)
            })
    }

    pub(crate) fn submission_context(
        &self,
        thread_id: &str,
        permissions: &PermissionProfileOverride,
    ) -> Option<ServerThreadSubmissionContext> {
        let thread = self.adapter.read_session(thread_id, false, false).ok()?;
        Some(ServerThreadSubmissionContext {
            cwd: thread.cwd,
            runtime_workspace_roots: permissions
                .runtime_workspace_roots
                .clone()
                .unwrap_or(thread.runtime_workspace_roots),
            mcp_registry: self.adapter.mcp_registry(thread_id)?,
        })
    }

    pub(crate) fn prepare_turn(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        prompt: &str,
        permissions: PermissionProfileOverride,
        rpc_id: &Value,
    ) -> io::Result<PreparedServerTurn> {
        self.adapter
            .prepare_turn(config, thread_id, prompt, permissions, rpc_id, None)
            .map(|inner| PreparedServerTurn { inner })
    }

    pub(crate) fn prepare_turn_with_interactions(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        prompt: &str,
        permissions: PermissionProfileOverride,
        rpc_id: &Value,
        interactions: JsonlInteractionTransport,
    ) -> io::Result<PreparedServerTurn> {
        self.adapter
            .prepare_turn(
                config,
                thread_id,
                prompt,
                permissions,
                rpc_id,
                Some(interactions),
            )
            .map(|inner| PreparedServerTurn { inner })
    }

    pub(crate) fn register_transport_turn(&mut self, turn: JsonlTransportTurn) {
        self.transport_turns.push(turn);
    }

    pub(crate) fn accepts_generation(
        &self,
        turn_id: &str,
        thread_id: &str,
        _generation: crate::runtime_host::GenerationFence,
    ) -> bool {
        self.adapter.accepts_turn(thread_id, turn_id)
    }

    pub(crate) fn resolve_turn_thread_id(&self, turn_id: &str) -> Option<String> {
        self.adapter.resolve_turn_thread_id(turn_id)
    }

    pub(crate) fn resolve_known_turn_thread_id(&self, turn_id: &str) -> Option<String> {
        self.adapter.resolve_known_turn_thread_id(turn_id)
    }

    pub(crate) fn prune_finished_turns(&mut self) {
        let mut pending = Vec::with_capacity(self.transport_turns.len());
        for mut turn in self.transport_turns.drain(..) {
            if turn.is_finished() {
                let _ = turn.wait_terminal();
            } else {
                pending.push(turn);
            }
        }
        self.transport_turns = pending;
    }

    #[cfg(test)]
    pub(crate) fn wait_active_turns(&mut self) {
        for turn in &mut self.transport_turns {
            let _ = turn.wait_terminal();
        }
        self.transport_turns.clear();
    }

    pub(crate) fn control_turn(
        &mut self,
        thread_id: Option<&str>,
        turn_id: &str,
        action: crate::unstable_surface::JsonlTurnControlAction,
    ) -> io::Result<crate::unstable_surface::JsonlTurnControlResult> {
        self.adapter.control_turn(thread_id, turn_id, action, None)
    }

    pub fn list_threads(
        &self,
        cursor: Option<&str>,
        limit: usize,
        filters: ThreadListFilters,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        search_term: Option<&str>,
    ) -> io::Result<StoredThreadSummaryPage> {
        self.adapter.list_sessions(
            cursor,
            limit,
            filters,
            sort_key,
            sort_direction,
            search_term,
        )
    }

    pub fn search_threads(
        &self,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
        include_archived: bool,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadSearchPage> {
        self.adapter.search_sessions(
            query,
            cursor,
            limit,
            include_archived,
            sort_key,
            sort_direction,
        )
    }

    pub fn read_thread_result(
        &self,
        thread_id: &str,
        include_messages: bool,
        include_turns: bool,
    ) -> io::Result<StoredThreadProjection> {
        self.adapter
            .read_session(thread_id, include_messages, include_turns)
    }

    pub fn list_thread_turns_result(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
    ) -> io::Result<StoredThreadTurnPage> {
        self.adapter
            .list_turns(thread_id, cursor, limit, sort_direction, items_view)
    }

    pub fn list_thread_items_result(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadItemPage> {
        self.adapter
            .list_items(thread_id, turn_id, cursor, limit, sort_direction)
    }

    pub fn update_thread_metadata_result(
        &self,
        thread_id: &str,
        patch: ThreadMetadataPatch,
    ) -> io::Result<()> {
        self.adapter.update_metadata(thread_id, patch)
    }

    pub(crate) fn persist_session_permission_grant(
        &self,
        thread_id: &str,
        client: &crate::unstable_surface::RuntimeSurfaceClientHandle,
        runtime_workspace_roots: &[std::path::PathBuf],
        permissions: &crate::protocol::RequestPermissionProfile,
    ) -> io::Result<()> {
        self.adapter.persist_session_permission_grant(
            thread_id,
            client,
            runtime_workspace_roots,
            permissions,
        )
    }
}

pub(crate) fn apply_permission_override(
    config: &mut RunConfig,
    permissions: PermissionProfileOverride,
) {
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
