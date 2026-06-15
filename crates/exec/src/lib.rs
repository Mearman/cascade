#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! Exec capability provider — terminals and processes.
//!
//! A self-contained capability provider for PTYs and headless processes.
//! PTYs are backed by [`portable_pty`]; processes by [`tokio::process`].
//!
//! This crate never touches the network or the grant store. It is called
//! exclusively by the authorised management dispatch path, mirroring how
//! backends are self-contained behind the `Backend` trait.

pub mod error;
pub mod proc;
pub mod pty;
pub mod session;

pub use error::ExecError;
pub use proc::ProcSpec;
pub use pty::PtySpec;
pub use session::{ExecEvent, ExecKind, ExecSessionId, ExecSessionRecord, ExecStreamKind};

use async_trait::async_trait;
use tokio::sync::mpsc;

/// The provider contract the engine drives.
///
/// Mirrors how `Backend` / `ManageDispatch` are injected: the engine holds an
/// `Arc<dyn ExecProvider>` and calls through this trait; the production
/// implementation is [`LocalExecProvider`]. Tests supply doubles.
#[async_trait]
pub trait ExecProvider: Send + Sync {
    /// Spawn a PTY session. Returns the new session id.
    async fn pty_spawn(&self, spec: PtySpec) -> Result<ExecSessionId, ExecError>;

    /// Write bytes to the master side of a PTY session (stdin).
    async fn pty_write(&self, id: ExecSessionId, bytes: &[u8]) -> Result<(), ExecError>;

    /// Resize a PTY session.
    async fn pty_resize(&self, id: ExecSessionId, cols: u16, rows: u16) -> Result<(), ExecError>;

    /// Send a signal to a PTY session's child process.
    async fn pty_kill(&self, id: ExecSessionId, signal: i32) -> Result<(), ExecError>;

    /// Spawn a headless process session. Returns the new session id.
    async fn proc_spawn(&self, spec: ProcSpec) -> Result<ExecSessionId, ExecError>;

    /// Send a signal to a headless process session.
    async fn proc_signal(&self, id: ExecSessionId, signal: i32) -> Result<(), ExecError>;

    /// Kill a headless process session immediately.
    async fn proc_kill(&self, id: ExecSessionId) -> Result<(), ExecError>;

    /// Subscribe to events from a session. Returns `None` when `id` is unknown.
    fn subscribe(&self, id: ExecSessionId) -> Option<mpsc::Receiver<ExecEvent>>;

    /// List all known sessions (live and recently exited). Used for enumeration
    /// by an authorised peer.
    fn list_sessions(&self) -> Vec<ExecSessionRecord>;
}

pub use local::LocalExecProvider;

mod local;
