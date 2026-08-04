use std::io;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::super::*;
use crate::tool_item_projection::user_message_item;

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
    state.prune_finished_turns();
    let active_thread_id = state.threads.resolve_turn_thread_id(turn_id);
    let known_thread_id = active_thread_id
        .clone()
        .or_else(|| state.threads.resolve_known_turn_thread_id(turn_id));
    if let (Some(requested), Some(actual)) = (thread_id, known_thread_id.as_deref())
        && requested != actual
    {
        return write_locked_event(
            &writer,
            &id,
            ServerEvent::error(format!(
                "turn {turn_id} does not belong to thread {requested}"
            )),
        );
    }
    if active_thread_id.is_none() && known_thread_id.is_some() {
        return write_locked_event(
            &writer,
            &id,
            ServerEvent::error(format!("turn is not active: {turn_id}")),
        );
    }
    let resolved_thread_id = active_thread_id.or_else(|| thread_id.map(ToString::to_string));
    let surface_action = match action {
        "interrupt" => crate::surface::JsonlTurnControlAction::Interrupt,
        "resume" => crate::surface::JsonlTurnControlAction::Resume,
        "steer" => crate::surface::JsonlTurnControlAction::Steer {
            input: crate::surface::SurfaceInputRequest {
                blocks: crate::surface::NonEmptyVec::try_new(vec![
                    crate::surface::SurfaceInputRequestBlock::Text {
                        text: crate::surface::DisplayText::new(input.cloned().unwrap_or_default()),
                    },
                ])
                .map_err(|error| io::Error::other(error.to_string()))?,
            },
        },
        _ => unreachable!("closed JSONL turn action"),
    };
    let result =
        match state
            .threads
            .control_turn(resolved_thread_id.as_deref(), turn_id, surface_action)
        {
            Ok(result) => result,
            Err(error) => {
                return write_locked_event(&writer, &id, ServerEvent::error(error.to_string()));
            }
        };
    let (status, steered_item) = match result {
        crate::surface::JsonlTurnControlResult::Idle { .. } => ("idle", None),
        crate::surface::JsonlTurnControlResult::Resolved { mutation } => match mutation {
            crate::surface::MutationReply::Committed { value, .. } => {
                let status = match value.echo.status {
                    crate::surface::JsonlResolvedTurnControlStatus::Interrupted => "interrupted",
                    crate::surface::JsonlResolvedTurnControlStatus::Resumed => "resumed",
                    crate::surface::JsonlResolvedTurnControlStatus::Steered => "steered",
                };
                let steered = value.input_item_id.map(|_| {
                    (
                        resolved_thread_id.clone().unwrap_or_default(),
                        input.cloned().unwrap_or_default(),
                    )
                });
                (status, steered)
            }
            crate::surface::MutationReply::Deferred { .. } => {
                return write_locked_event(
                    &writer,
                    &id,
                    ServerEvent::error("turn control is awaiting durable reconciliation"),
                );
            }
            crate::surface::MutationReply::Uncommitted { .. } => {
                return write_locked_event(
                    &writer,
                    &id,
                    ServerEvent::error(format!("turn is not active: {turn_id}")),
                );
            }
        },
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
    if let Some((thread_id, input)) = steered_item
        && !thread_id.is_empty()
    {
        write_locked_event(
            &writer,
            &id,
            ServerEvent::ItemStarted {
                thread_id: Value::from(thread_id),
                turn_id: Value::from(turn_id.to_string()),
                item: user_message_item(input),
            },
        )?;
    }
    Ok(())
}
