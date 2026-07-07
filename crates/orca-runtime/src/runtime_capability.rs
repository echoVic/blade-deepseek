#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeCapabilityPatch {
    SwitchModel {
        model: String,
        reason: String,
    },
    ReplaceAllowedTools {
        tool_names: Vec<String>,
        reason: String,
    },
    InjectSystemMessage {
        message: String,
        reason: String,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeCapabilitySnapshot {
    model_override: Option<String>,
    allowed_tools: Option<Vec<String>>,
    pending_system_messages: Vec<String>,
    transition_reasons: Vec<String>,
}

impl RuntimeCapabilitySnapshot {
    pub fn apply_patch(&mut self, patch: RuntimeCapabilityPatch) {
        match patch {
            RuntimeCapabilityPatch::SwitchModel { model, reason } => {
                self.model_override = Some(model);
                self.record_transition("switch_model", reason);
            }
            RuntimeCapabilityPatch::ReplaceAllowedTools { tool_names, reason } => {
                self.allowed_tools = Some(tool_names);
                self.record_transition("replace_allowed_tools", reason);
            }
            RuntimeCapabilityPatch::InjectSystemMessage { message, reason } => {
                self.pending_system_messages.push(message);
                self.record_transition("inject_system_message", reason);
            }
        }
    }

    pub fn model_override(&self) -> Option<&str> {
        self.model_override.as_deref()
    }

    pub fn allowed_tools(&self) -> Option<&[String]> {
        self.allowed_tools.as_deref()
    }

    pub fn pending_system_messages(&self) -> &[String] {
        &self.pending_system_messages
    }

    pub fn transition_reasons(&self) -> &[String] {
        &self.transition_reasons
    }

    fn record_transition(&mut self, kind: &str, reason: String) {
        self.transition_reasons.push(format!("{kind}: {reason}"));
    }
}
