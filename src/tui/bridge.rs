use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};

use crate::approval::policy::{ApprovalDecision, ApprovalPolicy, ApprovalRequest};
use crate::config::RunConfig;
use crate::provider::conversation::Conversation;
use crate::provider::tool_schema::deepseek_tools_schema_for_type;
use crate::provider::{self, ProviderConfig, ProviderStep};
use crate::runtime::agent_common;
use crate::runtime::cancel::CancelToken;
use crate::runtime::history::{self, SessionWriter};
use crate::runtime::subagent;
use crate::runtime::subagent_types::SubagentType;
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

pub struct TuiConversationSession {
    conversation: Conversation,
    writer: Option<SessionWriter>,
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
        let system_prompt =
            agent_common::build_agent_system_prompt(&cwd, 0, &SubagentType::General);
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
                    config.model.clone(),
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
                    config.model.clone(),
                    prompt_for_title,
                    parent_id,
                );
                start_writer_with_messages(meta, &conversation)
            }
        };

        Ok(Self {
            conversation,
            writer,
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
        model: config.model.clone(),
        tools_override: None,
    };

    let ctx_config = provider::context::ContextConfig::default();
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
            session.conversation = provider::context::compact(&session.conversation, &ctx_config);
            if let Some(writer) = &mut session.writer {
                let _ =
                    writer.append_compaction(before_messages, session.conversation.messages.len());
            }
        }

        let _ = event_tx.send(TuiEvent::TurnStarted { turn });

        let tx = event_tx.clone();
        let response = provider::call_streaming(
            config.provider,
            &session.conversation,
            &provider_config,
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

        for step in &response.steps {
            if let ProviderStep::ToolCall(tool_request) = step {
                let (should_stop, result) =
                    execute_tool_for_tui(config, &cwd, tool_request, event_tx, action_rx, 0);

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
    let request = subagent::create_subagent_request(tool_request);
    let description = request.description.clone();
    let subagent_type = request.subagent_type;

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

    let child = run_child_agent_for_tui(
        config,
        cwd,
        &request.prompt,
        event_tx,
        action_rx,
        subagent_depth + 1,
        &subagent_type,
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

#[allow(clippy::too_many_arguments)]
fn run_child_agent_for_tui(
    config: &RunConfig,
    cwd: &Path,
    prompt: &str,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    subagent_depth: u32,
    subagent_type: &SubagentType,
) -> TuiAgentResult {
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: config.model.clone(),
        tools_override: Some(deepseek_tools_schema_for_type(subagent_type)),
    };

    let ctx_config = provider::context::ContextConfig::default();
    let mut conversation = Conversation::new();
    conversation.add_system(agent_common::build_agent_system_prompt(
        cwd,
        subagent_depth,
        subagent_type,
    ));
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

        let child_cancel = CancelToken::new();
        let response = provider::call_streaming(
            config.provider,
            &conversation,
            &provider_config,
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

                let result_content = agent_common::format_tool_result_for_model(&result);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    use crate::approval::policy::ApprovalMode;
    use crate::config::{HistoryMode, OutputFormat, ProviderKind, RunConfig};

    fn config() -> RunConfig {
        RunConfig {
            prompt: String::new(),
            cwd: std::env::current_dir().ok(),
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: None,
            api_key: None,
            base_url: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
        }
    }

    #[test]
    fn tui_session_reuses_conversation_across_submits() {
        let config = config();
        let (event_tx, event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let mut session = TuiConversationSession::new_with_preloaded(&config, "first", None).expect("session");

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
        let mut session = TuiConversationSession::new_with_preloaded(&config, "first", None).expect("session");

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
