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
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeUserInputRequestArgs {
    question: String,
    #[serde(default)]
    choices: Vec<String>,
}

pub(crate) fn execute_user_input_tool(
    request: &ToolRequest,
    handler: &dyn RuntimeUserInputHandler,
) -> io::Result<ToolResult> {
    let args = parse_runtime_user_input_request(request)?;
    let input = RuntimeUserInputRequest {
        id: request.id.clone(),
        question: args.question,
        choices: args.choices,
    };
    Ok(match handler.request_user_input(&input)? {
        Some(answer) => ToolResult::completed(request, answer, false),
        None => ToolResult::failed(request, "user input request cancelled", None),
    })
}

pub(crate) fn parse_runtime_user_input_request(
    request: &ToolRequest,
) -> io::Result<RuntimeUserInputRequestArgs> {
    let raw = request.raw_arguments.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing request_user_input arguments JSON",
        )
    })?;
    let args: RuntimeUserInputRequestArgs = serde_json::from_str(raw).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid request_user_input arguments JSON: {error}"),
        )
    })?;
    if args.question.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing required request_user_input argument: question",
        ));
    }
    Ok(args)
}
