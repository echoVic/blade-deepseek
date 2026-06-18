use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::approval_types::ActionKind;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExternalToolConfig {
    pub name: String,
    pub description: String,
    pub action_kind: ActionKind,
    pub command: String,
    #[serde(default)]
    pub schema: Value,
}

impl ExternalToolConfig {
    pub fn parameters_schema(&self) -> Value {
        if self.schema.get("type").is_some() {
            return self.schema.clone();
        }

        let required = self
            .schema
            .as_object()
            .map(|properties| {
                properties
                    .keys()
                    .map(|key| Value::String(key.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        json!({
            "type": "object",
            "properties": self.schema,
            "required": required,
            "additionalProperties": false
        })
    }
}
