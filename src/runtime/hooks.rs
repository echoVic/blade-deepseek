use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::tools::{ToolRequest, ToolResult};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    SessionStart,
    SessionEnd,
    PreCompact,
    PostCompact,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::PreCompact => "pre_compact",
            Self::PostCompact => "post_compact",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HookConfig {
    pub event: HookEvent,
    pub command: String,
    pub tool: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct HookRunner {
    hooks: Vec<HookConfig>,
}

#[derive(Clone, Debug)]
pub struct HookContext<'a> {
    pub cwd: &'a str,
    pub session_status: Option<&'a str>,
    pub tool_request: Option<&'a ToolRequest>,
    pub tool_result: Option<&'a ToolResult>,
    pub before_messages: Option<usize>,
    pub after_messages: Option<usize>,
}

impl HookRunner {
    pub fn new(hooks: Vec<HookConfig>) -> Self {
        Self { hooks }
    }

    pub fn run(&self, event: HookEvent, context: HookContext<'_>) -> Result<(), String> {
        for hook in self.matching_hooks(event, context.tool_request) {
            let output = Command::new("sh")
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
                .output()
                .map_err(|error| format!("hook '{}' failed to start: {error}", hook.command))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(65536)])
                    .trim()
                    .to_string();
                let stdout = String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(65536)])
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
        }

        Ok(())
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

fn sanitize_env_value(value: &str) -> String {
    const MAX_ENV_VALUE_LEN: usize = 4096;
    let sanitized: String = value
        .chars()
        .take(MAX_ENV_VALUE_LEN)
        .map(|c| if c == '\n' || c == '\r' || c == '\0' { ' ' } else { c })
        .collect();
    sanitized
}

impl crate::tools::ToolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Denied => "denied",
            Self::NotImplemented => "not_implemented",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::policy::ActionKind;
    use crate::tools::{ToolName, ToolRequest};

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
                },
            )
            .unwrap_err();
        assert!(err.contains("blocked"));
    }
}
