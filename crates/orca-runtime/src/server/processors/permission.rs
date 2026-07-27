use std::io::{self, Write};

use serde_json::{Value, json};

use super::super::*;

pub(in crate::server::router) fn is_permission_operation(op: &ClientOp) -> bool {
    matches!(op, ClientOp::PermissionRespond { .. })
}

pub(in crate::server::router) fn dispatch_permission_operation<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    match op {
        ClientOp::PermissionRespond {
            request_id,
            decision,
            scope,
            permissions,
            strict_auto_review,
        } => run_permission_respond(
            config,
            state,
            request_id,
            *decision,
            *scope,
            permissions.clone(),
            *strict_auto_review,
            id,
            writer,
        ),
        _ => unreachable!("only permission operations can reach the permission processor"),
    }
}

fn run_permission_respond<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    request_id: &str,
    decision: protocol::PermissionResponseDecision,
    scope: protocol::PermissionGrantScope,
    permissions: protocol::RequestPermissionProfile,
    strict_auto_review: bool,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let pending = state.pending_permissions.remove(request_id)?;
    let Some(pending) = pending else {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(format!("unknown permission request: {request_id}")),
        );
    };
    if let Some((pending_thread_id, pending_turn_id, generation)) = pending.runtime_generation()
        && !state
            .threads
            .accepts_generation(pending_turn_id, pending_thread_id, generation)
    {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(format!(
                "permission request is no longer active: {request_id}"
            )),
        );
    }
    if decision == protocol::PermissionResponseDecision::Allow
        && scope == protocol::PermissionGrantScope::Session
        && !matches!(pending, PendingPermissionRequest::Surface { .. })
    {
        let session_grants = persist_session_permission_grant(
            pending.thread_id(),
            pending.runtime_workspace_roots(),
            &permissions,
        )?;
        state.threads.update_thread_metadata(
            pending.thread_id(),
            ThreadMetadataPatch {
                title: None,
                active_permission_profile: None,
                approval_mode: None,
                runtime_workspace_roots: None,
                permission_rules: None,
                additional_working_directories: Some(session_grants.additional_working_directories),
                network_domain_permissions: Some(session_grants.network_domain_permissions),
            },
        );
    }
    if let PendingPermissionRequest::Surface {
        client,
        interaction_id,
        target,
        thread_id,
        runtime_workspace_roots,
    } = pending
    {
        let restore = |state: &mut ServerState| {
            state.pending_permissions.restore(
                request_id.to_string(),
                PendingPermissionRequest::Surface {
                    client: client.clone(),
                    interaction_id: interaction_id.clone(),
                    target: target.clone(),
                    thread_id: thread_id.clone(),
                    runtime_workspace_roots: runtime_workspace_roots.clone(),
                },
            )
        };
        let permissions = materialize_surface_permission_profile(
            state,
            &thread_id,
            &runtime_workspace_roots,
            permissions,
        )?;
        let allow = decision == protocol::PermissionResponseDecision::Allow;
        if allow
            && scope == protocol::PermissionGrantScope::Session
            && let Err(error) = state.threads.persist_session_permission_grant(
                &thread_id,
                &client,
                &runtime_workspace_roots,
                &permissions,
            )
        {
            restore(state)?;
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "session permission settings did not commit: {error}"
                )),
            );
        }
        let answer = match &target {
            crate::unstable_surface::SurfaceInteractionKind::ToolApproval => {
                crate::unstable_surface::SurfaceClientInteractionAnswer::ToolApproval {
                    decision: if allow {
                        crate::unstable_surface::SurfaceAllowDeny::Allow
                    } else {
                        crate::unstable_surface::SurfaceAllowDeny::Deny
                    },
                }
            }
            crate::unstable_surface::SurfaceInteractionKind::PermissionRequest => {
                let scope = match scope {
                    protocol::PermissionGrantScope::Turn => {
                        crate::unstable_surface::PermissionGrantScope::Turn
                    }
                    protocol::PermissionGrantScope::Session => {
                        crate::unstable_surface::PermissionGrantScope::Session
                    }
                };
                let permissions = surface_permission_profile(&permissions);
                let decision = if allow {
                    crate::unstable_surface::SurfacePermissionClientDecision::Allow {
                        scope,
                        permissions,
                        strict_auto_review,
                    }
                } else {
                    crate::unstable_surface::SurfacePermissionClientDecision::Deny {
                        scope,
                        permissions,
                        strict_auto_review,
                    }
                };
                crate::unstable_surface::SurfaceClientInteractionAnswer::PermissionRequest {
                    decision,
                }
            }
            _ => {
                restore(state)?;
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!(
                        "permission request has incompatible interaction kind: {request_id}"
                    )),
                );
            }
        };
        match client.respond_interaction_by_id(
            crate::unstable_surface::SurfaceRequestId::new(),
            interaction_id.clone(),
            answer,
        ) {
            Ok(crate::unstable_surface::MutationReply::Committed { .. }) => {}
            Ok(crate::unstable_surface::MutationReply::Deferred { .. }) => {
                restore(state)?;
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!(
                        "permission response is awaiting durable reconciliation: {request_id}"
                    )),
                );
            }
            Ok(crate::unstable_surface::MutationReply::Uncommitted { .. }) => {
                restore(state)?;
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!(
                        "permission request is no longer active: {request_id}"
                    )),
                );
            }
            Err(_) => {
                restore(state)?;
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!(
                        "permission request is no longer active: {request_id}"
                    )),
                );
            }
        }
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::PermissionResolved {
                request_id: json!(request_id),
                decision: json!(decision),
                scope: json!(scope),
                strict_auto_review: json!(strict_auto_review),
            },
        );
    }
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::PermissionResolved {
            request_id: json!(request_id),
            decision: json!(decision),
            scope: json!(scope),
            strict_auto_review: json!(strict_auto_review),
        },
    )?;
    match pending {
        PendingPermissionRequest::Runtime { sender, .. } => {
            if sender
                .send(RuntimePermissionResponse {
                    decision,
                    scope,
                    permissions,
                    strict_auto_review,
                })
                .is_err()
            {
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!(
                        "permission request is no longer active: {request_id}"
                    )),
                );
            }
            Ok(())
        }
        PendingPermissionRequest::Surface { .. } => unreachable!("surface response returned above"),
        PendingPermissionRequest::CommandExec { mut request } => {
            if decision != protocol::PermissionResponseDecision::Allow {
                return protocol::write_server_event(
                    writer,
                    &request.event_id,
                    ServerEvent::error(format!("command/exec permission denied: {request_id}")),
                );
            }
            if permissions
                .shell
                .as_ref()
                .is_some_and(|shell| shell.unsandboxed)
            {
                request.options.permission_profile = None;
                request.options.sandbox_policy = protocol::CommandSandboxPolicy::DangerFullAccess;
            }
            run_command_exec(
                config,
                state,
                Some(&request.thread_id),
                &request.command,
                request.process_id.as_deref(),
                request.cwd.as_ref(),
                &request.env,
                &request.options,
                request.terminal,
                request.event_id,
                writer,
            )
        }
    }
}

fn materialize_surface_permission_profile(
    state: &ServerState,
    thread_id: &str,
    runtime_workspace_roots: &[std::path::PathBuf],
    mut permissions: protocol::RequestPermissionProfile,
) -> io::Result<protocol::RequestPermissionProfile> {
    let cwd = state
        .threads
        .thread(thread_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "permission thread is missing"))?
        .cwd()
        .to_string();
    if let Some(file_system) = permissions.file_system.as_mut() {
        for paths in [&mut file_system.read, &mut file_system.write]
            .into_iter()
            .flatten()
        {
            let mut materialized = Vec::new();
            for path in std::mem::take(paths) {
                for path in materialize_workspace_roots_paths(&cwd, runtime_workspace_roots, &path)
                {
                    if !materialized.contains(&path) {
                        materialized.push(path);
                    }
                }
            }
            *paths = materialized;
        }
    }
    Ok(permissions)
}

fn surface_permission_profile(
    permissions: &protocol::RequestPermissionProfile,
) -> crate::unstable_surface::SurfacePermissionProfile {
    crate::unstable_surface::SurfacePermissionProfile {
        file_system: permissions.file_system.as_ref().map(|file_system| {
            crate::unstable_surface::SurfaceFileSystemPermissionProfile {
                read: file_system.read.as_ref().map(|paths| {
                    paths
                        .iter()
                        .map(|path| {
                            crate::unstable_surface::SurfacePermissionPathLabel(
                                crate::unstable_surface::DisplayText::new(
                                    path.to_string_lossy().to_string(),
                                ),
                            )
                        })
                        .collect()
                }),
                write: file_system.write.as_ref().map(|paths| {
                    paths
                        .iter()
                        .map(|path| {
                            crate::unstable_surface::SurfacePermissionPathLabel(
                                crate::unstable_surface::DisplayText::new(
                                    path.to_string_lossy().to_string(),
                                ),
                            )
                        })
                        .collect()
                }),
            }
        }),
        network: permissions.network.as_ref().map(|network| {
            crate::unstable_surface::SurfacePermissionNetworkProfile {
                enabled: network.enabled,
                domains: network
                    .domains
                    .iter()
                    .map(|(domain, access)| {
                        (
                            crate::unstable_surface::SurfacePermissionDomainPattern(
                                crate::unstable_surface::DisplayText::new(domain.clone()),
                            ),
                            match access {
                                orca_core::config::PermissionProfileNetworkAccess::Allow => {
                                    crate::unstable_surface::SurfaceAllowDeny::Allow
                                }
                                orca_core::config::PermissionProfileNetworkAccess::Deny => {
                                    crate::unstable_surface::SurfaceAllowDeny::Deny
                                }
                            },
                        )
                    })
                    .collect(),
            }
        }),
        shell: permissions.shell.as_ref().map(|shell| {
            crate::unstable_surface::SurfaceShellPermissionProfile {
                unsandboxed: shell.unsandboxed,
            }
        }),
    }
}
