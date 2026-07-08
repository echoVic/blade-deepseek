use std::io::{self, Write};

use serde_json::Value;

use super::super::*;

pub(in crate::server::router) fn is_user_input_operation(op: &ClientOp) -> bool {
    matches!(op, ClientOp::UserInputRespond { .. })
}

pub(in crate::server::router) fn dispatch_user_input_operation<W: Write>(
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    match op {
        ClientOp::UserInputRespond { request_id, answer } => {
            run_user_input_respond(state, request_id, answer.clone(), id, writer)
        }
        _ => unreachable!("only user input operations can reach the user input processor"),
    }
}
