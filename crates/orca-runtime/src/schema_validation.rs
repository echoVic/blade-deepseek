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

    if let Some(expected_const) = schema_object.get("const")
        && value != expected_const
    {
        return Err(format!(
            "{path} expected const {expected_const}, got {value}"
        ));
    }

    if let Some(enum_values) = schema_object.get("enum") {
        let enum_values = enum_values
            .as_array()
            .ok_or_else(|| format!("{path}.enum must be an array"))?;
        if !enum_values.iter().any(|candidate| candidate == value) {
            return Err(format!("{path} must match one of the enum values"));
        }
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

    if let Some(additional_properties) = schema_object.get("additionalProperties")
        && let Some(object) = value.as_object()
    {
        validate_additional_properties(
            additional_properties,
            schema_object.get("properties"),
            object,
            path,
        )?;
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

    if let Some(items) = schema_object.get("items")
        && let Some(items_value) = value.as_array()
    {
        validate_array_items(items, items_value, path)?;
    }

    validate_length_keywords(schema_object, value, path)?;
    validate_numeric_keywords(schema_object, value, path)?;

    Ok(())
}

fn validate_additional_properties(
    additional_properties: &Value,
    properties: Option<&Value>,
    object: &serde_json::Map<String, Value>,
    path: &str,
) -> Result<(), String> {
    let known_properties = match properties {
        Some(properties) => Some(
            properties
                .as_object()
                .ok_or_else(|| format!("{path}.properties must be an object"))?,
        ),
        None => None,
    };

    if additional_properties == &Value::Bool(false) {
        for property in object.keys() {
            if known_properties
                .map(|properties| !properties.contains_key(property))
                .unwrap_or(true)
            {
                return Err(format!("{path}.{property} is not allowed"));
            }
        }
        return Ok(());
    }

    if additional_properties == &Value::Bool(true) {
        return Ok(());
    }

    let additional_schema = additional_properties
        .as_object()
        .ok_or_else(|| format!("{path}.additionalProperties must be a boolean or object"))?;
    let additional_schema = Value::Object(additional_schema.clone());
    for (property, property_value) in object {
        if known_properties
            .map(|properties| properties.contains_key(property))
            .unwrap_or(false)
        {
            continue;
        }
        validate_json_schema_subset(
            &additional_schema,
            property_value,
            &format!("{path}.{property}"),
        )?;
    }
    Ok(())
}

fn validate_array_items(items: &Value, values: &[Value], path: &str) -> Result<(), String> {
    if items.is_object() {
        for (index, item) in values.iter().enumerate() {
            validate_json_schema_subset(items, item, &format!("{path}[{index}]"))?;
        }
        return Ok(());
    }

    let tuple_schemas = items
        .as_array()
        .ok_or_else(|| format!("{path}.items must be an object or array"))?;
    for (index, item) in values.iter().enumerate() {
        if let Some(item_schema) = tuple_schemas.get(index) {
            validate_json_schema_subset(item_schema, item, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}

fn validate_length_keywords(
    schema_object: &serde_json::Map<String, Value>,
    value: &Value,
    path: &str,
) -> Result<(), String> {
    if let Some(length) = value.as_str().map(str::chars) {
        let length = length.count() as u64;
        validate_u64_min_max(schema_object, "minLength", "maxLength", length, path)?;
    }
    if let Some(items) = value.as_array() {
        validate_u64_min_max(
            schema_object,
            "minItems",
            "maxItems",
            items.len() as u64,
            path,
        )?;
    }
    Ok(())
}

fn validate_u64_min_max(
    schema_object: &serde_json::Map<String, Value>,
    min_keyword: &str,
    max_keyword: &str,
    actual: u64,
    path: &str,
) -> Result<(), String> {
    if let Some(minimum) = schema_object.get(min_keyword) {
        let minimum = schema_u64(minimum, path, min_keyword)?;
        if actual < minimum {
            return Err(format!(
                "{path} length {actual} is less than {min_keyword} {minimum}"
            ));
        }
    }
    if let Some(maximum) = schema_object.get(max_keyword) {
        let maximum = schema_u64(maximum, path, max_keyword)?;
        if actual > maximum {
            return Err(format!(
                "{path} length {actual} is greater than {max_keyword} {maximum}"
            ));
        }
    }
    Ok(())
}

fn validate_numeric_keywords(
    schema_object: &serde_json::Map<String, Value>,
    value: &Value,
    path: &str,
) -> Result<(), String> {
    let Some(actual) = value.as_f64() else {
        return Ok(());
    };
    if let Some(minimum) = schema_object.get("minimum") {
        let minimum = schema_f64(minimum, path, "minimum")?;
        if actual < minimum {
            return Err(format!(
                "{path} value {actual} is less than minimum {minimum}"
            ));
        }
    }
    if let Some(maximum) = schema_object.get("maximum") {
        let maximum = schema_f64(maximum, path, "maximum")?;
        if actual > maximum {
            return Err(format!(
                "{path} value {actual} is greater than maximum {maximum}"
            ));
        }
    }
    Ok(())
}

fn schema_u64(value: &Value, path: &str, keyword: &str) -> Result<u64, String> {
    value
        .as_u64()
        .ok_or_else(|| format!("{path}.{keyword} must be a non-negative integer"))
}

fn schema_f64(value: &Value, path: &str, keyword: &str) -> Result<f64, String> {
    value
        .as_f64()
        .ok_or_else(|| format!("{path}.{keyword} must be a number"))
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::validate_json_schema_subset;

    #[test]
    fn validates_common_json_schema_keywords() {
        let schema = json!({
            "type": "object",
            "required": ["findings"],
            "additionalProperties": false,
            "properties": {
                "findings": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 2,
                    "items": {
                        "type": "object",
                        "required": ["severity", "score", "title"],
                        "additionalProperties": false,
                        "properties": {
                            "severity": { "enum": ["low", "medium", "high"] },
                            "score": { "type": "number", "minimum": 0, "maximum": 1 },
                            "title": { "type": "string", "minLength": 3, "maxLength": 20 }
                        }
                    }
                }
            }
        });

        assert!(
            validate_json_schema_subset(
                &schema,
                &json!({
                    "findings": [
                        { "severity": "high", "score": 0.9, "title": "SQL injection" }
                    ]
                }),
                "$",
            )
            .is_ok()
        );

        let error = validate_json_schema_subset(
            &schema,
            &json!({
                "findings": [
                    { "severity": "critical", "score": 1.2, "title": "x", "extra": true }
                ],
                "unexpected": true
            }),
            "$",
        )
        .unwrap_err();

        assert!(error.contains("unexpected"));
    }
}
