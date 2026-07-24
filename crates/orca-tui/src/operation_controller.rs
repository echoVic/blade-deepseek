use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;
use std::{collections::HashMap, io};

use orca_core::cancel::{CancelToken, OperationId, OperationIdAllocator};
use orca_runtime::provider_stream::RuntimeProviderSuspensionControl;
use orca_runtime::runtime_host::PauseGoalRunResult;
use orca_runtime::runtime_host::{InterruptOperationResult, OperationHandle};

use crate::interaction_broker::TuiInteractionBroker;
use crate::interaction_broker::TuiInteractionWaiter;
use crate::types::{TuiEvent, TuiInteractionKey, TuiInteractionKind, TuiInteractionResponse};

pub(crate) trait TuiOperationInterrupt {
    fn interrupt_current(&self);
}

#[derive(Clone, Debug)]
pub(crate) struct TuiOperationController {
    hosted: Arc<HostedOperationState>,
    broker: TuiInteractionBroker,
    background_current: Arc<Mutex<Option<OperationId>>>,
    surface_ids: Arc<OperationIdAllocator>,
}

#[derive(Debug, Default)]
struct HostedOperationState {
    inner: Mutex<HostedOperationInner>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct HostedOperationInner {
    active: Option<Arc<OperationHandle>>,
    surface_active: Option<SurfaceActiveOperation>,
    interrupt_requested: bool,
    background_requested: bool,
    shutdown: bool,
}

impl TuiOperationController {
    pub(crate) fn hosted(broker: TuiInteractionBroker) -> Self {
        Self {
            hosted: Arc::new(HostedOperationState::default()),
            broker,
            background_current: Arc::new(Mutex::new(None)),
            surface_ids: Arc::new(OperationIdAllocator::default()),
        }
    }
    pub(crate) fn current_id(&self) -> Option<OperationId> {
        self.lock_hosted()
            .active
            .as_ref()
            .map(|operation| operation.id())
    }
    pub(crate) fn interrupt_current(&self) -> Option<OperationId> {
        let hosted = {
            let mut hosted = self.lock_hosted();
            if let Some(operation) = hosted.active.clone() {
                operation
            } else {
                if cancel_surface_or_shutdown(&mut hosted) {
                    return None;
                }
                hosted.interrupt_requested = true;
                return None;
            }
        };
        let operation_id = hosted.id();
        match hosted.interrupt() {
            Ok(
                InterruptOperationResult::Requested { .. }
                | InterruptOperationResult::AlreadyRequested { .. },
            ) => {}
            Ok(InterruptOperationResult::Stale { .. } | InterruptOperationResult::Idle { .. })
            | Err(_) => return None,
        };
        self.broker.interrupt(operation_id);
        let mut background = self.lock_background_current();
        if *background == Some(operation_id) {
            *background = None;
        }
        Some(operation_id)
    }

    pub(crate) fn pause_current_goal(&self) -> io::Result<bool> {
        let hosted = self.lock_hosted().active.clone();
        let Some(hosted) = hosted else {
            return Ok(false);
        };
        let operation_id = hosted.id();
        match hosted.pause_goal().map_err(io::Error::other)? {
            PauseGoalRunResult::Requested { .. } | PauseGoalRunResult::AlreadyRequested { .. } => {
                self.broker.interrupt(operation_id);
                let mut background = self.lock_background_current();
                if *background == Some(operation_id) {
                    *background = None;
                }
                Ok(true)
            }
            PauseGoalRunResult::NotGoalRun { .. }
            | PauseGoalRunResult::Stale { .. }
            | PauseGoalRunResult::Idle { .. } => Ok(false),
        }
    }

    pub(crate) fn request_background_current(&self) -> bool {
        let mut hosted = self.lock_hosted();
        if hosted.shutdown {
            return false;
        }
        if let Some(operation_id) = hosted.active.as_ref().map(|operation| operation.id()) {
            *self.lock_background_current() = Some(operation_id);
        } else {
            hosted.background_requested = true;
        }
        true
    }

    pub(crate) fn take_background_current(&self, operation_id: OperationId) -> bool {
        let mut background = self.lock_background_current();
        if *background == Some(operation_id) {
            *background = None;
            true
        } else {
            false
        }
    }

    pub(crate) fn shutdown(&self) {
        let hosted = {
            let mut hosted = self.lock_hosted();
            hosted.shutdown = true;
            hosted.active.clone()
        };
        if let Some(operation) = hosted {
            let _ = operation.interrupt();
        }
        self.cancel_surface_and_notify();
        self.broker.shutdown();
        *self.lock_background_current() = None;
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.lock_hosted().shutdown
    }

    pub(crate) fn install_hosted(&self, operation: Arc<OperationHandle>) -> io::Result<()> {
        let operation_id = operation.id();
        {
            let hosted = self.lock_hosted();
            if hosted.shutdown {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "TUI operation controller is shutting down",
                ));
            }
            if let Some(active) = hosted.active.as_ref() {
                return Err(io::Error::other(format!(
                    "TUI operation {:?} is still active",
                    active.id()
                )));
            }
        }
        self.broker.activate(operation_id)?;
        let mut hosted = self.lock_hosted();
        if hosted.shutdown {
            self.broker.complete(operation_id);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "TUI operation controller is shutting down",
            ));
        }
        let interrupt_requested = hosted.interrupt_requested;
        let background_requested = hosted.background_requested;
        hosted.interrupt_requested = false;
        hosted.background_requested = false;
        hosted.active = Some(Arc::clone(&operation));
        *self.lock_background_current() = background_requested.then_some(operation_id);
        drop(hosted);
        self.hosted.changed.notify_all();
        if interrupt_requested {
            let _ = operation.interrupt();
            self.broker.interrupt(operation_id);
        }
        Ok(())
    }

    pub(crate) fn wait_for_hosted(
        &self,
        operation_id: OperationId,
        cancel: &CancelToken,
    ) -> io::Result<TuiTurnControl> {
        let mut hosted = self.lock_hosted();
        loop {
            if hosted.shutdown || cancel.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "TUI hosted operation was cancelled before activation",
                ));
            }
            if let Some(active) = hosted.active.as_ref() {
                if active.id() != operation_id {
                    return Err(io::Error::other(format!(
                        "TUI hosted operation activation mismatch: expected {:?}, found {:?}",
                        operation_id,
                        active.id()
                    )));
                }
                return Ok(TuiTurnControl {
                    controller: self.clone(),
                    operation_id,
                    cancel: cancel.clone(),
                });
            }
            let (next, _) = self
                .hosted
                .changed
                .wait_timeout(hosted, Duration::from_millis(10))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            hosted = next;
        }
    }

    pub(crate) fn complete_hosted(&self, operation_id: OperationId) {
        self.broker.complete(operation_id);
        let mut hosted = self.lock_hosted();
        if hosted.active.as_ref().map(|operation| operation.id()) == Some(operation_id) {
            hosted.active = None;
        }
        hosted.interrupt_requested = false;
        hosted.background_requested = false;
        drop(hosted);
        let mut background = self.lock_background_current();
        if *background == Some(operation_id) {
            *background = None;
        }
        self.hosted.changed.notify_all();
    }

    pub(crate) fn broker(&self) -> &TuiInteractionBroker {
        &self.broker
    }

    fn lock_background_current(&self) -> MutexGuard<'_, Option<OperationId>> {
        self.background_current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_hosted(&self) -> MutexGuard<'_, HostedOperationInner> {
        self.hosted
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl TuiOperationInterrupt for TuiOperationController {
    fn interrupt_current(&self) {
        let _ = TuiOperationController::interrupt_current(self);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TuiTurnControl {
    controller: TuiOperationController,
    operation_id: OperationId,
    cancel: CancelToken,
}

impl TuiTurnControl {
    pub(crate) fn for_generation(
        controller: TuiOperationController,
        operation_id: OperationId,
        cancel: CancelToken,
    ) -> Self {
        Self {
            controller,
            operation_id,
            cancel,
        }
    }

    pub(crate) fn register_interaction(
        &self,
        kind: TuiInteractionKind,
        request_id: impl Into<String>,
    ) -> io::Result<TuiInteractionWaiter> {
        self.controller
            .wait_for_hosted(self.operation_id, &self.cancel)?
            .controller
            .broker()
            .register(self.operation_id, kind, request_id)
    }

    pub(crate) fn take_background_current(&self) -> bool {
        self.controller.take_background_current(self.operation_id)
    }
}

impl RuntimeProviderSuspensionControl for TuiTurnControl {
    fn take_suspension_request(&self) -> bool {
        TuiTurnControl::take_background_current(self)
    }
}

#[cfg_attr(test, allow(dead_code))]
impl TuiOperationController {
    fn cancel_surface_and_notify(&self) {
        let _ = cancel_surface_if_active(&mut self.lock_hosted());
        self.hosted.changed.notify_all();
    }

    pub(crate) fn install_surface(
        &self,
        client: orca_runtime::surface::RuntimeSurfaceClientHandle,
        operation_id: orca_runtime::surface::SurfaceOperationId,
    ) -> io::Result<()> {
        let interrupt_requested = {
            let mut hosted = self.lock_hosted();
            if hosted.shutdown {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "TUI operation controller is shutting down",
                ));
            }
            if hosted.active.is_some() || hosted.surface_active.is_some() {
                return Err(io::Error::other("TUI operation is still active"));
            }
            hosted.surface_active = Some(SurfaceActiveOperation {
                client: client.clone(),
                operation_id: operation_id.clone(),
                ui_operation_id: self.surface_ids.allocate(),
                interactions: HashMap::new(),
            });
            let requested = hosted.interrupt_requested;
            hosted.interrupt_requested = false;
            requested
        };
        self.hosted.changed.notify_all();
        if interrupt_requested {
            let _ = client
                .cancel_operation(orca_runtime::surface::SurfaceRequestId::new(), operation_id);
        }
        Ok(())
    }

    pub(crate) fn complete_surface(
        &self,
        operation_id: &orca_runtime::surface::SurfaceOperationId,
    ) {
        let mut hosted = self.lock_hosted();
        if hosted
            .surface_active
            .as_ref()
            .is_some_and(|active| &active.operation_id == operation_id)
        {
            hosted.surface_active = None;
        }
        hosted.interrupt_requested = false;
        drop(hosted);
        self.hosted.changed.notify_all();
    }

    pub(crate) fn register_surface_interaction(
        &self,
        interaction: &orca_runtime::unstable_surface::SurfaceInteractionView,
    ) -> Option<TuiEvent> {
        let mut hosted = self.lock_hosted();
        let active = hosted.surface_active.as_mut()?;
        if active.operation_id != interaction.fence.operation_id {
            return None;
        }
        let request_id = format!("{:?}", interaction.interaction_id);
        let (kind, event, permissions) = match &interaction.request {
            orca_runtime::unstable_surface::SurfaceInteractionRequest::ToolApproval {
                tool,
                description,
                preview,
                ..
            } => {
                let key = TuiInteractionKey::new(
                    active.ui_operation_id,
                    request_id.clone(),
                    TuiInteractionKind::Approval,
                );
                (
                    TuiInteractionKind::Approval,
                    TuiEvent::ApprovalNeeded {
                        key,
                        tool: tool.name.as_str().to_string(),
                        target: tool.target.as_ref().map(|value| value.as_str().to_string()),
                        preview: preview
                            .as_ref()
                            .or(Some(description))
                            .map(|value| value.as_str().to_string()),
                    },
                    None,
                )
            }
            orca_runtime::unstable_surface::SurfaceInteractionRequest::PermissionRequest {
                tool_call_id,
                reason,
                permissions,
                ..
            } => {
                let key = TuiInteractionKey::new(
                    active.ui_operation_id,
                    request_id.clone(),
                    TuiInteractionKind::Permission,
                );
                let tool = tool_call_id.as_str().to_string();
                (
                    TuiInteractionKind::Permission,
                    TuiEvent::PermissionApprovalNeeded {
                        key,
                        tool,
                        target: None,
                        preview: reason.as_ref().map(|value| value.as_str().to_string()),
                        permission_kind: permission_kind(permissions),
                    },
                    Some(permissions.clone()),
                )
            }
            orca_runtime::unstable_surface::SurfaceInteractionRequest::UserInput {
                question,
                suggestions,
            } => {
                let key = TuiInteractionKey::new(
                    active.ui_operation_id,
                    request_id.clone(),
                    TuiInteractionKind::UserInput,
                );
                (
                    TuiInteractionKind::UserInput,
                    TuiEvent::UserInputRequested {
                        key,
                        question: question.as_str().to_string(),
                        choices: suggestions
                            .iter()
                            .map(|value| value.as_str().to_string())
                            .collect(),
                    },
                    None,
                )
            }
            orca_runtime::unstable_surface::SurfaceInteractionRequest::McpElicitation {
                server_name,
                message,
                request,
                ..
            } => {
                let key = TuiInteractionKey::new(
                    active.ui_operation_id,
                    request_id.clone(),
                    TuiInteractionKind::McpElicitation,
                );
                let (mode, url, requested_schema_json) = match request {
                    orca_runtime::unstable_surface::SurfaceMcpElicitationRequest::Form {
                        requested_schema,
                        ..
                    } => (
                        orca_runtime::runtime_pending_interaction::RuntimeMcpElicitationMode::Form,
                        None,
                        requested_schema.as_ref().map(|value| {
                            serde_json::to_string(value)
                                .expect("surface MCP schema is serializable")
                        }),
                    ),
                    orca_runtime::unstable_surface::SurfaceMcpElicitationRequest::Url {
                        raw_url,
                        requested_schema,
                    } => (
                        orca_runtime::runtime_pending_interaction::RuntimeMcpElicitationMode::Url,
                        raw_url.as_ref().map(|value| value.as_str().to_string()),
                        requested_schema.as_ref().map(|value| {
                            serde_json::to_string(value)
                                .expect("surface MCP schema is serializable")
                        }),
                    ),
                };
                (
                    TuiInteractionKind::McpElicitation,
                    TuiEvent::McpElicitationRequested {
                        key,
                        server_name: server_name.as_str().to_string(),
                        mode,
                        message: message.as_str().to_string(),
                        url,
                        requested_schema_json,
                    },
                    None,
                )
            }
            _ => return None,
        };
        let key = match &event {
            TuiEvent::ApprovalNeeded { key, .. }
            | TuiEvent::PermissionApprovalNeeded { key, .. }
            | TuiEvent::UserInputRequested { key, .. }
            | TuiEvent::McpElicitationRequested { key, .. } => key.clone(),
            _ => return None,
        };
        active
            .interactions
            .entry(key)
            .or_insert(SurfaceInteractionBinding {
                client: active.client.clone(),
                interaction_id: interaction.interaction_id.clone(),
                kind,
                permissions,
            });
        Some(event)
    }

    pub(crate) fn respond_surface_interaction(
        &self,
        key: &TuiInteractionKey,
        response: &TuiInteractionResponse,
    ) -> io::Result<bool> {
        let binding = {
            let hosted = self.lock_hosted();
            hosted
                .surface_active
                .as_ref()
                .and_then(|active| active.interactions.get(key).cloned())
        };
        let Some(binding) = binding else {
            return Ok(false);
        };
        let answer = match (binding.kind, response) {
            (TuiInteractionKind::Approval, TuiInteractionResponse::Approval(approved)) => {
                orca_runtime::unstable_surface::SurfaceClientInteractionAnswer::ToolApproval {
                    decision: if *approved {
                        orca_runtime::unstable_surface::SurfaceAllowDeny::Allow
                    } else {
                        orca_runtime::unstable_surface::SurfaceAllowDeny::Deny
                    },
                }
            }
            (TuiInteractionKind::Permission, TuiInteractionResponse::Permission(approved)) => {
                let permissions = binding
                    .permissions
                    .clone()
                    .ok_or_else(|| io::Error::other("typed TUI permission profile is missing"))?;
                let decision = if *approved {
                    orca_runtime::unstable_surface::SurfacePermissionClientDecision::Allow {
                        scope: orca_runtime::unstable_surface::PermissionGrantScope::Turn,
                        permissions,
                        strict_auto_review: false,
                    }
                } else {
                    orca_runtime::unstable_surface::SurfacePermissionClientDecision::Deny {
                        scope: orca_runtime::unstable_surface::PermissionGrantScope::Turn,
                        permissions,
                        strict_auto_review: false,
                    }
                };
                orca_runtime::unstable_surface::SurfaceClientInteractionAnswer::PermissionRequest {
                    decision,
                }
            }
            (TuiInteractionKind::UserInput, TuiInteractionResponse::UserInput(answer)) => {
                orca_runtime::unstable_surface::SurfaceClientInteractionAnswer::UserInput {
                    decision: orca_runtime::unstable_surface::SurfaceUserInputDecision::Answer(
                        orca_runtime::unstable_surface::DisplayText::new(answer.clone()),
                    ),
                }
            }
            (
                TuiInteractionKind::McpElicitation,
                TuiInteractionResponse::McpElicitation {
                    accepted,
                    content_json,
                },
            ) => {
                let decision = if *accepted {
                    let content = serde_json::from_str(content_json.as_deref().unwrap_or("{}"))
                        .map_err(|error| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("invalid typed MCP elicitation content: {error}"),
                            )
                        })?;
                    orca_runtime::unstable_surface::SurfaceMcpElicitationDecision::Accept {
                        content,
                    }
                } else {
                    orca_runtime::unstable_surface::SurfaceMcpElicitationDecision::Decline
                };
                orca_runtime::unstable_surface::SurfaceClientInteractionAnswer::McpElicitation {
                    decision,
                }
            }
            _ => return Ok(false),
        };
        match binding.client.respond_interaction_by_id(
            orca_runtime::surface::SurfaceRequestId::new(),
            binding.interaction_id,
            answer,
        ) {
            Ok(orca_runtime::surface::MutationReply::Committed { .. }) => {
                let mut hosted = self.lock_hosted();
                if let Some(active) = hosted.surface_active.as_mut() {
                    active.interactions.remove(key);
                }
                Ok(true)
            }
            Ok(orca_runtime::surface::MutationReply::Deferred { .. })
            | Ok(orca_runtime::surface::MutationReply::Uncommitted { .. }) => Err(
                io::Error::other("typed TUI interaction response was not committed"),
            ),
            Err(error) => Err(io::Error::other(format!(
                "typed TUI interaction response failed: {error:?}"
            ))),
        }
    }

    #[cfg(test)]
    pub(crate) fn has_surface_active(&self) -> bool {
        self.lock_hosted().surface_active.is_some()
    }
}

#[derive(Clone)]
struct SurfaceActiveOperation {
    client: orca_runtime::surface::RuntimeSurfaceClientHandle,
    operation_id: orca_runtime::surface::SurfaceOperationId,
    ui_operation_id: OperationId,
    interactions: HashMap<TuiInteractionKey, SurfaceInteractionBinding>,
}

#[derive(Clone)]
struct SurfaceInteractionBinding {
    client: orca_runtime::surface::RuntimeSurfaceClientHandle,
    interaction_id: orca_runtime::surface::SurfaceInteractionId,
    kind: TuiInteractionKind,
    permissions: Option<orca_runtime::unstable_surface::SurfacePermissionProfile>,
}

impl std::fmt::Debug for SurfaceActiveOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SurfaceActiveOperation")
            .field("operation_id", &self.operation_id)
            .finish_non_exhaustive()
    }
}

fn cancel_surface_if_active(hosted: &mut HostedOperationInner) -> bool {
    let Some(surface) = hosted.surface_active.clone() else {
        return false;
    };
    let _ = surface.client.cancel_operation(
        orca_runtime::surface::SurfaceRequestId::new(),
        surface.operation_id,
    );
    true
}

fn cancel_surface_or_shutdown(hosted: &mut HostedOperationInner) -> bool {
    cancel_surface_if_active(hosted) || hosted.shutdown
}

fn permission_kind(
    profile: &orca_runtime::unstable_surface::SurfacePermissionProfile,
) -> orca_runtime::runtime_permission::RuntimePermissionRequestKind {
    if profile
        .network
        .as_ref()
        .is_some_and(|network| network.enabled == Some(true) || !network.domains.is_empty())
    {
        return orca_runtime::runtime_permission::RuntimePermissionRequestKind::NetworkBlock;
    }
    if profile
        .file_system
        .as_ref()
        .and_then(|filesystem| filesystem.write.as_ref())
        .is_some_and(|paths| !paths.is_empty())
    {
        return orca_runtime::runtime_permission::RuntimePermissionRequestKind::FilesystemWrite;
    }
    profile
        .shell
        .as_ref()
        .and_then(|shell| shell.unsandboxed.then_some(()))
        .map(|_| {
            orca_runtime::runtime_permission::RuntimePermissionRequestKind::UnsandboxedShellRetry
        })
        .unwrap_or(orca_runtime::runtime_permission::RuntimePermissionRequestKind::FilesystemWrite)
}

#[cfg(test)]
mod tests {
    use std::io;

    use crate::test_support::HostedOperationHarness;
    use crate::types::TuiInteractionKind;

    #[test]
    fn completing_hosted_operation_clears_current_and_wakes_waiter() {
        let mut operation = HostedOperationHarness::start();
        let controller = operation.controller().clone();
        let waiter = controller
            .broker()
            .register(
                operation.operation().id(),
                TuiInteractionKind::Approval,
                "approval",
            )
            .expect("register waiter");
        assert_eq!(controller.current_id(), Some(operation.operation().id()));

        operation.finish();

        assert_eq!(controller.current_id(), None);
        assert!(matches!(
            waiter.wait(),
            Err(error) if error.kind() == io::ErrorKind::Interrupted
        ));
    }

    #[test]
    fn hosted_controller_rejects_a_second_active_operation() {
        let first = HostedOperationHarness::start();
        let second = HostedOperationHarness::start();
        let controller = first.controller();

        let error = controller
            .install_hosted(second.operation_handle())
            .expect_err("second active operation must be rejected");

        assert!(error.to_string().contains("still active"));
        assert_eq!(controller.current_id(), Some(first.operation().id()));
    }

    #[test]
    fn background_current_turn_request_is_operation_scoped_and_one_shot() {
        let operation = HostedOperationHarness::start();
        let controller = operation.controller();
        assert!(controller.request_background_current());
        assert!(controller.take_background_current(operation.operation().id()));
        assert!(!controller.take_background_current(operation.operation().id()));
    }
}
