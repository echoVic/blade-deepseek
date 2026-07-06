use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::event_schema::RunStatus;
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;
use orca_provider::context::ContextConfig;
use orca_provider::tool_schema::deepseek_tools_schema_for_type_with_mcp_and_external;

use crate::agent_child::{ChildAgentRequest, ChildAgentResult};
use crate::agent_common;
use crate::instructions::ProjectInstructions;
use crate::memory::MemoryBlock;

pub const DEFAULT_CHILD_AGENT_MAX_TURNS: u32 = 128;

pub struct ChildAgentLoopSetup {
    pub mcp_registry: McpRegistry,
    pub provider_config: ProviderConfig,
    pub context_config: ContextConfig,
    pub conversation: Conversation,
    pub policy: ApprovalPolicy,
    pub(crate) turn: u32,
    pub(crate) reactive_compacted: bool,
}

pub enum ChildAgentTurnBudget {
    Continue,
    Stop(ChildAgentResult),
}

pub fn prepare_child_agent_loop(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
) -> ChildAgentLoopSetup {
    let mcp_registry = orca_mcp::initialize_registry(&config.mcp_servers);
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: Some(orca_core::model::FLASH_MODEL.to_string()),
        reasoning_effort: config.reasoning_effort,
        tools_override: Some(deepseek_tools_schema_for_type_with_mcp_and_external(
            &request.subagent_type,
            Some(&mcp_registry),
            &config.external_tools,
        )),
        mcp_registry: Some(mcp_registry.clone()),
        external_tools: config.external_tools.clone(),
    };

    let budget_model = config.model.as_option();
    let context_config =
        ContextConfig::for_model_with_runtime(budget_model.as_deref(), &config.model_runtime);
    let mut conversation = Conversation::new();
    conversation.add_system(agent_common::build_agent_system_prompt(
        cwd,
        request.depth,
        &request.subagent_type,
        Some(instructions),
        config.approval_mode,
        Some(memory),
    ));
    conversation.add_user(request.prompt.clone());

    let policy = ApprovalPolicy::new(config.approval_mode)
        .with_permission_rules(config.permission_rules.clone());

    ChildAgentLoopSetup {
        mcp_registry,
        provider_config,
        context_config,
        conversation,
        policy,
        turn: 0,
        reactive_compacted: false,
    }
}

pub fn advance_child_agent_turn(setup: &mut ChildAgentLoopSetup) -> ChildAgentTurnBudget {
    advance_child_agent_turn_with_limit(setup, DEFAULT_CHILD_AGENT_MAX_TURNS)
}

pub fn advance_child_agent_turn_with_limit(
    setup: &mut ChildAgentLoopSetup,
    max_turns: u32,
) -> ChildAgentTurnBudget {
    setup.turn = setup.turn.saturating_add(1);
    if setup.turn > max_turns {
        return ChildAgentTurnBudget::Stop(ChildAgentResult {
            status: RunStatus::BudgetExhausted,
            final_message: None,
            error: Some("max turns exhausted".to_string()),
        });
    }

    ChildAgentTurnBudget::Continue
}
