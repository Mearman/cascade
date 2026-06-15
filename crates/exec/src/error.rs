//! Error type for the exec capability provider.

use super::ExecSessionId;

/// Errors that can arise from exec capability operations.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// A spawn operation failed.
    #[error("failed to spawn process: {0}")]
    Spawn(#[source] anyhow::Error),

    /// The session id is not known to this provider.
    #[error("session {0:?} not found")]
    NotFound(ExecSessionId),

    /// A write could not be delivered because the session's stdin is closed.
    #[error("session stdin is closed")]
    WriteClosed,

    /// The session does not support resize (e.g. a headless process).
    #[error("resize is not supported for this session kind")]
    ResizeUnsupported,

    /// The requested signal is not supported on this platform.
    #[error("signal {0} is not supported on this platform")]
    SignalUnsupported(i32),

    /// An I/O error from an underlying system call or tokio operation.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
