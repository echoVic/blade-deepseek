use std::path::PathBuf;

use orca_core::approval_types::{
    ActionKind, ApprovalDecision, ApprovalRequest, ApprovalResolution,
};
use orca_core::tool_types::{ToolName, ToolRequest};
use orca_runtime::lifecycle::{
    RuntimeApprovalHandler, RuntimePermissionRequest, RuntimePermissionRequestHandler,
    RuntimeUserInputHandler, RuntimeUserInputRequest,
};
use orca_runtime::protocol::{RequestFileSystemPermissions, RequestPermissionProfile};
use orca_runtime::runtime_pending_interaction::{
    RuntimeMcpElicitationMode, RuntimeMcpElicitationRequest, RuntimePendingInteractionRecord,
    RuntimePendingInteractionStore,
};

use super::{
    TuiApprovalHandler, TuiMcpElicitationHandler, TuiPermissionRequestHandler, TuiUserInputHandler,
};
use crate::operation_controller::{TuiOperationController, TuiOperationScope, TuiTurnControl};
use crate::types::{TuiEvent, TuiInteractionResponse};

fn operation() -> (TuiOperationController, TuiOperationScope, TuiTurnControl) {
    let controller = TuiOperationController::default();
    let operation = controller.start().expect("start operation");
    let control = operation.control();
    (controller, operation, control)
}

fn approval() -> (ApprovalRequest, ToolRequest) {
    (
        ApprovalRequest {
            id: "approval-1".to_string(),
            action: ActionKind::Shell,
            description: "bash requested shell".to_string(),
            tool: Some("bash".to_string()),
            target: Some("echo hi".to_string()),
            preview: Some("$ echo hi".to_string()),
        },
        ToolRequest {
            id: "call-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: Some(r#"{"command":"echo hi"}"#.to_string()),
        },
    )
}

#[test]
fn interaction_handlers_are_owned_send_and_sync_values() {
    fn assert_owned<T: Send + Sync + 'static>() {}

    assert_owned::<TuiApprovalHandler>();
    assert_owned::<TuiPermissionRequestHandler>();
    assert_owned::<TuiUserInputHandler>();
    assert_owned::<TuiMcpElicitationHandler>();
}

#[test]
fn approval_waiter_projects_until_the_fenced_response_arrives() {
    let (controller, _operation, control) = operation();
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let store = RuntimePendingInteractionStore::default();
    let handler =
        TuiApprovalHandler::new(event_tx, control).with_pending_interactions(store.clone());
    let (approval, request) = approval();
    let join = std::thread::spawn(move || handler.resolve_interactive(&approval, &request));

    let key = match event_rx.recv().expect("approval event") {
        TuiEvent::ApprovalNeeded { key, .. } => key,
        event => panic!("expected approval event, got {event:?}"),
    };
    assert!(store.get("approval-1").is_some());
    controller
        .broker()
        .respond(&key, TuiInteractionResponse::Approval(true))
        .expect("approve interaction");

    let resolution: ApprovalResolution = join.join().expect("join approval").expect("resolution");
    assert_eq!(resolution.decision, ApprovalDecision::Allow);
    assert!(store.is_empty());
}

#[test]
fn permission_interrupt_wakes_waiter_as_a_denial() {
    let (controller, _operation, control) = operation();
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let handler = TuiPermissionRequestHandler::new(event_tx, control);
    let request = RuntimePermissionRequest {
        id: "permission-1".to_string(),
        reason: Some("need repo write".to_string()),
        permissions: RequestPermissionProfile {
            file_system: Some(RequestFileSystemPermissions {
                write: Some(vec![PathBuf::from("src")]),
                ..Default::default()
            }),
            ..Default::default()
        },
    };
    let join = std::thread::spawn(move || handler.request_permissions(&request));

    assert!(matches!(
        event_rx.recv().expect("permission event"),
        TuiEvent::PermissionApprovalNeeded { .. } | TuiEvent::ApprovalNeeded { .. }
    ));
    controller.interrupt_current();

    let response = join.join().expect("join permission").expect("response");
    assert_eq!(
        response.decision,
        orca_runtime::protocol::PermissionResponseDecision::Deny
    );
}

#[test]
fn user_input_waiter_accepts_only_its_typed_response() {
    let (controller, _operation, control) = operation();
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let store = RuntimePendingInteractionStore::default();
    let handler =
        TuiUserInputHandler::new(event_tx, control).with_pending_interactions(store.clone());
    let request = RuntimeUserInputRequest {
        id: "ask-1".to_string(),
        question: "Continue?".to_string(),
        choices: vec!["yes".to_string(), "no".to_string()],
    };
    let join = std::thread::spawn(move || handler.request_user_input(&request));

    let key = match event_rx.recv().expect("user input event") {
        TuiEvent::UserInputRequested { key, .. } => key,
        event => panic!("expected user input event, got {event:?}"),
    };
    assert!(store.get("ask-1").is_some());
    controller
        .broker()
        .respond(&key, TuiInteractionResponse::UserInput("yes".to_string()))
        .expect("answer interaction");

    assert_eq!(
        join.join().expect("join user input").expect("response"),
        Some("yes".to_string())
    );
    assert!(store.is_empty());
}

#[test]
fn mcp_elicitation_waiter_routes_fenced_json_response() {
    let (controller, _operation, control) = operation();
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let handler = TuiMcpElicitationHandler::new(event_tx, control);
    let request = RuntimeMcpElicitationRequest::new(
        "github",
        "device-flow",
        RuntimeMcpElicitationMode::Form,
        "Authorize GitHub",
        None,
        Some(r#"{"type":"object"}"#.to_string()),
    );
    let join = std::thread::spawn(move || handler.request_mcp_elicitation(&request));

    let key = match event_rx.recv().expect("MCP event") {
        TuiEvent::McpElicitationRequested { key, .. } => key,
        event => panic!("expected MCP event, got {event:?}"),
    };
    controller
        .broker()
        .respond(
            &key,
            TuiInteractionResponse::McpElicitation {
                accepted: true,
                content_json: Some(r#"{"account":"echoVic"}"#.to_string()),
            },
        )
        .expect("MCP response");

    assert_eq!(
        join.join().expect("join MCP").expect("response"),
        Some(r#"{"account":"echoVic"}"#.to_string())
    );
}

#[test]
fn projection_duplicate_does_not_take_live_waiter_ownership() {
    let (controller, _operation, control) = operation();
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let store = RuntimePendingInteractionStore::default();
    let request = RuntimeUserInputRequest {
        id: "duplicate".to_string(),
        question: "First projection".to_string(),
        choices: Vec::new(),
    };
    store
        .insert(RuntimePendingInteractionRecord::from_user_input(&request))
        .expect("insert existing projection");
    let handler =
        TuiUserInputHandler::new(event_tx, control).with_pending_interactions(store.clone());
    let request = RuntimeUserInputRequest {
        question: "Live waiter".to_string(),
        ..request
    };
    let join = std::thread::spawn(move || handler.request_user_input(&request));

    let key = match event_rx.recv().expect("live event") {
        TuiEvent::UserInputRequested { key, .. } => key,
        event => panic!("expected user input event, got {event:?}"),
    };
    controller
        .broker()
        .respond(&key, TuiInteractionResponse::UserInput("live".to_string()))
        .expect("live response");
    assert_eq!(
        join.join().expect("join live waiter").expect("response"),
        Some("live".to_string())
    );
    assert_eq!(
        store.get("duplicate").and_then(|record| record.question),
        Some("First projection".to_string())
    );
}
