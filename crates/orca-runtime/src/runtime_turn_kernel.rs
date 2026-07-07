use crate::extension::{
    ExtensionData, ExtensionRegistry, RuntimeExtensionContext, RuntimeExtensionStores,
};
use crate::runtime_state::RuntimeTurnReducer;
use crate::step_context::{RuntimeSamplingRequestState, RuntimeStepContext};

pub(crate) struct RuntimeTurnKernel<'a> {
    extension_stores: RuntimeExtensionStores<'a>,
    sampling_state: RuntimeSamplingRequestState,
    reducer: RuntimeTurnReducer<'a>,
}

impl<'a> RuntimeTurnKernel<'a> {
    pub(crate) fn new(thread_store: &'a ExtensionData, turn_store: &'a ExtensionData) -> Self {
        let extension_stores = RuntimeExtensionStores::new(thread_store, turn_store);
        Self {
            extension_stores,
            sampling_state: RuntimeSamplingRequestState::new(),
            reducer: RuntimeTurnReducer::from_extension_stores(extension_stores),
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

    pub(crate) fn bind_step_context(
        &self,
        mut step_context: RuntimeStepContext<'a>,
        extension_registry: &'a ExtensionRegistry,
    ) -> RuntimeStepContext<'a> {
        step_context.extensions = Some(RuntimeExtensionContext::new(
            extension_registry,
            self.extension_stores,
        ));
        step_context
    }

    #[allow(dead_code)]
    pub(crate) fn reducer(&self) -> RuntimeTurnReducer<'a> {
        self.reducer
    }
}
