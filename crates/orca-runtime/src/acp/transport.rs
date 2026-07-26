//! Stdio transport adapter for the ACP agent.

use tokio::io::{stdin, stdout};

/// Returns the native Tokio stdio pair consumed by the bounded ACP supervisor.
pub fn stdio() -> (
    impl tokio::io::AsyncRead + Unpin + Send,
    impl tokio::io::AsyncWrite + Unpin + Send,
) {
    (stdin(), stdout())
}
