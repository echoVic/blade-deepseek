use std::process::{Command, Stdio};
use std::time::Duration;

use orca_core::conversation::Conversation;
use orca_core::hook_types::{HookConfig, HookEvent};
use orca_core::provider_types::Usage;
use orca_core::tool_types::{ToolRequest, ToolResult};

#[derive(Clone, Debug, Default)]
pub struct HookRunner {
    hooks: Vec<HookConfig>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HookOutcome {
    pub modified_target: Option<String>,
    pub injected_context: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct HookContext<'a> {
    pub cwd: &'a str,
    pub session_status: Option<&'a str>,
    pub tool_request: Option<&'a ToolRequest>,
    pub tool_result: Option<&'a ToolResult>,
    pub before_messages: Option<usize>,
    pub after_messages: Option<usize>,
    pub usage: Option<&'a Usage>,
}

impl HookRunner {
    pub fn new(hooks: Vec<HookConfig>) -> Self {
        Self { hooks }
    }

    pub fn run(&self, event: HookEvent, context: HookContext<'_>) -> Result<HookOutcome, String> {
        self.run_with_timeout(event, context, Duration::from_secs(30))
    }

    fn run_with_timeout(
        &self,
        event: HookEvent,
        context: HookContext<'_>,
        timeout: Duration,
    ) -> Result<HookOutcome, String> {
        let mut outcome = HookOutcome::default();
        for hook in self.matching_hooks(event, context.tool_request) {
            let mut command = Command::new("sh");
            command
                .arg("-c")
                .arg(&hook.command)
                .env("ORCA_HOOK_EVENT", event.as_str())
                .env("ORCA_CWD", context.cwd)
                .env(
                    "ORCA_SESSION_STATUS",
                    context.session_status.unwrap_or_default(),
                )
                .env(
                    "ORCA_TOOL_NAME",
                    context
                        .tool_request
                        .map(|request| request.name.as_str())
                        .unwrap_or_default(),
                )
                .env(
                    "ORCA_TOOL_TARGET",
                    sanitize_env_value(
                        context
                            .tool_request
                            .and_then(|request| request.target.as_deref())
                            .unwrap_or_default(),
                    ),
                )
                .env(
                    "ORCA_TOOL_STATUS",
                    context
                        .tool_result
                        .map(|result| result.status.as_str())
                        .unwrap_or_default(),
                )
                .env(
                    "ORCA_COMPACT_BEFORE_MESSAGES",
                    context
                        .before_messages
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                )
                .env(
                    "ORCA_COMPACT_AFTER_MESSAGES",
                    context
                        .after_messages
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                )
                .env(
                    "ORCA_USAGE_INPUT_TOKENS",
                    context
                        .usage
                        .map(|usage| usage.input_tokens.to_string())
                        .unwrap_or_default(),
                )
                .env(
                    "ORCA_USAGE_OUTPUT_TOKENS",
                    context
                        .usage
                        .map(|usage| usage.output_tokens.to_string())
                        .unwrap_or_default(),
                )
                .env(
                    "ORCA_USAGE_CACHE_TOKENS",
                    context
                        .usage
                        .map(|usage| usage.cache_tokens.to_string())
                        .unwrap_or_default(),
                )
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            orca_tools::process::prepare_non_interactive_command(&mut command);
            let child = command
                .spawn()
                .map_err(|error| format!("hook '{}' failed to start: {error}", hook.command))?;
            let output = orca_tools::process::wait_for_child_output_with_timeout(child, timeout)
                .map_err(|error| format!("hook '{}' failed: {error}", hook.command))?;

            if output.timed_out {
                let stderr =
                    String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(65536)])
                        .trim()
                        .to_string();
                let stdout =
                    String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(65536)])
                        .trim()
                        .to_string();
                let detail = if stderr.is_empty() { stdout } else { stderr };
                return Err(if detail.is_empty() {
                    format!(
                        "hook '{}' timed out after {}s",
                        hook.command,
                        timeout.as_secs()
                    )
                } else {
                    format!(
                        "hook '{}' timed out after {}s: {detail}",
                        hook.command,
                        timeout.as_secs()
                    )
                });
            }

            if !output.status.success() {
                let stderr =
                    String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(65536)])
                        .trim()
                        .to_string();
                let stdout =
                    String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(65536)])
                        .trim()
                        .to_string();
                let detail = if stderr.is_empty() { stdout } else { stderr };
                return Err(if detail.is_empty() {
                    format!("hook '{}' exited with {}", hook.command, output.status)
                } else {
                    format!(
                        "hook '{}' exited with {}: {detail}",
                        hook.command, output.status
                    )
                });
            }

            let stdout = String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(65536)])
                .trim()
                .to_string();
            apply_hook_stdout(&stdout, &mut outcome)?;
        }

        Ok(outcome)
    }

    fn matching_hooks<'a>(
        &'a self,
        event: HookEvent,
        tool_request: Option<&ToolRequest>,
    ) -> impl Iterator<Item = &'a HookConfig> {
        self.hooks.iter().filter(move |hook| {
            hook.event == event
                && hook
                    .tool
                    .as_deref()
                    .map(|tool| {
                        tool_request
                            .map(|request| request.name.as_str() == tool)
                            .unwrap_or(false)
                    })
                    .unwrap_or(true)
        })
    }
}

fn apply_hook_stdout(stdout: &str, outcome: &mut HookOutcome) -> Result<(), String> {
    if stdout.is_empty() {
        return Ok(());
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout) else {
        outcome.injected_context.push(stdout.to_string());
        return Ok(());
    };

    let Some(action) = value.get("action").and_then(|value| value.as_str()) else {
        outcome.injected_context.push(stdout.to_string());
        return Ok(());
    };

    match action {
        "allow" => Ok(()),
        "deny" => Err(value
            .get("reason")
            .and_then(|value| value.as_str())
            .unwrap_or("hook denied request")
            .to_string()),
        "modify" => {
            if let Some(target) = value
                .get("modified_target")
                .or_else(|| value.get("modified_input"))
                .and_then(|value| value.as_str())
            {
                outcome.modified_target = Some(target.to_string());
            }
            Ok(())
        }
        "inject" => {
            if let Some(context) = value.get("context").and_then(|value| value.as_str()) {
                outcome.injected_context.push(context.to_string());
            }
            Ok(())
        }
        _ => {
            outcome.injected_context.push(stdout.to_string());
            Ok(())
        }
    }
}

pub fn conversation_with_hook_context(
    conversation: &Conversation,
    outcome: &HookOutcome,
) -> Conversation {
    let mut conversation = conversation.clone();
    if !outcome.injected_context.is_empty() {
        conversation.add_system_pinned(format!(
            "[Hook context]\n{}",
            outcome.injected_context.join("\n\n")
        ));
    }
    conversation
}

pub fn tool_request_with_hook_outcome(request: &ToolRequest, outcome: &HookOutcome) -> ToolRequest {
    let mut request = request.clone();
    if let Some(target) = outcome.modified_target.as_ref() {
        request.target = Some(target.clone());
    }
    request
}

fn sanitize_env_value(value: &str) -> String {
    const MAX_ENV_VALUE_LEN: usize = 4096;
    let sanitized: String = value
        .chars()
        .take(MAX_ENV_VALUE_LEN)
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\0' {
                ' '
            } else {
                c
            }
        })
        .collect();
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::provider_types::Usage;
    use orca_core::tool_types::{ToolName, ToolRequest};
    use std::time::Instant;

    #[test]
    fn pre_tool_hook_can_block() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: "echo blocked >&2; exit 7".to_string(),
            tool: Some("bash".to_string()),
        }]);
        let request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let err = runner
            .run(
                HookEvent::PreToolUse,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: Some(&request),
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            )
            .unwrap_err();
        assert!(err.contains("blocked"));
    }

    #[test]
    fn hook_timeout_kills_descendant_processes() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreModelCall,
            command: "printf before; sleep 5; printf after".to_string(),
            tool: None,
        }]);
        let start = Instant::now();

        let err = runner
            .run_with_timeout(
                HookEvent::PreModelCall,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: None,
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
                Duration::from_millis(200),
            )
            .unwrap_err();

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "hook should not wait for descendant processes"
        );
        assert!(
            err.contains("timed out after 0s: before"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parses_new_model_and_budget_hook_events() {
        assert_eq!(
            toml::from_str::<HookConfig>(
                r#"
event = "pre_model_call"
command = "true"
"#,
            )
            .unwrap()
            .event,
            HookEvent::PreModelCall
        );
        assert_eq!(
            toml::from_str::<HookConfig>(
                r#"
event = "post_model_call"
command = "true"
"#,
            )
            .unwrap()
            .event,
            HookEvent::PostModelCall
        );
        assert_eq!(
            toml::from_str::<HookConfig>(
                r#"
event = "on_budget_warning"
command = "true"
"#,
            )
            .unwrap()
            .event,
            HookEvent::OnBudgetWarning
        );
        assert_eq!(HookEvent::PreModelCall.as_str(), "pre_model_call");
        assert_eq!(HookEvent::PostModelCall.as_str(), "post_model_call");
        assert_eq!(HookEvent::OnBudgetWarning.as_str(), "on_budget_warning");
    }

    #[test]
    fn hook_json_deny_blocks_with_reason_even_when_exit_succeeds() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: r#"printf '%s' '{"action":"deny","reason":"violates policy X"}'"#.to_string(),
            tool: Some("bash".to_string()),
        }]);
        let request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo secret".to_string()),
            raw_arguments: None,
        };

        let err = runner
            .run(
                HookEvent::PreToolUse,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: Some(&request),
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            )
            .unwrap_err();

        assert_eq!(err, "violates policy X");
    }

    #[test]
    fn hook_json_modify_returns_modified_target() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: r#"printf '%s' '{"action":"modify","modified_target":"ls -la (sanitized)"}'"#
                .to_string(),
            tool: Some("bash".to_string()),
        }]);
        let request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("ls -la /tmp".to_string()),
            raw_arguments: None,
        };

        let outcome = runner
            .run(
                HookEvent::PreToolUse,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: Some(&request),
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            )
            .unwrap();

        assert_eq!(
            outcome.modified_target.as_deref(),
            Some("ls -la (sanitized)")
        );
    }

    #[test]
    fn hook_json_and_plain_stdout_can_inject_context() {
        let runner = HookRunner::new(vec![
            HookConfig {
                event: HookEvent::PreModelCall,
                command: r#"printf '%s' '{"action":"inject","context":"policy hint"}'"#.to_string(),
                tool: None,
            },
            HookConfig {
                event: HookEvent::PreModelCall,
                command: "printf '%s' 'legacy hint'".to_string(),
                tool: None,
            },
        ]);

        let outcome = runner
            .run(
                HookEvent::PreModelCall,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: None,
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            )
            .unwrap();

        assert_eq!(outcome.injected_context, vec!["policy hint", "legacy hint"]);
    }

    #[test]
    fn post_model_call_hook_receives_usage_environment() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PostModelCall,
            command: concat!(
                r#"test "$ORCA_USAGE_INPUT_TOKENS" = "120" && "#,
                r#"test "$ORCA_USAGE_OUTPUT_TOKENS" = "30" && "#,
                r#"test "$ORCA_USAGE_CACHE_TOKENS" = "10""#,
            )
            .to_string(),
            tool: None,
        }]);
        let usage = Usage {
            input_tokens: 120,
            output_tokens: 30,
            cache_tokens: 10,
        };

        runner
            .run(
                HookEvent::PostModelCall,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: None,
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: Some(&usage),
                },
            )
            .unwrap();
    }
}
