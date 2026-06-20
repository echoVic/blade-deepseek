use orca_core::plan_types::{PlanStatus, UpdatePlanArgs};
use orca_core::tool_types::{ToolRequest, ToolResult};

pub fn execute(request: &ToolRequest) -> ToolResult {
    match parse_args(request) {
        Ok(args) => ToolResult::completed(request, format_success(&args), false),
        Err(error) => ToolResult::failed(request, error, None),
    }
}

pub fn parse_args(request: &ToolRequest) -> Result<UpdatePlanArgs, String> {
    let raw = request
        .raw_arguments
        .as_deref()
        .ok_or_else(|| "update_plan requires raw JSON arguments".to_string())?;
    let args: UpdatePlanArgs =
        serde_json::from_str(raw).map_err(|error| format!("invalid update_plan JSON: {error}"))?;
    validate_args(args)
}

fn validate_args(args: UpdatePlanArgs) -> Result<UpdatePlanArgs, String> {
    for item in &args.plan {
        if item.step.trim().is_empty() {
            return Err("update_plan step cannot be empty".to_string());
        }
    }
    Ok(args)
}

fn format_success(args: &UpdatePlanArgs) -> String {
    let mut lines = Vec::with_capacity(args.plan.len() + 2);
    lines.push(format!("Plan updated ({} item(s)).", args.plan.len()));
    for item in &args.plan {
        let icon = match item.status {
            PlanStatus::Completed => "[x]",
            PlanStatus::InProgress => "[>]",
            PlanStatus::Pending => "[ ]",
        };
        lines.push(format!("  {icon} {}", item.step));
    }
    lines.join("\n")
}

pub fn format_context_message(args: &UpdatePlanArgs) -> String {
    let mut lines = Vec::with_capacity(args.plan.len() + 3);
    lines.push("[Pinned plan state]".to_string());
    if let Some(explanation) = args
        .explanation
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        lines.push(format!("explanation: {}", explanation.trim()));
    }
    for item in &args.plan {
        let status = match item.status {
            PlanStatus::Completed => "completed",
            PlanStatus::InProgress => "in_progress",
            PlanStatus::Pending => "pending",
        };
        lines.push(format!("[{status}] {}", item.step));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::plan_types::PlanItem;
    use orca_core::tool_types::ToolName;

    fn request(arguments: &str) -> ToolRequest {
        ToolRequest {
            id: "call-1".to_string(),
            name: ToolName::UpdatePlan,
            action: ActionKind::Read,
            target: Some("2 items".to_string()),
            raw_arguments: Some(arguments.to_string()),
        }
    }

    #[test]
    fn parses_valid_plan() {
        let args = parse_args(&request(
            r#"{"explanation":"starting","plan":[{"step":"Inspect code","status":"completed"},{"step":"Patch tool","status":"in_progress"}]}"#,
        ))
        .unwrap();

        assert_eq!(args.explanation.as_deref(), Some("starting"));
        assert_eq!(args.plan.len(), 2);
        assert_eq!(args.plan[1].status, PlanStatus::InProgress);
    }

    #[test]
    fn rejects_empty_step() {
        let error =
            parse_args(&request(r#"{"plan":[{"step":"  ","status":"pending"}]}"#)).unwrap_err();
        assert!(error.contains("cannot be empty"));
    }

    #[test]
    fn accepts_empty_plan() {
        let args = parse_args(&request(r#"{"plan":[]}"#)).unwrap();
        assert!(args.plan.is_empty());
    }

    #[test]
    fn handles_special_characters_in_step() {
        let json =
            r#"{"plan":[{"step":"Fix \"quotes\" & <tags> 🚀\nnewline","status":"pending"}]}"#;
        let args = parse_args(&request(json)).unwrap();
        assert!(args.plan[0].step.contains("quotes"));
        assert!(args.plan[0].step.contains("\u{1f680}"));
    }

    #[test]
    fn format_success_echoes_plan_state() {
        let args = UpdatePlanArgs {
            explanation: None,
            plan: vec![
                PlanItem {
                    step: "Done".to_string(),
                    status: PlanStatus::Completed,
                },
                PlanItem {
                    step: "Doing".to_string(),
                    status: PlanStatus::InProgress,
                },
                PlanItem {
                    step: "Todo".to_string(),
                    status: PlanStatus::Pending,
                },
            ],
        };
        let output = format_success(&args);
        assert!(output.contains("[x] Done"));
        assert!(output.contains("[>] Doing"));
        assert!(output.contains("[ ] Todo"));
    }

    #[test]
    fn format_context_message_marks_current_plan_state() {
        let args = UpdatePlanArgs {
            explanation: Some("working".to_string()),
            plan: vec![PlanItem {
                step: "Patch context".to_string(),
                status: PlanStatus::InProgress,
            }],
        };

        let output = format_context_message(&args);

        assert!(output.starts_with("[Pinned plan state]"));
        assert!(output.contains("working"));
        assert!(output.contains("[in_progress] Patch context"));
    }

    #[test]
    fn serializes_null_explanation() {
        let args = UpdatePlanArgs {
            explanation: None,
            plan: vec![],
        };
        let json = serde_json::to_string(&args).unwrap();
        assert!(json.contains(r#""explanation":null"#));
    }
}
