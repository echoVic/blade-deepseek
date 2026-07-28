#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StreamingMarkdownAction {
    UpdateTail(String),
    FreezeTail { text: String, trailing_blank: bool },
    AppendFrozen { text: String, trailing_blank: bool },
    ClearTail,
    FinishTail(String),
}

#[derive(Default)]
pub(crate) struct StreamingMarkdownAssembler {
    partial_line: String,
    current_block: String,
    finished: bool,
}

impl StreamingMarkdownAssembler {
    pub(crate) fn push(&mut self, text: &str) -> Vec<StreamingMarkdownAction> {
        if self.finished || text.is_empty() {
            return Vec::new();
        }
        self.partial_line.push_str(text);
        let Some(last_newline) = self.partial_line.rfind('\n') else {
            return Vec::new();
        };
        let complete = self.partial_line[..=last_newline].to_owned();
        self.partial_line.drain(..=last_newline);
        self.current_block.push_str(&complete);
        vec![StreamingMarkdownAction::UpdateTail(
            self.current_block.clone(),
        )]
    }

    pub(crate) fn finish(&mut self) -> Vec<StreamingMarkdownAction> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        if self.current_block.is_empty() && self.partial_line.is_empty() {
            Vec::new()
        } else {
            self.current_block.clear();
            vec![StreamingMarkdownAction::FinishTail(std::mem::take(
                &mut self.partial_line,
            ))]
        }
    }

    #[cfg(test)]
    fn held_text_for_test(&self) -> &str {
        &self.partial_line
    }
}

#[cfg(test)]
mod tests {
    use super::{StreamingMarkdownAction, StreamingMarkdownAssembler};

    fn reconstructed_action_text(actions: &[StreamingMarkdownAction]) -> String {
        let mut frozen = String::new();
        let mut tail = String::new();
        for action in actions {
            match action {
                StreamingMarkdownAction::UpdateTail(text) => tail.clone_from(text),
                StreamingMarkdownAction::FreezeTail { text, .. } => {
                    frozen.push_str(text);
                    tail.clear();
                }
                StreamingMarkdownAction::AppendFrozen { text, .. } => frozen.push_str(text),
                StreamingMarkdownAction::ClearTail => tail.clear(),
                StreamingMarkdownAction::FinishTail(text) => tail.push_str(text),
            }
        }
        frozen.push_str(&tail);
        frozen
    }

    #[test]
    fn partial_source_line_stays_hidden_until_newline_or_finish() {
        let mut assembler = StreamingMarkdownAssembler::default();
        assert!(assembler.push("hello").is_empty());
        assert_eq!(assembler.held_text_for_test(), "hello");
        assert_eq!(
            assembler.push(" world\n"),
            vec![StreamingMarkdownAction::UpdateTail(
                "hello world\n".to_string()
            )]
        );
        assert_eq!(assembler.held_text_for_test(), "");

        assert!(assembler.push("final").is_empty());
        assert_eq!(
            assembler.finish(),
            vec![StreamingMarkdownAction::FinishTail("final".to_string())]
        );
        assert!(assembler.finish().is_empty());
    }

    #[test]
    fn newline_gate_reconstructs_cjk_emoji_and_combining_text_exactly() {
        let mut assembler = StreamingMarkdownAssembler::default();
        let input = ["中", "文👍🏽e\u{301}", "\n尾", "行"];
        let mut actions = Vec::new();
        for piece in input {
            actions.extend(assembler.push(piece));
        }
        actions.extend(assembler.finish());
        assert_eq!(reconstructed_action_text(&actions), "中文👍🏽e\u{301}\n尾行");
    }
}
