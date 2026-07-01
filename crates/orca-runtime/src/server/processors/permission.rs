use std::io::{self, Write};

use serde_json::Value;

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
