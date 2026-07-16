use orca_core::provider_types::ProviderResponse;
use orca_core::thread_identity::TurnId;
use orca_core::thread_item_projection::{CompletedModelResponse, ModelResponseIdentity};

#[derive(Clone, Debug)]
pub struct RuntimeModelResponse {
    pub response: ProviderResponse,
    pub identity: ModelResponseIdentity,
}

impl RuntimeModelResponse {
    pub fn new(response: ProviderResponse, turn_id: TurnId) -> Self {
        Self {
            response,
            identity: ModelResponseIdentity::new(turn_id),
        }
    }

    pub fn from_parts(response: ProviderResponse, identity: ModelResponseIdentity) -> Self {
        Self { response, identity }
    }

    pub fn completed(&self) -> CompletedModelResponse {
        CompletedModelResponse::new(
            self.identity.clone(),
            self.response.assistant_content.clone(),
            self.response.assistant_reasoning.clone(),
            self.response.tool_calls.clone(),
        )
    }
}
