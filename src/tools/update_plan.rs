use serde::{Deserialize, Serialize};

use crate::tools::{ToolRequest, ToolResult};

const MAX_PLAN_ITEMS: usize = 50;
const MAX_STEP_LEN: usize = 200;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanItem {
    pub step: String,
    pub status: PlanStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpdatePlanArgs {
    pub explanation: Option<String>,
    pub plan: Vec<PlanItem>,
}

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
    if args.plan.len() > MAX_PLAN_ITEMS {
        return Err(format!(
            "update_plan accepts at most {MAX_PLAN_ITEMS} items, got {}",
            args.plan.len()
        ));
    }
    let mut in_progress = 0;
    for item in &args.plan {
        if item.step.trim().is_empty() {
            return Err("update_plan step cannot be empty".to_string());
        }
        if item.step.len() > MAX_STEP_LEN {
            return Err(format!(
                "update_plan step exceeds {MAX_STEP_LEN} characters"
            ));
        }
        if item.status == PlanStatus::InProgress {
            in_progress += 1;
        }
    }
    if in_progress > 1 {
        return Err("update_plan accepts at most one in_progress step".to_string());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::policy::ActionKind;
    use crate::tools::ToolName;

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
    fn rejects_multiple_in_progress_items() {
        let error = parse_args(&request(
            r#"{"plan":[{"step":"One","status":"in_progress"},{"step":"Two","status":"in_progress"}]}"#,
        ))
        .unwrap_err();

        assert!(error.contains("at most one"));
    }

    #[test]
    fn rejects_too_many_items() {
        let items: Vec<String> = (0..51)
            .map(|i| format!(r#"{{"step":"Step {i}","status":"pending"}}"#))
            .collect();
        let json = format!(r#"{{"plan":[{}]}}"#, items.join(","));
        let error = parse_args(&request(&json)).unwrap_err();

        assert!(error.contains("at most 50"));
    }

    #[test]
    fn rejects_step_exceeding_length_limit() {
        let long_step = "x".repeat(201);
        let json = format!(r#"{{"plan":[{{"step":"{long_step}","status":"pending"}}]}}"#);
        let error = parse_args(&request(&json)).unwrap_err();

        assert!(error.contains("exceeds 200 characters"));
    }

    #[test]
    fn accepts_empty_plan() {
        let args = parse_args(&request(r#"{"plan":[]}"#)).unwrap();
        assert!(args.plan.is_empty());
    }

    #[test]
    fn handles_special_characters_in_step() {
        let json = r#"{"plan":[{"step":"Fix \"quotes\" & <tags> 🚀\nnewline","status":"pending"}]}"#;
        let args = parse_args(&request(json)).unwrap();
        assert!(args.plan[0].step.contains("quotes"));
        assert!(args.plan[0].step.contains("🚀"));
    }

    #[test]
    fn format_success_echoes_plan_state() {
        let args = UpdatePlanArgs {
            explanation: None,
            plan: vec![
                PlanItem { step: "Done".to_string(), status: PlanStatus::Completed },
                PlanItem { step: "Doing".to_string(), status: PlanStatus::InProgress },
                PlanItem { step: "Todo".to_string(), status: PlanStatus::Pending },
            ],
        };
        let output = format_success(&args);
        assert!(output.contains("[x] Done"));
        assert!(output.contains("[>] Doing"));
        assert!(output.contains("[ ] Todo"));
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
