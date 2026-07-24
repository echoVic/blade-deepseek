use std::io;

use orca_core::tool_types::ToolResult;

use crate::model_response::RuntimeModelResponse;

pub trait RuntimeProviderResponseIngress: Send + Sync + std::fmt::Debug {
    fn commit_response(&self, response: &RuntimeModelResponse) -> io::Result<()>;
    fn commit_tool_results(&self, results: &[ToolResult]) -> io::Result<()>;

    fn commit_tool_result(&self, result: &ToolResult) -> io::Result<()> {
        self.commit_tool_results(std::slice::from_ref(result))
    }
}
