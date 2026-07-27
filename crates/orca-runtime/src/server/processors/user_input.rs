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
    let pending = state.direct_interactions.published_route(
        request_id,
        direct_interaction_adapter::JsonlDirectInteractionKind::UserInput,
    )?;
    let pending = match pending {
        Some(direct_interaction_adapter::JsonlDirectInteractionRoute::UserInput {
            client,
            interaction_id,
        }) => Some((client, interaction_id)),
        Some(direct_interaction_adapter::JsonlDirectInteractionRoute::McpElicitation {
            ..
        })
        | None => None,
    };
    let Some(pending) = pending else {
        return match state
            .direct_interactions
            .committed_replay(request_id, response_digest)?
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
    match pending.0.respond_interaction_by_id(
        response_request_id,
        pending.1,
        crate::unstable_surface::SurfaceClientInteractionAnswer::UserInput { decision },
    ) {
        Ok(crate::unstable_surface::MutationReply::Committed { .. }) => {}
        Ok(crate::unstable_surface::MutationReply::Deferred { mutation, .. }) => {
            state.direct_interactions.mark_committed_pending(
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
        .direct_interactions
        .settle_committed(request_id, response_digest)?;
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::UserInputResolved {
            request_id: json!(request_id),
            answered: json!(answered),
        },
    )
}
