use orca_core::goal_types::{
    GoalUpdate, ThreadGoal, ThreadGoalStatus, goal_usage_summary, validate_thread_goal_objective,
};
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
use serde::Deserialize;

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
    execute_unavailable(request)
}

pub fn execute_create(request: &ToolRequest) -> ToolResult {
    execute_unavailable(request)
}

pub fn execute_update(request: &ToolRequest) -> ToolResult {
    execute_unavailable(request)
}

pub fn parse_operation(request: &ToolRequest) -> Result<GoalToolOperation, String> {
    match request.name {
        ToolName::GetGoal => parse_get_args(request).map(|()| GoalToolOperation::Get),
        ToolName::CreateGoal => parse_create_args(request).map(|args| GoalToolOperation::Create {
            objective: args.objective,
            token_budget: args.token_budget,
        }),
        ToolName::UpdateGoal => parse_update_args(request).map(|args| {
            GoalToolOperation::Update(GoalUpdate {
                objective: None,
                status: args.status,
                token_budget: None,
            })
        }),
        _ => Err(format!(
            "unsupported goal tool operation: {}",
            request.name.as_str()
        )),
    }
}

pub fn completed_result(request: &ToolRequest, goal: Option<&ThreadGoal>) -> ToolResult {
    match (&request.name, goal) {
        (ToolName::GetGoal, Some(goal)) => {
            ToolResult::completed(request, format_goal("Goal active.", goal), false)
        }
        (ToolName::GetGoal, None) => {
            ToolResult::completed(request, "No goal is currently set.".to_string(), false)
        }
        (ToolName::CreateGoal, Some(goal)) => {
            ToolResult::completed(request, format_goal("Goal created.", goal), false)
        }
        (ToolName::CreateGoal, None) => ToolResult::failed(
            request,
            "cannot create a goal because an unfinished goal already exists",
            None,
        ),
        (ToolName::UpdateGoal, Some(goal)) => ToolResult::completed(
            request,
            format_goal(&format!("Goal {}.", goal_status_word(goal.status)), goal),
            false,
        ),
        (ToolName::UpdateGoal, None) => {
            ToolResult::failed(request, "no goal is currently set", None)
        }
        _ => unavailable_result(request),
    }
}

pub fn unavailable_result(request: &ToolRequest) -> ToolResult {
    ToolResult::failed(
        request,
        "goal tools are only available while goal mode is active",
        None,
    )
}

fn execute_unavailable(request: &ToolRequest) -> ToolResult {
    match parse_operation(request) {
        Ok(_) => unavailable_result(request),
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
    fn completed_get_result_formats_goal_context() {
        let request = request(ToolName::GetGoal, "{}");
        let goal = sample_goal(ThreadGoalStatus::Active);
        let result = completed_result(&request, Some(&goal));

        assert_eq!(result.status, ToolStatus::Completed);
        assert!(result.output.unwrap().contains("Goal active."));
    }

    #[test]
    fn completed_create_result_reports_unfinished_existing_goal() {
        let request = request(ToolName::CreateGoal, r#"{"objective":"ship goals"}"#);
        let result = completed_result(&request, None);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(result.error.as_deref().unwrap().contains("unfinished goal"));
    }

    #[test]
    fn parse_and_format_completed_update_operation() {
        let request = request(ToolName::UpdateGoal, r#"{"status":"complete"}"#);
        let operation = parse_operation(&request).unwrap();
        let GoalToolOperation::Update(update) = operation else {
            panic!("expected update operation");
        };
        let goal = sample_goal(update.status.unwrap());
        let result = completed_result(&request, Some(&goal));

        assert_eq!(result.status, ToolStatus::Completed);
        assert!(result.output.unwrap().contains("Goal complete."));
        assert_eq!(update.status, Some(ThreadGoalStatus::Complete));
    }
}
