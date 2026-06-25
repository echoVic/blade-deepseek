use serde_json::Value;

pub fn validate_json_schema_subset(
    schema: &Value,
    value: &Value,
    path: &str,
) -> Result<(), String> {
    let schema_object = schema
        .as_object()
        .ok_or_else(|| format!("{path} schema must be an object"))?;

    if let Some(expected_type) = schema_object.get("type") {
        validate_schema_type(expected_type, value, path)?;
    }

    if let Some(required) = schema_object.get("required") {
        let required = required
            .as_array()
            .ok_or_else(|| format!("{path}.required must be an array"))?;
        let object = value
            .as_object()
            .ok_or_else(|| format!("{path} expected object for required fields"))?;
        for required_field in required {
            let field = required_field
                .as_str()
                .ok_or_else(|| format!("{path}.required entries must be strings"))?;
            if !object.contains_key(field) {
                return Err(format!("{path}.{field} is required"));
            }
        }
    }

    if let Some(properties) = schema_object.get("properties") {
        let properties = properties
            .as_object()
            .ok_or_else(|| format!("{path}.properties must be an object"))?;
        let Some(object) = value.as_object() else {
            return Ok(());
        };
        for (property, property_schema) in properties {
            if let Some(property_value) = object.get(property) {
                validate_json_schema_subset(
                    property_schema,
                    property_value,
                    &format!("{path}.{property}"),
                )?;
            }
        }
    }

    Ok(())
}

fn validate_schema_type(expected_type: &Value, value: &Value, path: &str) -> Result<(), String> {
    if let Some(expected) = expected_type.as_str() {
        return validate_schema_type_name(expected, value, path);
    }

    let expected_types = expected_type
        .as_array()
        .ok_or_else(|| format!("{path}.type must be a string or array"))?;
    let mut expected_names = Vec::new();
    for expected_type in expected_types {
        let expected = expected_type
            .as_str()
            .ok_or_else(|| format!("{path}.type entries must be strings"))?;
        expected_names.push(expected);
        if schema_type_matches(expected, value) {
            return Ok(());
        }
    }
    Err(format!(
        "{path} expected one of {}, got {}",
        expected_names.join(", "),
        json_type_name(value)
    ))
}

fn validate_schema_type_name(expected: &str, value: &Value, path: &str) -> Result<(), String> {
    if schema_type_matches(expected, value) {
        Ok(())
    } else {
        Err(format!(
            "{path} expected {expected}, got {}",
            json_type_name(value)
        ))
    }
}

fn schema_type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
