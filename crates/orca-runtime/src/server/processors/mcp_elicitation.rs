use std::io::{self, Write};

use orca_mcp::McpElicitationResponse;
use serde_json::{Value, json};

use super::super::*;

pub(in crate::server::router) fn is_mcp_elicitation_operation(op: &ClientOp) -> bool {
    matches!(op, ClientOp::McpElicitationRespond { .. })
}

pub(in crate::server::router) fn dispatch_mcp_elicitation_operation<W: Write>(
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    match op {
        ClientOp::McpElicitationRespond {
            request_id,
            accepted,
            content_json,
        } => run_mcp_elicitation_respond(
            state,
            request_id,
            *accepted,
            content_json.clone(),
            id,
            writer,
        ),
        _ => {
            unreachable!("only MCP elicitation operations can reach the MCP elicitation processor")
        }
    }
}

fn run_mcp_elicitation_respond<W: Write>(
    state: &mut ServerState,
    request_id: &str,
    accepted: bool,
    content_json: Option<Value>,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let pending = state.pending_mcp_elicitations.remove(request_id)?;
    if let Some(pending) = pending {
        let (pending_thread_id, pending_turn_id, generation) = pending.generation_scope();
        if !state
            .threads
            .accepts_generation(pending_turn_id, pending_thread_id, generation)
        {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "MCP elicitation request is no longer active: {request_id}"
                )),
            );
        }
        protocol::write_server_event(
            writer,
            &id,
            ServerEvent::McpElicitationResolved {
                request_id: json!(request_id),
                accepted: json!(accepted),
            },
        )?;
        let response = if accepted {
            McpElicitationResponse::accept(content_json.unwrap_or_else(|| json!({})))
        } else {
            McpElicitationResponse::decline()
        };
        if pending.sender.send(response).is_err() {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "MCP elicitation request is no longer active: {request_id}"
                )),
            );
        }
        return Ok(());
    }
    let decision = if accepted {
        crate::unstable_surface::SurfaceMcpElicitationDecision::Accept {
            content: json_to_surface_data(content_json.unwrap_or_else(|| json!({})))?,
        }
    } else {
        crate::unstable_surface::SurfaceMcpElicitationDecision::Decline
    };
    let pending = state.pending_mcp_elicitations.remove_surface(request_id)?;
    let Some(pending) = pending else {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(format!("unknown MCP elicitation request: {request_id}")),
        );
    };
    let restore = |state: &mut ServerState| {
        state
            .pending_mcp_elicitations
            .restore_surface(request_id.to_string(), pending.clone())
    };
    match pending.client.respond_interaction_by_id(
        crate::unstable_surface::SurfaceRequestId::new(),
        pending.interaction_id.clone(),
        crate::unstable_surface::SurfaceClientInteractionAnswer::McpElicitation { decision },
    ) {
        Ok(crate::unstable_surface::MutationReply::Committed { .. }) => {}
        Ok(crate::unstable_surface::MutationReply::Deferred { .. }) => {
            restore(state)?;
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "MCP elicitation response is awaiting durable reconciliation: {request_id}"
                )),
            );
        }
        Ok(crate::unstable_surface::MutationReply::Uncommitted { .. }) | Err(_) => {
            restore(state)?;
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "MCP elicitation request is no longer active: {request_id}"
                )),
            );
        }
    }
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::McpElicitationResolved {
            request_id: json!(request_id),
            accepted: json!(accepted),
        },
    )
}

fn json_to_surface_data(value: Value) -> io::Result<crate::unstable_surface::SurfaceDataValue> {
    Ok(match value {
        Value::Null => crate::unstable_surface::SurfaceDataValue::Null,
        Value::Bool(value) => crate::unstable_surface::SurfaceDataValue::Boolean(value),
        Value::Number(value) => {
            if let Some(value) = value.as_u64() {
                crate::unstable_surface::SurfaceDataValue::Unsigned(value)
            } else if let Some(value) = value.as_i64().filter(|value| *value < 0) {
                crate::unstable_surface::SurfaceDataValue::Integer(
                    crate::unstable_surface::NegativeI64::try_new(value)
                        .map_err(|error| io::Error::other(error.to_string()))?,
                )
            } else {
                crate::unstable_surface::SurfaceDataValue::Number(
                    crate::unstable_surface::FiniteF64::try_new(
                        value
                            .as_f64()
                            .ok_or_else(|| io::Error::other("invalid MCP response number"))?,
                    )
                    .map_err(|error| io::Error::other(error.to_string()))?,
                )
            }
        }
        Value::String(value) => crate::unstable_surface::SurfaceDataValue::String(
            crate::unstable_surface::DisplayText::new(value),
        ),
        Value::Array(values) => crate::unstable_surface::SurfaceDataValue::Array(
            values
                .into_iter()
                .map(json_to_surface_data)
                .collect::<io::Result<Vec<_>>>()?,
        ),
        Value::Object(values) => crate::unstable_surface::SurfaceDataValue::Object(
            values
                .into_iter()
                .map(|(name, value)| {
                    Ok(crate::unstable_surface::SurfaceDataProperty {
                        name: crate::unstable_surface::DisplayText::new(name),
                        value: Box::new(json_to_surface_data(value)?),
                    })
                })
                .collect::<io::Result<Vec<_>>>()?,
        ),
    })
}
