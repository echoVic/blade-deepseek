use std::io;

use crate::extension::{ExtensionData, ToolCallOutcome};
use crate::goals;
use crate::runtime_directive::{RuntimeDirective, RuntimeDirectiveState};
use crate::runtime_permission::{
    RuntimePermissionRequest, RuntimePermissionRequestHandler, RuntimePermissionResponse,
    TurnPermissionOverlay,
};

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
    directive_state: DirectiveRuntimeState,
    permission_state: PermissionRuntimeState,
    tool_state: ToolRuntimeState<'a>,
}

impl<'a> RuntimeTurnReducer<'a> {
    pub fn new(thread_store: &'a ExtensionData, turn_store: &'a ExtensionData) -> Self {
        Self {
            directive_state: DirectiveRuntimeState,
            permission_state: PermissionRuntimeState,
            tool_state: ToolRuntimeState::new(thread_store, turn_store),
        }
    }

    pub fn apply_directive(
        &self,
        directive_state: &mut RuntimeDirectiveState,
        directive: RuntimeDirective,
    ) {
        self.directive_state.apply(directive_state, directive);
    }

    pub fn request_permission(
        &self,
        overlay: &mut TurnPermissionOverlay,
        handler: &dyn RuntimePermissionRequestHandler,
        request: RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        self.permission_state
            .request_permission(overlay, handler, request)
    }

    pub fn merge_permission_overlay(
        &self,
        overlay: &mut TurnPermissionOverlay,
        other: &TurnPermissionOverlay,
    ) {
        self.permission_state
            .merge_permission_overlay(overlay, other);
    }

    pub fn record_tool_finish(&self, finish: RuntimeToolFinish<'_>) {
        self.tool_state.record_finish(finish);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DirectiveRuntimeState;

impl DirectiveRuntimeState {
    pub fn apply(&self, directive_state: &mut RuntimeDirectiveState, directive: RuntimeDirective) {
        directive_state.apply(directive);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PermissionRuntimeState;

impl PermissionRuntimeState {
    pub fn request_permission(
        &self,
        overlay: &mut TurnPermissionOverlay,
        handler: &dyn RuntimePermissionRequestHandler,
        request: RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        overlay.request_and_merge(handler, request)
    }

    pub fn merge_permission_overlay(
        &self,
        overlay: &mut TurnPermissionOverlay,
        other: &TurnPermissionOverlay,
    ) {
        overlay.merge(other);
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
