use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub strict_capable: bool,
}

pub fn deepseek_tools_schema(definitions: &[ProviderToolDefinition]) -> Vec<Value> {
    definitions.iter().map(deepseek_tool_schema).collect()
}

pub fn deepseek_strict_tools_schema_for_endpoint(
    definitions: &[ProviderToolDefinition],
    base_url: &str,
) -> Option<Vec<Value>> {
    if !is_strict_capable_endpoint(base_url)
        || !definitions
            .iter()
            .any(|definition| definition.strict_capable)
    {
        return None;
    }

    Some(
        definitions
            .iter()
            .map(|definition| {
                let mut tool = deepseek_tool_schema(definition);
                if definition.strict_capable {
                    let function = tool["function"]
                        .as_object_mut()
                        .expect("provider-generated function object");
                    require_all_properties(
                        function
                            .get_mut("parameters")
                            .expect("provider-generated parameters"),
                    );
                    function.insert("strict".to_string(), Value::Bool(true));
                }
                tool
            })
            .collect(),
    )
}

fn deepseek_tool_schema(definition: &ProviderToolDefinition) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": definition.name,
            "description": definition.description,
            "parameters": definition.input_schema,
        }
    })
}

fn is_strict_capable_endpoint(base_url: &str) -> bool {
    base_url.trim_end_matches('/').ends_with("/beta")
}

fn require_all_properties(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let is_typed_object = object.get("type").and_then(Value::as_str) == Some("object");
    if is_typed_object {
        object.insert("additionalProperties".to_string(), Value::Bool(false));
        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            let required = properties.keys().cloned().map(Value::String).collect();
            object.insert("required".to_string(), Value::Array(required));
        }
    }

    if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
        for property in properties.values_mut() {
            require_all_properties(property);
        }
    }
    if let Some(items) = object.get_mut("items") {
        require_all_properties(items);
    }
    for keyword in ["oneOf", "anyOf", "allOf"] {
        if let Some(branches) = object.get_mut(keyword).and_then(Value::as_array_mut) {
            for branch in branches {
                require_all_properties(branch);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(strict_capable: bool) -> ProviderToolDefinition {
        ProviderToolDefinition {
            name: "demo".to_string(),
            description: "demo tool".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "required_value": { "type": "string" },
                    "optional_value": { "type": ["string", "null"] },
                    "nested": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" }
                        }
                    }
                },
                "required": ["required_value"],
                "additionalProperties": false
            }),
            strict_capable,
        }
    }

    #[test]
    fn base_lowering_is_deterministic_and_omits_strict() {
        let definitions = vec![definition(true)];
        let first = deepseek_tools_schema(&definitions);
        let second = deepseek_tools_schema(&definitions);

        assert_eq!(first, second);
        assert!(first[0]["function"].get("strict").is_none());
    }

    #[test]
    fn strict_lowering_uses_definition_metadata_instead_of_tool_names() {
        let definitions = vec![definition(true)];
        let tools = deepseek_strict_tools_schema_for_endpoint(
            &definitions,
            "https://api.deepseek.com/beta",
        )
        .expect("strict tools");

        assert_eq!(tools[0]["function"]["strict"], true);
        assert_eq!(
            tools[0]["function"]["parameters"]["required"],
            json!(["nested", "optional_value", "required_value"])
        );
        assert_eq!(
            tools[0]["function"]["parameters"]["properties"]["nested"]["additionalProperties"],
            false
        );
    }
}
