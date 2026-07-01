use std::io;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::super::*;

pub(in crate::server::router) fn is_control_operation(op: &ClientOp) -> bool {
    matches!(
        op,
        ClientOp::TurnInterrupt { .. } | ClientOp::TurnResume { .. } | ClientOp::TurnSteer { .. }
    )
}

pub(in crate::server::router) fn dispatch_control_operation<W: Write + Send + 'static>(
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: Arc<Mutex<W>>,
) -> io::Result<()> {
    match op {
        ClientOp::TurnInterrupt { thread_id, turn_id } => run_turn_control(
            state,
            "interrupt",
            thread_id.as_deref(),
            turn_id,
            None,
            id,
            writer,
        ),
        ClientOp::TurnResume { thread_id, turn_id } => run_turn_control(
            state,
            "resume",
            thread_id.as_deref(),
            turn_id,
            None,
            id,
            writer,
        ),
        ClientOp::TurnSteer {
            thread_id,
            turn_id,
            input,
        } => run_turn_control(
            state,
            "steer",
            thread_id.as_deref(),
            turn_id,
            Some(input),
            id,
            writer,
        ),
        _ => unreachable!("only turn control operations can reach the turn processor"),
    }
}
