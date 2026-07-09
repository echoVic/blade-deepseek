use orca_core::plan_types::{PlanStatus, UpdatePlanArgs};
use orca_core::tool_types::{ToolRequest, ToolResult};
use serde_json::Value;

const TOP_LEVEL_KEYS: [&str; 2] = ["explanation", "plan"];
const ITEM_KEYS: [&str; 2] = ["step", "status"];
const VALID_STATUSES: [&str; 3] = ["pending", "in_progress", "completed"];
// Checked in this order when deriving `status` from boolean flags.
const STATUS_FLAG_KEYS: [&str; 3] = ["completed", "in_progress", "pending"];

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
    let normalized = normalize_raw_arguments(raw);
    let effective = normalized.as_deref().unwrap_or(raw);
    let args: UpdatePlanArgs = serde_json::from_str(effective)
        .map_err(|error| format!("invalid update_plan JSON: {error}"))?;
    validate_args(args)
}

/// Models without training on this exact tool (DeepSeek in particular) habitually
/// emit plan items carrying boolean status flags alongside or instead of `status`
/// (`{"completed": true, "step": ...}`), which the strict schema rejects. Map those
/// flags onto `status` and drop unknown keys so the call survives validation.
/// Returns `Some(normalized)` when a rewrite happened, `None` when the arguments
/// were already clean or are not parseable JSON (left for validation to report).
pub fn normalize_raw_arguments(raw: &str) -> Option<String> {
    let mut value: Value = serde_json::from_str(raw).ok()?;
    if !normalize_args_value(&mut value) {
        return None;
    }
    serde_json::to_string(&value).ok()
}

fn normalize_args_value(value: &mut Value) -> bool {
    let Some(object) = value.as_object_mut() else {
        return false;
    };
    let mut changed = remove_unknown_keys(object, &TOP_LEVEL_KEYS);
    let Some(plan) = object.get_mut("plan").and_then(Value::as_array_mut) else {
        return changed;
    };
    for item in plan {
        let Some(item_object) = item.as_object_mut() else {
            continue;
        };
        let has_valid_status = item_object
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| VALID_STATUSES.contains(&status));
        if !has_valid_status
            && let Some(derived) = STATUS_FLAG_KEYS
                .iter()
                .find(|flag| item_object.get(**flag) == Some(&Value::Bool(true)))
        {
            item_object.insert("status".to_string(), Value::String((*derived).to_string()));
            changed = true;
        }
        changed |= remove_unknown_keys(item_object, &ITEM_KEYS);
    }
    changed
}

fn remove_unknown_keys(object: &mut serde_json::Map<String, Value>, known: &[&str]) -> bool {
    let unknown: Vec<String> = object
        .keys()
        .filter(|key| !known.contains(&key.as_str()))
        .cloned()
        .collect();
    for key in &unknown {
        object.remove(key);
    }
    !unknown.is_empty()
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

    // Real DeepSeek failure shape A (session 2026-07-09, lines 105/114): boolean
    // flags instead of `status`.
    #[test]
    fn parses_boolean_flags_without_status() {
        let args = parse_args(&request(
            r#"{"explanation":"done","plan":[{"completed":true,"step":"Create generator"},{"in_progress":true,"step":"Write tests"},{"pending":true,"step":"Verify"}]}"#,
        ))
        .unwrap();

        assert_eq!(args.plan[0].status, PlanStatus::Completed);
        assert_eq!(args.plan[1].status, PlanStatus::InProgress);
        assert_eq!(args.plan[2].status, PlanStatus::Pending);
    }

    // Real DeepSeek failure shape B (session 2026-07-09, lines 213/260/275/345):
    // redundant boolean flags alongside a valid `status`.
    #[test]
    fn parses_redundant_boolean_flags_alongside_status() {
        let args = parse_args(&request(
            r#"{"plan":[{"completed":true,"status":"completed","step":"Add backends"},{"in_progress":true,"status":"in_progress","step":"Update tests"}]}"#,
        ))
        .unwrap();

        assert_eq!(args.plan[0].status, PlanStatus::Completed);
        assert_eq!(args.plan[1].status, PlanStatus::InProgress);
    }

    #[test]
    fn normalize_maps_flags_and_strips_unknown_keys() {
        let normalized = normalize_raw_arguments(
            r#"{"note":"x","plan":[{"completed":true,"step":"a","priority":1}]}"#,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&normalized).unwrap();

        assert!(value.get("note").is_none());
        let item = &value["plan"][0];
        assert_eq!(item["status"], "completed");
        assert_eq!(item["step"], "a");
        assert!(item.get("completed").is_none());
        assert!(item.get("priority").is_none());
    }

    #[test]
    fn normalize_returns_none_for_clean_arguments() {
        assert!(
            normalize_raw_arguments(
                r#"{"explanation":"x","plan":[{"step":"a","status":"pending"}]}"#
            )
            .is_none()
        );
        assert!(normalize_raw_arguments("not json").is_none());
    }

    #[test]
    fn normalize_ignores_false_flags_and_keeps_valid_status() {
        // A false flag carries no status information: strip it, do not derive.
        let normalized =
            normalize_raw_arguments(r#"{"plan":[{"completed":false,"step":"a"}]}"#).unwrap();
        let value: Value = serde_json::from_str(&normalized).unwrap();
        assert!(value["plan"][0].get("status").is_none());

        // A valid status wins over a conflicting flag.
        let normalized = normalize_raw_arguments(
            r#"{"plan":[{"completed":true,"status":"pending","step":"a"}]}"#,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&normalized).unwrap();
        assert_eq!(value["plan"][0]["status"], "pending");
    }

    #[test]
    fn normalize_derives_status_when_existing_value_invalid() {
        let normalized =
            normalize_raw_arguments(r#"{"plan":[{"status":"done","completed":true,"step":"a"}]}"#)
                .unwrap();
        let value: Value = serde_json::from_str(&normalized).unwrap();
        assert_eq!(value["plan"][0]["status"], "completed");
    }
}
