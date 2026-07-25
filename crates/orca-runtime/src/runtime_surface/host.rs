use crate::goal_actor::GoalRuntimeHandle;
use crate::goal_store::CreateGoalInput;
use crate::runtime_host::{
    RuntimeHost, RuntimeHostError, RuntimeHostHandle, RuntimeThreadHandle,
    RuntimeThreadStartRequest,
};
use orca_core::config::RunConfig;
use orca_core::goal_runtime::{GoalNextAction, GoalPauseReason, GoalRecord, GoalTurnOrigin};
use orca_core::goal_types::ThreadGoal;

use super::{RuntimeSurfaceHandle, RuntimeSurfaceHostHandle};

/// A thread-scoped typed surface entry point.
#[derive(Clone)]
pub struct RuntimeSurfaceThreadHandle {
    runtime: RuntimeThreadHandle,
}

/// A thread-scoped Goal control facade. The actor handle remains private to
/// `orca-runtime`; clients receive domain values and runtime-owned errors only.
#[derive(Clone)]
pub struct RuntimeSurfaceGoalHandle {
    runtime: GoalRuntimeHandle,
}

impl RuntimeSurfaceGoalHandle {
    fn map_error(error: crate::goal_actor::GoalActorError) -> RuntimeHostError {
        RuntimeHostError::GoalControlFailed {
            message: error.to_string(),
        }
    }

    pub fn read(&self, session_id: &str) -> Result<Option<GoalRecord>, RuntimeHostError> {
        self.runtime.read(session_id).map_err(Self::map_error)
    }

    pub fn project_thread_goal(
        &self,
        session_id: &str,
    ) -> Result<Option<ThreadGoal>, RuntimeHostError> {
        self.runtime
            .project_thread_goal(session_id)
            .map_err(Self::map_error)
    }

    pub fn create(&self, input: CreateGoalInput) -> Result<GoalRecord, RuntimeHostError> {
        self.runtime.create(input).map_err(Self::map_error)
    }

    pub fn edit(
        &self,
        session_id: &str,
        objective: impl Into<String>,
        token_budget: Option<i64>,
        at: i64,
    ) -> Result<Option<GoalRecord>, RuntimeHostError> {
        self.runtime
            .edit(session_id, objective, token_budget, at)
            .map_err(Self::map_error)
    }

    pub fn clear(&self, session_id: &str) -> Result<(), RuntimeHostError> {
        self.runtime.clear(session_id).map_err(Self::map_error)
    }

    pub fn latest_active(&self) -> Result<Option<ThreadGoal>, RuntimeHostError> {
        self.runtime.latest_active().map_err(Self::map_error)
    }

    pub fn pause(
        &self,
        session_id: &str,
        reason: GoalPauseReason,
        message: impl Into<String>,
        at: i64,
    ) -> Result<GoalNextAction, RuntimeHostError> {
        self.runtime
            .pause(session_id, reason, message, at)
            .map_err(Self::map_error)
    }

    pub fn resume(
        &self,
        session_id: &str,
        origin: GoalTurnOrigin,
        at: i64,
    ) -> Result<GoalNextAction, RuntimeHostError> {
        self.runtime
            .resume(session_id, origin, at)
            .map_err(Self::map_error)
    }
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

    pub fn read_history(&self) -> Result<Vec<super::SurfaceHistoryMessage>, RuntimeHostError> {
        self.runtime.read_surface_history()
    }

    pub fn backtrack_last_user(&self) -> Result<Option<String>, RuntimeHostError> {
        self.runtime.backtrack_last_user()
    }

    pub fn goal(&self) -> Result<RuntimeSurfaceGoalHandle, RuntimeHostError> {
        self.runtime
            .goal_runtime()
            .map(|runtime| RuntimeSurfaceGoalHandle { runtime })
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
