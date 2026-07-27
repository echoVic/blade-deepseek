use std::io::{self, Write};

use serde_json::{Value, json};

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

fn run_user_input_respond<W: Write>(
    state: &mut ServerState,
    request_id: &str,
    answer: Option<String>,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let pending = state.pending_user_inputs.remove(request_id)?;
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
                    "user input request is no longer active: {request_id}"
                )),
            );
        }
        protocol::write_server_event(
            writer,
            &id,
            ServerEvent::UserInputResolved {
                request_id: json!(request_id),
                answered: json!(answer.is_some()),
            },
        )?;
        if pending.sender.send(answer).is_err() {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "user input request is no longer active: {request_id}"
                )),
            );
        }
        return Ok(());
    }
    let pending = state.pending_user_inputs.remove_surface(request_id)?;
    let Some(pending) = pending else {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(format!("unknown user input request: {request_id}")),
        );
    };
    let restore = |state: &mut ServerState| {
        state
            .pending_user_inputs
            .restore_surface(request_id.to_string(), pending.clone())
    };
    let answered = answer.is_some();
    let decision = match answer {
        Some(answer) => crate::unstable_surface::SurfaceUserInputDecision::Answer(
            crate::unstable_surface::DisplayText::new(answer),
        ),
        None => crate::unstable_surface::SurfaceUserInputDecision::Cancel,
    };
    match pending.client.respond_interaction_by_id(
        crate::unstable_surface::SurfaceRequestId::new(),
        pending.interaction_id.clone(),
        crate::unstable_surface::SurfaceClientInteractionAnswer::UserInput { decision },
    ) {
        Ok(crate::unstable_surface::MutationReply::Committed { .. }) => {}
        Ok(crate::unstable_surface::MutationReply::Deferred { .. }) => {
            restore(state)?;
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "user input response is awaiting durable reconciliation: {request_id}"
                )),
            );
        }
        Ok(crate::unstable_surface::MutationReply::Uncommitted { .. }) | Err(_) => {
            restore(state)?;
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "user input request is no longer active: {request_id}"
                )),
            );
        }
    }
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::UserInputResolved {
            request_id: json!(request_id),
            answered: json!(answered),
        },
    )
}
