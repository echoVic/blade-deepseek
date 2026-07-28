use orca_runtime::mentions::MentionBindings;
use std::collections::VecDeque;

use crate::composer_textarea::expand_pending_pastes;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueuedUserMessage {
    visible_text: String,
    submission_text: String,
    composer_bindings: MentionBindings,
    submission_bindings: MentionBindings,
    pending_pastes: Vec<(String, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueuedComposerState {
    pub(crate) visible_text: String,
    pub(crate) mention_bindings: MentionBindings,
    pub(crate) pending_pastes: Vec<(String, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueuedPreviewSnapshot {
    pub(crate) len: usize,
    pub(crate) first: String,
    pub(crate) second: Option<String>,
    pub(crate) latest: Option<String>,
}

impl QueuedPreviewSnapshot {
    pub(crate) fn from_queue(queue: &VecDeque<QueuedUserMessage>) -> Option<Self> {
        Self::from_queue_with(queue, || {})
    }

    fn from_queue_with(
        queue: &VecDeque<QueuedUserMessage>,
        mut on_read: impl FnMut(),
    ) -> Option<Self> {
        let len = queue.len();
        let first = queue.front().map(|message| {
            on_read();
            message.preview_text()
        })?;
        let (second, latest) = if len == 2 {
            let second = queue.get(1).map(|message| {
                on_read();
                message.preview_text()
            });
            (second, None)
        } else if len > 2 {
            let latest = queue.back().map(|message| {
                on_read();
                message.preview_text()
            });
            (None, latest)
        } else {
            (None, None)
        };
        Some(Self {
            len,
            first,
            second,
            latest,
        })
    }

    #[cfg(test)]
    fn from_queue_with_probe(
        queue: &VecDeque<QueuedUserMessage>,
        on_read: impl FnMut(),
    ) -> Option<Self> {
        Self::from_queue_with(queue, on_read)
    }
}

impl QueuedUserMessage {
    pub(crate) fn from_composer(
        visible_text: String,
        pending_pastes: Vec<(String, String)>,
        mut mention_bindings: MentionBindings,
    ) -> Option<Self> {
        mention_bindings.reconcile(&visible_text);

        let trimmed_visible = visible_text.trim().to_string();
        if trimmed_visible.is_empty() {
            return None;
        }

        let mut composer_bindings = mention_bindings.clone();
        composer_bindings.reconcile(&trimmed_visible);

        let expanded = expand_pending_pastes(&visible_text, &pending_pastes);
        let submission_text = expanded.trim().to_string();
        let mut submission_bindings = mention_bindings;
        submission_bindings.reconcile(&expanded);
        submission_bindings.reconcile(&submission_text);

        let pending_pastes = pending_pastes
            .into_iter()
            .filter(|(placeholder, _)| trimmed_visible.contains(placeholder))
            .collect();

        Some(Self {
            visible_text: trimmed_visible,
            submission_text,
            composer_bindings,
            submission_bindings,
            pending_pastes,
        })
    }

    pub(crate) fn visible_text(&self) -> &str {
        &self.visible_text
    }

    pub(crate) fn submission_text(&self) -> &str {
        &self.submission_text
    }

    #[cfg(test)]
    pub(crate) fn composer_bindings(&self) -> &MentionBindings {
        &self.composer_bindings
    }

    pub(crate) fn submission_bindings(&self) -> &MentionBindings {
        &self.submission_bindings
    }

    pub(crate) fn preview_text(&self) -> String {
        self.visible_text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub(crate) fn into_composer_state(self) -> QueuedComposerState {
        QueuedComposerState {
            visible_text: self.visible_text,
            mention_bindings: self.composer_bindings,
            pending_pastes: self.pending_pastes,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::path::PathBuf;

    use orca_runtime::mentions::{MentionBinding, MentionBindings, MentionFileKind, MentionTarget};

    use super::*;

    fn binding(text: &str, visible: &str) -> MentionBindings {
        let start = text.find(visible).expect("visible mention");
        MentionBindings::from_bindings(
            text,
            vec![MentionBinding {
                start,
                end: start + visible.len(),
                visible: visible.to_string(),
                target: MentionTarget::File {
                    root: PathBuf::from("/workspace"),
                    path: visible.trim_start_matches('@').to_string(),
                    kind: MentionFileKind::File,
                },
            }],
        )
    }

    #[test]
    fn queued_message_rejects_blank_input_and_preserves_atomic_composer_state() {
        assert!(
            QueuedUserMessage::from_composer(
                " \n ".to_string(),
                Vec::new(),
                MentionBindings::default(),
            )
            .is_none()
        );

        let visible = "review @item.rs [Pasted Content 1001 chars]";
        let pasted = "body\n".repeat(201);
        let message = QueuedUserMessage::from_composer(
            visible.to_string(),
            vec![("[Pasted Content 1001 chars]".to_string(), pasted.clone())],
            binding(visible, "@item.rs"),
        )
        .expect("queued message");

        assert_eq!(message.visible_text(), visible);
        assert_eq!(
            message.submission_text(),
            format!("review @item.rs {}", pasted.trim())
        );
        assert_eq!(message.composer_bindings().bindings().len(), 1);
        assert_eq!(message.submission_bindings().bindings().len(), 1);

        let restored = message.into_composer_state();
        assert_eq!(restored.visible_text, visible);
        assert_eq!(restored.pending_pastes.len(), 1);
        assert_eq!(restored.mention_bindings.bindings().len(), 1);
    }

    #[test]
    fn queued_preview_collapses_whitespace_and_never_expands_large_paste() {
        let visible = "alpha\n  beta [Pasted Content 1001 chars]";
        let message = QueuedUserMessage::from_composer(
            visible.to_string(),
            vec![(
                "[Pasted Content 1001 chars]".to_string(),
                "secret payload\n".repeat(100),
            )],
            MentionBindings::default(),
        )
        .unwrap();

        assert_eq!(
            message.preview_text(),
            "alpha beta [Pasted Content 1001 chars]"
        );
        assert!(!message.preview_text().contains("secret payload"));
    }

    #[test]
    fn queued_preview_snapshot_reads_at_most_head_and_tail() {
        let queue = (0..64)
            .map(|index| {
                QueuedUserMessage::from_composer(
                    format!("item {index}"),
                    Vec::new(),
                    MentionBindings::default(),
                )
                .unwrap()
            })
            .collect::<VecDeque<_>>();
        let reads = Cell::new(0);
        let snapshot = QueuedPreviewSnapshot::from_queue_with_probe(&queue, || {
            reads.set(reads.get() + 1);
        })
        .unwrap();
        assert_eq!(snapshot.len, 64);
        assert_eq!(snapshot.first, "item 0");
        assert_eq!(snapshot.second, None);
        assert_eq!(snapshot.latest.as_deref(), Some("item 63"));
        assert!(reads.get() <= 2);
    }

    #[test]
    fn queued_preview_snapshot_reads_both_items_for_length_two() {
        let queue = ["first", "second"]
            .into_iter()
            .map(|text| {
                QueuedUserMessage::from_composer(
                    text.to_string(),
                    Vec::new(),
                    MentionBindings::default(),
                )
                .unwrap()
            })
            .collect::<VecDeque<_>>();
        let snapshot = QueuedPreviewSnapshot::from_queue(&queue).unwrap();
        assert_eq!(snapshot.first, "first");
        assert_eq!(snapshot.second.as_deref(), Some("second"));
        assert_eq!(snapshot.latest, None);
    }
}
