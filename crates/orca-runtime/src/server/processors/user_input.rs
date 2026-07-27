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
    let response_digest = jsonl_response_digest(&json!({ "answer": &answer }))?;
    let pending = state.pending_user_inputs.route_legacy(request_id)?;
    if let Some(pending) = pending {
        let (pending_thread_id, pending_turn_id, generation) = pending.generation_scope();
        if !state
            .threads
            .accepts_generation(pending_turn_id, pending_thread_id, generation)
        {
            state.pending_user_inputs.settle_legacy(request_id)?;
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
            state.pending_user_inputs.settle_legacy(request_id)?;
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "user input request is no longer active: {request_id}"
                )),
            );
        }
        state.pending_user_inputs.settle_legacy(request_id)?;
        return Ok(());
    }
    let pending = state.pending_user_inputs.surface_route(request_id)?;
    let Some(pending) = pending else {
        return match state
            .pending_user_inputs
            .surface_committed_replay(request_id, response_digest)?
        {
            JsonlCommittedReplay::SameResponse => protocol::write_server_event(
                writer,
                &id,
                ServerEvent::UserInputResolved {
                    request_id: json!(request_id),
                    answered: json!(answer.is_some()),
                },
            ),
            JsonlCommittedReplay::ConflictingResponse => protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "user input request already resolved with a different response: {request_id}"
                )),
            ),
            JsonlCommittedReplay::NotCommitted => protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!("unknown user input request: {request_id}")),
            ),
        };
    };
    let answered = answer.is_some();
    let decision = match answer {
        Some(answer) => crate::unstable_surface::SurfaceUserInputDecision::Answer(
            crate::unstable_surface::DisplayText::new(answer),
        ),
        None => crate::unstable_surface::SurfaceUserInputDecision::Cancel,
    };
    let response_request_id = crate::unstable_surface::SurfaceRequestId::new();
    match pending.client.respond_interaction_by_id(
        response_request_id,
        pending.interaction_id.clone(),
        crate::unstable_surface::SurfaceClientInteractionAnswer::UserInput { decision },
    ) {
        Ok(crate::unstable_surface::MutationReply::Committed { .. }) => {}
        Ok(crate::unstable_surface::MutationReply::Deferred { mutation, .. }) => {
            state.pending_user_inputs.mark_surface_committed_pending(
                request_id,
                &mutation,
                response_digest,
            )?;
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "user input response is awaiting durable reconciliation: {request_id}"
                )),
            );
        }
        Ok(crate::unstable_surface::MutationReply::Uncommitted { .. }) | Err(_) => {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "user input request is no longer active: {request_id}"
                )),
            );
        }
    }
    state
        .pending_user_inputs
        .settle_surface(request_id, response_digest)?;
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::UserInputResolved {
            request_id: json!(request_id),
            answered: json!(answered),
        },
    )
}
