//! ACP (Agent Client Protocol) adapter layer.
//!
//! Provides `orca --mode=acp` as a parallel entry point that projects the ACP
//! wire protocol onto the existing `RuntimeHost`, `EventEnvelope` and
//! `GenerationFence` internals without replacing the internal JSONL protocol.

mod agent;
mod event_map;
#[allow(dead_code)]
pub(crate) mod rpc_facade;
mod transport;

pub use agent::OrcaAcpAgent;

use agent_client_protocol::{AgentSideConnection, Client, SessionNotification};
use orca_core::config::RunConfig;
use std::rc::Rc;
use tokio::sync::mpsc;

use self::agent::AcpClientBridge;
use crate::runtime_host::RuntimeHost;

/// Runs the ACP agent on stdio. Returns a process exit code.
pub fn run(config: RunConfig) -> i32 {
    let host = match RuntimeHost::start() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("orca: failed to start runtime host: {e}");
            return 1;
        }
    };
    let surface_host = host.surface_handle();

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("orca: failed to build tokio runtime: {e}");
            return 1;
        }
    };

    let local_set = tokio::task::LocalSet::new();
    let exit_code = local_set.block_on(&rt, async {
        let (note_tx, mut note_rx) = mpsc::channel::<SessionNotification>(256);
        let (client_bridge, mut permission_rx) = AcpClientBridge::new();
        let agent = OrcaAcpAgent::new_typed_bounded(surface_host, config, note_tx)
            .with_client_bridge(client_bridge.clone());

        let (incoming, outgoing) = transport::stdio();
        let (conn, io_task) = AgentSideConnection::new(agent, outgoing, incoming, |fut| {
            tokio::task::spawn_local(fut);
        });
        let conn = Rc::new(conn);

        // Drain notifications from the runtime onto the ACP connection.
        let notification_conn = Rc::clone(&conn);
        tokio::task::spawn_local(async move {
            while let Some(notification) = note_rx.recv().await {
                if notification_conn
                    .session_notification(notification)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let permission_conn = Rc::clone(&conn);
        let permission_bridge = std::sync::Arc::clone(&client_bridge);
        tokio::task::spawn_local(async move {
            while let Some(request) = permission_rx.recv().await {
                if !permission_bridge.is_pending(&request.key) {
                    continue;
                }
                let connection = Rc::clone(&permission_conn);
                let bridge = std::sync::Arc::clone(&permission_bridge);
                tokio::task::spawn_local(async move {
                    let result = connection
                        .request_permission(request.request)
                        .await
                        .map_err(|error| {
                            crate::acp::agent::AcpPermissionWaitError::Client(format!("{error:?}"))
                        });
                    bridge.complete_permission(&request.key, result);
                });
            }
        });

        let exit_code = match io_task.await {
            Ok(()) => 0,
            Err(_) => 1,
        };
        client_bridge.cancel_all();
        exit_code
    });

    drop(host);
    exit_code
}
