//! ACP (Agent Client Protocol) adapter layer.
//!
//! Provides `orca --mode=acp` as a parallel entry point that projects the ACP
//! wire protocol onto the existing `RuntimeHost`, `EventEnvelope` and
//! `GenerationFence` internals without replacing the internal JSONL protocol.

mod agent;
mod event_map;
mod transport;

pub use agent::OrcaAcpAgent;

use agent_client_protocol::{AgentSideConnection, Client, SessionNotification};
use orca_core::config::RunConfig;
use tokio::sync::mpsc;

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
    let host_handle = host.handle();

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
        let (note_tx, mut note_rx) = mpsc::unbounded_channel::<SessionNotification>();
        let agent = OrcaAcpAgent::new(host_handle, config, note_tx);

        let (incoming, outgoing) = transport::stdio();
        let (conn, io_task) = AgentSideConnection::new(agent, outgoing, incoming, |fut| {
            tokio::task::spawn_local(fut);
        });

        // Drain notifications from the runtime onto the ACP connection.
        tokio::task::spawn_local(async move {
            while let Some(notification) = note_rx.recv().await {
                let _ = conn.session_notification(notification).await;
            }
        });

        match io_task.await {
            Ok(()) => 0,
            Err(_) => 1,
        }
    });

    drop(host);
    exit_code
}
