use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};
use std::thread;

use crate::approval::policy::{ApprovalDecision, ApprovalPolicy, ApprovalRequest};
use crate::config::RunConfig;
use crate::mcp::McpRegistry;
use crate::model::ModelRouteContext;
use crate::provider::conversation::Conversation;
use crate::provider::tool_schema::{
    deepseek_tools_schema_for_type_with_mcp, deepseek_tools_schema_with_mcp,
};
use crate::provider::{self, ProviderConfig, ProviderStep};
use crate::runtime::agent_common;
use crate::runtime::cancel::CancelToken;
use crate::runtime::cost::CostTracker;
use crate::runtime::history::{self, SessionWriter};
use crate::runtime::hooks::{HookContext, HookEvent, HookRunner};
use crate::runtime::instructions::{self, ProjectInstructions};
use crate::runtime::memory::{self, MemoryBlock};
use crate::runtime::subagent;
use crate::runtime::subagent_types::SubagentType;
use crate::tools;
use crate::tui::diff;
use crate::tui::types::{TuiEvent, UserAction};

const DEFAULT_MAX_TURNS: u32 = 128;

#[derive(Clone, Debug)]
struct TuiAgentResult {
    status: String,
    final_message: Option<String>,
    error: Option<String>,
    cost_tracker: CostTracker,
}

pub struct TuiConversationSession {
    conversation: Conversation,
    writer: Option<SessionWriter>,
    instructions: ProjectInstructions,
    cost_tracker: CostTracker,
    mcp_registry: McpRegistry,
    hooks: HookRunner,
    memory: MemoryBlock,
}

impl TuiConversationSession {
    pub fn new_with_preloaded(
        config: &RunConfig,
        prompt_for_title: &str,
        preloaded: Option<history::SessionTranscript>,
    ) -> std::io::Result<Self> {
        let cwd = config
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let instructions = load_project_instructions(&cwd);
        let memory = memory::load_for_cwd(&cwd);
        let mcp_registry = crate::mcp::initialize_registry(&config.mcp_servers);
        let hooks = HookRunner::new(config.hooks.clone());
        let system_prompt = agent_common::build_agent_system_prompt(
            &cwd,
            0,
            &SubagentType::General,
            Some(&instructions),
            config.approval_mode,
            Some(&memory),
        );
        let (conversation, loaded_transcript) = match &config.history_mode {
            crate::config::HistoryMode::Resume(selector)
            | crate::config::HistoryMode::Fork(selector) => {
                let transcript = match preloaded {
                    Some(t) => t,
                    None => history::load_session(selector)?,
                };
                let conv = history::resume_conversation(&transcript, system_prompt);
                (conv, Some(transcript))
            }
            crate::config::HistoryMode::Record | crate::config::HistoryMode::Disabled => {
                let mut conversation = Conversation::new();
                conversation.add_system(system_prompt);
                (conversation, None)
            }
        };

        let writer = match &config.history_mode {
            crate::config::HistoryMode::Disabled => None,
            crate::config::HistoryMode::Record | crate::config::HistoryMode::Resume(_) => {
                let meta = history::create_meta(
                    &cwd,
                    config.provider.as_str(),
                    config.model.as_history_value(),
                    prompt_for_title,
                );
                start_writer_with_messages(meta, &conversation)
            }
            crate::config::HistoryMode::Fork(_) => {
                let parent_id = loaded_transcript
                    .map(|transcript| transcript.meta.session_id)
                    .unwrap_or_default();
                let meta = history::create_fork_meta(
                    &cwd,
                    config.provider.as_str(),
                    config.model.as_history_value(),
                    prompt_for_title,
                    parent_id,
                );
                start_writer_with_messages(meta, &conversation)
            }
        };

        Ok(Self {
            conversation,
            writer,
            instructions,
            cost_tracker: CostTracker::new(None),
            mcp_registry,
            hooks,
            memory,
        })
    }

    fn append_message(&mut self, message: &crate::provider::conversation::Message) {
        if let Some(writer) = &mut self.writer {
            if let Err(error) = writer.append_message(message) {
                eprintln!("orca: warning: history write failed: {error}");
                self.writer = None;
            }
        }
    }

    fn complete(&mut self, status: &str) {
        if let Some(writer) = &mut self.writer {
            if let Err(error) = writer.complete(status) {
                eprintln!("orca: warning: history completion write failed: {error}");
            }
        }
    }

    pub fn backtrack_last_user(&mut self) -> Option<String> {
        self.conversation.backtrack_last_user()
    }

    pub fn set_model(&mut self, model: Option<&str>) {
        self.cost_tracker.set_model(model);
    }

    pub fn compact(&mut self, config: &RunConfig, cwd: &Path) -> (usize, usize) {
        let before_messages = self.conversation.messages.len();
        let _ = self.hooks.run(
            HookEvent::PreCompact,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: Some(before_messages),
                after_messages: None,
            },
        );
        let provider_config = ProviderConfig {
            api_key: config.api_key.clone(),
            base_url: config.base_url.clone(),
            model: Some(crate::model::auxiliary_model().to_string()),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
        };
        let compaction = provider::context::compact_with_summary(
            config.provider,
            &self.conversation,
            &provider::context::ContextConfig::default(),
            &provider_config,
        );
        self.conversation = compaction.conversation;
        let after_messages = self.conversation.messages.len();
        if let Some(writer) = &mut self.writer {
            let _ = writer.append_compaction(before_messages, after_messages);
            if let provider::context::CompactionKind::RemoteSummary(summary) = compaction.kind {
                let _ = writer.append_summary(before_messages, after_messages, summary);
            }
        }
        let _ = self.hooks.run(
            HookEvent::PostCompact,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: Some(before_messages),
                after_messages: Some(after_messages),
            },
        );
        (before_messages, after_messages)
    }
}

fn start_writer_with_messages(
    meta: history::SessionMeta,
    conversation: &Conversation,
) -> Option<SessionWriter> {
    match SessionWriter::start_from_meta(meta) {
        Ok(mut writer) => {
            for message in &conversation.messages {
                if let Err(error) = writer.append_message(message) {
                    eprintln!("orca: warning: history write failed: {error}");
                    return None;
                }
            }
            Some(writer)
        }
        Err(error) => {
            eprintln!("orca: warning: failed to initialize history: {error}");
            None
        }
    }
}

fn load_project_instructions(cwd: &Path) -> ProjectInstructions {
    instructions::load_for_cwd_or_default(cwd)
}

pub fn run_agent_for_tui(
    config: &RunConfig,
    session: &mut TuiConversationSession,
    prompt: &str,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    cancel: &CancelToken,
) {
    let cwd = config
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: Some(crate::model::FLASH_MODEL.to_string()),
        tools_override: Some(deepseek_tools_schema_with_mcp(Some(&session.mcp_registry))),
        mcp_registry: Some(session.mcp_registry.clone()),
    };

    let ctx_config = provider::context::ContextConfig::default();
    let policy = ApprovalPolicy::new(config.approval_mode)
        .with_permission_rules(config.permission_rules.clone());
    session.conversation.add_user(prompt.to_string());
    if let Some(message) = session.conversation.messages.last().cloned() {
        session.append_message(&message);
    }

    let mut turn: u32 = 0;

    loop {
        turn += 1;

        if turn > DEFAULT_MAX_TURNS {
            let _ = event_tx.send(TuiEvent::Error("max turns exhausted".to_string()));
            let _ = event_tx.send(TuiEvent::SessionCompleted {
                status: "budget_exhausted".to_string(),
            });
            session.complete("budget_exhausted");
            return;
        }

        if provider::context::needs_compaction(&session.conversation, &ctx_config) {
            let before_messages = session.conversation.messages.len();
            if let Err(error) = session.hooks.run(
                HookEvent::PreCompact,
                HookContext {
                    cwd: &cwd.display().to_string(),
                    session_status: None,
                    tool_request: None,
                    tool_result: None,
                    before_messages: Some(before_messages),
                    after_messages: None,
                },
            ) {
                let _ = event_tx.send(TuiEvent::Error(format!("pre_compact hook failed: {error}")));
            }
            let compaction = provider::context::compact_with_summary(
                config.provider,
                &session.conversation,
                &ctx_config,
                &provider_config,
            );
            session.conversation = compaction.conversation;
            let after_messages = session.conversation.messages.len();
            if let Some(writer) = &mut session.writer {
                let _ = writer.append_compaction(before_messages, after_messages);
                if let provider::context::CompactionKind::RemoteSummary(summary) = compaction.kind {
                    let _ = writer.append_summary(before_messages, after_messages, summary);
                }
            }
            if let Err(error) = session.hooks.run(
                HookEvent::PostCompact,
                HookContext {
                    cwd: &cwd.display().to_string(),
                    session_status: None,
                    tool_request: None,
                    tool_result: None,
                    before_messages: Some(before_messages),
                    after_messages: Some(after_messages),
                },
            ) {
                let _ = event_tx.send(TuiEvent::Error(format!(
                    "post_compact hook failed: {error}"
                )));
            }
        }

        let _ = event_tx.send(TuiEvent::TurnStarted { turn });

        let route_decision = config.model.route(ModelRouteContext {
            subagent_type: &SubagentType::General,
            subagent_model: None,
        });
        session
            .cost_tracker
            .set_model(Some(&route_decision.actual_model));
        let mut turn_provider_config = provider_config.clone();
        turn_provider_config.model = Some(route_decision.actual_model);

        let tx = event_tx.clone();
        let response = provider::call_streaming(
            config.provider,
            &session.conversation,
            &turn_provider_config,
            cancel,
            &mut |step| match step {
                ProviderStep::ReasoningDelta(text) => {
                    let _ = tx.send(TuiEvent::ReasoningDelta(text.to_string()));
                }
                ProviderStep::MessageDelta(text) => {
                    let _ = tx.send(TuiEvent::MessageDelta(text.to_string()));
                }
                _ => {}
            },
        );

        if let Some(usage) = response.usage
            && !usage.is_empty()
        {
            let totals = session.cost_tracker.add_usage(usage);
            let _ = event_tx.send(TuiEvent::UsageUpdated(totals));
            if let Some(writer) = &mut session.writer {
                let _ = writer.append_usage(totals);
            }
            if let Some(max_budget) = config.max_budget_usd
                && totals.estimated_cost_usd > max_budget
            {
                let _ = event_tx.send(TuiEvent::Error(format!(
                    "budget exhausted: estimated cost ${:.6} exceeded limit ${:.6}",
                    totals.estimated_cost_usd, max_budget
                )));
                let _ = event_tx.send(TuiEvent::SessionCompleted {
                    status: "budget_exhausted".to_string(),
                });
                session.complete("budget_exhausted");
                return;
            }
        }

        if cancel.is_cancelled() {
            let _ = event_tx.send(TuiEvent::SessionCompleted {
                status: "interrupted".to_string(),
            });
            session.complete("interrupted");
            return;
        }

        let mut had_error = false;
        for step in &response.steps {
            if let ProviderStep::Error(message) = step {
                let _ = event_tx.send(TuiEvent::Error(message.clone()));
                had_error = true;
                break;
            }
        }

        if had_error {
            let _ = event_tx.send(TuiEvent::SessionCompleted {
                status: "failed".to_string(),
            });
            session.complete("failed");
            return;
        }

        if response.tool_calls.is_empty() {
            session.conversation.add_assistant(
                response.assistant_content,
                response.assistant_reasoning,
                vec![],
            );
            if let Some(message) = session.conversation.messages.last().cloned() {
                session.append_message(&message);
            }
            if config.auto_memory {
                let provider_config = ProviderConfig {
                    api_key: config.api_key.clone(),
                    base_url: config.base_url.clone(),
                    model: Some(crate::model::auxiliary_model().to_string()),
                    tools_override: Some(Vec::new()),
                    mcp_registry: None,
                };
                if let Err(error) = memory::extract_project_memory(
                    config.provider,
                    &provider_config,
                    &cwd,
                    &session.conversation.messages,
                ) {
                    let _ = event_tx.send(TuiEvent::Error(format!(
                        "memory extraction failed: {error}"
                    )));
                }
            }
            let _ = event_tx.send(TuiEvent::SessionCompleted {
                status: "success".to_string(),
            });
            session.complete("success");
            return;
        }

        session.conversation.add_assistant(
            response.assistant_content,
            response.assistant_reasoning,
            response.tool_calls.clone(),
        );
        if let Some(message) = session.conversation.messages.last().cloned() {
            session.append_message(&message);
        }

        let tool_requests: Vec<tools::ToolRequest> = response
            .steps
            .iter()
            .filter_map(|step| match step {
                ProviderStep::ToolCall(tool_request) => Some(tool_request.clone()),
                _ => None,
            })
            .collect();
        let mut index = 0;
        while index < tool_requests.len() {
            if should_run_subagent_batch(config, &tool_requests[index], 0) {
                let batch_end = collect_subagent_batch(config, &tool_requests, index);
                let results = execute_subagent_batch_for_tui(
                    config,
                    &cwd,
                    &tool_requests[index..batch_end],
                    event_tx,
                    0,
                    &session.instructions,
                    &session.memory,
                    &session.hooks,
                );
                for (should_stop, result, child_cost) in results {
                    session.cost_tracker.merge(&child_cost);
                    let result_content = agent_common::format_tool_result_for_model(&result);
                    session
                        .conversation
                        .add_tool_result(result.id.clone(), result_content);
                    if let Some(message) = session.conversation.messages.last().cloned() {
                        session.append_message(&message);
                    }
                    if should_stop {
                        let _ = event_tx.send(TuiEvent::SessionCompleted {
                            status: "approval_required".to_string(),
                        });
                        session.complete("approval_required");
                        return;
                    }
                }
                index = batch_end;
                continue;
            }

            let tool_request = &tool_requests[index];
            let (should_stop, result, child_cost) = execute_tool_for_tui(
                config,
                &cwd,
                tool_request,
                event_tx,
                action_rx,
                0,
                &policy,
                &session.instructions,
                &session.memory,
                &session.mcp_registry,
                &session.hooks,
            );

            if let Some(c) = child_cost {
                session.cost_tracker.merge(&c);
            }

            let result_content = agent_common::format_tool_result_for_model(&result);
            session
                .conversation
                .add_tool_result(tool_request.id.clone(), result_content);
            if let Some(message) = session.conversation.messages.last().cloned() {
                session.append_message(&message);
            }

            if should_stop {
                let _ = event_tx.send(TuiEvent::SessionCompleted {
                    status: "approval_required".to_string(),
                });
                session.complete("approval_required");
                return;
            }
            index += 1;
        }
    }
}

fn should_run_subagent_batch(
    config: &RunConfig,
    tool_request: &tools::ToolRequest,
    subagent_depth: u32,
) -> bool {
    tool_request.name == tools::ToolName::Subagent
        && subagent_depth < config.subagents.max_depth
        && config.subagents.max_parallel > 1
}

fn collect_subagent_batch(
    config: &RunConfig,
    tool_requests: &[tools::ToolRequest],
    start: usize,
) -> usize {
    let max_end = (start + config.subagents.max_parallel).min(tool_requests.len());
    let mut end = start;
    while end < max_end && tool_requests[end].name == tools::ToolName::Subagent {
        end += 1;
    }
    end
}

#[allow(clippy::too_many_arguments)]
fn execute_subagent_batch_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_requests: &[tools::ToolRequest],
    event_tx: &Sender<TuiEvent>,
    subagent_depth: u32,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
) -> Vec<(bool, tools::ToolResult, CostTracker)> {
    let mut handles = Vec::new();
    let mut results: Vec<Option<(bool, tools::ToolResult, CostTracker)>> =
        vec![None; tool_requests.len()];

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        let request = subagent::create_subagent_request(tool_request);
        let description = request.description.clone();
        let subagent_type = request.subagent_type;
        let _ = event_tx.send(TuiEvent::SubagentStarted {
            id: tool_request.id.clone(),
            description: description.clone(),
        });

        if subagent_depth >= config.subagents.max_depth {
            let error = format!("subagent max depth {} reached", config.subagents.max_depth);
            let _ = event_tx.send(TuiEvent::SubagentCompleted {
                id: tool_request.id.clone(),
                description,
                status: "failed".to_string(),
                output: None,
                error: Some(error.clone()),
            });
            results[idx] = Some((
                true,
                tools::ToolResult::failed(tool_request, error, None),
                CostTracker::new(None),
            ));
            continue;
        }

        let mut child_config = config.clone();
        child_config.model = child_config
            .model
            .with_subagent_override(request.model.clone());
        let child_cwd = cwd.to_path_buf();
        let child_prompt = request.prompt;
        let child_instructions = instructions.clone();
        let child_memory = memory.clone();
        let child_hooks = hooks.clone();
        let child_tool_request = tool_request.clone();
        handles.push((
            idx,
            description,
            thread::spawn(move || {
                let child = run_child_agent_for_tui_silent(
                    &child_config,
                    &child_cwd,
                    &child_prompt,
                    subagent_depth + 1,
                    &subagent_type,
                    &child_instructions,
                    &child_memory,
                    &child_hooks,
                );
                (child_tool_request, child)
            }),
        ));
    }

    for (idx, description, handle) in handles {
        let (tool_request, child) = match handle.join() {
            Ok(result) => result,
            Err(_) => {
                let tool_request = &tool_requests[idx];
                let result =
                    tools::ToolResult::failed(tool_request, "subagent thread panicked", None);
                let _ = event_tx.send(TuiEvent::SubagentCompleted {
                    id: tool_request.id.clone(),
                    description,
                    status: "failed".to_string(),
                    output: None,
                    error: result.error.clone(),
                });
                results[idx] = Some((true, result, CostTracker::new(None)));
                continue;
            }
        };

        let (should_stop, result, cost_tracker) =
            child_result_to_tui_tool_result(&tool_request, &description, child, event_tx);
        results[idx] = Some((should_stop, result, cost_tracker));
    }

    results
        .into_iter()
        .map(|result| result.expect("each subagent batch item has a result"))
        .collect()
}

fn execute_tool_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_request: &tools::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    subagent_depth: u32,
    policy: &ApprovalPolicy,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
) -> (bool, tools::ToolResult, Option<CostTracker>) {
    if agent_common::requires_approval(tool_request.action) {
        let approval = ApprovalRequest {
            id: format!("approval-{}", tool_request.id),
            action: tool_request.action,
            description: format!(
                "{} requested {}",
                tool_request.name.as_str(),
                tool_request.action.as_str()
            ),
        };
        let resolution = policy.resolve_for_tool(
            &approval,
            tool_request.name.as_str(),
            tool_request.target.as_deref(),
        );

        match resolution.decision {
            ApprovalDecision::Allow => {}
            ApprovalDecision::Ask => {
                let _ = event_tx.send(TuiEvent::ApprovalNeeded {
                    id: approval.id.clone(),
                    tool: tool_request.name.as_str().to_string(),
                    target: tool_request.target.clone(),
                });

                let allowed = match action_rx.recv() {
                    Ok(UserAction::Approve(v)) => v,
                    _ => false,
                };

                if !allowed {
                    let result = tools::ToolResult::denied(tool_request, "user denied");
                    let _ = event_tx.send(TuiEvent::ToolRequested {
                        name: tool_request.name.as_str().to_string(),
                        target: tool_request.target.clone(),
                    });
                    let _ = event_tx.send(TuiEvent::ToolCompleted {
                        name: tool_request.name.as_str().to_string(),
                        status: "denied".to_string(),
                        output: String::new(),
                        diff: None,
                    });
                    return (true, result, None);
                }
            }
            ApprovalDecision::Deny => {
                let result = tools::ToolResult::denied(tool_request, resolution.reason.clone());
                let _ = event_tx.send(TuiEvent::ToolRequested {
                    name: tool_request.name.as_str().to_string(),
                    target: tool_request.target.clone(),
                });
                let _ = event_tx.send(TuiEvent::ToolCompleted {
                    name: tool_request.name.as_str().to_string(),
                    status: "denied".to_string(),
                    output: String::new(),
                    diff: None,
                });
                return (true, result, None);
            }
        }
    }

    let mut rendered_diff = None;
    let (result, child_cost) = if tool_request.name == tools::ToolName::Subagent {
        let (r, c) = execute_subagent_for_tui(
            config,
            cwd,
            tool_request,
            event_tx,
            action_rx,
            subagent_depth,
            instructions,
            memory,
            hooks,
        );
        (r, Some(c))
    } else {
        let _ = event_tx.send(TuiEvent::ToolRequested {
            name: tool_request.name.as_str().to_string(),
            target: tool_request.target.clone(),
        });
        if let Err(error) = hooks.run(
            HookEvent::PreToolUse,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: Some(tool_request),
                tool_result: None,
                before_messages: None,
                after_messages: None,
            },
        ) {
            let result = tools::ToolResult::failed(
                tool_request,
                format!("pre_tool_use hook blocked tool: {error}"),
                None,
            );
            let _ = event_tx.send(TuiEvent::ToolCompleted {
                name: tool_request.name.as_str().to_string(),
                status: "failed".to_string(),
                output: result.error.clone().unwrap_or_default(),
                diff: None,
            });
            return (true, result, None);
        }
        let before = diff::capture_before(tool_request, cwd);
        let result = tools::execute_with_mcp(tool_request, cwd, mcp_registry);
        if matches!(result.status, tools::ToolStatus::Completed) {
            rendered_diff = before.and_then(diff::render_after);
        }
        (result, None)
    };

    if tool_request.name != tools::ToolName::Subagent {
        let _ = event_tx.send(TuiEvent::ToolCompleted {
            name: tool_request.name.as_str().to_string(),
            status: format!("{:?}", result.status).to_lowercase(),
            output: result.output.clone().unwrap_or_default(),
            diff: rendered_diff,
        });
        if let Err(error) = hooks.run(
            HookEvent::PostToolUse,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: Some(tool_request),
                tool_result: Some(&result),
                before_messages: None,
                after_messages: None,
            },
        ) {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "post_tool_use hook failed: {error}"
            )));
        }
    }

    let failed = matches!(
        result.status,
        tools::ToolStatus::Failed | tools::ToolStatus::Denied
    );
    (failed, result, child_cost)
}

fn execute_subagent_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_request: &tools::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    subagent_depth: u32,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
) -> (tools::ToolResult, CostTracker) {
    let request = subagent::create_subagent_request(tool_request);
    let description = request.description.clone();
    let subagent_type = request.subagent_type;

    let _ = event_tx.send(TuiEvent::SubagentStarted {
        id: tool_request.id.clone(),
        description: description.clone(),
    });

    if subagent_depth >= config.subagents.max_depth {
        let error = format!("subagent max depth {} reached", config.subagents.max_depth);
        let _ = event_tx.send(TuiEvent::SubagentCompleted {
            id: tool_request.id.clone(),
            description,
            status: "failed".to_string(),
            output: None,
            error: Some(error.clone()),
        });
        return (
            tools::ToolResult::failed(tool_request, error, None),
            CostTracker::new(None),
        );
    }

    let mut child_config = config.clone();
    child_config.model = child_config
        .model
        .with_subagent_override(request.model.clone());
    let child = run_child_agent_for_tui(
        &child_config,
        cwd,
        &request.prompt,
        event_tx,
        action_rx,
        subagent_depth + 1,
        &subagent_type,
        instructions,
        memory,
        hooks,
    );

    if child.status == "success" {
        let output = child
            .final_message
            .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
        let _ = event_tx.send(TuiEvent::SubagentCompleted {
            id: tool_request.id.clone(),
            description,
            status: "completed".to_string(),
            output: Some(output.clone()),
            error: None,
        });
        (
            tools::ToolResult::completed(
                tool_request,
                format!("Subagent status: success\n\n{output}"),
                false,
            ),
            child.cost_tracker,
        )
    } else {
        let error = child
            .error
            .unwrap_or_else(|| format!("subagent ended with status {}", child.status));
        let _ = event_tx.send(TuiEvent::SubagentCompleted {
            id: tool_request.id.clone(),
            description,
            status: "failed".to_string(),
            output: child.final_message,
            error: Some(error.clone()),
        });
        (
            tools::ToolResult::failed(tool_request, error, None),
            child.cost_tracker,
        )
    }
}

fn child_result_to_tui_tool_result(
    tool_request: &tools::ToolRequest,
    description: &str,
    child: TuiAgentResult,
    event_tx: &Sender<TuiEvent>,
) -> (bool, tools::ToolResult, CostTracker) {
    let cost_tracker = child.cost_tracker.clone();
    if child.status == "success" {
        let output = child
            .final_message
            .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
        let _ = event_tx.send(TuiEvent::SubagentCompleted {
            id: tool_request.id.clone(),
            description: description.to_string(),
            status: "completed".to_string(),
            output: Some(output.clone()),
            error: None,
        });
        (
            false,
            tools::ToolResult::completed(
                tool_request,
                format!("Subagent status: success\n\n{output}"),
                false,
            ),
            cost_tracker,
        )
    } else {
        let error = child
            .error
            .unwrap_or_else(|| format!("subagent ended with status {}", child.status));
        let _ = event_tx.send(TuiEvent::SubagentCompleted {
            id: tool_request.id.clone(),
            description: description.to_string(),
            status: "failed".to_string(),
            output: child.final_message,
            error: Some(error.clone()),
        });
        (
            true,
            tools::ToolResult::failed(tool_request, error, None),
            cost_tracker,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn run_child_agent_for_tui(
    config: &RunConfig,
    cwd: &Path,
    prompt: &str,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
) -> TuiAgentResult {
    let mcp_registry = crate::mcp::initialize_registry(&config.mcp_servers);
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: Some(crate::model::FLASH_MODEL.to_string()),
        tools_override: Some(deepseek_tools_schema_for_type_with_mcp(
            subagent_type,
            Some(&mcp_registry),
        )),
        mcp_registry: Some(mcp_registry.clone()),
    };

    let ctx_config = provider::context::ContextConfig::default();
    let mut conversation = Conversation::new();
    conversation.add_system(agent_common::build_agent_system_prompt(
        cwd,
        subagent_depth,
        subagent_type,
        Some(instructions),
        config.approval_mode,
        Some(memory),
    ));
    conversation.add_user(prompt.to_string());

    let policy = ApprovalPolicy::new(config.approval_mode)
        .with_permission_rules(config.permission_rules.clone());
    let mut child_cost_tracker = CostTracker::new(None);
    let mut turn: u32 = 0;
    loop {
        turn += 1;
        if turn > DEFAULT_MAX_TURNS {
            return TuiAgentResult {
                status: "budget_exhausted".to_string(),
                final_message: None,
                error: Some("max turns exhausted".to_string()),
                cost_tracker: child_cost_tracker,
            };
        }

        if provider::context::needs_compaction(&conversation, &ctx_config) {
            conversation = provider::context::compact(&conversation, &ctx_config);
        }

        let child_cancel = CancelToken::new();
        let route_decision = config.model.route(ModelRouteContext {
            subagent_type,
            subagent_model: None,
        });
        child_cost_tracker.set_model(Some(&route_decision.actual_model));
        let mut turn_provider_config = provider_config.clone();
        turn_provider_config.model = Some(route_decision.actual_model);

        let response = provider::call_streaming(
            config.provider,
            &conversation,
            &turn_provider_config,
            &child_cancel,
            &mut |_| {},
        );

        if let Some(error) = response.steps.iter().find_map(|step| match step {
            ProviderStep::Error(message) => Some(message.clone()),
            _ => None,
        }) {
            return TuiAgentResult {
                status: "failed".to_string(),
                final_message: None,
                error: Some(error),
                cost_tracker: child_cost_tracker,
            };
        }

        if let Some(usage) = response.usage
            && !usage.is_empty()
        {
            child_cost_tracker.add_usage(usage);
        }

        if response.tool_calls.is_empty() {
            conversation.add_assistant(
                response.assistant_content.clone(),
                response.assistant_reasoning,
                vec![],
            );
            return TuiAgentResult {
                status: "success".to_string(),
                final_message: response.assistant_content,
                error: None,
                cost_tracker: child_cost_tracker,
            };
        }

        conversation.add_assistant(
            response.assistant_content,
            response.assistant_reasoning,
            response.tool_calls.clone(),
        );

        for step in &response.steps {
            if let ProviderStep::ToolCall(tool_request) = step {
                let (should_stop, result, child_cost) = execute_tool_for_tui(
                    config,
                    cwd,
                    tool_request,
                    event_tx,
                    action_rx,
                    subagent_depth,
                    &policy,
                    instructions,
                    memory,
                    &mcp_registry,
                    hooks,
                );

                if let Some(c) = child_cost {
                    child_cost_tracker.merge(&c);
                }

                let result_content = agent_common::format_tool_result_for_model(&result);
                conversation.add_tool_result(tool_request.id.clone(), result_content);

                if should_stop {
                    return TuiAgentResult {
                        status: "failed".to_string(),
                        final_message: None,
                        error: result.error,
                        cost_tracker: child_cost_tracker,
                    };
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_child_agent_for_tui_silent(
    config: &RunConfig,
    cwd: &Path,
    prompt: &str,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
) -> TuiAgentResult {
    let (event_tx, _event_rx) = std::sync::mpsc::channel();
    let (action_tx, action_rx) = std::sync::mpsc::channel();
    drop(action_tx);
    run_child_agent_for_tui(
        config,
        cwd,
        prompt,
        &event_tx,
        &action_rx,
        subagent_depth,
        subagent_type,
        instructions,
        memory,
        hooks,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    use crate::approval::policy::ApprovalMode;
    use crate::config::{HistoryMode, OutputFormat, ProviderKind, RunConfig};
    use crate::model::ModelSelection;

    fn config() -> RunConfig {
        RunConfig {
            prompt: String::new(),
            cwd: std::env::current_dir().ok(),
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            api_key: None,
            base_url: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            permission_rules: Default::default(),
            max_budget_usd: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            subagents: Default::default(),
            theme: crate::config::ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    #[test]
    fn tui_session_reuses_conversation_across_submits() {
        let config = config();
        let (event_tx, event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let mut session =
            TuiConversationSession::new_with_preloaded(&config, "first", None).expect("session");

        run_agent_for_tui(
            &config,
            &mut session,
            "first prompt",
            &event_tx,
            &action_rx,
            &cancel,
        );
        run_agent_for_tui(
            &config,
            &mut session,
            "mock_history_echo",
            &event_tx,
            &action_rx,
            &cancel,
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        let echoed = events.iter().find_map(|event| match event {
            TuiEvent::MessageDelta(text) if text.contains("Mock history users") => {
                Some(text.as_str())
            }
            _ => None,
        });
        assert!(
            echoed
                .unwrap_or_default()
                .contains("first prompt | mock_history_echo")
        );
    }

    #[test]
    fn tui_session_backtracks_last_user_before_next_submit() {
        let config = config();
        let (event_tx, event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let mut session =
            TuiConversationSession::new_with_preloaded(&config, "first", None).expect("session");

        run_agent_for_tui(
            &config,
            &mut session,
            "first prompt",
            &event_tx,
            &action_rx,
            &cancel,
        );
        run_agent_for_tui(
            &config,
            &mut session,
            "second prompt",
            &event_tx,
            &action_rx,
            &cancel,
        );

        assert_eq!(
            session.backtrack_last_user(),
            Some("second prompt".to_string())
        );

        run_agent_for_tui(
            &config,
            &mut session,
            "mock_history_echo",
            &event_tx,
            &action_rx,
            &cancel,
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        let echoed = events.iter().rev().find_map(|event| match event {
            TuiEvent::MessageDelta(text) if text.contains("Mock history users") => {
                Some(text.as_str())
            }
            _ => None,
        });
        let echoed = echoed.unwrap_or_default();
        assert!(echoed.contains("first prompt | mock_history_echo"));
        assert!(!echoed.contains("second prompt"));
    }
}
