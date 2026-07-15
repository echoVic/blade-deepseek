pub(crate) type PendingWorkflowNotifications = crate::types::PendingWorkflowNotificationQueue;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TuiBackgroundTurnContinuationRequest {
    task_id: String,
}

impl TuiBackgroundTurnContinuationRequest {
    pub(crate) fn new(task_id: String) -> Self {
        Self { task_id }
    }

    pub(crate) fn task_id(&self) -> &str {
        &self.task_id
    }
}
