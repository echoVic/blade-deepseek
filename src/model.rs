use serde::{Deserialize, Serialize};

use crate::runtime::subagent_types::SubagentType;

pub const FLASH_MODEL: &str = "deepseek-v4-flash";
pub const PRO_MODEL: &str = "deepseek-v4-pro";
pub const AUTO_MODEL: &str = "auto";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelSelection {
    value: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRouteReason {
    Explicit,
    DefaultPro,
    SubagentType,
    SubagentOverride,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelRouteDecision {
    pub requested_model: Option<String>,
    pub actual_model: String,
    pub reason: ModelRouteReason,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct ModelRouteContext<'a> {
    pub subagent_type: &'a SubagentType,
    pub subagent_model: Option<&'a str>,
}

impl ModelSelection {
    pub fn parse(value: Option<String>) -> Result<Self, String> {
        if let Some(model) = value.as_deref() {
            validate_model(model)?;
        }
        Ok(Self { value })
    }

    pub fn from_unchecked(value: Option<String>) -> Self {
        Self { value }
    }

    pub fn as_option(&self) -> Option<String> {
        self.value.clone()
    }

    pub fn as_deref(&self) -> Option<&str> {
        match self.value.as_deref() {
            Some(AUTO_MODEL) | None => None,
            other => other,
        }
    }

    pub fn as_history_value(&self) -> Option<String> {
        Some(self.display_name().to_string())
    }

    pub fn display_name(&self) -> &str {
        self.value.as_deref().unwrap_or(AUTO_MODEL)
    }

    pub fn with_subagent_override(&self, model: Option<String>) -> Self {
        match model.as_deref() {
            Some(AUTO_MODEL) | None => self.clone(),
            Some(_) => Self { value: model },
        }
    }

    pub fn route(&self, context: ModelRouteContext<'_>) -> ModelRouteDecision {
        if let Some(override_model) = context.subagent_model {
            return ModelRouteDecision {
                requested_model: self.value.clone(),
                actual_model: override_model.to_string(),
                reason: ModelRouteReason::SubagentOverride,
            };
        }

        match self.value.as_deref() {
            Some(FLASH_MODEL) => ModelRouteDecision {
                requested_model: self.value.clone(),
                actual_model: FLASH_MODEL.to_string(),
                reason: ModelRouteReason::Explicit,
            },
            Some(PRO_MODEL) => ModelRouteDecision {
                requested_model: self.value.clone(),
                actual_model: PRO_MODEL.to_string(),
                reason: ModelRouteReason::Explicit,
            },
            _ => ModelRouteDecision {
                requested_model: self.value.clone(),
                actual_model: PRO_MODEL.to_string(),
                reason: ModelRouteReason::DefaultPro,
            },
        }
    }
}

/// Model used for auxiliary/background tasks (compaction, memory extraction).
/// Always returns the cheapest model to minimize cost on utility work.
pub fn auxiliary_model() -> &'static str {
    FLASH_MODEL
}

pub fn validate_model(model: &str) -> Result<(), String> {
    match model {
        AUTO_MODEL | FLASH_MODEL | PRO_MODEL => Ok(()),
        other => Err(format!(
            "unsupported model '{other}'. Allowed models: auto, {FLASH_MODEL}, {PRO_MODEL}"
        )),
    }
}

pub fn allowed_models() -> &'static [&'static str] {
    &[AUTO_MODEL, FLASH_MODEL, PRO_MODEL]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ModelRouteContext<'static> {
        ModelRouteContext {
            subagent_type: &SubagentType::General,
            subagent_model: None,
        }
    }

    #[test]
    fn auto_defaults_to_pro() {
        let selection = ModelSelection::parse(None).unwrap();
        let decision = selection.route(context());
        assert_eq!(decision.actual_model, PRO_MODEL);
        assert_eq!(decision.reason, ModelRouteReason::DefaultPro);
    }

    #[test]
    fn explicit_flash_stays_flash() {
        let selection = ModelSelection::parse(Some(FLASH_MODEL.to_string())).unwrap();
        let decision = selection.route(context());
        assert_eq!(decision.actual_model, FLASH_MODEL);
        assert_eq!(decision.reason, ModelRouteReason::Explicit);
    }

    #[test]
    fn explicit_pro_stays_pro() {
        let selection = ModelSelection::parse(Some(PRO_MODEL.to_string())).unwrap();
        let decision = selection.route(context());
        assert_eq!(decision.actual_model, PRO_MODEL);
        assert_eq!(decision.reason, ModelRouteReason::Explicit);
    }

    #[test]
    fn subagent_model_override_wins() {
        let selection = ModelSelection::parse(None).unwrap();
        let ctx = ModelRouteContext {
            subagent_type: &SubagentType::General,
            subagent_model: Some(FLASH_MODEL),
        };
        let decision = selection.route(ctx);
        assert_eq!(decision.actual_model, FLASH_MODEL);
        assert_eq!(decision.reason, ModelRouteReason::SubagentOverride);
    }

    #[test]
    fn auto_subagent_override_preserves_parent_router() {
        let selection = ModelSelection::parse(None).unwrap();
        assert_eq!(
            selection
                .with_subagent_override(Some(AUTO_MODEL.to_string()))
                .display_name(),
            AUTO_MODEL
        );
    }

    #[test]
    fn auxiliary_model_returns_flash() {
        assert_eq!(auxiliary_model(), FLASH_MODEL);
    }
}
