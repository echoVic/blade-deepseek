use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use agent_client_protocol::{
    Agent, AuthenticateRequest, CancelNotification, InitializeRequest, LoadSessionRequest,
    NewSessionRequest, PromptRequest, ReadTextFileRequest, ReadTextFileResponse,
    RequestPermissionResponse, SessionId, WriteTextFileRequest, WriteTextFileResponse,
};
use orca_core::config::RunConfig;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, oneshot};

use super::agent::{
    ACP_NOTIFICATION_CAPACITY, AcpClientBridge, AcpNotificationDelivery, AcpPermissionWaitError,
    OrcaAcpAgent,
};
use super::rpc_facade::{
    FrameDirection, InboundFrame, LocalHandlerCompletion, LocalHandlerFuture,
    ResponseSessionResolver, RpcFacadeConfig, RpcFacadeError, RpcFacadeHandle, TransportFrame,
    spawn_local_rpc_facade_with_response_session_resolver,
};
use crate::runtime_surface::{
    AcpReadTextFileSettlement, AcpWriteTextFileSettlement, CapabilityRevision,
    SurfaceCapabilityCallId,
};
use crate::surface::{RuntimeSurfaceClientHandle, RuntimeSurfaceHostHandle};

const ACP_REVERSE_REQUEST_DEADLINE: Duration = Duration::from_secs(120);

struct PendingPermissionRoute {
    session_id: SessionId,
    key: String,
    completed: oneshot::Sender<()>,
}

#[derive(Default)]
struct PermissionRoutes {
    next_request_id: Cell<i64>,
    pending: Arc<Mutex<HashMap<i64, PendingPermissionRoute>>>,
}

struct PendingReadTextFileRoute {
    session_id: SessionId,
    call_id: SurfaceCapabilityCallId,
    capability_revision: CapabilityRevision,
    client: RuntimeSurfaceClientHandle,
    physically_written: bool,
    completed: oneshot::Sender<AcpReadTextFileSettlement>,
}

#[derive(Default)]
struct ReadTextFileRoutes {
    pending: Arc<Mutex<HashMap<i64, PendingReadTextFileRoute>>>,
}

struct WriteTextFileRoutes {
    pending: Arc<Mutex<HashMap<i64, PendingWriteTextFileRoute>>>,
    response_observer: Option<Arc<Notify>>,
    written_observer: Option<Arc<Notify>>,
}

impl Default for WriteTextFileRoutes {
    fn default() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            response_observer: None,
            written_observer: None,
        }
    }
}

struct PendingWriteTextFileRoute {
    session_id: SessionId,
    call_id: SurfaceCapabilityCallId,
    capability_revision: CapabilityRevision,
    client: RuntimeSurfaceClientHandle,
    delivery_possible: bool,
    completed: oneshot::Sender<AcpWriteTextFileSettlement>,
}

struct CapabilityRequestIds {
    next_request_id: Cell<i64>,
}

impl Default for CapabilityRequestIds {
    fn default() -> Self {
        Self {
            next_request_id: Cell::new(-1),
        }
    }
}

impl CapabilityRequestIds {
    fn reserve(&self) -> Option<i64> {
        let request_id = self.next_request_id.get();
        self.next_request_id.set(request_id.checked_sub(1)?);
        Some(request_id)
    }
}

pub(crate) async fn run_connection<R, W>(
    surface_host: RuntimeSurfaceHostHandle,
    config: RunConfig,
    reader: R,
    writer: W,
) -> Result<(), RpcFacadeError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    run_connection_inner(surface_host, config, reader, writer, None, None).await
}

async fn run_connection_inner<R, W>(
    surface_host: RuntimeSurfaceHostHandle,
    config: RunConfig,
    reader: R,
    writer: W,
    write_response_observer: Option<Arc<Notify>>,
    write_written_observer: Option<Arc<Notify>>,
) -> Result<(), RpcFacadeError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (notification_tx, notification_rx) = tokio::sync::mpsc::channel(ACP_NOTIFICATION_CAPACITY);
    let (client_bridge, permission_rx, read_text_file_rx, write_text_file_rx) =
        AcpClientBridge::new_with_capability_lanes();
    let agent = Rc::new(
        OrcaAcpAgent::new_supervised(surface_host, config, notification_tx)
            .with_client_bridge(Arc::clone(&client_bridge)),
    );
    let facade_slot = Rc::new(RefCell::new(None::<RpcFacadeHandle>));
    let permission_routes = Rc::new(PermissionRoutes::default());
    let read_text_file_routes = Rc::new(ReadTextFileRoutes::default());
    let write_text_file_routes = Rc::new(WriteTextFileRoutes {
        response_observer: write_response_observer,
        written_observer: write_written_observer,
        ..WriteTextFileRoutes::default()
    });
    let capability_request_ids = Rc::new(CapabilityRequestIds::default());
    let handler = {
        let agent = Rc::clone(&agent);
        let facade_slot = Rc::clone(&facade_slot);
        let client_bridge = Arc::clone(&client_bridge);
        let permission_routes = Rc::clone(&permission_routes);
        let read_text_file_routes = Rc::clone(&read_text_file_routes);
        let write_text_file_routes = Rc::clone(&write_text_file_routes);
        Rc::new(move |frame: InboundFrame| {
            handle_inbound(
                Rc::clone(&agent),
                Rc::clone(&facade_slot),
                Arc::clone(&client_bridge),
                Rc::clone(&permission_routes),
                Rc::clone(&read_text_file_routes),
                Rc::clone(&write_text_file_routes),
                frame,
            )
        })
    };
    let response_routes = Arc::clone(&permission_routes.pending);
    let read_response_routes = Arc::clone(&read_text_file_routes.pending);
    let write_response_routes = Arc::clone(&write_text_file_routes.pending);
    let response_session_resolver: ResponseSessionResolver = Arc::new(move |request_id| {
        response_routes
            .lock()
            .expect("ACP permission route mutex is not poisoned")
            .get(&request_id)
            .map(|route| route.session_id.to_string())
            .or_else(|| {
                read_response_routes
                    .lock()
                    .expect("ACP read route mutex is not poisoned")
                    .get(&request_id)
                    .map(|route| route.session_id.to_string())
            })
            .or_else(|| {
                write_response_routes
                    .lock()
                    .expect("ACP write route mutex is not poisoned")
                    .get(&request_id)
                    .map(|route| route.session_id.to_string())
            })
    });
    let (facade, supervisor) = spawn_local_rpc_facade_with_response_session_resolver(
        reader,
        writer,
        handler,
        response_session_resolver,
        RpcFacadeConfig::default(),
    );
    *facade_slot.borrow_mut() = Some(facade.clone());

    let notification_task =
        tokio::task::spawn_local(dispatch_notifications(facade.clone(), notification_rx));
    let permission_task = tokio::task::spawn_local(dispatch_permissions(
        facade.clone(),
        Arc::clone(&client_bridge),
        Rc::clone(&permission_routes),
        permission_rx,
    ));
    let mut read_text_file_task = tokio::task::spawn_local(dispatch_read_text_files(
        facade.clone(),
        Arc::clone(&client_bridge),
        Rc::clone(&read_text_file_routes),
        Rc::clone(&capability_request_ids),
        read_text_file_rx,
    ));
    let mut write_text_file_task = tokio::task::spawn_local(dispatch_write_text_files(
        facade,
        Arc::clone(&client_bridge),
        Rc::clone(&write_text_file_routes),
        capability_request_ids,
        write_text_file_rx,
    ));

    let result = supervisor.wait().await.map(|_| ());
    client_bridge.cancel_all();
    retire_all_permission_routes(&permission_routes);
    retire_all_read_text_file_routes(&read_text_file_routes);
    retire_all_write_text_file_routes(&write_text_file_routes);
    notification_task.abort();
    permission_task.abort();
    let _ = notification_task.await;
    let _ = permission_task.await;
    if tokio::time::timeout(Duration::from_secs(5), &mut read_text_file_task)
        .await
        .is_err()
    {
        read_text_file_task.abort();
        let _ = read_text_file_task.await;
    }
    if tokio::time::timeout(Duration::from_secs(5), &mut write_text_file_task)
        .await
        .is_err()
    {
        write_text_file_task.abort();
        let _ = write_text_file_task.await;
    }
    result
}

fn handle_inbound(
    agent: Rc<OrcaAcpAgent>,
    facade_slot: Rc<RefCell<Option<RpcFacadeHandle>>>,
    client_bridge: Arc<AcpClientBridge>,
    permission_routes: Rc<PermissionRoutes>,
    read_text_file_routes: Rc<ReadTextFileRoutes>,
    write_text_file_routes: Rc<WriteTextFileRoutes>,
    frame: InboundFrame,
) -> LocalHandlerFuture {
    Box::pin(async move {
        let value = frame.json_value()?;
        if frame.method().is_none() {
            if !handle_read_text_file_response(&read_text_file_routes, &value)
                && !handle_write_text_file_response(&write_text_file_routes, &value)
            {
                handle_permission_response(&client_bridge, &permission_routes, &value);
            }
            return Ok(empty_completion());
        }
        let method = frame.method().expect("checked method").to_string();
        let request_id = value.get("id").cloned();
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let facade = facade_slot
            .borrow()
            .as_ref()
            .cloned()
            .ok_or(RpcFacadeError::Sealed)?;

        match method.as_str() {
            "initialize" => {
                let result = decode::<InitializeRequest>(params)
                    .map_err(agent_client_protocol::Error::into_internal_error);
                let result = match result {
                    Ok(args) => Agent::initialize(agent.as_ref(), args).await,
                    Err(error) => Err(error),
                };
                Ok(response_completion(facade, request_id, result))
            }
            "authenticate" => {
                let result = decode::<AuthenticateRequest>(params)
                    .map_err(agent_client_protocol::Error::into_internal_error);
                let result = match result {
                    Ok(args) => Agent::authenticate(agent.as_ref(), args).await,
                    Err(error) => Err(error),
                };
                Ok(response_completion(facade, request_id, result))
            }
            "session/new" => {
                let result = decode::<NewSessionRequest>(params)
                    .map_err(agent_client_protocol::Error::into_internal_error);
                let result = match result {
                    Ok(args) => Agent::new_session(agent.as_ref(), args).await,
                    Err(error) => Err(error),
                };
                Ok(response_completion(facade, request_id, result))
            }
            "session/load" => {
                let result = decode::<LoadSessionRequest>(params)
                    .map_err(agent_client_protocol::Error::into_internal_error);
                let result = match result {
                    Ok(args) => Agent::load_session(agent.as_ref(), args).await,
                    Err(error) => Err(error),
                };
                Ok(response_completion(facade, request_id, result))
            }
            "session/prompt" => {
                let result = match decode::<PromptRequest>(params) {
                    Ok(args) => {
                        let inbound_sequence = frame.session_sequence().ok_or_else(|| {
                            agent_client_protocol::Error::invalid_params()
                                .data("ACP prompt is missing a session sequence")
                        });
                        match inbound_sequence {
                            Ok(inbound_sequence) => {
                                agent.admit_prompt(args, Some(inbound_sequence)).await
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(agent_client_protocol::Error::invalid_params()
                        .data(format!("invalid ACP prompt: {error}"))),
                };
                match result {
                    Ok(admitted) => Ok(Box::pin(async move {
                        let result = agent.complete_prompt(admitted).await;
                        let _ = send_response(&facade, request_id, result).await;
                    }) as LocalHandlerCompletion),
                    Err(error) => Ok(response_completion::<Value>(facade, request_id, Err(error))),
                }
            }
            "session/cancel" => {
                let args: CancelNotification =
                    decode(params).map_err(|error| RpcFacadeError::Protocol {
                        message: format!("invalid ACP cancel: {error}"),
                    })?;
                let session_id = args.session_id.clone();
                Agent::cancel(agent.as_ref(), args).await.map_err(|error| {
                    RpcFacadeError::Protocol {
                        message: format!("ACP cancel failed: {error:?}"),
                    }
                })?;
                retire_session_permission_routes(&client_bridge, &permission_routes, &session_id);
                retire_session_read_text_file_routes(&read_text_file_routes, &session_id);
                retire_session_write_text_file_routes(&write_text_file_routes, &session_id);
                Ok(empty_completion())
            }
            _ => {
                let error = agent_client_protocol::Error::method_not_found()
                    .data(format!("unsupported ACP method '{method}'"));
                Ok(response_completion::<Value>(facade, request_id, Err(error)))
            }
        }
    })
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, serde_json::Error> {
    serde_json::from_value(value)
}

fn empty_completion() -> LocalHandlerCompletion {
    Box::pin(async {})
}

fn response_completion<T>(
    facade: RpcFacadeHandle,
    request_id: Option<Value>,
    result: Result<T, agent_client_protocol::Error>,
) -> LocalHandlerCompletion
where
    T: Serialize + 'static,
{
    Box::pin(async move {
        let _ = send_response(&facade, request_id, result).await;
    })
}

async fn send_response<T>(
    facade: &RpcFacadeHandle,
    request_id: Option<Value>,
    result: Result<T, agent_client_protocol::Error>,
) -> Result<(), RpcFacadeError>
where
    T: Serialize,
{
    let Some(request_id) = request_id else {
        return Ok(());
    };
    let value = match result {
        Ok(result) => json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": result,
        }),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": error,
        }),
    };
    enqueue_json(facade, value).await
}

async fn dispatch_notifications(
    facade: RpcFacadeHandle,
    mut notifications: tokio::sync::mpsc::Receiver<AcpNotificationDelivery>,
) {
    while let Some(delivery) = notifications.recv().await {
        let value = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": delivery.notification,
        });
        let result = enqueue_json(&facade, value)
            .await
            .map_err(|error| error.to_string());
        let failed = result.is_err();
        let _ = delivery.acknowledgement.send(result);
        if failed {
            break;
        }
    }
}

async fn dispatch_permissions(
    facade: RpcFacadeHandle,
    client_bridge: Arc<AcpClientBridge>,
    routes: Rc<PermissionRoutes>,
    mut requests: tokio::sync::mpsc::Receiver<super::agent::AcpPermissionRequest>,
) {
    while let Some(request) = requests.recv().await {
        if !client_bridge.is_pending(&request.key) {
            continue;
        }
        let request_id = routes.next_request_id.get();
        let Some(next_request_id) = request_id.checked_add(1) else {
            client_bridge.complete_permission(
                &request.key,
                Err(AcpPermissionWaitError::Client(
                    "ACP reverse request id exhausted".to_string(),
                )),
            );
            break;
        };
        routes.next_request_id.set(next_request_id);
        let (completed, completion) = oneshot::channel();
        routes
            .pending
            .lock()
            .expect("ACP permission route mutex is not poisoned")
            .insert(
                request_id,
                PendingPermissionRoute {
                    session_id: request.request.session_id.clone(),
                    key: request.key.clone(),
                    completed,
                },
            );
        let value = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "session/request_permission",
            "params": request.request,
        });
        if let Err(error) = enqueue_json(&facade, value).await {
            routes
                .pending
                .lock()
                .expect("ACP permission route mutex is not poisoned")
                .remove(&request_id);
            client_bridge.complete_permission(
                &request.key,
                Err(AcpPermissionWaitError::Client(error.to_string())),
            );
            break;
        }
        if tokio::time::timeout(ACP_REVERSE_REQUEST_DEADLINE, completion)
            .await
            .is_err()
        {
            routes
                .pending
                .lock()
                .expect("ACP permission route mutex is not poisoned")
                .remove(&request_id);
            client_bridge.complete_permission(
                &request.key,
                Err(AcpPermissionWaitError::Client(
                    "ACP permission response timed out".to_string(),
                )),
            );
        }
    }
}

async fn dispatch_read_text_files(
    facade: RpcFacadeHandle,
    client_bridge: Arc<AcpClientBridge>,
    routes: Rc<ReadTextFileRoutes>,
    request_ids: Rc<CapabilityRequestIds>,
    mut requests: tokio::sync::mpsc::Receiver<super::agent::AcpReadTextFileRequest>,
) {
    while let Some(request) = requests.recv().await {
        let Some(request_id) = request_ids.reserve() else {
            let _ = request.client.settle_acp_read_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpReadTextFileSettlement::FailedBeforeWrite {
                    message: "ACP read reverse request id exhausted".to_string(),
                },
            );
            break;
        };
        let session_id = SessionId::new(request.dispatch.acp_session_id.as_str().to_string());
        let params = ReadTextFileRequest::new(
            session_id.clone(),
            request.dispatch.path.as_path().to_path_buf(),
        )
        .line(request.dispatch.line)
        .limit(request.dispatch.limit);
        let (completed, mut completion) = oneshot::channel();
        routes
            .pending
            .lock()
            .expect("ACP read route mutex is not poisoned")
            .insert(
                request_id,
                PendingReadTextFileRoute {
                    session_id: session_id.clone(),
                    call_id: request.dispatch.call_id.clone(),
                    capability_revision: request.dispatch.capability_revision,
                    client: request.client.clone(),
                    physically_written: false,
                    completed,
                },
            );
        let value = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "fs/read_text_file",
            "params": params,
        });
        let mut encoded = match serde_json::to_vec(&value) {
            Ok(encoded) => encoded,
            Err(error) => {
                routes
                    .pending
                    .lock()
                    .expect("ACP read route mutex is not poisoned")
                    .remove(&request_id);
                let _ = request.client.settle_acp_read_text_file(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpReadTextFileSettlement::FailedBeforeWrite {
                        message: format!("ACP read request could not be encoded: {error}"),
                    },
                );
                break;
            }
        };
        encoded.push(b'\n');
        if !client_bridge.begin_capability_write(&session_id) {
            routes
                .pending
                .lock()
                .expect("ACP read route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_read_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpReadTextFileSettlement::FailedBeforeWrite {
                    message: "ACP read request was cancelled before write".to_string(),
                },
            );
            continue;
        }
        if request
            .client
            .claim_acp_read_text_file_write(
                request.dispatch.call_id.clone(),
                request.dispatch.capability_revision,
            )
            .is_err()
        {
            client_bridge.finish_capability_write(&session_id);
            routes
                .pending
                .lock()
                .expect("ACP read route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_read_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpReadTextFileSettlement::FailedBeforeWrite {
                    message: "ACP read runtime write claim was unavailable".to_string(),
                },
            );
            continue;
        }
        let write_receipt =
            match facade.enqueue(TransportFrame::new(FrameDirection::AgentToClient, encoded)) {
                Ok(receipt) => receipt,
                Err(error) => {
                    client_bridge.finish_capability_write(&session_id);
                    routes
                        .pending
                        .lock()
                        .expect("ACP read route mutex is not poisoned")
                        .remove(&request_id);
                    let _ = request.client.settle_acp_read_text_file(
                        request.dispatch.call_id,
                        request.dispatch.capability_revision,
                        AcpReadTextFileSettlement::FailedBeforeWrite {
                            message: format!("ACP read request was rejected before write: {error}"),
                        },
                    );
                    break;
                }
            };
        if let Some(route) = routes
            .pending
            .lock()
            .expect("ACP read route mutex is not poisoned")
            .get_mut(&request_id)
        {
            // Once admitted to the writer lane, a write/flush failure cannot
            // prove that zero bytes reached the peer.
            route.physically_written = true;
        }
        if let Err(error) = write_receipt.ack().await {
            routes
                .pending
                .lock()
                .expect("ACP read route mutex is not poisoned")
                .remove(&request_id);
            let settlement = completion.try_recv().unwrap_or_else(|_| {
                AcpReadTextFileSettlement::ObservationUnavailable {
                    message: format!(
                        "ACP read request may have been written but acknowledgement failed: {error}"
                    ),
                }
            });
            let _ = request.client.settle_acp_read_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                settlement,
            );
            client_bridge.finish_capability_write(&session_id);
            break;
        }
        if request
            .client
            .mark_acp_read_text_file_written(
                request.dispatch.call_id.clone(),
                request.dispatch.capability_revision,
            )
            .is_err()
        {
            routes
                .pending
                .lock()
                .expect("ACP read route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_read_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpReadTextFileSettlement::ObservationUnavailable {
                    message:
                        "ACP read request was written but its durable write acknowledgement failed"
                            .to_string(),
                },
            );
            client_bridge.finish_capability_write(&session_id);
            break;
        }
        client_bridge.finish_capability_write(&session_id);
        let settlement = match tokio::time::timeout(ACP_REVERSE_REQUEST_DEADLINE, completion).await
        {
            Ok(Ok(settlement)) => settlement,
            Ok(Err(_)) => AcpReadTextFileSettlement::ObservationUnavailable {
                message: "ACP read response route was dropped".to_string(),
            },
            Err(_) => {
                routes
                    .pending
                    .lock()
                    .expect("ACP read route mutex is not poisoned")
                    .remove(&request_id);
                AcpReadTextFileSettlement::ObservationUnavailable {
                    message: "ACP read response timed out".to_string(),
                }
            }
        };
        let _ = request.client.settle_acp_read_text_file(
            request.dispatch.call_id,
            request.dispatch.capability_revision,
            settlement,
        );
    }
}

async fn dispatch_write_text_files(
    facade: RpcFacadeHandle,
    client_bridge: Arc<AcpClientBridge>,
    routes: Rc<WriteTextFileRoutes>,
    request_ids: Rc<CapabilityRequestIds>,
    mut requests: tokio::sync::mpsc::Receiver<super::agent::AcpWriteTextFileRequest>,
) {
    while let Some(request) = requests.recv().await {
        let Some(request_id) = request_ids.reserve() else {
            let _ = request.client.settle_acp_write_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpWriteTextFileSettlement::FailedBeforeWrite {
                    message: "ACP write reverse request id exhausted".to_string(),
                },
            );
            break;
        };
        let session_id = SessionId::new(request.dispatch.acp_session_id.as_str().to_string());
        let params = WriteTextFileRequest::new(
            session_id.clone(),
            request.dispatch.path.as_path().to_path_buf(),
            request.dispatch.content,
        );
        let value = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "fs/write_text_file",
            "params": params,
        });
        let mut encoded = match serde_json::to_vec(&value) {
            Ok(encoded) => encoded,
            Err(error) => {
                let _ = request.client.settle_acp_write_text_file(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpWriteTextFileSettlement::FailedBeforeWrite {
                        message: format!("ACP write request could not be encoded: {error}"),
                    },
                );
                continue;
            }
        };
        encoded.push(b'\n');
        let (completed, mut completion) = oneshot::channel();
        routes
            .pending
            .lock()
            .expect("ACP write route mutex is not poisoned")
            .insert(
                request_id,
                PendingWriteTextFileRoute {
                    session_id: session_id.clone(),
                    call_id: request.dispatch.call_id.clone(),
                    capability_revision: request.dispatch.capability_revision,
                    client: request.client.clone(),
                    delivery_possible: false,
                    completed,
                },
            );
        if !client_bridge.begin_capability_write(&session_id) {
            routes
                .pending
                .lock()
                .expect("ACP write route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_write_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpWriteTextFileSettlement::FailedBeforeWrite {
                    message: "ACP write request was cancelled before delivery".to_string(),
                },
            );
            continue;
        }
        if request
            .client
            .permit_acp_write_text_file_delivery(
                request.dispatch.call_id.clone(),
                request.dispatch.capability_revision,
            )
            .is_err()
        {
            client_bridge.finish_capability_write(&session_id);
            routes
                .pending
                .lock()
                .expect("ACP write route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_write_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                    message:
                        "ACP write delivery barrier could not be observed after runtime admission"
                            .to_string(),
                },
            );
            continue;
        }
        if let Some(route) = routes
            .pending
            .lock()
            .expect("ACP write route mutex is not poisoned")
            .get_mut(&request_id)
        {
            route.delivery_possible = true;
        }
        let write_receipt =
            match facade.enqueue(TransportFrame::new(FrameDirection::AgentToClient, encoded)) {
                Ok(receipt) => receipt,
                Err(error) => {
                    client_bridge.finish_capability_write(&session_id);
                    routes
                        .pending
                        .lock()
                        .expect("ACP write route mutex is not poisoned")
                        .remove(&request_id);
                    let _ = request.client.settle_acp_write_text_file(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                        message: format!(
                            "ACP write request was rejected after delivery became possible: {error}"
                        ),
                    },
                );
                    break;
                }
            };
        if let Err(error) = write_receipt.ack().await {
            routes
                .pending
                .lock()
                .expect("ACP write route mutex is not poisoned")
                .remove(&request_id);
            if let Ok(settlement) = completion.try_recv() {
                // A decoded response proves that the request reached the peer
                // even when the local write/flush acknowledgement failed.
                // Establish (or retain) Written before forwarding that exact
                // response so the runtime owner cannot reject and lose it.
                let _ = request.client.mark_acp_write_text_file_written(
                    request.dispatch.call_id.clone(),
                    request.dispatch.capability_revision,
                );
                let _ = request.client.settle_acp_write_text_file(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    settlement,
                );
            } else {
                let _ = request.client.settle_acp_write_text_file(
                    request.dispatch.call_id,
                    request.dispatch.capability_revision,
                    AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                        message: format!(
                            "ACP file write may have occurred but acknowledgement failed: {error}"
                        ),
                    },
                );
            }
            client_bridge.finish_capability_write(&session_id);
            break;
        }
        if request
            .client
            .mark_acp_write_text_file_written(
                request.dispatch.call_id.clone(),
                request.dispatch.capability_revision,
            )
            .is_err()
        {
            routes
                .pending
                .lock()
                .expect("ACP write route mutex is not poisoned")
                .remove(&request_id);
            let _ = request.client.settle_acp_write_text_file(
                request.dispatch.call_id,
                request.dispatch.capability_revision,
                AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                    message:
                        "ACP file write completed but its durable acknowledgement was unavailable"
                            .to_string(),
                },
            );
            client_bridge.finish_capability_write(&session_id);
            break;
        }
        if let Some(observer) = &routes.written_observer {
            observer.notify_one();
        }
        client_bridge.finish_capability_write(&session_id);
        let settlement = match tokio::time::timeout(ACP_REVERSE_REQUEST_DEADLINE, completion).await
        {
            Ok(Ok(settlement)) => settlement,
            Ok(Err(_)) => AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                message: "ACP write response route was dropped".to_string(),
            },
            Err(_) => {
                routes
                    .pending
                    .lock()
                    .expect("ACP write route mutex is not poisoned")
                    .remove(&request_id);
                AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                    message: "ACP write response timed out".to_string(),
                }
            }
        };
        let _ = request.client.settle_acp_write_text_file(
            request.dispatch.call_id,
            request.dispatch.capability_revision,
            settlement,
        );
    }
}

fn handle_read_text_file_response(routes: &ReadTextFileRoutes, value: &Value) -> bool {
    let Some(request_id) = value.get("id").and_then(Value::as_i64) else {
        return false;
    };
    let Some(route) = routes
        .pending
        .lock()
        .expect("ACP read route mutex is not poisoned")
        .remove(&request_id)
    else {
        return false;
    };
    let settlement = if let Some(result) = value.get("result") {
        match serde_json::from_value::<ReadTextFileResponse>(result.clone()) {
            Ok(response) => AcpReadTextFileSettlement::Completed {
                content: response.content,
            },
            Err(error) => AcpReadTextFileSettlement::ObservationUnavailable {
                message: format!("invalid ACP read response: {error}"),
            },
        }
    } else {
        let code = value
            .get("error")
            .and_then(|error| error.get("code"))
            .map(Value::to_string)
            .unwrap_or_else(|| "unknown".to_string());
        let message = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("ACP read request failed")
            .to_string();
        AcpReadTextFileSettlement::RemoteError { code, message }
    };
    let _ = route.completed.send(settlement);
    true
}

fn handle_write_text_file_response(routes: &WriteTextFileRoutes, value: &Value) -> bool {
    let Some(request_id) = value.get("id").and_then(Value::as_i64) else {
        return false;
    };
    let Some(route) = routes
        .pending
        .lock()
        .expect("ACP write route mutex is not poisoned")
        .remove(&request_id)
    else {
        return false;
    };
    let settlement = if let Some(result) = value.get("result") {
        match serde_json::from_value::<WriteTextFileResponse>(result.clone()) {
            Ok(_) => AcpWriteTextFileSettlement::Completed,
            Err(error) => AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                message: format!("invalid ACP write response: {error}"),
            },
        }
    } else {
        let code = value
            .get("error")
            .and_then(|error| error.get("code"))
            .map(Value::to_string)
            .unwrap_or_else(|| "unknown".to_string());
        let message = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("ACP write request failed")
            .to_string();
        AcpWriteTextFileSettlement::RemoteError { code, message }
    };
    let _ = route.completed.send(settlement);
    if let Some(observer) = &routes.response_observer {
        observer.notify_one();
    }
    true
}

fn handle_permission_response(bridge: &AcpClientBridge, routes: &PermissionRoutes, value: &Value) {
    let Some(request_id) = value.get("id").and_then(Value::as_i64) else {
        return;
    };
    let Some(route) = routes
        .pending
        .lock()
        .expect("ACP permission route mutex is not poisoned")
        .remove(&request_id)
    else {
        return;
    };
    let result = if let Some(result) = value.get("result") {
        serde_json::from_value::<RequestPermissionResponse>(result.clone()).map_err(|error| {
            AcpPermissionWaitError::Client(format!("invalid ACP permission response: {error}"))
        })
    } else {
        let message = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("ACP permission request failed")
            .to_string();
        Err(AcpPermissionWaitError::Client(message))
    };
    bridge.complete_permission(&route.key, result);
    let _ = route.completed.send(());
}

fn retire_session_permission_routes(
    bridge: &AcpClientBridge,
    routes: &PermissionRoutes,
    session_id: &SessionId,
) {
    let request_ids = routes
        .pending
        .lock()
        .expect("ACP permission route mutex is not poisoned")
        .iter()
        .filter_map(|(request_id, route)| (route.session_id == *session_id).then_some(*request_id))
        .collect::<Vec<_>>();
    for request_id in request_ids {
        if let Some(route) = routes
            .pending
            .lock()
            .expect("ACP permission route mutex is not poisoned")
            .remove(&request_id)
        {
            bridge.complete_permission(&route.key, Err(AcpPermissionWaitError::Cancelled));
            let _ = route.completed.send(());
        }
    }
}

fn retire_session_read_text_file_routes(routes: &ReadTextFileRoutes, session_id: &SessionId) {
    let request_ids = routes
        .pending
        .lock()
        .expect("ACP read route mutex is not poisoned")
        .iter()
        .filter_map(|(request_id, route)| (route.session_id == *session_id).then_some(*request_id))
        .collect::<Vec<_>>();
    for request_id in request_ids {
        if let Some(route) = routes
            .pending
            .lock()
            .expect("ACP read route mutex is not poisoned")
            .remove(&request_id)
        {
            let _ = route
                .completed
                .send(AcpReadTextFileSettlement::ObservationUnavailable {
                    message: "ACP read response route was cancelled".to_string(),
                });
        }
    }
}

fn retire_session_write_text_file_routes(routes: &WriteTextFileRoutes, session_id: &SessionId) {
    let request_ids = routes
        .pending
        .lock()
        .expect("ACP write route mutex is not poisoned")
        .iter()
        .filter_map(|(request_id, route)| (route.session_id == *session_id).then_some(*request_id))
        .collect::<Vec<_>>();
    for request_id in request_ids {
        if let Some(route) = routes
            .pending
            .lock()
            .expect("ACP write route mutex is not poisoned")
            .remove(&request_id)
        {
            let settlement = if route.delivery_possible {
                AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                    message:
                        "ACP write response route was cancelled after delivery became possible"
                            .to_string(),
                }
            } else {
                AcpWriteTextFileSettlement::FailedBeforeWrite {
                    message: "ACP write response route was cancelled before delivery".to_string(),
                }
            };
            let _ = route.completed.send(settlement);
        }
    }
}

fn retire_all_permission_routes(routes: &PermissionRoutes) {
    let pending = routes
        .pending
        .lock()
        .expect("ACP permission route mutex is not poisoned")
        .drain()
        .map(|(_, route)| route)
        .collect::<Vec<_>>();
    for route in pending {
        let _ = route.completed.send(());
    }
}

fn retire_all_read_text_file_routes(routes: &ReadTextFileRoutes) {
    let pending = routes
        .pending
        .lock()
        .expect("ACP read route mutex is not poisoned")
        .drain()
        .map(|(_, route)| route)
        .collect::<Vec<_>>();
    for route in pending {
        let durable_settlement = if route.physically_written {
            AcpReadTextFileSettlement::ObservationUnavailable {
                message: "ACP read response route was retired after write".to_string(),
            }
        } else {
            AcpReadTextFileSettlement::FailedBeforeWrite {
                message: "ACP read response route was retired before write".to_string(),
            }
        };
        // The dispatcher task is aborted immediately after connection teardown.
        // Settle through the runtime owner first so the durable call cannot be
        // stranded merely because this adapter task loses its final timeslice.
        let _ = route.client.settle_acp_read_text_file(
            route.call_id,
            route.capability_revision,
            durable_settlement,
        );
        let _ = route.completed.send(if route.physically_written {
            AcpReadTextFileSettlement::ObservationUnavailable {
                message: "ACP read response route was retired after write".to_string(),
            }
        } else {
            AcpReadTextFileSettlement::FailedBeforeWrite {
                message: "ACP read response route was retired before write".to_string(),
            }
        });
    }
}

fn retire_all_write_text_file_routes(routes: &WriteTextFileRoutes) {
    let pending = routes
        .pending
        .lock()
        .expect("ACP write route mutex is not poisoned")
        .drain()
        .map(|(_, route)| route)
        .collect::<Vec<_>>();
    for route in pending {
        let durable_settlement = if route.delivery_possible {
            AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                message: "ACP write response route was retired after delivery became possible"
                    .to_string(),
            }
        } else {
            AcpWriteTextFileSettlement::FailedBeforeWrite {
                message: "ACP write response route was retired before delivery".to_string(),
            }
        };
        let _ = route.client.settle_acp_write_text_file(
            route.call_id,
            route.capability_revision,
            durable_settlement,
        );
        let _ = route.completed.send(if route.delivery_possible {
            AcpWriteTextFileSettlement::ExternalEffectAmbiguous {
                message: "ACP write response route was retired after delivery became possible"
                    .to_string(),
            }
        } else {
            AcpWriteTextFileSettlement::FailedBeforeWrite {
                message: "ACP write response route was retired before delivery".to_string(),
            }
        });
    }
}

async fn enqueue_json(facade: &RpcFacadeHandle, value: Value) -> Result<(), RpcFacadeError> {
    let mut encoded = serde_json::to_vec(&value).map_err(|error| RpcFacadeError::Protocol {
        message: error.to_string(),
    })?;
    encoded.push(b'\n');
    facade
        .enqueue(TransportFrame::new(FrameDirection::AgentToClient, encoded))?
        .ack()
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Waker};
    use std::time::Instant;

    use agent_client_protocol::{
        CancelNotification, ClientCapabilities, ContentBlock, FileSystemCapabilities,
        Implementation, InitializeRequest, NewSessionRequest, PromptRequest, ProtocolVersion,
    };
    use orca_core::cancel::CancelToken;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::RawToolCall;
    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_core::model::ModelSelection;
    use orca_core::provider_types::{ProviderResponse, ProviderStep};
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
    use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

    use super::*;
    use crate::model_response::RuntimeModelResponse;
    use crate::runtime_host::{
        GenerationContext, HostedTurnRequest, RuntimeHost, ThreadOperationExecutor,
        ThreadOperationOutcome,
    };
    use crate::thread::RuntimeThread;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    struct WaitForCancelExecutor;

    struct CompleteWithMessageExecutor;

    struct ReadTextFileExecutor {
        content_tx: std::sync::mpsc::SyncSender<String>,
    }

    struct WriteTextFileExecutor {
        outcome_tx: std::sync::mpsc::SyncSender<Result<(), io::ErrorKind>>,
    }

    #[derive(Default)]
    struct FlushFailureSignal {
        fail: AtomicBool,
        waker: Mutex<Option<Waker>>,
    }

    impl FlushFailureSignal {
        fn fail(&self) {
            self.fail.store(true, Ordering::Release);
            if let Some(waker) = self.waker.lock().unwrap().take() {
                waker.wake();
            }
        }
    }

    struct FailWriteTextFileFlush<W> {
        inner: W,
        signal: Arc<FlushFailureSignal>,
        fail_current_flush: bool,
    }

    impl<W> FailWriteTextFileFlush<W> {
        fn new(inner: W, signal: Arc<FlushFailureSignal>) -> Self {
            Self {
                inner,
                signal,
                fail_current_flush: false,
            }
        }
    }

    impl<W: AsyncWrite + Unpin> AsyncWrite for FailWriteTextFileFlush<W> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if bytes
                .windows(b"fs/write_text_file".len())
                .any(|window| window == b"fs/write_text_file")
            {
                self.fail_current_flush = true;
            }
            Pin::new(&mut self.inner).poll_write(cx, bytes)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.fail_current_flush {
                if self.signal.fail.load(Ordering::Acquire) {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "injected write-text-file flush failure",
                    )));
                }
                *self.signal.waker.lock().unwrap() = Some(cx.waker().clone());
                return Poll::Pending;
            }
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    impl ThreadOperationExecutor for CompleteWithMessageExecutor {
        fn run_turn(
            &self,
            _thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let turn_request = request.thread_turn_request(generation);
            let ingress = turn_request
                .provider_response_ingress()
                .expect("typed ACP operation provides response ingress");
            ingress.commit_response(&RuntimeModelResponse::new(
                ProviderResponse {
                    steps: Vec::new(),
                    assistant_content: Some("typed update".to_string()),
                    assistant_reasoning: None,
                    tool_calls: Vec::new(),
                    usage: None,
                },
                request.turn_id().clone(),
            ))?;
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for ReadTextFileExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let tool = ToolRequest {
                id: "read-capability-1".to_string(),
                name: ToolName::ReadFile,
                action: orca_core::approval_types::ActionKind::Read,
                target: Some("/workspace/notes.txt".to_string()),
                raw_arguments: Some(
                    r#"{"path":"/workspace/notes.txt","line":2,"limit":3}"#.to_string(),
                ),
            };
            let turn_request = request.thread_turn_request(generation);
            let ingress = turn_request
                .provider_response_ingress()
                .expect("typed ACP operation provides response ingress");
            ingress.commit_response(&RuntimeModelResponse::new(
                ProviderResponse {
                    steps: vec![ProviderStep::ToolCall(tool.clone())],
                    assistant_content: None,
                    assistant_reasoning: None,
                    tool_calls: vec![RawToolCall {
                        id: tool.id.clone(),
                        function_name: tool.name.as_str().to_string(),
                        arguments: tool.raw_arguments.clone().unwrap(),
                    }],
                    usage: None,
                },
                request.turn_id().clone(),
            ))?;
            let content = match generation.read_text_file_from_acp_client(
                &tool,
                PathBuf::from("/workspace/notes.txt"),
                Some(2),
                Some(3),
            ) {
                Ok(content) => content,
                Err(_) if cancel.is_cancelled() => return Ok(RunStatus::Cancelled.into()),
                Err(error) => return Err(error),
            };
            self.content_tx.send(content).unwrap();
            ingress.commit_tool_result(&ToolResult::completed(
                &tool,
                "read through ACP client".to_string(),
                false,
            ))?;
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for WriteTextFileExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let tool = ToolRequest {
                id: "write-capability-1".to_string(),
                name: ToolName::WriteFile,
                action: orca_core::approval_types::ActionKind::Write,
                target: Some("/workspace/output.txt".to_string()),
                raw_arguments: Some(
                    r#"{"path":"/workspace/output.txt","content":"written by Orca\n"}"#.to_string(),
                ),
            };
            let turn_request = request.thread_turn_request(generation);
            let ingress = turn_request
                .provider_response_ingress()
                .expect("typed ACP operation provides response ingress");
            ingress.commit_response(&RuntimeModelResponse::new(
                ProviderResponse {
                    steps: vec![ProviderStep::ToolCall(tool.clone())],
                    assistant_content: None,
                    assistant_reasoning: None,
                    tool_calls: vec![RawToolCall {
                        id: tool.id.clone(),
                        function_name: tool.name.as_str().to_string(),
                        arguments: tool.raw_arguments.clone().unwrap(),
                    }],
                    usage: None,
                },
                request.turn_id().clone(),
            ))?;
            let outcome = generation.write_text_file_to_acp_client(
                &tool,
                PathBuf::from("/workspace/output.txt"),
                "written by Orca\n".to_string(),
            );
            self.outcome_tx
                .send(outcome.as_ref().map(|_| ()).map_err(io::Error::kind))
                .unwrap();
            outcome?;
            ingress.commit_tool_result(&ToolResult::completed(
                &tool,
                "wrote through ACP client".to_string(),
                false,
            ))?;
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for WaitForCancelExecutor {
        fn run_turn(
            &self,
            _thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            _generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let deadline = Instant::now() + TEST_TIMEOUT;
            while !cancel.is_cancelled() {
                assert!(
                    Instant::now() < deadline,
                    "ACP cancel did not reach runtime"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(RunStatus::Cancelled.into())
        }
    }

    #[test]
    fn bounded_production_connection_binds_prompt_before_later_wire_cancel() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let host = RuntimeHost::start_with_executor(Arc::new(WaitForCancelExecutor)).unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0")),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id.clone()),
                    vec![ContentBlock::from("wait".to_string())],
                ),
            )
            .await;
            write_notification(
                &mut client_write,
                "session/cancel",
                CancelNotification::new(SessionId::new(session_id)),
            )
            .await;

            let prompt = read_response(&mut client_read, 3).await;
            assert_eq!(prompt["result"]["stopReason"], "cancelled");
            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn production_connection_flushes_typed_updates_before_prompt_response() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let host =
                RuntimeHost::start_with_executor(Arc::new(CompleteWithMessageExecutor)).unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0")),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("complete".to_string())],
                ),
            )
            .await;

            let first = read_value(&mut client_read).await;
            assert_eq!(first["method"], "session/update");
            assert_eq!(first["params"]["update"]["content"]["text"], "typed update");
            let prompt = read_response(&mut client_read, 3).await;
            assert_eq!(prompt["result"]["stopReason"], "end_turn");

            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn production_connection_routes_read_text_file_through_runtime_owned_capability_call() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (content_tx, content_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(ReadTextFileExecutor { content_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().read_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id.clone()),
                    vec![ContentBlock::from("read notes".to_string())],
                ),
            )
            .await;

            let read_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "fs/read_text_file" {
                    break value;
                }
            };
            assert_eq!(read_request["params"]["sessionId"], session_id);
            assert_eq!(read_request["params"]["path"], "/workspace/notes.txt");
            assert_eq!(read_request["params"]["line"], 2);
            assert_eq!(read_request["params"]["limit"], 3);
            let read_id = read_request["id"].as_i64().expect("reverse request id");
            write_raw_response(
                &mut client_write,
                read_id,
                json!({ "content": "line two\nline three\nline four\n" }),
            )
            .await;

            let prompt = read_response(&mut client_read, 3).await;
            assert_eq!(prompt["result"]["stopReason"], "end_turn");
            assert_eq!(
                content_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
                "line two\nline three\nline four\n"
            );

            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn production_connection_routes_write_text_file_after_runtime_delivery_barrier() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(WriteTextFileExecutor { outcome_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().write_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id.clone()),
                    vec![ContentBlock::from("write output".to_string())],
                ),
            )
            .await;

            let write_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "fs/write_text_file" {
                    break value;
                }
            };
            assert_eq!(write_request["params"]["sessionId"], session_id);
            assert_eq!(write_request["params"]["path"], "/workspace/output.txt");
            assert_eq!(write_request["params"]["content"], "written by Orca\n");
            assert!(
                outcome_rx.try_recv().is_err(),
                "tool waiter completed before the physical response"
            );
            let write_id = write_request["id"].as_i64().expect("reverse request id");
            write_raw_response(&mut client_write, write_id, json!({})).await;

            let prompt = read_response(&mut client_read, 3).await;
            assert_eq!(prompt["result"]["stopReason"], "end_turn");
            assert_eq!(outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap(), Ok(()));

            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn decoded_write_response_survives_local_flush_ack_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(WriteTextFileExecutor { outcome_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let failure = Arc::new(FlushFailureSignal::default());
            let response_observed = Arc::new(Notify::new());
            let connection = tokio::task::spawn_local(run_connection_inner(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                FailWriteTextFileFlush::new(server_write, Arc::clone(&failure)),
                Some(Arc::clone(&response_observed)),
                None,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().write_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("write output".to_string())],
                ),
            )
            .await;
            let write_request = loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "fs/write_text_file" {
                    break value;
                }
            };
            let write_id = write_request["id"].as_i64().expect("reverse request id");
            write_raw_response(&mut client_write, write_id, json!({})).await;
            tokio::time::timeout(TEST_TIMEOUT, response_observed.notified())
                .await
                .expect("write response decoded before injected flush failure");
            failure.fail();

            assert_eq!(
                tokio::task::spawn_blocking(move || outcome_rx.recv_timeout(TEST_TIMEOUT))
                    .await
                    .expect("outcome waiter")
                    .expect("write outcome"),
                Ok(()),
                "a decoded response must remain settleable when local flush acknowledgement fails"
            );
            let _ = client_write.shutdown().await;
            drop(client_read);
            let connection_error = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect_err("injected flush failure must fail the connection");
            assert!(
                matches!(connection_error, RpcFacadeError::Flush { .. }),
                "unexpected connection error: {connection_error:?}"
            );
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn connection_loss_after_write_delivery_reports_ambiguous_effect_without_success() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(WriteTextFileExecutor { outcome_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().write_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("write output".to_string())],
                ),
            )
            .await;
            loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "fs/write_text_file" {
                    break;
                }
            }

            client_write.shutdown().await.unwrap();
            drop(client_write);
            drop(client_read);
            let _ = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task");
            assert_eq!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
                Err(io::ErrorKind::Other),
                "an acknowledged physical write without a response must not report success"
            );
            host.shutdown().unwrap();
            assert_persisted_external_effect_ambiguity(&transcript_path);
        });
    }

    #[test]
    fn cancelling_delivered_write_preserves_external_effect_ambiguity() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(WriteTextFileExecutor { outcome_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().write_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id.clone()),
                    vec![ContentBlock::from("write output".to_string())],
                ),
            )
            .await;
            loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "fs/write_text_file" {
                    break;
                }
            }

            write_notification(
                &mut client_write,
                "session/cancel",
                CancelNotification::new(SessionId::new(session_id)),
            )
            .await;
            let prompt = read_response(&mut client_read, 3).await;
            assert!(
                prompt["error"]["data"]
                    .as_str()
                    .is_some_and(|message| message.contains("ambiguous")),
                "delivered write cancellation must surface ambiguity: {prompt}"
            );
            assert!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap().is_err(),
                "cancelled delivered write must never report success"
            );

            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
            assert_persisted_external_effect_ambiguity(&transcript_path);
        });
    }

    #[test]
    fn host_shutdown_preserves_delivered_write_external_effect_ambiguity() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(WriteTextFileExecutor { outcome_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let written = Arc::new(Notify::new());
            let connection = tokio::task::spawn_local(run_connection_inner(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
                None,
                Some(Arc::clone(&written)),
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().write_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("write output".to_string())],
                ),
            )
            .await;
            loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "fs/write_text_file" {
                    break;
                }
            }
            tokio::time::timeout(TEST_TIMEOUT, written.notified())
                .await
                .expect("write delivery acknowledgement");

            tokio::task::spawn_blocking(move || host.shutdown())
                .await
                .expect("host shutdown task")
                .expect("host shutdown");
            assert_eq!(
                outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
                Err(io::ErrorKind::Interrupted)
            );
            assert_persisted_external_effect_ambiguity(&transcript_path);

            let _ = client_write.shutdown().await;
            drop(client_read);
            let _ = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task");
        });
    }

    #[test]
    fn failed_delivery_checkpoint_prevents_write_request_from_reaching_wire() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(WriteTextFileExecutor { outcome_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().write_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();
            let transcript_path = crate::thread_store::find_session_path(&session_id, true)
                .unwrap()
                .expect("recording ACP session path");
            crate::runtime_surface::JsonlSurfaceCommitLedger::
                inject_capability_delivery_checkpoint_failures(transcript_path, 3);

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id),
                    vec![ContentBlock::from("write output".to_string())],
                ),
            )
            .await;
            let mut outcome_task =
                tokio::task::spawn_blocking(move || outcome_rx.recv_timeout(TEST_TIMEOUT).unwrap());
            let outcome = loop {
                tokio::select! {
                    outcome = &mut outcome_task => break outcome.expect("outcome task"),
                    value = read_value(&mut client_read) => {
                        assert_ne!(
                            value["method"], "fs/write_text_file",
                            "wire write occurred before the durable delivery barrier"
                        );
                    }
                }
            };
            assert_eq!(outcome, Err(io::ErrorKind::Other));

            client_write.shutdown().await.unwrap();
            drop(client_read);
            let _ = tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task");
            host.shutdown().unwrap();
        });
    }

    #[test]
    fn cancelling_prompt_terminalizes_outstanding_read_text_file_call() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let (content_tx, _content_rx) = std::sync::mpsc::sync_channel(1);
            let host =
                RuntimeHost::start_with_executor(Arc::new(ReadTextFileExecutor { content_tx }))
                    .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);

            write_request(
                &mut client_write,
                1,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("bounded-test", "0.0.0"))
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new().read_text_file(true)),
                    ),
            )
            .await;
            let _ = read_response(&mut client_read, 1).await;

            write_request(
                &mut client_write,
                2,
                "session/new",
                NewSessionRequest::new(cwd.path().to_path_buf()),
            )
            .await;
            let new_session = read_response(&mut client_read, 2).await;
            let session_id = new_session["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_string();

            write_request(
                &mut client_write,
                3,
                "session/prompt",
                PromptRequest::new(
                    SessionId::new(session_id.clone()),
                    vec![ContentBlock::from("read notes".to_string())],
                ),
            )
            .await;
            loop {
                let value = read_value(&mut client_read).await;
                if value["method"] == "fs/read_text_file" {
                    break;
                }
            }

            write_notification(
                &mut client_write,
                "session/cancel",
                CancelNotification::new(SessionId::new(session_id)),
            )
            .await;
            let prompt = read_response(&mut client_read, 3).await;
            assert_eq!(
                prompt["result"]["stopReason"], "cancelled",
                "unexpected prompt response: {prompt}"
            );

            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
        });
    }

    async fn write_request(
        writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
        id: i64,
        method: &str,
        params: impl Serialize,
    ) {
        let mut encoded = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .unwrap();
        encoded.push(b'\n');
        writer.write_all(&encoded).await.unwrap();
    }

    async fn write_notification(
        writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
        method: &str,
        params: impl Serialize,
    ) {
        let mut encoded = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .unwrap();
        encoded.push(b'\n');
        writer.write_all(&encoded).await.unwrap();
    }

    async fn write_raw_response(
        writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
        id: i64,
        result: Value,
    ) {
        let mut encoded = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
        .unwrap();
        encoded.push(b'\n');
        writer.write_all(&encoded).await.unwrap();
    }

    async fn read_response<R>(reader: &mut BufReader<R>, id: i64) -> Value
    where
        R: AsyncRead + Unpin,
    {
        loop {
            let value = read_value(reader).await;
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                return value;
            }
        }
    }

    async fn read_value<R>(reader: &mut BufReader<R>) -> Value
    where
        R: AsyncRead + Unpin,
    {
        let mut line = String::new();
        tokio::time::timeout(TEST_TIMEOUT, reader.read_line(&mut line))
            .await
            .expect("ACP frame timeout")
            .expect("ACP frame read");
        assert!(!line.is_empty(), "ACP connection closed before next frame");
        serde_json::from_str(&line).unwrap()
    }

    fn persisted_surface_events(path: &std::path::Path) -> Vec<crate::surface::SurfaceEvent> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|record| record["type"] == "runtime.surface_commit.prepared")
            .filter_map(|record| record.get("batch").cloned())
            .filter_map(|batch| {
                serde_json::from_value::<crate::runtime_surface::StoredSurfaceCommitBatchV1>(batch)
                    .ok()
            })
            .filter_map(|batch| batch.into_live().ok())
            .flat_map(|batch| {
                batch
                    .events
                    .as_slice()
                    .iter()
                    .map(|event| event.event.clone())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn assert_persisted_external_effect_ambiguity(path: &std::path::Path) {
        let events = persisted_surface_events(path);
        assert!(events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Tool(crate::surface::ToolPatch::Completed {
                    result: crate::unstable_surface::SurfaceToolResult {
                        terminal: crate::unstable_surface::SurfaceToolTerminal {
                            kind: crate::unstable_surface::SurfaceToolResultKind::ExternalEffectAmbiguous,
                            ..
                        },
                        ..
                    },
                })
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Operation(
                    crate::surface::OperationPatch::GenerationStopped {
                        reason:
                            crate::unstable_surface::GenerationStopReason::ExecutionFailed {
                                class: crate::unstable_surface::GenerationExecutionFailureClass::ExternalEffectAmbiguous,
                                ..
                            },
                        ..
                    }
                )
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                crate::surface::SurfaceEvent::Operation(crate::surface::OperationPatch::Terminal {
                    record: crate::unstable_surface::OperationTerminalRecord {
                        terminal: crate::surface::OperationTerminal::Failed {
                            class: crate::surface::FailureClass::ExternalEffectAmbiguous,
                            ..
                        },
                        ..
                    },
                })
            )
        }));
    }

    fn test_config(cwd: PathBuf) -> RunConfig {
        RunConfig {
            app_version: "test".to_string(),
            prompt: String::new(),
            cwd: Some(cwd),
            output_format: OutputFormat::Jsonl,
            approval_mode: orca_core::approval_types::ApprovalMode::FullAuto,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: HashMap::new(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::default(),
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }
}
