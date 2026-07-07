use std::io;

use orca_core::conversation::Conversation;
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;

use crate::cost::CostTracker;
use crate::extension::{
    ExtensionData, ExtensionRegistry, RuntimeExtensionContext, RuntimeExtensionStores,
};
use crate::provider_turn::RuntimeProviderResponseInput;
use crate::runtime_state::RuntimeTurnReducer;
use crate::step_context::{RuntimeSamplingRequestState, RuntimeStepContext};
use crate::thread_store::SessionWriter;
use crate::workflow_execution::BackgroundWorkflowRun;

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

    #[cfg(test)]
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

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn provider_response_input<'response, W: io::Write>(
        &'response mut self,
        mut step_context: RuntimeStepContext<'response>,
        extension_registry: &'response ExtensionRegistry,
        events: &'response mut EventFactory,
        sink: &'response mut EventSink<W>,
        conversation: &'response mut Conversation,
        history_writer: Option<&'response mut SessionWriter>,
        cost_tracker: &'response mut CostTracker,
        background_workflows: &'response mut Vec<BackgroundWorkflowRun>,
    ) -> RuntimeProviderResponseInput<'response, W>
    where
        'a: 'response,
    {
        step_context.extensions = Some(RuntimeExtensionContext::new(
            extension_registry,
            self.extension_stores,
        ));
        RuntimeProviderResponseInput {
            step_context,
            sampling_state: &mut self.sampling_state,
            events,
            sink,
            conversation,
            history_writer,
            cost_tracker,
            background_workflows,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn reducer(&self) -> RuntimeTurnReducer<'a> {
        self.reducer
    }
}
