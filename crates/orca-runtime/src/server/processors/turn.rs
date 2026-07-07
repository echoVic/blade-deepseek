use std::io;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

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

fn run_turn_control<W: Write + Send + 'static>(
    state: &mut ServerState,
    action: &str,
    thread_id: Option<&str>,
    turn_id: &str,
    input: Option<&String>,
    id: Value,
    writer: Arc<Mutex<W>>,
) -> io::Result<()> {
    let mut steered_item = None;
    let status = if let Some(control) = state.active_turns.get_mut(turn_id) {
        if let Some(expected_thread_id) = thread_id {
            if expected_thread_id != control.thread_id {
                return write_locked_event(
                    &writer,
                    &id,
                    ServerEvent::error(format!(
                        "turn {turn_id} does not belong to thread {expected_thread_id}"
                    )),
                );
            }
        }
        match action {
            "interrupt" => {
                control.cancel.cancel();
                "interrupted"
            }
            "resume" => {
                control.cancel.reset();
                "resumed"
            }
            "steer" => {
                if let Some(input) = input {
                    control.steer_handle.push(input.clone());
                    steered_item = Some((control.thread_id.clone(), input.clone()));
                }
                "steered"
            }
            _ => "running",
        }
    } else if let Some(actual_thread_id) = state.threads.completed_turn_thread_id(turn_id) {
        if let Some(expected_thread_id) = thread_id {
            if expected_thread_id != actual_thread_id {
                return write_locked_event(
                    &writer,
                    &id,
                    ServerEvent::error(format!(
                        "turn {turn_id} does not belong to thread {expected_thread_id}"
                    )),
                );
            }
        }
        return write_locked_event(
            &writer,
            &id,
            ServerEvent::error(format!("turn is not active: {turn_id}")),
        );
    } else {
        "idle"
    };
    write_locked_event(
        &writer,
        &id,
        ServerEvent::TurnControlled {
            action: Value::from(action.to_string()),
            turn_id: Value::from(turn_id.to_string()),
            status: Value::from(status),
            input: input
                .map(|input| Value::from(input.clone()))
                .unwrap_or(Value::Null),
        },
    )?;
    if let Some((thread_id, input)) = steered_item {
        write_locked_event(
            &writer,
            &id,
            ServerEvent::ItemStarted {
                thread_id: Value::from(thread_id),
                turn_id: Value::from(turn_id.to_string()),
                item: json!({
                    "type": "user_message",
                    "role": "user",
                    "content": input,
                }),
            },
        )?;
    }
    Ok(())
}
