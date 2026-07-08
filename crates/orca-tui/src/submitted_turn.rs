use orca_runtime::mentions;

use crate::types::PendingWorkflowNotification;

enum SubmittedTurnKind {
    User(String),
    WorkflowNotification(PendingWorkflowNotification),
}

pub(crate) struct SubmittedTurnPresentation {
    pub(crate) task_label: Option<String>,
    pub(crate) backtrack_target: bool,
}

impl SubmittedTurnPresentation {
    fn user() -> Self {
        Self {
            task_label: None,
            backtrack_target: true,
        }
    }

    fn workflow_notification(id: &str) -> Self {
        Self {
            task_label: Some(workflow_notification_task_label(id)),
            backtrack_target: false,
        }
    }
}

pub(crate) struct SubmittedTurn {
    kind: SubmittedTurnKind,
    pub(crate) presentation: SubmittedTurnPresentation,
}

impl SubmittedTurn {
    pub(crate) fn user(prompt: String) -> Self {
        Self {
            kind: SubmittedTurnKind::User(prompt),
            presentation: SubmittedTurnPresentation::user(),
        }
    }

    pub(crate) fn workflow_notification(notification: PendingWorkflowNotification) -> Self {
        let id = notification.id.clone();
        Self {
            kind: SubmittedTurnKind::WorkflowNotification(notification),
            presentation: SubmittedTurnPresentation::workflow_notification(&id),
        }
    }

    pub(crate) fn prompt(&self) -> &str {
        match &self.kind {
            SubmittedTurnKind::User(prompt) => prompt,
            SubmittedTurnKind::WorkflowNotification(notification) => &notification.prompt,
        }
    }

    pub(crate) fn prompt_for_model(&self, cwd: &std::path::Path) -> Result<String, String> {
        match &self.kind {
            SubmittedTurnKind::User(prompt) => mentions::expand_file_mentions(prompt, cwd),
            SubmittedTurnKind::WorkflowNotification(notification) => {
                Ok(notification.prompt.clone())
            }
        }
    }

    pub(crate) fn title_seed(&self, model_prompt: &str) -> String {
        match &self.kind {
            SubmittedTurnKind::User(_) => model_prompt.to_string(),
            SubmittedTurnKind::WorkflowNotification(_) => self
                .presentation
                .task_label
                .clone()
                .unwrap_or_else(|| model_prompt.to_string()),
        }
    }

    pub(crate) fn with_model_prompt(mut self, prompt: String) -> Self {
        self.kind = match self.kind {
            SubmittedTurnKind::User(_) => SubmittedTurnKind::User(prompt),
            SubmittedTurnKind::WorkflowNotification(mut notification) => {
                notification.prompt = prompt;
                SubmittedTurnKind::WorkflowNotification(notification)
            }
        };
        self
    }
}

fn workflow_notification_task_label(id: &str) -> String {
    format!("Workflow notification {id}")
}
