use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};

use crate::approval::policy::{ActionKind, ApprovalDecision, ApprovalPolicy, ApprovalRequest};
use crate::config::RunConfig;
use crate::provider::conversation::Conversation;
use crate::provider::system_prompt::build_system_prompt;
use crate::provider::{self, ProviderConfig, ProviderStep};
use crate::tools;
use crate::tui::types::{TuiEvent, UserAction};

const DEFAULT_MAX_TURNS: u32 = 128;
const MAX_SUBAGENT_DEPTH: u32 = 1;

#[derive(Clone, Debug)]
struct TuiAgentResult {
    status: String,
    final_message: Option<String>,
    error: Option<String>,
}

pub fn run_agent_for_tui(
    config: &RunConfig,
    prompt: &str,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
) {
    let cwd = config
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: config.model.clone(),
    };

    let ctx_config = provider::context::ContextConfig::default();
    let mut conversation = Conversation::new();
    conversation.add_system(build_agent_system_prompt(&cwd, 0));
    conversation.add_user(prompt.to_string());

    let mut turn: u32 = 0;

    loop {
        turn += 1;

        if turn > DEFAULT_MAX_TURNS {
            let _ = event_tx.send(TuiEvent::Error("max turns exhausted".to_string()));
            let _ = event_tx.send(TuiEvent::SessionCompleted {
                status: "budget_exhausted".to_string(),
            });
            return;
        }

        if provider::context::needs_compaction(&conversation, &ctx_config) {
            conversation = provider::context::compact(&conversation, &ctx_config);
        }

        let _ = event_tx.send(TuiEvent::TurnStarted { turn });

        let tx = event_tx.clone();
        let response = provider::call_streaming(
            config.provider,
            &conversation,
            &provider_config,
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
            return;
        }

        if response.tool_calls.is_empty() {
            conversation.add_assistant(
                response.assistant_content,
                response.assistant_reasoning,
                vec![],
            );
            let _ = event_tx.send(TuiEvent::SessionCompleted {
                status: "success".to_string(),
            });
            return;
        }

        conversation.add_assistant(
            response.assistant_content,
            response.assistant_reasoning,
            response.tool_calls.clone(),
        );

        for step in &response.steps {
            if let ProviderStep::ToolCall(tool_request) = step {
                let (should_stop, result) =
                    execute_tool_for_tui(config, &cwd, tool_request, event_tx, action_rx, 0);

                let result_content = format_tool_result_for_model(&result);
                conversation.add_tool_result(tool_request.id.clone(), result_content);

                if should_stop {
                    let _ = event_tx.send(TuiEvent::SessionCompleted {
                        status: "approval_required".to_string(),
                    });
                    return;
                }
            }
        }
    }
}

fn execute_tool_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_request: &tools::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    subagent_depth: u32,
) -> (bool, tools::ToolResult) {
    let policy = ApprovalPolicy::new(config.approval_mode);

    if requires_approval(tool_request.action) {
        let approval = ApprovalRequest {
            id: format!("approval-{}", tool_request.id),
            action: tool_request.action,
            description: format!(
                "{} requested {}",
                tool_request.name.as_str(),
                tool_request.action.as_str()
            ),
        };
        let resolution = policy.resolve(&approval);

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
                    });
                    return (true, result);
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
                });
                return (true, result);
            }
        }
    }

    let result = if tool_request.name == tools::ToolName::Subagent {
        execute_subagent_for_tui(
            config,
            cwd,
            tool_request,
            event_tx,
            action_rx,
            subagent_depth,
        )
    } else {
        let _ = event_tx.send(TuiEvent::ToolRequested {
            name: tool_request.name.as_str().to_string(),
            target: tool_request.target.clone(),
        });
        tools::execute(tool_request, cwd)
    };

    if tool_request.name != tools::ToolName::Subagent {
        let _ = event_tx.send(TuiEvent::ToolCompleted {
            name: tool_request.name.as_str().to_string(),
            status: format!("{:?}", result.status).to_lowercase(),
            output: result.output.clone().unwrap_or_default(),
        });
    }

    let failed = matches!(
        result.status,
        tools::ToolStatus::Failed | tools::ToolStatus::Denied
    );
    (failed, result)
}

fn execute_subagent_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_request: &tools::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    subagent_depth: u32,
) -> tools::ToolResult {
    let description = subagent_field(tool_request, "description")
        .or_else(|| tool_request.target.clone())
        .unwrap_or_else(|| "subagent".to_string());

    let _ = event_tx.send(TuiEvent::SubagentStarted {
        id: tool_request.id.clone(),
        description: description.clone(),
    });

    if subagent_depth >= MAX_SUBAGENT_DEPTH {
        let error = "nested subagents are disabled in this MVP";
        let _ = event_tx.send(TuiEvent::SubagentCompleted {
            id: tool_request.id.clone(),
            description,
            status: "failed".to_string(),
            output: None,
            error: Some(error.to_string()),
        });
        return tools::ToolResult::failed(tool_request, error, None);
    }

    let prompt = subagent_field(tool_request, "prompt").unwrap_or_else(|| description.clone());

    let child = run_child_agent_for_tui(
        config,
        cwd,
        &prompt,
        event_tx,
        action_rx,
        subagent_depth + 1,
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
        tools::ToolResult::completed(
            tool_request,
            format!("Subagent status: success\n\n{output}"),
            false,
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
        tools::ToolResult::failed(tool_request, error, None)
    }
}

fn run_child_agent_for_tui(
    config: &RunConfig,
    cwd: &Path,
    prompt: &str,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    subagent_depth: u32,
) -> TuiAgentResult {
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: config.model.clone(),
    };

    let ctx_config = provider::context::ContextConfig::default();
    let mut conversation = Conversation::new();
    conversation.add_system(build_agent_system_prompt(cwd, subagent_depth));
    conversation.add_user(prompt.to_string());

    let mut turn: u32 = 0;
    loop {
        turn += 1;
        if turn > DEFAULT_MAX_TURNS {
            return TuiAgentResult {
                status: "budget_exhausted".to_string(),
                final_message: None,
                error: Some("max turns exhausted".to_string()),
            };
        }

        if provider::context::needs_compaction(&conversation, &ctx_config) {
            conversation = provider::context::compact(&conversation, &ctx_config);
        }

        let response = provider::call_streaming(
            config.provider,
            &conversation,
            &provider_config,
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
            };
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
            };
        }

        conversation.add_assistant(
            response.assistant_content,
            response.assistant_reasoning,
            response.tool_calls.clone(),
        );

        for step in &response.steps {
            if let ProviderStep::ToolCall(tool_request) = step {
                let (should_stop, result) = execute_tool_for_tui(
                    config,
                    cwd,
                    tool_request,
                    event_tx,
                    action_rx,
                    subagent_depth,
                );

                let result_content = format_tool_result_for_model(&result);
                conversation.add_tool_result(tool_request.id.clone(), result_content);

                if should_stop {
                    return TuiAgentResult {
                        status: "failed".to_string(),
                        final_message: None,
                        error: result.error,
                    };
                }
            }
        }
    }
}

fn subagent_field(tool_request: &tools::ToolRequest, field: &str) -> Option<String> {
    let raw = tool_request.raw_arguments.as_ref()?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    value[field].as_str().map(String::from)
}

fn build_agent_system_prompt(cwd: &Path, subagent_depth: u32) -> String {
    let mut prompt = build_system_prompt(cwd);
    if subagent_depth > 0 {
        prompt.push_str(
            "\n\n## Subagent Role\nYou are running as a synchronous subagent. Complete only the delegated task and return a concise report for the parent agent. Do not assume the user can see your intermediate tool output.",
        );
    }
    prompt
}

fn format_tool_result_for_model(result: &tools::ToolResult) -> String {
    match (&result.output, &result.error) {
        (Some(output), _) => {
            if result.truncated {
                format!("{output}\n[output truncated]")
            } else {
                output.clone()
            }
        }
        (_, Some(error)) => format!("ERROR: {error}"),
        _ => "(no output)".to_string(),
    }
}

fn requires_approval(action: ActionKind) -> bool {
    matches!(action, ActionKind::Write | ActionKind::Shell)
}
