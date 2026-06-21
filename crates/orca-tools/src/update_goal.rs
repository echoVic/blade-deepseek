use std::cell::RefCell;
use std::sync::Arc;

use orca_core::goal_types::{GoalUpdate, ThreadGoal, ThreadGoalStatus, goal_usage_summary};
use orca_core::tool_types::{ToolRequest, ToolResult};
use serde::Deserialize;

type GoalUpdateHandler =
    Arc<dyn Fn(GoalUpdate) -> Result<Option<ThreadGoal>, String> + Send + Sync>;

thread_local! {
    static GOAL_UPDATE_HANDLER: RefCell<Option<GoalUpdateHandler>> = RefCell::new(None);
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct UpdateGoalArgs {
    pub status: Option<ThreadGoalStatus>,
    #[serde(default)]
    pub objective: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

pub fn execute(request: &ToolRequest) -> ToolResult {
    match parse_args(request).and_then(dispatch_update) {
        Ok(Some(goal)) => ToolResult::completed(
            request,
            format!(
                "Goal {}.\n{}",
                orca_core::goal_types::goal_status_label(goal.status),
                goal_usage_summary(&goal)
            ),
            false,
        ),
        Ok(None) => ToolResult::failed(request, "no goal is currently set", None),
        Err(error) => ToolResult::failed(request, error, None),
    }
}

pub fn parse_args(request: &ToolRequest) -> Result<UpdateGoalArgs, String> {
    let raw = request
        .raw_arguments
        .as_deref()
        .ok_or_else(|| "update_goal requires raw JSON arguments".to_string())?;
    let args: UpdateGoalArgs =
        serde_json::from_str(raw).map_err(|error| format!("invalid update_goal JSON: {error}"))?;
    if args.status.is_none() && args.objective.is_none() {
        return Err("update_goal requires status or objective".to_string());
    }
    Ok(args)
}

pub fn with_goal_update_handler<R>(handler: GoalUpdateHandler, f: impl FnOnce() -> R) -> R {
    struct Reset(Option<GoalUpdateHandler>);

    impl Drop for Reset {
        fn drop(&mut self) {
            let previous = self.0.take();
            GOAL_UPDATE_HANDLER.with(|slot| {
                *slot.borrow_mut() = previous;
            });
        }
    }

    let previous = GOAL_UPDATE_HANDLER.with(|slot| slot.borrow_mut().replace(handler));
    let _reset = Reset(previous);
    f()
}

fn dispatch_update(args: UpdateGoalArgs) -> Result<Option<ThreadGoal>, String> {
    let update = GoalUpdate {
        objective: args.objective,
        status: args.status,
        token_budget: None,
    };
    GOAL_UPDATE_HANDLER.with(|slot| {
        let Some(handler) = slot.borrow().clone() else {
            return Err("update_goal is only available while goal mode is active".to_string());
        };
        handler(update)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::ToolName;
    use std::sync::{Arc, Mutex};

    fn request(arguments: &str) -> ToolRequest {
        ToolRequest {
            id: "call-1".to_string(),
            name: ToolName::UpdateGoal,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(arguments.to_string()),
        }
    }

    #[test]
    fn parses_status_update() {
        let args = parse_args(&request(r#"{"status":"complete"}"#)).unwrap();
        assert_eq!(args.status, Some(ThreadGoalStatus::Complete));
    }

    #[test]
    fn rejects_missing_update_fields() {
        let error = parse_args(&request(r#"{"reason":"done"}"#)).unwrap_err();
        assert_eq!(error, "update_goal requires status or objective");
    }

    #[test]
    fn execute_fails_without_goal_context() {
        let result = execute(&request(r#"{"status":"blocked"}"#));
        assert!(result.error.unwrap().contains("goal mode is active"));
    }

    #[test]
    fn execute_uses_installed_goal_context() {
        let seen = Arc::new(Mutex::new(None));
        let seen_for_handler = Arc::clone(&seen);
        let handler: GoalUpdateHandler = Arc::new(move |update| {
            *seen_for_handler.lock().unwrap() = Some(update.clone());
            Ok(Some(ThreadGoal {
                session_id: "session-1".to_string(),
                objective: update
                    .objective
                    .clone()
                    .unwrap_or_else(|| "ship goals".to_string()),
                status: update.status.unwrap_or(ThreadGoalStatus::Active),
                token_budget: Some(50_000),
                tokens_used: 1_000,
                time_used_seconds: 60,
                created_at: 1,
                updated_at: 2,
            }))
        });

        let result = with_goal_update_handler(handler, || {
            execute(&request(
                r#"{"status":"complete","objective":"ship goals"}"#,
            ))
        });

        assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
        assert!(result.output.unwrap().contains("Goal complete."));
        assert_eq!(
            seen.lock().unwrap().as_ref().unwrap().status,
            Some(ThreadGoalStatus::Complete)
        );
    }
}
