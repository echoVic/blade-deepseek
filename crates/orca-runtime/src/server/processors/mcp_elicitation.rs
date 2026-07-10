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
    let Some(pending) = pending else {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(format!("unknown MCP elicitation request: {request_id}")),
        );
    };
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
    Ok(())
}
