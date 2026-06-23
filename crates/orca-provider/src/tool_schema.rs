use std::collections::HashSet;

use serde_json::Value;

use orca_core::external_config::ExternalToolConfig;
use orca_core::subagent_types::SubagentType;
use orca_mcp::McpRegistry;
use orca_tools::registry::{self, ToolRegistry};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolSchemaMode {
    Base,
    Goal,
}

pub fn deepseek_tools_schema_with_mcp_and_external(
    mcp_registry: Option<&McpRegistry>,
    external_tools: &[ExternalToolConfig],
) -> Vec<Value> {
    deepseek_tools_schema_with_mcp_external_and_mode(
        mcp_registry,
        external_tools,
        ToolSchemaMode::Base,
    )
}

pub fn deepseek_goal_tools_schema_with_mcp_and_external(
    mcp_registry: Option<&McpRegistry>,
    external_tools: &[ExternalToolConfig],
) -> Vec<Value> {
    deepseek_tools_schema_with_mcp_external_and_mode(
        mcp_registry,
        external_tools,
        ToolSchemaMode::Goal,
    )
}

fn deepseek_tools_schema_with_mcp_external_and_mode(
    mcp_registry: Option<&McpRegistry>,
    external_tools: &[ExternalToolConfig],
    mode: ToolSchemaMode,
) -> Vec<Value> {
    if mcp_registry.is_none() {
        let registry = registry::tool_registry_with_mcp_and_external(None, external_tools);
        return deepseek_tools_schema_from_registry_with_mode(&registry, mode);
    }

    let registry = registry::tool_registry_with_mcp_and_external(mcp_registry, external_tools);
    deepseek_tools_schema_from_registry_with_mode(&registry, mode)
}

pub fn deepseek_tools_schema_from_registry(registry: &ToolRegistry) -> Vec<Value> {
    deepseek_tools_schema_from_registry_with_mode(registry, ToolSchemaMode::Base)
}

pub fn deepseek_goal_tools_schema_from_registry(registry: &ToolRegistry) -> Vec<Value> {
    deepseek_tools_schema_from_registry_with_mode(registry, ToolSchemaMode::Goal)
}

fn deepseek_tools_schema_from_registry_with_mode(
    registry: &ToolRegistry,
    mode: ToolSchemaMode,
) -> Vec<Value> {
    registry
        .model_visible_tools()
        .filter(|tool| tool_visible_in_schema_mode(tool.name(), mode))
        .map(|tool| tool.schema())
        .collect()
}

pub fn tool_visible_in_schema_mode(name: &str, mode: ToolSchemaMode) -> bool {
    mode == ToolSchemaMode::Goal || !matches!(name, "get_goal" | "create_goal" | "update_goal")
}

pub fn deepseek_tools_schema_for_type_with_mcp_and_external(
    subagent_type: &SubagentType,
    mcp_registry: Option<&McpRegistry>,
    external_tools: &[ExternalToolConfig],
) -> Vec<Value> {
    let allowed = subagent_type.allowed_tools();
    let registry = registry::tool_registry_with_mcp_and_external(mcp_registry, external_tools);
    let allowed_canonical_names = allowed
        .iter()
        .filter_map(|name| {
            registry
                .resolve(name)
                .map(|resolved| resolved.tool.name().to_string())
        })
        .collect::<HashSet<_>>();

    registry
        .model_visible_tools()
        .filter(|tool| {
            let name = tool.name();
            name.starts_with("mcp__")
                || external_tools.iter().any(|external| external.name == name)
                || (name != "subagent" && allowed_canonical_names.contains(name))
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
            .filter(|tool| tool_visible_in_schema_mode(tool.name(), ToolSchemaMode::Base))
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

    #[test]
    fn base_schema_hides_goal_only_tool() {
        let registry = orca_tools::registry::default_tool_registry();
        let tools = deepseek_tools_schema_from_registry(registry);
        let names = tools
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<Vec<_>>();

        assert!(!names.contains(&"get_goal"));
        assert!(!names.contains(&"create_goal"));
        assert!(!names.contains(&"update_goal"));
    }

    #[test]
    fn goal_schema_exposes_goal_tools() {
        let registry = orca_tools::registry::default_tool_registry();
        let tools = deepseek_goal_tools_schema_from_registry(registry);
        let names = tools
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"get_goal"));
        assert!(names.contains(&"create_goal"));
        assert!(names.contains(&"update_goal"));
    }

    #[test]
    fn typed_subagent_schema_resolves_allowed_list_files_alias_to_glob() {
        let tools = deepseek_tools_schema_for_type_with_mcp_and_external(
            &SubagentType::CodeReviewer,
            None,
            &[],
        );
        let names = tools
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"glob"));
        assert!(!names.contains(&"list_files"));
        assert!(!names.contains(&"subagent"));
    }

    // --- DeepSeek prefix-cache stability locks ---------------------------------
    //
    // The tool schema is part of the IMMUTABLE PREFIX sent to DeepSeek on every
    // turn. If its byte serialization changes between turns the server-side
    // prefix cache misses for the entire conversation. These tests pin the two
    // properties that guarantee byte-stability: deterministic tool ordering and
    // deterministic JSON key ordering.

    fn external_tool(name: &str) -> ExternalToolConfig {
        ExternalToolConfig {
            name: name.to_string(),
            description: format!("external {name}"),
            action_kind: orca_core::approval_types::ActionKind::Read,
            command: "true".to_string(),
            schema: serde_json::json!({}),
        }
    }

    #[test]
    fn builtin_schema_bytes_are_identical_across_rebuilds() {
        // Two independent builds of the base schema must serialize to the exact
        // same bytes — anything else means a non-deterministic source crept in
        // (e.g. a HashMap-backed iteration order).
        let first = serde_json::to_string(&deepseek_tools_schema_with_mcp_and_external(None, &[]))
            .expect("serialize first build");
        let second = serde_json::to_string(&deepseek_tools_schema_with_mcp_and_external(None, &[]))
            .expect("serialize second build");
        assert_eq!(first, second);
    }

    #[test]
    fn external_tools_keep_config_order_after_builtins() {
        let externals = [
            external_tool("zzz_last"),
            external_tool("aaa_first"),
            external_tool("mmm_middle"),
        ];
        let tools = deepseek_tools_schema_with_mcp_and_external(None, &externals);
        let names = tools
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<Vec<_>>();

        // Builtins come first; the external block preserves config order rather
        // than being sorted (sorting would still be stable, but config order is
        // what the registry guarantees and what we must lock).
        let external_names: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| {
                name.starts_with("zzz") || name.starts_with("aaa") || name.starts_with("mmm")
            })
            .collect();
        assert_eq!(external_names, vec!["zzz_last", "aaa_first", "mmm_middle"]);

        // Re-running with the same input is byte-identical.
        let again = deepseek_tools_schema_with_mcp_and_external(None, &externals);
        assert_eq!(
            serde_json::to_string(&tools).unwrap(),
            serde_json::to_string(&again).unwrap()
        );
    }

    #[test]
    fn json_object_keys_serialize_in_sorted_order() {
        // serde_json without the `preserve_order` feature serializes object keys
        // in sorted (BTreeMap) order. If a future dependency change enables
        // `preserve_order`, key order would become insertion-dependent and the
        // prefix cache could silently break. This test fails loudly in that case.
        let value = serde_json::json!({
            "type": "function",
            "function": {
                "name": "demo",
                "description": "d",
                "parameters": { "b": 1, "a": 2 }
            }
        });
        let serialized = serde_json::to_string(&value).unwrap();
        assert!(
            serialized.find("\"a\"").unwrap() < serialized.find("\"b\"").unwrap(),
            "object keys must serialize sorted; got {serialized}"
        );
        // Top-level: "function" sorts before "type".
        assert!(serialized.find("\"function\"").unwrap() < serialized.find("\"type\"").unwrap());
    }
}
