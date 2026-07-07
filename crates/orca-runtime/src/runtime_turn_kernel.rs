use crate::extension::{ExtensionData, RuntimeExtensionStores};
use crate::runtime_state::RuntimeTurnReducer;
use crate::step_context::RuntimeSamplingRequestState;

pub(crate) struct RuntimeTurnKernel<'a> {
    sampling_state: RuntimeSamplingRequestState,
    reducer: RuntimeTurnReducer<'a>,
}

impl<'a> RuntimeTurnKernel<'a> {
    pub(crate) fn new(thread_store: &'a ExtensionData, turn_store: &'a ExtensionData) -> Self {
        Self {
            sampling_state: RuntimeSamplingRequestState::new(),
            reducer: RuntimeTurnReducer::new(thread_store, turn_store),
        }
    }

    pub(crate) fn from_extension_stores(extension_stores: RuntimeExtensionStores<'a>) -> Self {
        Self::new(
            extension_stores.thread_store(),
            extension_stores.turn_store(),
        )
    }

    pub(crate) fn sampling_state_mut(&mut self) -> &mut RuntimeSamplingRequestState {
        &mut self.sampling_state
    }

    #[allow(dead_code)]
    pub(crate) fn reducer(&self) -> RuntimeTurnReducer<'a> {
        self.reducer
    }
}
