//! Stdio transport adapter for the ACP agent.

use tokio::io::{stdin, stdout};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Returns a `(reader, writer)` pair adapting tokio stdio to the
/// `futures::io::{AsyncRead, AsyncWrite}` traits required by
/// `AgentSideConnection`.
pub fn stdio() -> (impl futures::io::AsyncRead + Unpin, impl futures::io::AsyncWrite + Unpin) {
    (stdin().compat(), stdout().compat_write())
}
