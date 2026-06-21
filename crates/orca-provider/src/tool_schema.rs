use serde_json::Value;

use orca_core::external_config::ExternalToolConfig;
use orca_core::subagent_types::SubagentType;
use orca_mcp::McpRegistry;
use orca_tools::registry::{self, ToolRegistry};

pub fn deepseek_tools_schema_with_mcp_and_external(
    mcp_registry: Option<&McpRegistry>,
    external_tools: &[ExternalToolConfig],
) -> Vec<Value> {
    if mcp_registry.is_none() {
        let registry = registry::tool_registry_with_mcp_and_external(None, external_tools);
        return deepseek_tools_schema_from_registry(&registry);
    }

    let registry = registry::tool_registry_with_mcp_and_external(mcp_registry, external_tools);
    deepseek_tools_schema_from_registry(&registry)
}

pub fn deepseek_tools_schema_from_registry(registry: &ToolRegistry) -> Vec<Value> {
    registry
        .model_visible_tools()
        .map(|tool| tool.schema())
        .collect()
}

pub fn deepseek_tools_schema_for_type_with_mcp_and_external(
    subagent_type: &SubagentType,
    mcp_registry: Option<&McpRegistry>,
    external_tools: &[ExternalToolConfig],
) -> Vec<Value> {
    let allowed = subagent_type.allowed_tools();
    let registry = registry::tool_registry_with_mcp_and_external(mcp_registry, external_tools);

    registry
        .iter()
        .filter(|tool| {
            let name = tool.name();
            name.starts_with("mcp__")
                || external_tools.iter().any(|external| external.name == name)
                || (name != "subagent" && allowed.contains(&name))
        })
        .map(|tool| tool.schema())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_generate_schema_from_tool_registry() {
        let registry = orca_tools::registry::default_tool_registry();
        let expected: Vec<Value> = registry
            .model_visible_tools()
            .map(|tool| tool.schema())
            .collect();

        assert_eq!(deepseek_tools_schema_from_registry(registry), expected);
    }

    #[test]
    fn generated_schema_uses_model_visible_tools_only() {
        let registry = orca_tools::registry::default_tool_registry();
        let tools = deepseek_tools_schema_from_registry(registry);
        let names = tools
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"glob"));
        assert!(!names.contains(&"list_files"));
    }
}
