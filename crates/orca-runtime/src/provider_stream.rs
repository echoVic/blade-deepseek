use std::sync::mpsc;
use std::time::Duration;

use orca_core::provider_types::{ProviderResponse, ProviderStep};
use orca_provider::{ProviderStreamEvent, ProviderStreamingCall};

pub trait RuntimeProviderSuspensionControl: std::fmt::Debug + Send + Sync {
    fn take_suspension_request(&self) -> bool;
}

pub enum RuntimeProviderSuspensionEvent {
    Step(ProviderStep),
    Completed(ProviderResponse),
}

pub struct RuntimeProviderSuspension {
    stream: ProviderStreamingCall,
    model: Option<String>,
}

impl RuntimeProviderSuspension {
    pub(crate) fn new(stream: ProviderStreamingCall, model: Option<String>) -> Self {
        Self { stream, model }
    }

    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<RuntimeProviderSuspensionEvent, mpsc::RecvTimeoutError> {
        match self.stream.recv_timeout(timeout)? {
            ProviderStreamEvent::Step(delivery) => Ok(RuntimeProviderSuspensionEvent::Step(
                delivery.step().clone(),
            )),
            ProviderStreamEvent::Completed(response) => {
                Ok(RuntimeProviderSuspensionEvent::Completed(response))
            }
        }
    }

    pub fn cancel(&self) {
        self.stream.cancel();
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}
