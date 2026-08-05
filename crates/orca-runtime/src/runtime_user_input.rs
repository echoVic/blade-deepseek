use std::collections::{BTreeMap, HashSet};
use std::io;

use orca_core::tool_types::{ToolRequest, ToolResult};
use serde::Deserialize;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeUserInputRequest {
    pub id: String,
    pub question: String,
    pub choices: Vec<String>,
}

pub trait RuntimeUserInputHandler {
    fn request_user_input(&self, request: &RuntimeUserInputRequest) -> io::Result<Option<String>>;
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskUserQuestionArgs {
    questions: Vec<AskUserQuestion>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AskUserQuestion {
    header: String,
    question: String,
    options: Vec<AskUserQuestionOption>,
    #[serde(default)]
    multi_select: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskUserQuestionOption {
    label: String,
    description: String,
    #[serde(default)]
    preview: Option<String>,
}

pub(crate) fn execute_user_input_tool(
    request: &ToolRequest,
    handler: &dyn RuntimeUserInputHandler,
) -> io::Result<ToolResult> {
    execute_ask_user_question_tool(request, handler)
}

pub(crate) fn execute_ask_user_question_tool(
    request: &ToolRequest,
    handler: &dyn RuntimeUserInputHandler,
) -> io::Result<ToolResult> {
    let questions = parse_ask_user_question_request(request)?;
    let mut answers = BTreeMap::new();

    for (index, question) in questions.into_iter().enumerate() {
        let question_text = question.question.trim().to_string();
        let mut presentation = format!("{}: {question_text}", question.header.trim());
        if question.multi_select {
            presentation.push_str(
                "\nSelect one or more choices separated by commas, or type a custom answer.",
            );
        }
        let choices = question
            .options
            .into_iter()
            .map(|option| {
                let mut choice = format!("{} - {}", option.label.trim(), option.description.trim());
                if let Some(preview) = option.preview.filter(|preview| !preview.trim().is_empty()) {
                    choice.push_str("\nPreview:\n");
                    choice.push_str(preview.trim());
                }
                choice
            })
            .collect();
        let input = RuntimeUserInputRequest {
            id: format!("{}:question:{}", request.id, index + 1),
            question: presentation,
            choices,
        };
        let Some(answer) = handler.request_user_input(&input)? else {
            return Ok(ToolResult::cancelled(
                request,
                "user question request cancelled",
                None,
            ));
        };
        answers.insert(question_text, answer);
    }

    let output =
        serde_json::to_string(&serde_json::json!({ "answers": answers })).map_err(|error| {
            io::Error::other(format!(
                "failed to serialize ask_user_question answers: {error}"
            ))
        })?;
    Ok(ToolResult::completed(request, output, false))
}

fn parse_ask_user_question_request(request: &ToolRequest) -> io::Result<Vec<AskUserQuestion>> {
    let raw = request.raw_arguments.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing ask_user_question arguments JSON",
        )
    })?;
    let args: AskUserQuestionArgs = serde_json::from_str(raw).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid ask_user_question arguments JSON: {error}"),
        )
    })?;
    if !(1..=4).contains(&args.questions.len()) {
        return Err(invalid_questionnaire(
            "ask_user_question requires between 1 and 4 questions",
        ));
    }

    let mut question_texts = HashSet::new();
    for (index, question) in args.questions.iter().enumerate() {
        let position = index + 1;
        let header = question.header.trim();
        if header.is_empty() || header.chars().count() > 12 {
            return Err(invalid_questionnaire(format!(
                "ask_user_question question {position} header must contain 1 to 12 characters"
            )));
        }
        let question_text = question.question.trim();
        if question_text.is_empty() {
            return Err(invalid_questionnaire(format!(
                "ask_user_question question {position} text must not be empty"
            )));
        }
        if !question_texts.insert(question_text.to_string()) {
            return Err(invalid_questionnaire(format!(
                "ask_user_question question {position} duplicates an earlier question"
            )));
        }
        if !(2..=4).contains(&question.options.len()) {
            return Err(invalid_questionnaire(format!(
                "ask_user_question question {position} requires between 2 and 4 options"
            )));
        }
        let mut labels = HashSet::new();
        for (option_index, option) in question.options.iter().enumerate() {
            let option_position = option_index + 1;
            let label = option.label.trim();
            if label.is_empty() {
                return Err(invalid_questionnaire(format!(
                    "ask_user_question question {position} option {option_position} label must not be empty"
                )));
            }
            if !labels.insert(label.to_string()) {
                return Err(invalid_questionnaire(format!(
                    "ask_user_question question {position} option labels must be distinct"
                )));
            }
            if option.description.trim().is_empty() {
                return Err(invalid_questionnaire(format!(
                    "ask_user_question question {position} option {option_position} description must not be empty"
                )));
            }
        }
    }

    Ok(args.questions)
}

fn invalid_questionnaire(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolStatus};

    use super::*;

    struct RecordingHandler {
        answers: Mutex<VecDeque<Option<String>>>,
        requests: Mutex<Vec<RuntimeUserInputRequest>>,
    }

    impl RecordingHandler {
        fn new(answers: impl IntoIterator<Item = Option<&'static str>>) -> Self {
            Self {
                answers: Mutex::new(
                    answers
                        .into_iter()
                        .map(|answer| answer.map(str::to_string))
                        .collect(),
                ),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl RuntimeUserInputHandler for RecordingHandler {
        fn request_user_input(
            &self,
            request: &RuntimeUserInputRequest,
        ) -> io::Result<Option<String>> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(self.answers.lock().unwrap().pop_front().flatten())
        }
    }

    fn questionnaire_request(arguments: &str) -> ToolRequest {
        ToolRequest {
            id: "ask-1".to_string(),
            name: ToolName::plain("ask_user_question"),
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(arguments.to_string()),
        }
    }

    #[test]
    fn ask_user_question_collects_ordered_answers_through_typed_handler() {
        let request = questionnaire_request(
            r#"{
                "questions": [
                    {
                        "header": "Runtime",
                        "question": "Which path?",
                        "options": [
                            {"label": "Reuse", "description": "Use the runtime broker"},
                            {"label": "New", "description": "Create another interaction path"}
                        ],
                        "multiSelect": false
                    },
                    {
                        "header": "Signals",
                        "question": "Which signals?",
                        "options": [
                            {"label": "Logs", "description": "Capture structured logs"},
                            {"label": "Metrics", "description": "Capture numeric metrics", "preview": "p95"}
                        ],
                        "multiSelect": true
                    }
                ]
            }"#,
        );
        let handler = RecordingHandler::new([Some("Reuse"), Some("Logs, Metrics")]);

        let result = execute_ask_user_question_tool(&request, &handler).expect("questionnaire");

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(result.output.as_deref().unwrap()).unwrap(),
            serde_json::json!({
                "answers": {
                    "Which path?": "Reuse",
                    "Which signals?": "Logs, Metrics"
                }
            })
        );
        let requests = handler.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].id, "ask-1:question:1");
        assert_eq!(requests[0].question, "Runtime: Which path?");
        assert_eq!(
            requests[0].choices,
            [
                "Reuse - Use the runtime broker",
                "New - Create another interaction path"
            ]
        );
        assert_eq!(requests[1].id, "ask-1:question:2");
        assert!(requests[1].question.contains("Select one or more"));
        assert_eq!(
            requests[1].choices,
            [
                "Logs - Capture structured logs",
                "Metrics - Capture numeric metrics\nPreview:\np95"
            ]
        );
    }

    #[test]
    fn ask_user_question_cancels_whole_tool_when_any_question_is_dismissed() {
        let request = questionnaire_request(
            r#"{"questions":[{"header":"Runtime","question":"Which path?","options":[{"label":"Reuse","description":"Use it"},{"label":"New","description":"Replace it"}]}]}"#,
        );
        let handler = RecordingHandler::new([None]);

        let result = execute_ask_user_question_tool(&request, &handler).expect("cancel result");

        assert_eq!(result.status, ToolStatus::Cancelled);
        assert_eq!(
            result.error.as_deref(),
            Some("user question request cancelled")
        );
    }

    #[test]
    fn ask_user_question_rejects_invalid_questionnaire_bounds_and_content() {
        let invalid_arguments = [
            r#"{"questions":[]}"#,
            r#"{"questions":[{"header":"One","question":"1?","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}]},{"header":"Two","question":"2?","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}]},{"header":"Three","question":"3?","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}]},{"header":"Four","question":"4?","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}]},{"header":"Five","question":"5?","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}]}]}"#,
            r#"{"questions":[{"header":"","question":"Which?","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}]}]}"#,
            r#"{"questions":[{"header":"This header is too long","question":"Which?","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}]}]}"#,
            r#"{"questions":[{"header":"Choice","question":"Which?","options":[{"label":"Only","description":"One"}]}]}"#,
            r#"{"questions":[{"header":"Choice","question":"Which?","options":[{"label":"Same","description":"One"},{"label":"Same","description":"Two"}]}]}"#,
            r#"{"questions":[{"header":"Choice","question":"Which?","options":[{"label":"A","description":"One"},{"label":"B","description":"Two"}],"multi_select":true}]}"#,
        ];

        for arguments in invalid_arguments {
            let error = execute_ask_user_question_tool(
                &questionnaire_request(arguments),
                &RecordingHandler::new([]),
            )
            .expect_err(arguments);
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{arguments}");
        }
    }
}
