//! ACP (Agent Client Protocol) adapter layer.
//!
//! Provides `orca --mode=acp` as a bounded protocol adapter over the same
//! runtime-owned typed surface used by the TUI.

mod agent;
#[allow(dead_code)]
pub(crate) mod rpc_facade;
mod supervisor;
mod transport;

pub use agent::OrcaAcpAgent;

use orca_core::config::RunConfig;

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
        let (incoming, outgoing) = transport::stdio();
        match supervisor::run_connection(surface_host, config, incoming, outgoing).await {
            Ok(()) => 0,
            Err(_) => 1,
        }
    });

    drop(host);
    exit_code
}
