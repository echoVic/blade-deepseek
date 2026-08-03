use std::collections::HashSet;

use orca_core::subagent_types::SubagentType;
use orca_core::tool_types::{ToolName, ToolRequest};
use serde_json::Value;

use crate::registry::ToolRegistry;

const GOAL_TOOL_NAMES: &[&str] = &["get_goal", "create_goal", "update_goal"];
const STRICT_MODE_TOOL_NAMES: &[&str] = &["glob", "update_goal", "update_plan"];

#[derive(Clone, Debug, PartialEq)]
pub struct CanonicalToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub strict_capable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolPolicy {
    selection: ToolSelection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ToolSelection {
    Base,
    Goal,
    Allowed(Vec<String>),
    Subagent(Vec<String>),
}

impl ToolPolicy {
    pub fn base() -> Self {
        Self {
            selection: ToolSelection::Base,
        }
    }

    pub fn goal() -> Self {
        Self {
            selection: ToolSelection::Goal,
        }
    }

    pub fn allowed<S: AsRef<str>>(names: &[S]) -> Self {
        Self {
            selection: ToolSelection::Allowed(
                names.iter().map(|name| name.as_ref().to_string()).collect(),
            ),
        }
    }

    pub fn for_subagent(subagent_type: &SubagentType) -> Self {
        Self {
            selection: ToolSelection::Subagent(
                subagent_type
                    .allowed_tools()
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            ),
        }
    }
}

pub fn canonical_tool_definitions(
    policy: &ToolPolicy,
    registry: &ToolRegistry,
) -> Vec<CanonicalToolDefinition> {
    let allowed = match &policy.selection {
        ToolSelection::Allowed(names) | ToolSelection::Subagent(names) => {
            Some(canonical_allowed_names(registry, names))
        }
        ToolSelection::Base | ToolSelection::Goal => None,
    };

    registry
        .model_visible_tools()
        .filter(|tool| match &policy.selection {
            ToolSelection::Base => !GOAL_TOOL_NAMES.contains(&tool.name()),
            ToolSelection::Goal => true,
            ToolSelection::Allowed(_) => allowed
                .as_ref()
                .is_some_and(|allowed| allowed.contains(tool.name())),
            ToolSelection::Subagent(_) => {
                matches!(tool.spec().name, ToolName::Mcp(_) | ToolName::External(_))
                    || (tool.name() != "subagent"
                        && allowed
                            .as_ref()
                            .is_some_and(|allowed| allowed.contains(tool.name())))
            }
        })
        .map(|tool| CanonicalToolDefinition {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            input_schema: tool.spec().input_schema.clone(),
            strict_capable: STRICT_MODE_TOOL_NAMES.contains(&tool.name()),
        })
        .collect()
}

pub fn normalize_tool_arguments(name: &ToolName, value: Value) -> Result<Value, String> {
    let raw = serde_json::to_string(&value)
        .map_err(|error| format!("failed to serialize tool arguments: {error}"))?;
    let normalized = match name {
        ToolName::UpdatePlan => crate::update_plan::normalize_raw_arguments(&raw).unwrap_or(raw),
        ToolName::UpdateGoal => crate::update_goal::normalized_update_raw_arguments(&raw),
        _ => raw,
    };
    serde_json::from_str(&normalized)
        .map_err(|error| format!("failed to parse normalized tool arguments: {error}"))
}

pub fn normalize_tool_request(
    registry: &ToolRegistry,
    request: &ToolRequest,
) -> Result<ToolRequest, String> {
    let resolved = registry.resolve(request.name.as_str());
    let name = resolved
        .as_ref()
        .map(|resolved| resolved.requested_name.clone())
        .unwrap_or_else(|| request.name.clone());
    let action = resolved
        .as_ref()
        .map(|resolved| resolved.spec.capabilities.action_kind())
        .unwrap_or(request.action);

    let (raw_arguments, arguments) = match request.raw_arguments.as_deref() {
        Some(raw) => {
            let value: Value = serde_json::from_str(raw)
                .map_err(|error| format!("arguments are not valid JSON: {error}"))?;
            let normalized = normalize_tool_arguments(&name, value.clone())?;
            let raw_arguments = if normalized == value {
                raw.to_string()
            } else {
                serde_json::to_string(&normalized)
                    .map_err(|error| format!("failed to serialize normalized arguments: {error}"))?
            };
            (Some(raw_arguments), Some(normalized))
        }
        None => (None, None),
    };
    let target = arguments
        .as_ref()
        .and_then(|arguments| tool_target(&name, arguments))
        .or_else(|| request.target.clone());

    Ok(ToolRequest {
        id: request.id.clone(),
        name,
        action,
        target,
        raw_arguments,
    })
}

fn canonical_allowed_names(registry: &ToolRegistry, names: &[String]) -> HashSet<String> {
    names
        .iter()
        .filter_map(|name| {
            registry
                .resolve(name)
                .map(|resolved| resolved.tool.name().to_string())
        })
        .collect()
}

fn tool_target(name: &ToolName, arguments: &Value) -> Option<String> {
    match name {
        ToolName::ReadFile | ToolName::Edit | ToolName::WriteFile => {
            arguments["path"].as_str().map(String::from)
        }
        ToolName::ListFiles | ToolName::Glob => arguments["path"]
            .as_str()
            .map(String::from)
            .or_else(|| Some(".".to_string())),
        ToolName::Grep => arguments["pattern"].as_str().map(String::from),
        ToolName::Bash => arguments["command"].as_str().map(String::from),
        ToolName::GitStatus => Some(".".to_string()),
        ToolName::Subagent => arguments["description"]
            .as_str()
            .or_else(|| arguments["prompt"].as_str())
            .map(String::from),
        ToolName::WebSearch => arguments["query"].as_str().map(String::from),
        ToolName::UpdatePlan => arguments["plan"]
            .as_array()
            .map(|plan| format!("{} items", plan.len())),
        ToolName::Mcp(name) | ToolName::External(name) => Some(name.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_plan_boolean_status_flags() {
        let normalized = normalize_tool_arguments(
            &ToolName::UpdatePlan,
            serde_json::json!({
                "plan": [{ "step": "inspect", "completed": true }]
            }),
        )
        .expect("normalize plan");

        assert_eq!(normalized["plan"][0]["status"], "completed");
        assert!(normalized["plan"][0].get("completed").is_none());
    }
}
