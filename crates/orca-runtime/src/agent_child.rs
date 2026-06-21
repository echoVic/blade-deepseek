use std::io;
use std::path::Path;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::subagent_types::SubagentType;
use orca_mcp::McpRegistry;

use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::memory::MemoryBlock;

#[derive(Clone, Debug)]
pub struct ChildAgentRequest {
    pub prompt: String,
    pub subagent_type: SubagentType,
    pub model: Option<String>,
    pub depth: u32,
    pub emit_deltas: bool,
}

#[derive(Clone, Debug)]
pub struct ChildAgentResult {
    pub status: RunStatus,
    pub final_message: Option<String>,
    pub error: Option<String>,
}

type ChildAgentExecutor<W> = fn(
    &RunConfig,
    &ChildAgentRequest,
    &mut ChildAgentRuntime<'_, W>,
    &mut CostTracker,
) -> io::Result<ChildAgentResult>;

pub(crate) struct ChildAgentRuntime<'a, W: io::Write> {
    pub cwd: &'a Path,
    pub events: &'a mut EventFactory,
    pub sink: &'a mut EventSink<W>,
    pub instructions: &'a ProjectInstructions,
    pub memory: &'a MemoryBlock,
    pub mcp_registry: &'a McpRegistry,
    pub hooks: &'a HookRunner,
    pub cancel: &'a CancelToken,
    executor: ChildAgentExecutor<W>,
}

impl<'a, W: io::Write> ChildAgentRuntime<'a, W> {
    pub(crate) fn new(
        cwd: &'a Path,
        events: &'a mut EventFactory,
        sink: &'a mut EventSink<W>,
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
        cancel: &'a CancelToken,
        executor: ChildAgentExecutor<W>,
    ) -> Self {
        Self {
            cwd,
            events,
            sink,
            instructions,
            memory,
            mcp_registry,
            hooks,
            cancel,
            executor,
        }
    }

    fn execute(
        &mut self,
        config: &RunConfig,
        request: &ChildAgentRequest,
        child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        (self.executor)(config, request, self, child_cost_tracker)
    }
}

pub(crate) fn run_child_agent<W: io::Write>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    runtime: &mut ChildAgentRuntime<'_, W>,
) -> (ChildAgentResult, CostTracker) {
    let mut child_config = config.clone();
    child_config.model = child_config
        .model
        .with_subagent_override(request.model.clone());
    let mut child_cost_tracker = CostTracker::new(child_config.model.as_deref());
    let result = runtime
        .execute(&child_config, request, &mut child_cost_tracker)
        .unwrap_or_else(|error| ChildAgentResult {
            status: RunStatus::Failed,
            final_message: None,
            error: Some(error.to_string()),
        });
    (result, child_cost_tracker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::cancel::CancelToken;
    use orca_core::config::{
        HistoryMode, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::event_schema::EventFactory;
    use orca_core::event_sink::EventSink;
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::hook_types::HookConfig;
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::{AUTO_MODEL, FLASH_MODEL, ModelSelection};
    use orca_core::provider_types::Usage;
    use orca_core::subagent_config::SubagentConfig;
    use std::io::Cursor;

    use crate::hooks::HookRunner;
    use crate::instructions::ProjectInstructions;
    use crate::memory::MemoryBlock;

    fn config(model: Option<&str>) -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(model.map(str::to_string)),
            model_runtime: Default::default(),
            api_key: None,
            base_url: None,
            mcp_servers: Vec::<McpServerConfig>::new(),
            hooks: Vec::<HookConfig>::new(),
            external_tools: Vec::<ExternalToolConfig>::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            permission_rules: PermissionRules::default(),
            max_budget_usd: None,
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::default(),
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    fn runtime<'a>(
        sink: &'a mut EventSink<Cursor<Vec<u8>>>,
        events: &'a mut EventFactory,
        cancel: &'a CancelToken,
        executor: ChildAgentExecutor<Cursor<Vec<u8>>>,
    ) -> ChildAgentRuntime<'a, Cursor<Vec<u8>>> {
        let instructions = Box::leak(Box::new(ProjectInstructions::default()));
        let memory = Box::leak(Box::new(MemoryBlock::default()));
        let mcp_registry = Box::leak(Box::new(McpRegistry::default()));
        let hooks = Box::leak(Box::new(HookRunner::new(Vec::new())));
        let cwd = Box::leak(Box::new(std::env::temp_dir()));
        ChildAgentRuntime::new(
            cwd.as_path(),
            events,
            sink,
            instructions,
            memory,
            mcp_registry,
            hooks,
            cancel,
            executor,
        )
    }

    #[test]
    fn run_child_agent_applies_subagent_model_override() {
        let request = ChildAgentRequest {
            prompt: "inspect repo".to_string(),
            subagent_type: SubagentType::General,
            model: Some(FLASH_MODEL.to_string()),
            depth: 1,
            emit_deltas: false,
        };
        let cancel = CancelToken::new();
        let mut events = EventFactory::new("test-run".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
        let mut runtime = runtime(&mut sink, &mut events, &cancel, |child_config, _, _, _| {
            assert_eq!(child_config.model.as_deref(), Some(FLASH_MODEL));
            Ok(ChildAgentResult {
                status: RunStatus::Success,
                final_message: Some("ok".to_string()),
                error: None,
            })
        });

        let (result, _) = run_child_agent(&config(None), &request, &mut runtime);

        assert_eq!(result.status, RunStatus::Success);
    }

    #[test]
    fn run_child_agent_ignores_auto_override() {
        let request = ChildAgentRequest {
            prompt: "inspect repo".to_string(),
            subagent_type: SubagentType::General,
            model: Some(AUTO_MODEL.to_string()),
            depth: 1,
            emit_deltas: false,
        };
        let cancel = CancelToken::new();
        let mut events = EventFactory::new("test-run".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
        let mut runtime = runtime(&mut sink, &mut events, &cancel, |child_config, _, _, _| {
            assert_eq!(child_config.model.as_deref(), Some("deepseek-v4-pro"));
            Ok(ChildAgentResult {
                status: RunStatus::Success,
                final_message: None,
                error: None,
            })
        });

        let _ = run_child_agent(&config(Some("deepseek-v4-pro")), &request, &mut runtime);
    }

    #[test]
    fn run_child_agent_preserves_cost_tracker_on_loop_error() {
        let request = ChildAgentRequest {
            prompt: "inspect repo".to_string(),
            subagent_type: SubagentType::General,
            model: None,
            depth: 1,
            emit_deltas: false,
        };
        let cancel = CancelToken::new();
        let mut events = EventFactory::new("test-run".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
        let mut runtime = runtime(&mut sink, &mut events, &cancel, |_, _, _, tracker| {
            tracker.add_usage(Usage {
                input_tokens: 7,
                output_tokens: 3,
                cache_tokens: 2,
            });
            Err(io::Error::other("child loop failed"))
        });

        let (result, tracker) = run_child_agent(&config(None), &request, &mut runtime);

        assert_eq!(result.status, RunStatus::Failed);
        assert_eq!(result.error.as_deref(), Some("child loop failed"));
        let tracker_debug = format!("{tracker:?}");
        assert!(tracker_debug.contains("input_tokens: 7"));
        assert!(tracker_debug.contains("output_tokens: 3"));
    }
}
