use orca_runtime::mentions;

use crate::types::PendingWorkflowNotification;

enum SubmittedTurnKind {
    User {
        prompt: String,
        bindings: mentions::MentionBindings,
    },
    WorkflowNotification(PendingWorkflowNotification),
}

struct SubmittedTurnPresentation {
    task_label: Option<String>,
    backtrack_target: bool,
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
    presentation: SubmittedTurnPresentation,
}

impl SubmittedTurn {
    pub(crate) fn user(prompt: String) -> Self {
        Self {
            kind: SubmittedTurnKind::User {
                bindings: mentions::MentionBindings::new(&prompt),
                prompt,
            },
            presentation: SubmittedTurnPresentation::user(),
        }
    }

    pub(crate) fn user_with_mentions(prompt: String, bindings: mentions::MentionBindings) -> Self {
        Self {
            kind: SubmittedTurnKind::User { prompt, bindings },
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
            SubmittedTurnKind::User { prompt, .. } => prompt,
            SubmittedTurnKind::WorkflowNotification(notification) => &notification.prompt,
        }
    }

    pub(crate) fn task_label(&self) -> Option<&str> {
        self.presentation.task_label.as_deref()
    }

    pub(crate) fn is_backtrack_target(&self) -> bool {
        self.presentation.backtrack_target
    }

    pub(crate) fn prompt_for_model(
        &self,
        cwd: &std::path::Path,
        workspace_roots: &[std::path::PathBuf],
        mcp_registry: &orca_mcp::McpRegistry,
    ) -> Result<String, String> {
        match &self.kind {
            SubmittedTurnKind::User { prompt, bindings } => {
                mentions::expand_mentions(prompt, bindings, cwd, workspace_roots, mcp_registry)
            }
            SubmittedTurnKind::WorkflowNotification(notification) => {
                Ok(notification.prompt.clone())
            }
        }
    }

    pub(crate) fn title_seed(&self, model_prompt: &str) -> String {
        match &self.kind {
            SubmittedTurnKind::User { .. } => model_prompt.to_string(),
            SubmittedTurnKind::WorkflowNotification(_) => self
                .presentation
                .task_label
                .clone()
                .unwrap_or_else(|| model_prompt.to_string()),
        }
    }

    pub(crate) fn with_model_prompt(mut self, prompt: String) -> Self {
        self.kind = match self.kind {
            SubmittedTurnKind::User { .. } => SubmittedTurnKind::User {
                bindings: mentions::MentionBindings::new(&prompt),
                prompt,
            },
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
