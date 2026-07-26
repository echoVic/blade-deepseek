pub use orca_runtime::acp::*;

use orca_core::config::RunConfig;
use orca_runtime::runtime_host::RuntimeHost;

pub fn run(config: RunConfig) -> i32 {
    let host = match RuntimeHost::start() {
        Ok(host) => host,
        Err(error) => {
            eprintln!("orca: failed to start runtime host: {error}");
            return 1;
        }
    };
    let exit_code = orca_runtime::acp::run_with_surface_host(host.surface_handle(), config);
    drop(host);
    exit_code
}
