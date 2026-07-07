use orca_core::conversation::Conversation;

use crate::runtime_capability::{RuntimeCapabilityPatch, RuntimeCapabilitySnapshot};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeDirective {
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

impl From<RuntimeDirective> for RuntimeCapabilityPatch {
    fn from(directive: RuntimeDirective) -> Self {
        match directive {
            RuntimeDirective::SwitchModel { model, reason } => {
                RuntimeCapabilityPatch::SwitchModel { model, reason }
            }
            RuntimeDirective::ReplaceAllowedTools { tool_names, reason } => {
                RuntimeCapabilityPatch::ReplaceAllowedTools { tool_names, reason }
            }
            RuntimeDirective::InjectSystemMessage { message, reason } => {
                RuntimeCapabilityPatch::InjectSystemMessage { message, reason }
            }
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeDirectiveState {
    capabilities: RuntimeCapabilitySnapshot,
}

impl RuntimeDirectiveState {
    pub(crate) fn apply(&mut self, directive: RuntimeDirective) {
        self.apply_patch(directive.into());
    }

    pub(crate) fn apply_patch(&mut self, patch: RuntimeCapabilityPatch) {
        self.capabilities.apply_patch(patch);
    }

    pub fn capabilities(&self) -> &RuntimeCapabilitySnapshot {
        &self.capabilities
    }

    pub fn model_override(&self) -> Option<&str> {
        self.capabilities.model_override()
    }

    pub fn allowed_tools(&self) -> Option<&[String]> {
        self.capabilities.allowed_tools()
    }

    pub fn pending_system_messages(&self) -> &[String] {
        self.capabilities.pending_system_messages()
    }

    pub fn transition_reasons(&self) -> &[String] {
        self.capabilities.transition_reasons()
    }
}

pub(crate) fn conversation_with_runtime_system_messages(
    conversation: &Conversation,
    messages: &[String],
) -> Conversation {
    let mut conversation = conversation.clone();
    if !messages.is_empty() {
        conversation.add_system_pinned(format!(
            "[Runtime directive context]\n{}",
            messages.join("\n\n")
        ));
    }
    conversation
}
