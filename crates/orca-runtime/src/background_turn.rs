use orca_core::provider_types::{ProviderResponse, ProviderStep};
use orca_core::thread_identity::TurnId;

use crate::model_response::RuntimeModelResponse;
use crate::tasks::TaskRegistry;

#[derive(Clone, Debug)]
pub struct ApprovedBackgroundTurnContinuation {
    pub response: RuntimeModelResponse,
    pub preapproved_tool_call_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RuntimeTurnContinuation {
    pub response: RuntimeModelResponse,
    pub preapproved_tool_call_id: Option<String>,
}

impl ApprovedBackgroundTurnContinuation {
    pub fn into_runtime_turn_continuation(self) -> RuntimeTurnContinuation {
        RuntimeTurnContinuation {
            response: self.response,
            preapproved_tool_call_id: self.preapproved_tool_call_id,
        }
    }
}

impl RuntimeTurnContinuation {
    pub fn from_response(response: ProviderResponse, turn_id: TurnId) -> Self {
        Self {
            response: RuntimeModelResponse::new(response, turn_id),
            preapproved_tool_call_id: None,
        }
    }

    pub fn preapproved_tool_call_id(&self) -> Option<&str> {
        self.preapproved_tool_call_id.as_deref()
    }
}

pub fn take_approved_background_turn_continuation(
    task_registry: &TaskRegistry,
    task_id: &str,
) -> Result<Option<ApprovedBackgroundTurnContinuation>, String> {
    let Some(response) = task_registry.take_approved_pending_provider_response(task_id)? else {
        return Ok(None);
    };
    let preapproved_tool_call_id = provider_response_first_tool_call_id(&response.response);
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

#[cfg(test)]
mod tests {
    use orca_core::approval_types::ActionKind;
    use orca_core::provider_types::{ProviderResponse, ProviderStep};
    use orca_core::thread_identity::TurnId;
    use orca_core::tool_types::{ToolName, ToolRequest};

    use super::ApprovedBackgroundTurnContinuation;
    use crate::model_response::RuntimeModelResponse;

    #[test]
    fn approved_background_continuation_converts_to_runtime_turn_continuation() {
        let response = ProviderResponse {
            steps: vec![ProviderStep::ToolCall(ToolRequest {
                id: "shell-1".to_string(),
                name: ToolName::Bash,
                action: ActionKind::Shell,
                target: Some("echo hi".to_string()),
                raw_arguments: None,
            })],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };

        let continuation = ApprovedBackgroundTurnContinuation {
            response: RuntimeModelResponse::new(response, TurnId::new()),
            preapproved_tool_call_id: Some("shell-1".to_string()),
        }
        .into_runtime_turn_continuation();

        assert_eq!(continuation.preapproved_tool_call_id(), Some("shell-1"));
    }
}
