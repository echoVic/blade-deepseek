use orca_core::provider_types::{ProviderResponse, ProviderStep};

use crate::tasks::TaskRegistry;

#[derive(Clone, Debug)]
pub struct ApprovedBackgroundTurnContinuation {
    pub response: ProviderResponse,
    pub preapproved_tool_call_id: Option<String>,
}

pub fn take_approved_background_turn_continuation(
    task_registry: &TaskRegistry,
    task_id: &str,
) -> Result<Option<ApprovedBackgroundTurnContinuation>, String> {
    let Some(response) = task_registry.take_approved_pending_provider_response(task_id)? else {
        return Ok(None);
    };
    let preapproved_tool_call_id = provider_response_first_tool_call_id(&response);
    Ok(Some(ApprovedBackgroundTurnContinuation {
        response,
        preapproved_tool_call_id,
    }))
}

fn provider_response_first_tool_call_id(response: &ProviderResponse) -> Option<String> {
    response
        .steps
        .iter()
        .find_map(|step| match step {
            ProviderStep::ToolCall(request) => Some(request.id.clone()),
            _ => None,
        })
        .or_else(|| {
            response
                .tool_calls
                .first()
                .map(|tool_call| tool_call.id.clone())
        })
}
