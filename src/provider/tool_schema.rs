use serde_json::Value;

use crate::mcp::McpRegistry;
use crate::runtime::subagent_types::SubagentType;
use crate::tools::registry::{self, ToolRegistry};

pub fn deepseek_tools_schema() -> Vec<Value> {
    let registry = registry::default_tool_registry();
    deepseek_tools_schema_from_registry(registry)
}

pub fn deepseek_tools_schema_with_mcp(mcp_registry: Option<&McpRegistry>) -> Vec<Value> {
    if mcp_registry.is_none() {
        return deepseek_tools_schema();
    }

    let registry = registry::tool_registry_with_mcp(mcp_registry);
    deepseek_tools_schema_from_registry(&registry)
}

pub fn deepseek_tools_schema_from_registry(registry: &ToolRegistry) -> Vec<Value> {
    registry.iter().map(|tool| tool.schema()).collect()
}

pub fn deepseek_tools_schema_for_type_with_mcp(
    subagent_type: &SubagentType,
    mcp_registry: Option<&McpRegistry>,
) -> Vec<Value> {
    let allowed = subagent_type.allowed_tools();
    let registry = registry::tool_registry_with_mcp(mcp_registry);

    registry
        .iter()
        .filter(|tool| {
            let name = tool.name();
            name.starts_with("mcp__") || (name != "subagent" && allowed.contains(&name))
        })
        .map(|tool| tool.schema())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::client::McpRegistry;
    use crate::mcp::types::McpTool;

    #[test]
    fn merges_mcp_tools_into_schema() {
        let registry = McpRegistry::from_tools_for_test(vec![McpTool {
            server: "demo".to_string(),
            name: "search".to_string(),
            schema_name: "mcp__demo__search".to_string(),
            description: Some("search docs".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }),
        }]);

        let schema = deepseek_tools_schema_with_mcp(Some(&registry));
        assert!(
            schema
                .iter()
                .any(|tool| { tool["function"]["name"] == "mcp__demo__search" })
        );
    }

    #[test]
    fn can_generate_schema_from_tool_registry() {
        let registry = crate::tools::registry::default_tool_registry();
        let expected: Vec<Value> = registry.iter().map(|tool| tool.schema()).collect();

        assert_eq!(deepseek_tools_schema_from_registry(registry), expected);
    }
}
