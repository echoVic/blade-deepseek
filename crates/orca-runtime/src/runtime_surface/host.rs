use crate::runtime_host::{
    RuntimeHost, RuntimeHostError, RuntimeHostHandle, RuntimeThreadHandle,
    RuntimeThreadStartRequest,
};
use orca_core::config::RunConfig;

use super::{RuntimeSurfaceHandle, RuntimeSurfaceHostHandle};

/// A thread-scoped typed surface entry point.
#[derive(Clone)]
pub struct RuntimeSurfaceThreadHandle {
    runtime: RuntimeThreadHandle,
}

impl RuntimeSurfaceHostHandle {
    pub fn start_thread(
        &self,
        config: RunConfig,
        title: impl Into<String>,
    ) -> Result<RuntimeSurfaceThreadHandle, RuntimeHostError> {
        self.start_thread_with_request(RuntimeThreadStartRequest::new(config, title))
    }

    pub fn start_thread_with_request(
        &self,
        request: RuntimeThreadStartRequest,
    ) -> Result<RuntimeSurfaceThreadHandle, RuntimeHostError> {
        self.runtime
            .as_ref()
            .ok_or(RuntimeHostError::HostUnavailable)?
            .start_thread_with_request(request)
            .map(RuntimeSurfaceThreadHandle::from_runtime)
    }
}

impl RuntimeSurfaceThreadHandle {
    fn from_runtime(runtime: RuntimeThreadHandle) -> Self {
        Self { runtime }
    }

    pub fn thread_id(&self) -> &str {
        self.runtime.thread_id()
    }

    pub fn surface(&self) -> RuntimeSurfaceHandle {
        self.runtime.surface()
    }

    pub fn acp_surface(&self) -> Option<RuntimeSurfaceHandle> {
        self.runtime.acp_surface()
    }

    pub(crate) fn legacy(&self) -> RuntimeThreadHandle {
        self.runtime.clone()
    }
}

impl RuntimeThreadHandle {
    pub fn typed_surface(&self) -> RuntimeSurfaceThreadHandle {
        RuntimeSurfaceThreadHandle::from_runtime(self.clone())
    }
}

impl RuntimeHost {
    pub fn surface_handle(&self) -> RuntimeSurfaceHostHandle {
        RuntimeSurfaceHostHandle::from_runtime(self.handle())
    }
}

impl RuntimeHostHandle {
    pub fn surface_handle(&self) -> RuntimeSurfaceHostHandle {
        RuntimeSurfaceHostHandle::from_runtime(self.clone())
    }
}
