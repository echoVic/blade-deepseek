use crate::extension::{ExtensionData, ToolCallOutcome};
use crate::goals;

#[derive(Clone, Copy, Debug)]
pub struct RuntimeToolFinish<'a> {
    pub tool_name: &'a str,
    pub call_id: &'a str,
    pub outcome: ToolCallOutcome,
}

#[derive(Clone, Copy, Debug)]
pub struct ToolRuntimeState<'a> {
    thread_store: &'a ExtensionData,
    turn_store: &'a ExtensionData,
}

#[derive(Clone, Copy, Debug)]
pub struct RuntimeTurnReducer<'a> {
    tool_state: ToolRuntimeState<'a>,
}

impl<'a> RuntimeTurnReducer<'a> {
    pub fn new(thread_store: &'a ExtensionData, turn_store: &'a ExtensionData) -> Self {
        Self {
            tool_state: ToolRuntimeState::new(thread_store, turn_store),
        }
    }

    pub fn record_tool_finish(&self, finish: RuntimeToolFinish<'_>) {
        self.tool_state.record_finish(finish);
    }
}

impl<'a> ToolRuntimeState<'a> {
    pub fn new(thread_store: &'a ExtensionData, turn_store: &'a ExtensionData) -> Self {
        Self {
            thread_store,
            turn_store,
        }
    }

    pub fn record_finish(&self, finish: RuntimeToolFinish<'_>) {
        goals::record_goal_tool_finish(
            self.thread_store,
            self.turn_store,
            finish.tool_name,
            finish.call_id,
            finish.outcome,
        );
    }
}
