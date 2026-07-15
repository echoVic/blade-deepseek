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
            .active_turns
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
    {
        let session_grants = persist_session_permission_grant(
            pending.thread_id(),
            pending.runtime_workspace_roots(),
            &permissions,
        )?;
        state.active_turns.apply_session_permission_grant(
            pending.thread_id(),
            session_grants.additional_working_directories.clone(),
            session_grants.network_domain_permissions.clone(),
        );
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
