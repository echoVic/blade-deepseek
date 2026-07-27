use std::collections::{BTreeSet, HashMap};
use std::io::{self, Write};

use orca_core::config::{HistoryMode, OutputFormat, RunConfig};
use orca_core::thread_identity::TurnId;
use sha2::{Digest, Sha256};

use super::mcp_elicitation_manager::{
    PendingMcpElicitationManager, PendingSurfaceMcpElicitationRequest,
};
use super::permission_manager::PendingPermissionManager;
use super::user_input_manager::{PendingSurfaceUserInputRequest, PendingUserInputManager};
use crate::runtime_host::{HostedOperationWriter, RuntimeHost, RuntimeHostError};
use crate::server_runtime::{PermissionProfileOverride, apply_permission_override};
use crate::thread_store::{
    SortDirection, StoredThreadItemPage, StoredThreadProjection, StoredThreadSearchPage,
    StoredThreadSummaryPage, StoredThreadTurnPage, ThreadListFilters, ThreadMetadataPatch,
    ThreadSortKey, TurnItemsView,
};
use crate::unstable_surface::{
    AssistantChannel, AssistantPatch, AttachResult, DetachRequest, DisplayText, FreshAttachRequest,
    InteractionPatch, LegacyTurnId, MutationReply, NonEmptyVec, OperationIngressCorrelation,
    OperationKind, OperationPatch, OperationRequestIntent, OperationSettingsPreparation,
    OperationTerminal, ReplayabilityRequest, RuntimeSettingsPatch, RuntimeSurfaceClientHandle,
    RuntimeSurfaceHandle, RuntimeSurfaceHostHandle, RuntimeSurfaceThreadHandle, Sha256Digest,
    SurfaceAdmissionLeaseId, SurfaceAssistantStream, SurfaceAttachmentRole, SurfaceCapability,
    SurfaceCommitBatch, SurfaceEvent, SurfaceInputRequest, SurfaceInputRequestBlock,
    SurfaceInteractionKind, SurfaceInteractionRequest, SurfaceInteractionRoute, SurfaceOperationId,
    SurfaceRequestId, SurfaceScope, SurfaceSubscriptionItem, SurfaceSubscriptionReceiver,
    SurfaceToolRequest, SurfaceToolResultKind, ToolPatch, ToolTerminalSource, UncommittedMutation,
};

#[derive(Clone)]
pub(crate) struct JsonlInteractionTransport {
    permissions: PendingPermissionManager,
    user_inputs: PendingUserInputManager,
    mcp_elicitations: PendingMcpElicitationManager,
}

impl JsonlInteractionTransport {
    pub(super) fn new(
        permissions: PendingPermissionManager,
        user_inputs: PendingUserInputManager,
        mcp_elicitations: PendingMcpElicitationManager,
    ) -> Self {
        Self {
            permissions,
            user_inputs,
            mcp_elicitations,
        }
    }
}

pub(crate) struct JsonlSurfaceAdapter {
    host: Option<RuntimeHost>,
    surface_host: RuntimeSurfaceHostHandle,
    threads: HashMap<String, JsonlThreadBinding>,
}

struct JsonlThreadBinding {
    thread: RuntimeSurfaceThreadHandle,
}

pub(crate) struct PreparedJsonlTurn {
    thread_id: String,
    turn_id: TurnId,
    surface: RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
    operation_id: SurfaceOperationId,
    admission_lease_id: SurfaceAdmissionLeaseId,
    subscription: SurfaceSubscriptionReceiver,
    interactions: Option<JsonlInteractionTransport>,
    runtime_workspace_roots: Vec<std::path::PathBuf>,
}

pub(crate) struct JsonlTransportTurn {
    thread_id: String,
    turn_id: TurnId,
    worker: Option<std::thread::JoinHandle<io::Result<()>>>,
}

impl JsonlSurfaceAdapter {
    pub(crate) fn start() -> io::Result<Self> {
        let host = RuntimeHost::start().map_err(runtime_host_error)?;
        let surface_host = host.surface_handle().bind_new_connection();
        Ok(Self {
            host: Some(host),
            surface_host,
            threads: HashMap::new(),
        })
    }

    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        self.threads.clear();
        let Some(host) = self.host.take() else {
            return Ok(());
        };
        host.shutdown().map_err(runtime_host_error)
    }

    pub(crate) fn connection_id(&self) -> Option<crate::unstable_surface::SurfaceConnectionId> {
        self.surface_host.connection_id().cloned()
    }

    pub(crate) fn start_thread(&mut self, config: &RunConfig) -> io::Result<String> {
        let config = jsonl_thread_config(config);
        self.start_record(config, "(empty prompt)")
    }

    pub(crate) fn resume_thread(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        permissions: PermissionProfileOverride,
    ) -> io::Result<String> {
        if self.threads.contains_key(thread_id) {
            if !permissions.is_empty() {
                self.persist_permission_override(thread_id, config, permissions)?;
            }
            return Ok(thread_id.to_string());
        }
        let mut config = config.clone();
        config.output_format = OutputFormat::Jsonl;
        config.history_mode = HistoryMode::Resume(thread_id.to_string());
        config.show_session_picker = false;
        config.desktop_notifications = false;
        apply_permission_override(&mut config, permissions);
        self.start_record(config, "(resumed prompt)")
    }

    pub(crate) fn fork_thread(
        &mut self,
        config: &RunConfig,
        thread_id: &str,
        permissions: PermissionProfileOverride,
    ) -> io::Result<String> {
        let binding = self.threads.get(thread_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown thread: {thread_id}"),
            )
        })?;
        let surface = binding
            .thread
            .jsonl_surface()
            .ok_or_else(|| io::Error::other("JSONL runtime surface unavailable"))?;
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Jsonl,
            requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => return Err(io::Error::other("JSONL fork source snapshot unavailable")),
        };
        let mut config = config.clone();
        apply_surface_settings_to_run_config(
            &mut config,
            &attachment.baseline.snapshot.settings.effective,
        )?;
        let _ = surface.detach(
            &attachment.client,
            DetachRequest {
                request_id: SurfaceRequestId::new(),
            },
        );
        config.output_format = OutputFormat::Jsonl;
        config.history_mode = HistoryMode::Fork(thread_id.to_string());
        config.show_session_picker = false;
        config.desktop_notifications = false;
        apply_permission_override(&mut config, permissions);
        self.start_record(config, "(empty prompt)")
    }

    fn start_record(&mut self, config: RunConfig, title: &str) -> io::Result<String> {
        let thread = self
            .surface_host
            .start_thread(config, title)
            .map_err(runtime_host_error)?;
        let thread_id = thread.thread_id().to_string();
        self.threads
            .insert(thread_id.clone(), JsonlThreadBinding { thread });
        Ok(thread_id)
    }

    pub(crate) fn has_thread(&self, thread_id: &str) -> bool {
        self.threads.contains_key(thread_id)
    }

    pub(crate) fn task_registry(&self, thread_id: &str) -> Option<crate::tasks::TaskRegistry> {
        self.threads
            .get(thread_id)
            .map(|binding| binding.thread.task_registry())
    }

    pub(crate) fn mcp_registry(&self, thread_id: &str) -> Option<orca_mcp::McpRegistry> {
        self.threads
            .get(thread_id)
            .map(|binding| binding.thread.mcp_registry())
    }

    pub(crate) fn jsonl_surface(&self, thread_id: &str) -> Option<RuntimeSurfaceHandle> {
        self.threads
            .get(thread_id)
            .and_then(|binding| binding.thread.jsonl_surface())
    }

    pub(crate) fn accepts_turn(&self, thread_id: &str, turn_id: &str) -> bool {
        self.thread_has_turn(thread_id, turn_id, true)
    }

    fn thread_has_turn(&self, thread_id: &str, turn_id: &str, active_only: bool) -> bool {
        let Some(surface) = self.jsonl_surface(thread_id) else {
            return false;
        };
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Jsonl,
            requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => return false,
        };
        let snapshot = &attachment.baseline.snapshot;
        let accepts = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .any(|operation| {
                (!active_only || operation.terminal.is_none())
                    && matches!(
                        &operation.intent.origin,
                        crate::unstable_surface::OperationOrigin::JsonlThreadTurn {
                            legacy_turn_id,
                            ..
                        } if legacy_turn_id.0.as_str() == turn_id
                    )
            });
        let _ = surface.detach(
            &attachment.client,
            DetachRequest {
                request_id: SurfaceRequestId::new(),
            },
        );
        accepts
    }

    pub(crate) fn resolve_turn_thread_id(&self, turn_id: &str) -> Option<String> {
        let mut thread_ids = self.threads.keys().cloned().collect::<Vec<_>>();
        thread_ids.sort();
        thread_ids
            .into_iter()
            .find(|thread_id| self.accepts_turn(thread_id, turn_id))
    }

    pub(crate) fn resolve_known_turn_thread_id(&self, turn_id: &str) -> Option<String> {
        let mut thread_ids = self.threads.keys().cloned().collect::<Vec<_>>();
        thread_ids.sort();
        thread_ids
            .into_iter()
            .find(|thread_id| self.thread_has_turn(thread_id, turn_id, false))
    }

    pub(crate) fn list_sessions(
        &self,
        cursor: Option<&str>,
        limit: usize,
        filters: ThreadListFilters,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        search_term: Option<&str>,
    ) -> io::Result<StoredThreadSummaryPage> {
        self.surface_host.jsonl_list_sessions(
            cursor,
            limit,
            filters,
            sort_key,
            sort_direction,
            search_term,
        )
    }

    pub(crate) fn search_sessions(
        &self,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
        include_archived: bool,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadSearchPage> {
        self.surface_host.jsonl_search_sessions(
            query,
            cursor,
            limit,
            include_archived,
            sort_key,
            sort_direction,
        )
    }

    pub(crate) fn read_session(
        &self,
        thread_id: &str,
        include_messages: bool,
        include_turns: bool,
    ) -> io::Result<StoredThreadProjection> {
        if (include_messages || include_turns)
            && let Some(binding) = self.threads.get(thread_id)
        {
            return binding
                .thread
                .jsonl_read_live_projection(include_messages, include_turns)
                .map_err(runtime_host_error);
        }
        self.surface_host
            .jsonl_read_session(thread_id, include_messages, include_turns)
    }

    pub(crate) fn list_turns(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
    ) -> io::Result<StoredThreadTurnPage> {
        if let Some(binding) = self.threads.get(thread_id) {
            return binding
                .thread
                .jsonl_list_live_turns(cursor, limit, sort_direction, items_view)
                .map_err(runtime_host_error);
        }
        self.surface_host
            .jsonl_list_turns(thread_id, cursor, limit, sort_direction, items_view)
    }

    pub(crate) fn list_items(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadItemPage> {
        if let Some(binding) = self.threads.get(thread_id) {
            return binding
                .thread
                .jsonl_list_live_items(turn_id, cursor, limit, sort_direction)
                .map_err(runtime_host_error);
        }
        self.surface_host
            .jsonl_list_items(thread_id, turn_id, cursor, limit, sort_direction)
    }

    pub(crate) fn update_metadata(
        &self,
        thread_id: &str,
        patch: ThreadMetadataPatch,
    ) -> io::Result<()> {
        self.surface_host
            .jsonl_update_session_metadata(thread_id, patch)
    }

    pub(crate) fn control_turn(
        &self,
        thread_id: Option<&str>,
        turn_id: &str,
        action: crate::unstable_surface::JsonlTurnControlAction,
        preferred_client: Option<RuntimeSurfaceClientHandle>,
    ) -> io::Result<crate::unstable_surface::JsonlTurnControlResult> {
        let candidate_ids = match thread_id {
            Some(thread_id) => vec![thread_id.to_string()],
            None => {
                let mut ids = self.threads.keys().cloned().collect::<Vec<_>>();
                ids.sort();
                ids
            }
        };
        let legacy_turn_id = LegacyTurnId(DisplayText::new(turn_id));
        for candidate_id in candidate_ids {
            let Some(binding) = self.threads.get(&candidate_id) else {
                continue;
            };
            let surface = binding
                .thread
                .jsonl_surface()
                .ok_or_else(|| io::Error::other("JSONL runtime surface unavailable"))?;
            let mut transient_attachment = None;
            let client = if let Some(client) = preferred_client
                .as_ref()
                .filter(|client| client.thread_id() == surface.thread_id())
            {
                client.clone()
            } else {
                let attachment = match surface.attach_fresh(FreshAttachRequest {
                    request_id: SurfaceRequestId::new(),
                    role: SurfaceAttachmentRole::Jsonl,
                    requested_capabilities: BTreeSet::from([
                        SurfaceCapability::ReadSnapshot,
                        SurfaceCapability::ControlBoundOperation,
                    ]),
                    interaction_capabilities: BTreeSet::new(),
                }) {
                    AttachResult::FreshAttached { attachment } => attachment,
                    _ => {
                        return Err(io::Error::other("JSONL control surface attach failed"));
                    }
                };
                let client = attachment.client.clone();
                transient_attachment = Some(attachment);
                client
            };
            let result = self
                .surface_host
                .control_jsonl_turn(
                    client,
                    SurfaceRequestId::new(),
                    Some(surface.thread_id().clone()),
                    legacy_turn_id.clone(),
                    action.clone(),
                )
                .map_err(|error| io::Error::other(format!("JSONL turn control failed: {error:?}")));
            if let Some(attachment) = transient_attachment {
                let client = attachment.client;
                let keep_attached = matches!(
                    result.as_ref(),
                    Ok(crate::unstable_surface::JsonlTurnControlResult::Resolved {
                        mutation: MutationReply::Committed { value, .. }
                    }) if value.echo.status
                        == crate::unstable_surface::JsonlResolvedTurnControlStatus::Resumed
                );
                if !keep_attached {
                    let _ = surface.detach(
                        &client,
                        DetachRequest {
                            request_id: SurfaceRequestId::new(),
                        },
                    );
                }
            }
            let result = result?;
            if thread_id.is_some()
                || matches!(
                    result,
                    crate::unstable_surface::JsonlTurnControlResult::Resolved { .. }
                )
            {
                return Ok(result);
            }
        }
        Ok(crate::unstable_surface::JsonlTurnControlResult::Idle {
            request_id: SurfaceRequestId::new(),
            echo: crate::unstable_surface::JsonlIdleTurnControlWireEcho {
                legacy_turn_id,
                action: match action {
                    crate::unstable_surface::JsonlTurnControlAction::Interrupt => {
                        crate::unstable_surface::JsonlTurnControlWireAction::Interrupt
                    }
                    crate::unstable_surface::JsonlTurnControlAction::Resume => {
                        crate::unstable_surface::JsonlTurnControlWireAction::Resume
                    }
                    crate::unstable_surface::JsonlTurnControlAction::Steer { .. } => {
                        crate::unstable_surface::JsonlTurnControlWireAction::Steer
                    }
                },
                status: crate::unstable_surface::JsonlIdleTurnControlStatus::Idle,
                legacy_input: None,
            },
        })
    }

    pub(crate) fn prepare_turn(
        &self,
        config: &RunConfig,
        thread_id: &str,
        prompt: &str,
        permissions: PermissionProfileOverride,
        rpc_id: &serde_json::Value,
        interactions: Option<JsonlInteractionTransport>,
    ) -> io::Result<PreparedJsonlTurn> {
        let binding = self.threads.get(thread_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown thread: {thread_id}"),
            )
        })?;
        let surface = binding
            .thread
            .jsonl_surface()
            .ok_or_else(|| io::Error::other("JSONL runtime surface unavailable"))?;
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Jsonl,
            requested_capabilities: BTreeSet::from([
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::SubmitOperation,
                SurfaceCapability::ControlBoundOperation,
                SurfaceCapability::ManageThreadSettings,
                SurfaceCapability::RespondGrantedInteraction,
            ]),
            interaction_capabilities: BTreeSet::from([
                SurfaceInteractionKind::PermissionRequest,
                SurfaceInteractionKind::UserInput,
                SurfaceInteractionKind::McpElicitation,
            ]),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => return Err(io::Error::other("JSONL runtime surface attach failed")),
        };
        let baseline = &attachment.baseline.snapshot;
        let runtime_workspace_roots =
            permissions
                .runtime_workspace_roots
                .clone()
                .unwrap_or_else(|| {
                    baseline
                        .settings
                        .effective
                        .workspace_roots
                        .iter()
                        .map(|root| root.as_path().to_path_buf())
                        .collect()
                });
        let settings_preparation = settings_preparation(config, permissions, baseline)?;
        let turn_id = TurnId::new();
        let input = SurfaceInputRequest {
            blocks: NonEmptyVec::try_new(vec![SurfaceInputRequestBlock::Text {
                text: DisplayText::new(prompt),
            }])
            .map_err(|error| io::Error::other(error.to_string()))?,
        };
        let rpc_id_digest = Sha256Digest::new(Sha256::digest(rpc_id.to_string().as_bytes()).into());
        let intent = OperationRequestIntent {
            correlation: OperationIngressCorrelation::JsonlThreadTurn {
                rpc_id_digest,
                legacy_turn_id: LegacyTurnId(DisplayText::new(turn_id.to_string())),
            },
            kind: OperationKind::UserTurn,
            input: Some(input),
            replayability: ReplayabilityRequest::CaptureReplayableCapsule,
            settings_preparation,
        };
        let reserved = committed(
            attachment
                .client
                .reserve_operation(SurfaceRequestId::new(), intent),
            "JSONL surface reserve",
        )?;
        let operation_id = reserved.operation_id.clone();
        let admission_lease_id = reserved.lease.lease_id;

        let subscription = surface
            .claim_subscription(&attachment.subscription)
            .ok_or_else(|| io::Error::other("JSONL surface subscription unavailable"))?;

        Ok(PreparedJsonlTurn {
            thread_id: thread_id.to_string(),
            turn_id,
            surface,
            client: attachment.client,
            operation_id,
            admission_lease_id,
            subscription,
            interactions,
            runtime_workspace_roots,
        })
    }

    fn persist_permission_override(
        &self,
        thread_id: &str,
        config: &RunConfig,
        permissions: PermissionProfileOverride,
    ) -> io::Result<()> {
        let binding = self.threads.get(thread_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown thread: {thread_id}"),
            )
        })?;
        let surface = binding
            .thread
            .jsonl_surface()
            .ok_or_else(|| io::Error::other("JSONL runtime surface unavailable"))?;
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Jsonl,
            requested_capabilities: BTreeSet::from([
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::ManageThreadSettings,
            ]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => return Err(io::Error::other("JSONL settings surface attach failed")),
        };
        let preparation = settings_preparation(config, permissions, &attachment.baseline.snapshot)?;
        if let OperationSettingsPreparation::ApplyThreadOverridesBeforeRequested {
            expected_settings_revision,
            patches,
            ..
        } = preparation
        {
            committed(
                attachment.client.update_settings(
                    SurfaceRequestId::new(),
                    expected_settings_revision,
                    patches,
                ),
                "JSONL settings update",
            )?;
        }
        let _ = surface.detach(
            &attachment.client,
            DetachRequest {
                request_id: SurfaceRequestId::new(),
            },
        );
        Ok(())
    }

    pub(crate) fn persist_session_permission_grant(
        &self,
        thread_id: &str,
        client: &RuntimeSurfaceClientHandle,
        runtime_workspace_roots: &[std::path::PathBuf],
        permissions: &crate::protocol::RequestPermissionProfile,
    ) -> io::Result<()> {
        let surface = self
            .jsonl_surface(thread_id)
            .ok_or_else(|| io::Error::other("JSONL runtime surface unavailable"))?;
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Jsonl,
            requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => {
                return Err(io::Error::other(
                    "JSONL permission settings snapshot unavailable",
                ));
            }
        };
        let settings = &attachment.baseline.snapshot.settings;
        let cwd = settings
            .effective
            .cwd
            .as_path()
            .to_str()
            .ok_or_else(|| io::Error::other("JSONL thread cwd is not valid UTF-8"))?;
        let mut directories = settings.effective.additional_working_directories.clone();
        if let Some(file_system) = permissions.file_system.as_ref() {
            for requested in file_system
                .write
                .iter()
                .flatten()
                .chain(file_system.read.iter().flatten())
                .filter(|path| !path.as_os_str().is_empty())
            {
                for path in super::materialize_workspace_roots_paths(
                    cwd,
                    runtime_workspace_roots,
                    requested,
                ) {
                    let path = crate::unstable_surface::CanonicalPath::try_new(path)
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    if !directories.iter().any(|directory| directory.path == path) {
                        directories.push(
                            crate::unstable_surface::SurfaceAdditionalWorkingDirectory {
                                path,
                                source: crate::unstable_surface::NonEmptyText::try_new("session")
                                    .expect("session source is non-empty"),
                            },
                        );
                    }
                }
            }
        }
        let mut network = settings.effective.network_permissions.clone();
        if let Some(requested) = permissions.network.as_ref() {
            if requested.enabled.is_some() {
                network.enabled = requested.enabled;
            }
            for (domain, access) in &requested.domains {
                let domain = crate::unstable_surface::CanonicalDomainName::try_new(domain.clone())
                    .map_err(|error| io::Error::other(error.to_string()))?;
                let access = match access {
                    orca_core::config::PermissionProfileNetworkAccess::Allow => {
                        crate::unstable_surface::SurfaceNetworkDomainAccess::Allow
                    }
                    orca_core::config::PermissionProfileNetworkAccess::Deny => {
                        crate::unstable_surface::SurfaceNetworkDomainAccess::Deny
                    }
                };
                if let Some(existing) = network
                    .domains
                    .iter_mut()
                    .find(|permission| permission.domain == domain)
                {
                    existing.access = access;
                } else {
                    network
                        .domains
                        .push(crate::unstable_surface::SurfaceNetworkDomainPermission {
                            domain,
                            access,
                        });
                }
            }
            network
                .domains
                .sort_by(|left, right| left.domain.as_str().cmp(right.domain.as_str()));
        }
        let mut patches = Vec::new();
        if directories != settings.effective.additional_working_directories {
            patches.push(RuntimeSettingsPatch::ReplaceAdditionalWorkingDirectories { directories });
        }
        if network != settings.effective.network_permissions {
            patches.push(RuntimeSettingsPatch::ReplaceNetworkPermissions {
                permissions: network,
            });
        }
        let update_result = if let Ok(patches) = NonEmptyVec::try_new(patches) {
            committed(
                client.update_settings(SurfaceRequestId::new(), settings.thread_revision, patches),
                "JSONL session permission settings update",
            )
            .map(|_| ())
        } else {
            self.surface_host
                .jsonl_update_session_metadata(
                    thread_id,
                    ThreadMetadataPatch {
                        title: None,
                        active_permission_profile: None,
                        approval_mode: None,
                        runtime_workspace_roots: None,
                        permission_rules: None,
                        additional_working_directories: Some(
                            settings
                                .effective
                                .additional_working_directories
                                .iter()
                                .map(|directory| orca_core::config::AdditionalWorkingDirectory {
                                    path: directory.path.as_path().to_path_buf(),
                                    source: directory.source.as_str().to_string(),
                                })
                                .collect(),
                        ),
                        network_domain_permissions: Some(
                            settings
                                .effective
                                .network_permissions
                                .domains
                                .iter()
                                .map(|permission| {
                                    (
                                        permission.domain.as_str().to_string(),
                                        match permission.access {
                                            crate::unstable_surface::SurfaceNetworkDomainAccess::Allow => {
                                                orca_core::config::PermissionProfileNetworkAccess::Allow
                                            }
                                            crate::unstable_surface::SurfaceNetworkDomainAccess::Deny => {
                                                orca_core::config::PermissionProfileNetworkAccess::Deny
                                            }
                                        },
                                    )
                                })
                                .collect(),
                        ),
                    },
                )
                .map(|_| ())
        };
        let _ = surface.detach(
            &attachment.client,
            DetachRequest {
                request_id: SurfaceRequestId::new(),
            },
        );
        update_result
    }
}

fn apply_surface_settings_to_run_config(
    config: &mut RunConfig,
    settings: &crate::unstable_surface::SurfaceRuntimeSettings,
) -> io::Result<()> {
    config.cwd = Some(settings.cwd.as_path().to_path_buf());
    config.runtime_workspace_roots = Some(
        settings
            .workspace_roots
            .iter()
            .map(|root| root.as_path().to_path_buf())
            .collect(),
    );
    config.approval_mode = match settings.approval_mode {
        crate::unstable_surface::SurfaceApprovalMode::Suggest => {
            orca_core::approval_types::ApprovalMode::Suggest
        }
        crate::unstable_surface::SurfaceApprovalMode::AutoEdit => {
            orca_core::approval_types::ApprovalMode::AutoEdit
        }
        crate::unstable_surface::SurfaceApprovalMode::FullAuto => {
            orca_core::approval_types::ApprovalMode::FullAuto
        }
        crate::unstable_surface::SurfaceApprovalMode::Plan => {
            orca_core::approval_types::ApprovalMode::Plan
        }
    };
    config.active_permission_profile = settings.active_permission_profile.as_ref().map(|profile| {
        orca_core::config::ActivePermissionProfile {
            id: profile.id.as_str().to_string(),
            extends: profile
                .extends
                .as_ref()
                .map(|value| value.as_str().to_string()),
        }
    });
    config.permission_rules = orca_core::approval_rules::PermissionRules {
        rules: settings
            .permission_rules
            .ordered_rules
            .iter()
            .map(|rule| {
                orca_core::approval_rules::PermissionRule::new(
                    rule.tool.as_str(),
                    rule.pattern.as_str(),
                    match rule.decision {
                        crate::unstable_surface::SurfacePermissionDecision::Allow => {
                            orca_core::approval_types::Decision::Allow
                        }
                        crate::unstable_surface::SurfacePermissionDecision::Prompt => {
                            orca_core::approval_types::Decision::Prompt
                        }
                        crate::unstable_surface::SurfacePermissionDecision::Deny => {
                            orca_core::approval_types::Decision::Deny
                        }
                    },
                )
            })
            .collect(),
    };
    config.additional_working_directories = settings
        .additional_working_directories
        .iter()
        .map(|directory| orca_core::config::AdditionalWorkingDirectory {
            path: directory.path.as_path().to_path_buf(),
            source: directory.source.as_str().to_string(),
        })
        .collect();
    config.reasoning_effort = match settings.reasoning_effort {
        crate::unstable_surface::SurfaceReasoningEffort::High => {
            orca_core::config::ReasoningEffort::High
        }
        crate::unstable_surface::SurfaceReasoningEffort::Max => {
            orca_core::config::ReasoningEffort::Max
        }
        crate::unstable_surface::SurfaceReasoningEffort::Low
        | crate::unstable_surface::SurfaceReasoningEffort::Medium => {
            return Err(io::Error::other(
                "JSONL fork source uses an unsupported reasoning effort",
            ));
        }
    };
    config.model =
        orca_core::model::ModelSelection::from_unchecked(Some(settings.model.as_str().to_string()));
    Ok(())
}

impl PreparedJsonlTurn {
    pub(crate) fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub(crate) fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub(crate) fn start<W>(self, writer: W) -> io::Result<JsonlTransportTurn>
    where
        W: HostedOperationWriter + Send + 'static,
    {
        committed(
            self.client.admit_reserved_with_output(
                SurfaceRequestId::new(),
                self.operation_id.clone(),
                self.admission_lease_id,
                DiscardHostedOperationWriter,
            ),
            "JSONL surface admission",
        )?;
        let thread_id = self.thread_id.clone();
        let turn_id = self.turn_id.clone();
        let worker = std::thread::spawn(move || {
            drain_jsonl_surface(
                self.surface,
                self.client,
                self.subscription,
                self.operation_id,
                self.thread_id,
                self.turn_id,
                self.interactions,
                self.runtime_workspace_roots,
                writer,
            )
        });
        Ok(JsonlTransportTurn {
            thread_id,
            turn_id,
            worker: Some(worker),
        })
    }
}

impl JsonlTransportTurn {
    pub(crate) fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub(crate) fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }

    pub(crate) fn wait_terminal(&mut self) -> io::Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|_| io::Error::other("JSONL surface projection worker panicked"))?
    }
}

#[derive(Default)]
struct DiscardHostedOperationWriter;

impl Write for DiscardHostedOperationWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl HostedOperationWriter for DiscardHostedOperationWriter {
    fn finish_generation(&mut self, _commit_terminal: bool) -> io::Result<()> {
        Ok(())
    }
}

fn drain_jsonl_surface<W>(
    surface: RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
    mut subscription: SurfaceSubscriptionReceiver,
    operation_id: SurfaceOperationId,
    thread_id: String,
    turn_id: TurnId,
    interactions: Option<JsonlInteractionTransport>,
    runtime_workspace_roots: Vec<std::path::PathBuf>,
    mut writer: W,
) -> io::Result<()>
where
    W: HostedOperationWriter,
{
    let mut projector = JsonlSurfaceProjector::new(
        surface.clone(),
        thread_id,
        turn_id,
        operation_id,
        client.clone(),
        interactions,
        runtime_workspace_roots,
    );
    let result = loop {
        let Some(item) = subscription.recv_timeout(std::time::Duration::from_millis(100)) else {
            continue;
        };
        match item {
            SurfaceSubscriptionItem::Batch { batch } => {
                if project_surface_batch(&mut projector, &batch, &mut writer)? {
                    writer.finish_generation(true)?;
                    break Ok(());
                }
            }
            SurfaceSubscriptionItem::Gap { .. } => {
                write_runtime_event(
                    &mut writer,
                    "error",
                    &projector.thread_id,
                    serde_json::json!({
                        "message": "thread surface snapshot required; reconnect and resume the thread"
                    }),
                )?;
                writer.finish_generation(false)?;
                break Err(io::Error::other(
                    "thread surface snapshot required; reconnect and resume the thread",
                ));
            }
            SurfaceSubscriptionItem::Sealed { .. } => {
                writer.finish_generation(false)?;
                break Err(io::Error::other("JSONL surface subscription sealed"));
            }
        }
    };
    let _ = surface.detach(
        &client,
        DetachRequest {
            request_id: SurfaceRequestId::new(),
        },
    );
    result
}

struct JsonlSurfaceProjector {
    surface: RuntimeSurfaceHandle,
    thread_id: String,
    turn_id: TurnId,
    operation_id: SurfaceOperationId,
    streams: HashMap<String, SurfaceAssistantStream>,
    tools: HashMap<String, SurfaceToolRequest>,
    client: RuntimeSurfaceClientHandle,
    interactions: Option<JsonlInteractionTransport>,
    runtime_workspace_roots: Vec<std::path::PathBuf>,
}

impl JsonlSurfaceProjector {
    fn new(
        surface: RuntimeSurfaceHandle,
        thread_id: String,
        turn_id: TurnId,
        operation_id: SurfaceOperationId,
        client: RuntimeSurfaceClientHandle,
        interactions: Option<JsonlInteractionTransport>,
        runtime_workspace_roots: Vec<std::path::PathBuf>,
    ) -> Self {
        Self {
            surface,
            thread_id,
            turn_id,
            operation_id,
            streams: HashMap::new(),
            tools: HashMap::new(),
            client,
            interactions,
            runtime_workspace_roots,
        }
    }
}

fn project_surface_batch<W: Write>(
    projector: &mut JsonlSurfaceProjector,
    batch: &SurfaceCommitBatch,
    writer: &mut W,
) -> io::Result<bool> {
    for envelope in batch.events.as_slice() {
        if !scope_belongs_to_operation(&envelope.scope, &projector.operation_id) {
            continue;
        }
        if project_surface_event(projector, &envelope.event, writer)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn scope_belongs_to_operation(scope: &SurfaceScope, operation_id: &SurfaceOperationId) -> bool {
    match scope {
        SurfaceScope::Thread => false,
        SurfaceScope::Operation {
            operation_id: scoped,
        } => scoped == operation_id,
        SurfaceScope::Generation { fence } => &fence.operation_id == operation_id,
        SurfaceScope::Background { fence } => &fence.operation_fence.operation_id == operation_id,
        SurfaceScope::Goal {
            causative_generation,
            ..
        } => causative_generation
            .as_ref()
            .is_some_and(|fence| &fence.operation_id == operation_id),
    }
}

fn project_surface_event<W: Write>(
    projector: &mut JsonlSurfaceProjector,
    event: &SurfaceEvent,
    writer: &mut W,
) -> io::Result<bool> {
    match event {
        SurfaceEvent::Operation(OperationPatch::AgentLoopTurnStarted { turn }) => {
            write_runtime_event(
                writer,
                "turn.started",
                &projector.thread_id,
                serde_json::json!({
                    "turn_id": projector.turn_id.to_string(),
                    "turn": {
                        "turn_id": projector.turn_id.to_string(),
                        "ordinal": turn.ordinal,
                    },
                    "task": {
                        "task_id": turn.task_id.as_str(),
                        "status": "running",
                    },
                }),
            )?;
        }
        SurfaceEvent::Operation(OperationPatch::Terminal { record })
            if record.operation_id == projector.operation_id =>
        {
            let _ = projector.surface.detach(
                &projector.client,
                DetachRequest {
                    request_id: SurfaceRequestId::new(),
                },
            );
            write_runtime_event(
                writer,
                "session.completed",
                &projector.thread_id,
                serde_json::json!({ "status": terminal_status(&record.terminal) }),
            )?;
            return Ok(true);
        }
        SurfaceEvent::Assistant(AssistantPatch::StreamOpened { stream }) => {
            projector
                .streams
                .insert(serialized_id(&stream.stream_id), stream.clone());
        }
        SurfaceEvent::Assistant(AssistantPatch::Delta {
            stream_id, text, ..
        }) => {
            let Some(stream) = projector.streams.get(&serialized_id(stream_id)) else {
                return Ok(false);
            };
            let event_type = match stream.channel {
                AssistantChannel::Message => "assistant.message.delta",
                AssistantChannel::Reasoning => "assistant.reasoning.delta",
                AssistantChannel::Plan => "assistant.message.delta",
            };
            let text = match stream.channel {
                AssistantChannel::Plan => {
                    format!("<proposed_plan>{}</proposed_plan>", text.as_str())
                }
                _ => text.as_str().to_string(),
            };
            let payload = match stream.channel {
                AssistantChannel::Reasoning => serde_json::json!({
                    "turn_id": projector.turn_id.to_string(),
                    "item_id": stream.item_id.to_string(),
                    "text": text,
                }),
                _ => serde_json::json!({
                    "turn_id": projector.turn_id.to_string(),
                    "agent_message_item_id": stream.item_id.to_string(),
                    "plan_item_id": stream.item_id.to_string(),
                    "text": text,
                }),
            };
            write_runtime_event(writer, event_type, &projector.thread_id, payload)?;
        }
        SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted { response }) => {
            let message_id = response
                .message_item
                .as_ref()
                .map(|item| item.id.clone())
                .unwrap_or_else(orca_core::thread_identity::ConversationItemId::new);
            let plan_id = response
                .plan_item
                .as_ref()
                .map(|item| item.id.clone())
                .unwrap_or_else(orca_core::thread_identity::ConversationItemId::new);
            let reasoning_id = response
                .reasoning_item
                .as_ref()
                .map(|item| item.id.clone())
                .unwrap_or_else(orca_core::thread_identity::ConversationItemId::new);
            let mut assistant_content = response
                .message_item
                .as_ref()
                .map(|item| item.text.as_str().to_string())
                .unwrap_or_default();
            if let Some(plan) = response.plan_item.as_ref() {
                assistant_content.push_str("<proposed_plan>");
                assistant_content.push_str(plan.text.as_str());
                assistant_content.push_str("</proposed_plan>");
            }
            let assistant_reasoning = response.reasoning_item.as_ref().map(|item| {
                if item.content.as_str().is_empty() {
                    item.summary.as_str().to_string()
                } else {
                    item.content.as_str().to_string()
                }
            });
            write_runtime_event(
                writer,
                "model.response.completed",
                &projector.thread_id,
                serde_json::json!({
                    "identity": {
                        "turn_id": projector.turn_id.to_string(),
                        "item_ids": {
                            "conversation_item_id": message_id.to_string(),
                            "plan_item_id": plan_id.to_string(),
                            "reasoning_item_id": reasoning_id.to_string(),
                        },
                    },
                    "assistant_content": (!assistant_content.is_empty()).then_some(assistant_content),
                    "assistant_reasoning": assistant_reasoning,
                    "tool_calls": [],
                }),
            )?;
        }
        SurfaceEvent::Assistant(AssistantPatch::StreamDiscarded { stream_id, .. }) => {
            projector.streams.remove(&serialized_id(stream_id));
        }
        SurfaceEvent::Tool(ToolPatch::Requested { request }) => {
            projector
                .tools
                .insert(request.tool_call_id.as_str().to_string(), request.clone());
            write_runtime_event(
                writer,
                "tool.call.requested",
                &projector.thread_id,
                serde_json::json!({
                    "id": request.tool_call_id.as_str(),
                    "name": request.name.as_str(),
                    "target": request.target.as_ref().map(DisplayText::as_str),
                    "raw_arguments": request.raw_arguments.as_str(),
                }),
            )?;
        }
        SurfaceEvent::Tool(ToolPatch::Completed { result }) => {
            let request = projector.tools.remove(result.tool_call_id.as_str());
            write_runtime_event(
                writer,
                "tool.call.completed",
                &projector.thread_id,
                serde_json::json!({
                    "id": result.tool_call_id.as_str(),
                    "name": result.name.as_str(),
                    "target": request.as_ref().and_then(|value| value.target.as_ref()).map(DisplayText::as_str),
                    "raw_arguments": request.as_ref().map(|value| value.raw_arguments.as_str()),
                    "status": tool_status(result.terminal.kind),
                    "output": result.output.as_ref().map(DisplayText::as_str),
                    "error": result.error.as_ref().map(DisplayText::as_str),
                    "exit_code": result.exit_code,
                    "kind": tool_result_kind(result.terminal.kind),
                    "terminal_source": match result.terminal.source {
                        ToolTerminalSource::Observed => "observed",
                        ToolTerminalSource::CompatibilityRepair => "compatibility_repair",
                    },
                    "truncated": result.truncated,
                }),
            )?;
        }
        SurfaceEvent::Plan(plan) => {
            let items = plan
                .items
                .iter()
                .map(|item| {
                    serde_json::json!({
                        "step": item.step.as_str(),
                        "status": match item.status {
                            crate::unstable_surface::SurfacePlanStatus::Pending => "pending",
                            crate::unstable_surface::SurfacePlanStatus::InProgress => "in_progress",
                            crate::unstable_surface::SurfacePlanStatus::Completed => "completed",
                        },
                    })
                })
                .collect::<Vec<_>>();
            write_runtime_event(
                writer,
                "plan.updated",
                &projector.thread_id,
                serde_json::json!({
                    "explanation": plan.explanation.as_ref().map(DisplayText::as_str),
                    "plan": items,
                }),
            )?;
        }
        SurfaceEvent::Interaction(InteractionPatch::Requested { interaction })
            if routes_interaction(&projector.client, &interaction.route) =>
        {
            let Some(transport) = projector.interactions.as_ref() else {
                return Ok(false);
            };
            let request_id = format!(
                "{}-{}",
                projector.turn_id,
                serialized_id(&interaction.interaction_id)
            );
            match &interaction.request {
                SurfaceInteractionRequest::ToolApproval { description, .. } => {
                    let Some(request_id) = register_or_settle_unavailable(
                        transport.permissions.insert_surface(
                            request_id.clone(),
                            projector.client.clone(),
                            interaction.interaction_id.clone(),
                            interaction.kind,
                            projector.thread_id.clone(),
                            projector.runtime_workspace_roots.clone(),
                        ),
                        projector,
                        interaction,
                    )?
                    else {
                        return Ok(false);
                    };
                    let payload = serde_json::json!({
                        "request_id": request_id,
                        "thread_id": projector.thread_id,
                        "turn_id": projector.turn_id.to_string(),
                        "reason": description.as_str(),
                        "permissions": {},
                    });
                    let frame_digest =
                        super::opaque_permission_router::jsonl_response_digest(&payload)?;
                    transport
                        .permissions
                        .mark_writing(&request_id, frame_digest)?;
                    write_runtime_event(
                        writer,
                        "surface.permission.requested",
                        &projector.thread_id,
                        payload,
                    )?;
                    writer.flush()?;
                    transport
                        .permissions
                        .mark_published(&request_id, frame_digest)?;
                }
                SurfaceInteractionRequest::PermissionRequest {
                    reason,
                    permissions,
                    ..
                } => {
                    let Some(request_id) = register_or_settle_unavailable(
                        transport.permissions.insert_surface(
                            request_id.clone(),
                            projector.client.clone(),
                            interaction.interaction_id.clone(),
                            interaction.kind,
                            projector.thread_id.clone(),
                            projector.runtime_workspace_roots.clone(),
                        ),
                        projector,
                        interaction,
                    )?
                    else {
                        return Ok(false);
                    };
                    let payload = serde_json::json!({
                        "request_id": request_id,
                        "thread_id": projector.thread_id,
                        "turn_id": projector.turn_id.to_string(),
                        "reason": reason.as_ref().map(DisplayText::as_str),
                        "permissions": surface_permissions_wire(permissions),
                    });
                    let frame_digest =
                        super::opaque_permission_router::jsonl_response_digest(&payload)?;
                    transport
                        .permissions
                        .mark_writing(&request_id, frame_digest)?;
                    write_runtime_event(
                        writer,
                        "surface.permission.requested",
                        &projector.thread_id,
                        payload,
                    )?;
                    writer.flush()?;
                    transport
                        .permissions
                        .mark_published(&request_id, frame_digest)?;
                }
                SurfaceInteractionRequest::UserInput {
                    question,
                    suggestions,
                } => {
                    let Some(request_id) = register_or_settle_unavailable(
                        transport.user_inputs.insert_surface(
                            request_id.clone(),
                            PendingSurfaceUserInputRequest {
                                client: projector.client.clone(),
                                interaction_id: interaction.interaction_id.clone(),
                            },
                        ),
                        projector,
                        interaction,
                    )?
                    else {
                        return Ok(false);
                    };
                    let payload = serde_json::json!({
                        "request_id": request_id,
                        "thread_id": projector.thread_id,
                        "turn_id": projector.turn_id.to_string(),
                        "question": question.as_str(),
                        "choices": suggestions.iter().map(DisplayText::as_str).collect::<Vec<_>>(),
                    });
                    let frame_digest =
                        super::opaque_permission_router::jsonl_response_digest(&payload)?;
                    transport
                        .user_inputs
                        .mark_surface_writing(&request_id, frame_digest)?;
                    write_runtime_event(
                        writer,
                        "surface.user_input.requested",
                        &projector.thread_id,
                        payload,
                    )?;
                    writer.flush()?;
                    transport
                        .user_inputs
                        .mark_surface_published(&request_id, frame_digest)?;
                }
                SurfaceInteractionRequest::McpElicitation {
                    server_name,
                    message,
                    request,
                    ..
                } => {
                    let Some(request_id) = register_or_settle_unavailable(
                        transport.mcp_elicitations.insert_surface(
                            request_id.clone(),
                            PendingSurfaceMcpElicitationRequest {
                                client: projector.client.clone(),
                                interaction_id: interaction.interaction_id.clone(),
                            },
                        ),
                        projector,
                        interaction,
                    )?
                    else {
                        return Ok(false);
                    };
                    let (mode, url, requested_schema) = match request {
                        crate::unstable_surface::SurfaceMcpElicitationRequest::Form {
                            requested_schema,
                            ..
                        } => (
                            "form",
                            serde_json::Value::Null,
                            requested_schema
                                .as_ref()
                                .map(surface_data_wire)
                                .unwrap_or(serde_json::Value::Null),
                        ),
                        crate::unstable_surface::SurfaceMcpElicitationRequest::Url {
                            raw_url,
                            requested_schema,
                        } => (
                            "url",
                            raw_url
                                .as_ref()
                                .map(|url| serde_json::Value::from(url.as_str()))
                                .unwrap_or(serde_json::Value::Null),
                            requested_schema
                                .as_ref()
                                .map(surface_data_wire)
                                .unwrap_or(serde_json::Value::Null),
                        ),
                    };
                    let payload = serde_json::json!({
                        "request_id": request_id,
                        "thread_id": projector.thread_id,
                        "turn_id": projector.turn_id.to_string(),
                        "server_name": server_name.as_str(),
                        "mode": mode,
                        "message": message.as_str(),
                        "url": url,
                        "requested_schema": requested_schema,
                    });
                    let frame_digest =
                        super::opaque_permission_router::jsonl_response_digest(&payload)?;
                    transport
                        .mcp_elicitations
                        .mark_surface_writing(&request_id, frame_digest)?;
                    write_runtime_event(
                        writer,
                        "surface.mcp_elicitation.requested",
                        &projector.thread_id,
                        payload,
                    )?;
                    writer.flush()?;
                    transport
                        .mcp_elicitations
                        .mark_surface_published(&request_id, frame_digest)?;
                }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(false)
}

fn register_or_settle_unavailable(
    registration: io::Result<String>,
    projector: &JsonlSurfaceProjector,
    interaction: &crate::unstable_surface::SurfaceInteractionView,
) -> io::Result<Option<String>> {
    let Err(registration_error) = registration else {
        return Ok(registration.ok());
    };
    let answer = match &interaction.request {
        SurfaceInteractionRequest::ToolApproval { .. } => {
            crate::unstable_surface::SurfaceClientInteractionAnswer::ToolApproval {
                decision: crate::unstable_surface::SurfaceAllowDeny::Deny,
            }
        }
        SurfaceInteractionRequest::PermissionRequest { permissions, .. } => {
            crate::unstable_surface::SurfaceClientInteractionAnswer::PermissionRequest {
                decision: crate::unstable_surface::SurfacePermissionClientDecision::Deny {
                    scope: crate::unstable_surface::PermissionGrantScope::Turn,
                    permissions: permissions.clone(),
                    strict_auto_review: false,
                },
            }
        }
        SurfaceInteractionRequest::UserInput { .. } => {
            crate::unstable_surface::SurfaceClientInteractionAnswer::UserInput {
                decision: crate::unstable_surface::SurfaceUserInputDecision::Cancel,
            }
        }
        SurfaceInteractionRequest::McpElicitation { .. } => {
            crate::unstable_surface::SurfaceClientInteractionAnswer::McpElicitation {
                decision: crate::unstable_surface::SurfaceMcpElicitationDecision::Decline,
            }
        }
        SurfaceInteractionRequest::BackgroundApproval { .. } => {
            return Err(io::Error::other(format!(
                "{registration_error}; JSONL background approval routing is not active"
            )));
        }
    };
    match projector.client.respond_interaction_by_id(
        SurfaceRequestId::new(),
        interaction.interaction_id.clone(),
        answer,
    ) {
        Ok(MutationReply::Committed { .. }) | Ok(MutationReply::Deferred { .. }) => Ok(None),
        Ok(MutationReply::Uncommitted { .. }) | Err(_) => Err(io::Error::other(format!(
            "{registration_error}; runtime retained recovery ownership for rejected JSONL interaction"
        ))),
    }
}

fn surface_data_wire(value: &crate::unstable_surface::SurfaceDataValue) -> serde_json::Value {
    match value {
        crate::unstable_surface::SurfaceDataValue::Null => serde_json::Value::Null,
        crate::unstable_surface::SurfaceDataValue::Boolean(value) => {
            serde_json::Value::Bool(*value)
        }
        crate::unstable_surface::SurfaceDataValue::Integer(value) => {
            serde_json::Value::from(value.get())
        }
        crate::unstable_surface::SurfaceDataValue::Unsigned(value) => {
            serde_json::Value::from(*value)
        }
        crate::unstable_surface::SurfaceDataValue::Number(value) => {
            serde_json::json!(value.get())
        }
        crate::unstable_surface::SurfaceDataValue::String(value) => {
            serde_json::Value::from(value.as_str())
        }
        crate::unstable_surface::SurfaceDataValue::Array(values) => {
            serde_json::Value::Array(values.iter().map(surface_data_wire).collect())
        }
        crate::unstable_surface::SurfaceDataValue::Object(properties) => serde_json::Value::Object(
            properties
                .iter()
                .map(|property| {
                    (
                        property.name.as_str().to_string(),
                        surface_data_wire(&property.value),
                    )
                })
                .collect(),
        ),
    }
}

fn routes_interaction(
    client: &RuntimeSurfaceClientHandle,
    route: &SurfaceInteractionRoute,
) -> bool {
    match route {
        SurfaceInteractionRoute::Unassigned { .. } => false,
        SurfaceInteractionRoute::Exclusive { attachment_id, .. } => {
            attachment_id == client.attachment_id()
        }
        SurfaceInteractionRoute::SharedFirstCommitWins { attachments, .. } => {
            attachments.as_set().contains(client.attachment_id())
        }
    }
}

fn surface_permissions_wire(
    permissions: &crate::unstable_surface::SurfacePermissionProfile,
) -> serde_json::Value {
    let file_system = permissions.file_system.as_ref().map(|profile| {
        serde_json::json!({
            "read": profile.read.as_ref().map(|paths| paths.iter().map(|path| path.0.as_str()).collect::<Vec<_>>()),
            "write": profile.write.as_ref().map(|paths| paths.iter().map(|path| path.0.as_str()).collect::<Vec<_>>()),
        })
    });
    let network = permissions.network.as_ref().map(|profile| {
        let domains = profile
            .domains
            .iter()
            .map(|(domain, access)| {
                (
                    domain.0.as_str().to_string(),
                    serde_json::Value::from(match access {
                        crate::unstable_surface::SurfaceAllowDeny::Allow => "allow",
                        crate::unstable_surface::SurfaceAllowDeny::Deny => "deny",
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        serde_json::json!({
            "enabled": profile.enabled,
            "domains": domains,
        })
    });
    serde_json::json!({
        "fileSystem": file_system,
        "network": network,
        "shell": permissions.shell.as_ref().map(|shell| serde_json::json!({
            "unsandboxed": shell.unsandboxed,
        })),
    })
}

fn write_runtime_event<W: Write>(
    writer: &mut W,
    event_type: &str,
    run_id: &str,
    payload: serde_json::Value,
) -> io::Result<()> {
    serde_json::to_writer(
        &mut *writer,
        &serde_json::json!({
            "type": event_type,
            "run_id": run_id,
            "payload": payload,
        }),
    )?;
    writeln!(writer)
}

fn serialized_id<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_default()
}

fn terminal_status(terminal: &OperationTerminal) -> &'static str {
    match terminal {
        OperationTerminal::Succeeded { .. } => "success",
        OperationTerminal::Cancelled { .. } | OperationTerminal::Shutdown { .. } => "cancelled",
        OperationTerminal::BudgetExhausted { .. } => "budget_exhausted",
        OperationTerminal::NotAdmitted { .. } => "not_admitted",
        OperationTerminal::Failed {
            class: crate::unstable_surface::FailureClass::LegacyApprovalRequired,
            ..
        } => "approval_required",
        OperationTerminal::Failed { .. }
        | OperationTerminal::Panicked { .. }
        | OperationTerminal::JoinFailed { .. }
        | OperationTerminal::AbortedByRuntimeRestart { .. } => "failed",
    }
}

fn tool_status(kind: SurfaceToolResultKind) -> &'static str {
    match kind {
        SurfaceToolResultKind::Success => "completed",
        SurfaceToolResultKind::Cancelled => "cancelled",
        _ => "failed",
    }
}

fn tool_result_kind(kind: SurfaceToolResultKind) -> &'static str {
    match kind {
        SurfaceToolResultKind::Success => "success",
        SurfaceToolResultKind::Failed => "failed",
        SurfaceToolResultKind::Denied => "denied",
        SurfaceToolResultKind::Cancelled => "cancelled",
        SurfaceToolResultKind::TimedOut => "timed_out",
        SurfaceToolResultKind::InvalidArguments => "invalid_arguments",
        SurfaceToolResultKind::ExternalEffectAmbiguous => "external_effect_ambiguous",
        SurfaceToolResultKind::ObservationUnavailable => "observation_unavailable",
        SurfaceToolResultKind::CleanupAmbiguous => "cleanup_ambiguous",
    }
}

fn settings_preparation(
    config: &RunConfig,
    permissions: PermissionProfileOverride,
    snapshot: &crate::unstable_surface::SurfaceSnapshot,
) -> io::Result<OperationSettingsPreparation> {
    let mut updated = config.clone();
    apply_surface_settings_to_run_config(&mut updated, &snapshot.settings.effective)?;
    apply_permission_override(&mut updated, permissions);
    let patches = settings_patches(&updated, snapshot)?;
    if patches.is_empty() {
        Ok(OperationSettingsPreparation::UseCurrent {
            expected_settings_revision: snapshot.settings.thread_revision,
            expected_policy_epoch: snapshot.settings.effective.policy_epoch,
        })
    } else {
        Ok(
            OperationSettingsPreparation::ApplyThreadOverridesBeforeRequested {
                expected_settings_revision: snapshot.settings.thread_revision,
                expected_policy_epoch: snapshot.settings.effective.policy_epoch,
                patches: NonEmptyVec::try_new(patches)
                    .map_err(|error| io::Error::other(error.to_string()))?,
            },
        )
    }
}

fn settings_patches(
    config: &RunConfig,
    snapshot: &crate::unstable_surface::SurfaceSnapshot,
) -> io::Result<Vec<RuntimeSettingsPatch>> {
    let mut patches = Vec::new();
    let approval_mode = match config.approval_mode {
        orca_core::approval_types::ApprovalMode::Suggest => {
            crate::unstable_surface::SurfaceApprovalMode::Suggest
        }
        orca_core::approval_types::ApprovalMode::AutoEdit => {
            crate::unstable_surface::SurfaceApprovalMode::AutoEdit
        }
        orca_core::approval_types::ApprovalMode::FullAuto => {
            crate::unstable_surface::SurfaceApprovalMode::FullAuto
        }
        orca_core::approval_types::ApprovalMode::Plan => {
            crate::unstable_surface::SurfaceApprovalMode::Plan
        }
    };
    if snapshot.settings.effective.approval_mode != approval_mode {
        patches.push(RuntimeSettingsPatch::SetApprovalMode {
            mode: approval_mode,
        });
    }
    if let Some(roots) = config.runtime_workspace_roots.as_ref() {
        let roots = roots
            .iter()
            .cloned()
            .map(crate::unstable_surface::CanonicalPath::try_new)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io::Error::other(error.to_string()))?;
        if snapshot.settings.effective.workspace_roots != roots {
            patches.push(RuntimeSettingsPatch::SetWorkspaceRoots { roots });
        }
    }
    let profile = config
        .active_permission_profile
        .as_ref()
        .map(|profile| {
            Ok(crate::unstable_surface::SurfaceActivePermissionProfile {
                id: crate::unstable_surface::NonEmptyText::try_new(profile.id.clone())?,
                extends: profile
                    .extends
                    .as_ref()
                    .map(|value| crate::unstable_surface::NonEmptyText::try_new(value.clone()))
                    .transpose()?,
            })
        })
        .transpose()
        .map_err(|error: crate::unstable_surface::SurfaceValueError| {
            io::Error::other(error.to_string())
        })?;
    if snapshot.settings.effective.active_permission_profile != profile {
        patches.push(RuntimeSettingsPatch::SetActivePermissionProfile { profile });
    }
    let rules = config
        .permission_rules
        .rules
        .iter()
        .map(|rule| {
            Ok(crate::unstable_surface::SurfacePermissionRule {
                tool: crate::unstable_surface::NonEmptyText::try_new(rule.tool.clone())?,
                pattern: crate::unstable_surface::NonEmptyText::try_new(rule.pattern.clone())?,
                decision: match rule.decision {
                    orca_core::approval_types::Decision::Allow => {
                        crate::unstable_surface::SurfacePermissionDecision::Allow
                    }
                    orca_core::approval_types::Decision::Prompt => {
                        crate::unstable_surface::SurfacePermissionDecision::Prompt
                    }
                    orca_core::approval_types::Decision::Deny => {
                        crate::unstable_surface::SurfacePermissionDecision::Deny
                    }
                },
            })
        })
        .collect::<Result<Vec<_>, crate::unstable_surface::SurfaceValueError>>()
        .map_err(|error| io::Error::other(error.to_string()))?;
    if snapshot.settings.effective.permission_rules.ordered_rules != rules {
        patches.push(RuntimeSettingsPatch::ReplacePermissionRules { rules });
    }
    let directories = config
        .additional_working_directories
        .iter()
        .map(|directory| {
            Ok(crate::unstable_surface::SurfaceAdditionalWorkingDirectory {
                path: crate::unstable_surface::CanonicalPath::try_new(directory.path.clone())?,
                source: crate::unstable_surface::NonEmptyText::try_new(directory.source.clone())?,
            })
        })
        .collect::<Result<Vec<_>, crate::unstable_surface::SurfaceValueError>>()
        .map_err(|error| io::Error::other(error.to_string()))?;
    if snapshot.settings.effective.additional_working_directories != directories {
        patches.push(RuntimeSettingsPatch::ReplaceAdditionalWorkingDirectories { directories });
    }
    Ok(patches)
}

fn committed<T>(
    result: Result<MutationReply<T>, crate::unstable_surface::SurfaceClientCommandError>,
    action: &str,
) -> io::Result<T> {
    match result.map_err(|error| io::Error::other(format!("{action} failed: {error:?}")))? {
        MutationReply::Committed { value, .. } => Ok(value),
        MutationReply::Deferred { mutation, .. } => Err(io::Error::other(format!(
            "{action} deferred: request={:?} commit={:?}",
            mutation.request_id, mutation.commit_id
        ))),
        MutationReply::Uncommitted { mutation } => Err(io::Error::other(format!(
            "{action} did not commit: {}",
            uncommitted_message(&mutation)
        ))),
    }
}

fn uncommitted_message(mutation: &UncommittedMutation) -> &str {
    match mutation {
        UncommittedMutation::Invalid { error, .. } => error.error().message.as_str(),
        UncommittedMutation::Stale { error, .. } => error.error().message.as_str(),
        UncommittedMutation::Unavailable { error, .. } => error.error().message.as_str(),
        UncommittedMutation::CommitFailed { error, .. } => error.error().message.as_str(),
    }
}

fn jsonl_thread_config(config: &RunConfig) -> RunConfig {
    let mut config = config.clone();
    if let Some(roots) = config.runtime_workspace_roots.as_mut() {
        for root in roots {
            if let Ok(canonical) = std::fs::canonicalize(&*root) {
                *root = canonical;
            }
        }
    }
    config.output_format = OutputFormat::Jsonl;
    config.history_mode = HistoryMode::Record;
    config.show_session_picker = false;
    config.desktop_notifications = false;
    config
}

fn runtime_host_error(error: RuntimeHostError) -> io::Error {
    io::Error::other(error.to_string())
}
