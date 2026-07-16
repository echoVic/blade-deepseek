use std::cell::RefCell;
use std::sync::Arc;

use orca_core::goal_types::{
    GoalUpdate, ThreadGoal, ThreadGoalStatus, goal_usage_summary, validate_thread_goal_objective,
};
use orca_core::tool_types::{ToolRequest, ToolResult};
use serde::Deserialize;

type GoalHandler =
    Arc<dyn Fn(GoalToolOperation) -> Result<Option<ThreadGoal>, String> + Send + Sync>;

thread_local! {
    static GOAL_HANDLER: RefCell<Option<GoalHandler>> = RefCell::new(None);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalToolOperation {
    Get,
    Create {
        objective: String,
        token_budget: Option<i64>,
    },
    Update(GoalUpdate),
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CreateGoalArgs {
    pub objective: String,
    #[serde(default)]
    pub token_budget: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct UpdateGoalArgs {
    pub status: Option<ThreadGoalStatus>,
    #[serde(default)]
    pub objective: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

const UPDATE_GOAL_STATUS_FLAG_KEYS: [&str; 3] = ["complete", "completed", "blocked"];

pub fn execute_get(request: &ToolRequest) -> ToolResult {
    match parse_get_args(request).and_then(|()| dispatch(GoalToolOperation::Get)) {
        Ok(Some(goal)) => ToolResult::completed(request, format_goal("Goal active.", &goal), false),
        Ok(None) => ToolResult::completed(request, "No goal is currently set.".to_string(), false),
        Err(error) => ToolResult::failed(request, error, None),
    }
}

pub fn execute_create(request: &ToolRequest) -> ToolResult {
    match parse_create_args(request).and_then(|args| {
        dispatch(GoalToolOperation::Create {
            objective: args.objective,
            token_budget: args.token_budget,
        })
    }) {
        Ok(Some(goal)) => {
            ToolResult::completed(request, format_goal("Goal created.", &goal), false)
        }
        Ok(None) => ToolResult::failed(
            request,
            "cannot create a goal because an unfinished goal already exists",
            None,
        ),
        Err(error) => ToolResult::failed(request, error, None),
    }
}

pub fn execute_update(request: &ToolRequest) -> ToolResult {
    match parse_update_args(request)
        .and_then(|args| dispatch(GoalToolOperation::Update(update_from_args(args))))
    {
        Ok(Some(goal)) => ToolResult::completed(
            request,
            format_goal(&format!("Goal {}.", goal_status_word(goal.status)), &goal),
            false,
        ),
        Ok(None) => ToolResult::failed(request, "no goal is currently set", None),
        Err(error) => ToolResult::failed(request, error, None),
    }
}

pub fn parse_get_args(request: &ToolRequest) -> Result<(), String> {
    match request.raw_arguments.as_deref() {
        None | Some("") | Some("{}") => Ok(()),
        Some(raw) => {
            let value: serde_json::Value = serde_json::from_str(raw)
                .map_err(|error| format!("invalid get_goal JSON: {error}"))?;
            if value.as_object().is_some_and(serde_json::Map::is_empty) {
                Ok(())
            } else {
                Err("get_goal does not accept arguments".to_string())
            }
        }
    }
}

pub fn parse_create_args(request: &ToolRequest) -> Result<CreateGoalArgs, String> {
    let raw = request
        .raw_arguments
        .as_deref()
        .ok_or_else(|| "create_goal requires raw JSON arguments".to_string())?;
    let mut args: CreateGoalArgs =
        serde_json::from_str(raw).map_err(|error| format!("invalid create_goal JSON: {error}"))?;
    args.objective = args.objective.trim().to_string();
    validate_thread_goal_objective(&args.objective)?;
    if let Some(token_budget) = args.token_budget
        && token_budget <= 0
    {
        return Err("create_goal token_budget must be positive".to_string());
    }
    Ok(args)
}

pub fn parse_update_args(request: &ToolRequest) -> Result<UpdateGoalArgs, String> {
    let raw = request
        .raw_arguments
        .as_deref()
        .ok_or_else(|| "update_goal requires raw JSON arguments".to_string())?;
    let normalized = normalize_update_raw_arguments(raw);
    let effective = normalized.as_deref().unwrap_or(raw);
    let args: UpdateGoalArgs = serde_json::from_str(effective)
        .map_err(|error| format!("invalid update_goal JSON: {error}"))?;
    if args.objective.is_some() {
        return Err(
            "update_goal cannot change the goal objective; use create_goal for a new goal or /goal edit from the TUI"
                .to_string(),
        );
    }
    let Some(status) = args.status else {
        return Err("update_goal requires status".to_string());
    };
    if !matches!(
        status,
        ThreadGoalStatus::Complete | ThreadGoalStatus::Blocked
    ) {
        return Err("update_goal can only set status to complete or blocked".to_string());
    }
    Ok(args)
}

pub fn normalize_update_raw_arguments(raw: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let object = value.as_object_mut()?;
    let mut changed = false;

    if object.get("status").and_then(serde_json::Value::as_str) == Some("completed") {
        object.insert(
            "status".to_string(),
            serde_json::Value::String("complete".to_string()),
        );
        changed = true;
    }

    let has_valid_status = object
        .get("status")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|status| matches!(status, "complete" | "blocked"));
    if !has_valid_status {
        let derived = if object.get("complete") == Some(&serde_json::Value::Bool(true))
            || object.get("completed") == Some(&serde_json::Value::Bool(true))
        {
            Some("complete")
        } else if object.get("blocked") == Some(&serde_json::Value::Bool(true)) {
            Some("blocked")
        } else {
            None
        };
        if let Some(status) = derived {
            object.insert(
                "status".to_string(),
                serde_json::Value::String(status.to_string()),
            );
            changed = true;
        }
    }

    for key in UPDATE_GOAL_STATUS_FLAG_KEYS {
        if object.remove(key).is_some() {
            changed = true;
        }
    }

    changed
        .then(|| serde_json::to_string(&value).ok())
        .flatten()
}

pub fn normalized_update_raw_arguments(raw: &str) -> String {
    normalize_update_raw_arguments(raw).unwrap_or_else(|| raw.to_string())
}

pub fn with_goal_handler<R>(handler: GoalHandler, f: impl FnOnce() -> R) -> R {
    struct Reset(Option<GoalHandler>);

    impl Drop for Reset {
        fn drop(&mut self) {
            let previous = self.0.take();
            GOAL_HANDLER.with(|slot| {
                *slot.borrow_mut() = previous;
            });
        }
    }

    let previous = GOAL_HANDLER.with(|slot| slot.borrow_mut().replace(handler));
    let _reset = Reset(previous);
    f()
}

fn dispatch(operation: GoalToolOperation) -> Result<Option<ThreadGoal>, String> {
    GOAL_HANDLER.with(|slot| {
        let Some(handler) = slot.borrow().clone() else {
            return Err("goal tools are only available while goal mode is active".to_string());
        };
        handler(operation)
    })
}

fn update_from_args(args: UpdateGoalArgs) -> GoalUpdate {
    GoalUpdate {
        objective: None,
        status: args.status,
        token_budget: None,
    }
}

fn format_goal(prefix: &str, goal: &ThreadGoal) -> String {
    format!("{prefix}\n{}", goal_usage_summary(goal))
}

fn goal_status_word(status: ThreadGoalStatus) -> &'static str {
    match status {
        ThreadGoalStatus::Active => "active",
        ThreadGoalStatus::Paused => "paused",
        ThreadGoalStatus::Blocked => "blocked",
        ThreadGoalStatus::Stalled => "stalled",
        ThreadGoalStatus::UsageLimited => "usage limited",
        ThreadGoalStatus::BudgetLimited => "budget limited",
        ThreadGoalStatus::Complete => "complete",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolStatus};
    use std::sync::{Arc, Mutex};

    fn request(name: ToolName, arguments: &str) -> ToolRequest {
        ToolRequest {
            id: "call-1".to_string(),
            name,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(arguments.to_string()),
        }
    }

    fn sample_goal(status: ThreadGoalStatus) -> ThreadGoal {
        ThreadGoal {
            session_id: "session-1".to_string(),
            objective: "ship goals".to_string(),
            status,
            token_budget: Some(50_000),
            tokens_used: 1_000,
            time_used_seconds: 60,
            created_at: 1,
            updated_at: 2,
        }
    }

    #[test]
    fn parses_create_goal() {
        let args = parse_create_args(&request(
            ToolName::CreateGoal,
            r#"{"objective":" ship it ","token_budget":1000}"#,
        ))
        .unwrap();
        assert_eq!(args.objective, "ship it");
        assert_eq!(args.token_budget, Some(1000));
    }

    #[test]
    fn rejects_non_positive_create_budget() {
        let error = parse_create_args(&request(
            ToolName::CreateGoal,
            r#"{"objective":"ship it","token_budget":0}"#,
        ))
        .unwrap_err();
        assert_eq!(error, "create_goal token_budget must be positive");
    }

    #[test]
    fn parses_status_update() {
        let args =
            parse_update_args(&request(ToolName::UpdateGoal, r#"{"status":"complete"}"#)).unwrap();
        assert_eq!(args.status, Some(ThreadGoalStatus::Complete));
    }

    #[test]
    fn normalizes_completed_status_alias() {
        let args = parse_update_args(&request(
            ToolName::UpdateGoal,
            r#"{"status":"completed","reason":"done"}"#,
        ))
        .unwrap();

        assert_eq!(args.status, Some(ThreadGoalStatus::Complete));
        assert_eq!(args.reason.as_deref(), Some("done"));
    }

    #[test]
    fn normalizes_boolean_goal_status_flags() {
        let complete = parse_update_args(&request(
            ToolName::UpdateGoal,
            r#"{"complete":true,"reason":"done"}"#,
        ))
        .unwrap();
        let completed = parse_update_args(&request(
            ToolName::UpdateGoal,
            r#"{"completed":true,"reason":"done"}"#,
        ))
        .unwrap();
        let blocked = parse_update_args(&request(
            ToolName::UpdateGoal,
            r#"{"blocked":true,"reason":"waiting"}"#,
        ))
        .unwrap();

        assert_eq!(complete.status, Some(ThreadGoalStatus::Complete));
        assert_eq!(completed.status, Some(ThreadGoalStatus::Complete));
        assert_eq!(blocked.status, Some(ThreadGoalStatus::Blocked));
    }

    #[test]
    fn rejects_missing_update_fields() {
        let error =
            parse_update_args(&request(ToolName::UpdateGoal, r#"{"reason":"done"}"#)).unwrap_err();
        assert_eq!(error, "update_goal requires status");
    }

    #[test]
    fn rejects_model_attempts_to_pause_or_resume_goal() {
        let error = parse_update_args(&request(ToolName::UpdateGoal, r#"{"status":"paused"}"#))
            .unwrap_err();
        assert_eq!(
            error,
            "update_goal can only set status to complete or blocked"
        );
    }

    #[test]
    fn rejects_model_attempts_to_replace_objective() {
        let error = parse_update_args(&request(
            ToolName::UpdateGoal,
            r#"{"status":"complete","objective":"smaller goal"}"#,
        ))
        .unwrap_err();
        assert_eq!(
            error,
            "update_goal cannot change the goal objective; use create_goal for a new goal or /goal edit from the TUI"
        );
    }

    #[test]
    fn execute_fails_without_goal_context() {
        let result = execute_update(&request(ToolName::UpdateGoal, r#"{"status":"blocked"}"#));
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("goal mode is active")
        );
    }

    #[test]
    fn execute_get_uses_installed_goal_context() {
        let handler: GoalHandler = Arc::new(move |operation| {
            assert_eq!(operation, GoalToolOperation::Get);
            Ok(Some(sample_goal(ThreadGoalStatus::Active)))
        });

        let result = with_goal_handler(handler, || execute_get(&request(ToolName::GetGoal, "{}")));

        assert_eq!(result.status, ToolStatus::Completed);
        assert!(result.output.unwrap().contains("Goal active."));
    }

    #[test]
    fn execute_create_reports_unfinished_existing_goal() {
        let handler: GoalHandler = Arc::new(move |operation| {
            assert!(matches!(operation, GoalToolOperation::Create { .. }));
            Ok(None)
        });

        let result = with_goal_handler(handler, || {
            execute_create(&request(
                ToolName::CreateGoal,
                r#"{"objective":"ship goals"}"#,
            ))
        });

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(result.error.as_deref().unwrap().contains("unfinished goal"));
    }

    #[test]
    fn execute_update_uses_installed_goal_context() {
        let seen = Arc::new(Mutex::new(None));
        let seen_for_handler = Arc::clone(&seen);
        let handler: GoalHandler = Arc::new(move |operation| {
            if let GoalToolOperation::Update(update) = operation {
                *seen_for_handler.lock().unwrap() = Some(update.clone());
                Ok(Some(sample_goal(update.status.unwrap())))
            } else {
                panic!("expected update");
            }
        });

        let result = with_goal_handler(handler, || {
            execute_update(&request(ToolName::UpdateGoal, r#"{"status":"complete"}"#))
        });

        assert_eq!(result.status, ToolStatus::Completed);
        assert!(result.output.unwrap().contains("Goal complete."));
        assert_eq!(
            seen.lock().unwrap().as_ref().unwrap().status,
            Some(ThreadGoalStatus::Complete)
        );
    }
}
