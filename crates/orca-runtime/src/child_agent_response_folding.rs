use orca_approval::ApprovalPolicy;
use orca_core::event_schema::RunStatus;
use orca_core::provider_types::{ProviderResponse, ProviderStep};
use orca_core::tool_types::{ToolRequest, ToolResult};
use orca_mcp::McpRegistry;

use crate::agent_common;
use crate::child_agent_loop_setup::ChildAgentLoopSetup;
use crate::child_agent_types::ChildAgentResult;
use crate::cost::CostTracker;

pub enum ChildAgentProviderResponseFold {
    Complete(ChildAgentResult),
    ContinueToTools,
}

pub enum ChildAgentToolResultFold {
    Continue,
    Stop(ChildAgentResult),
}

pub struct ChildAgentToolExecution {
    pub should_stop: bool,
    pub result: ToolResult,
    pub child_cost: Option<CostTracker>,
}

pub struct ChildAgentToolContext<'a> {
    pub policy: &'a ApprovalPolicy,
    pub mcp_registry: &'a McpRegistry,
}

pub fn fold_child_agent_provider_response(
    setup: &mut ChildAgentLoopSetup,
    response: &ProviderResponse,
    child_cost_tracker: &mut CostTracker,
) -> ChildAgentProviderResponseFold {
    if let Some(usage) = response.usage
        && !usage.is_empty()
    {
        child_cost_tracker.add_usage(usage);
    }

    if response.tool_calls.is_empty() {
        setup.conversation.add_assistant(
            response.assistant_content.clone(),
            response.assistant_reasoning.clone(),
            vec![],
        );
        return ChildAgentProviderResponseFold::Complete(ChildAgentResult {
            status: RunStatus::Success,
            final_message: response.assistant_content.clone(),
            error: None,
        });
    }

    setup.conversation.add_assistant(
        response.assistant_content.clone(),
        response.assistant_reasoning.clone(),
        response.tool_calls.clone(),
    );
    ChildAgentProviderResponseFold::ContinueToTools
}

pub fn child_agent_tool_requests(response: &ProviderResponse) -> Vec<&ToolRequest> {
    response
        .steps
        .iter()
        .filter_map(|step| match step {
            ProviderStep::ToolCall(request) => Some(request),
            _ => None,
        })
        .collect()
}

pub fn fold_child_agent_tool_result(
    setup: &mut ChildAgentLoopSetup,
    tool_request: &ToolRequest,
    should_stop: bool,
    result: ToolResult,
    child_cost: Option<CostTracker>,
    child_cost_tracker: &mut CostTracker,
) -> ChildAgentToolResultFold {
    if let Some(cost) = child_cost {
        child_cost_tracker.merge(&cost);
    }

    let result_content = agent_common::format_tool_result_for_model(&result);
    setup
        .conversation
        .add_tool_result(tool_request.id.clone(), result_content);

    if should_stop {
        return ChildAgentToolResultFold::Stop(ChildAgentResult {
            status: RunStatus::Failed,
            final_message: None,
            error: result.error,
        });
    }

    ChildAgentToolResultFold::Continue
}
