//! Session identifier and event types shared between PTY and process providers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A monotonically-assigned session identifier.
///
/// The `u64` is per-node and per-provider lifetime; it is never reused within
/// one provider instance. Matches the `exec_sessions.id` column type and the
/// `session: u64` wire field in the management verbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExecSessionId(pub u64);

impl std::fmt::Display for ExecSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Whether a session is backed by a PTY or a headless process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecKind {
    /// A PTY session (interactive terminal).
    Pty,
    /// A headless process session (no TTY).
    Proc,
}

/// Which I/O stream an [`ExecEvent::Output`] or write targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecStreamKind {
    /// Inbound data to the process (stdin). Only meaningful for writes.
    Stdin,
    /// Outbound data from the process standard output.
    Stdout,
    /// Outbound data from the process standard error.
    Stderr,
}

/// An event emitted by a running session toward the manager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExecEvent {
    /// Bytes arrived on stdout or stderr.
    Output {
        /// Which stream the bytes came from.
        stream: ExecStreamKind,
        /// The raw bytes.
        bytes: Vec<u8>,
    },
    /// The process exited.
    Exited {
        /// The exit code, if the process exited normally.
        code: Option<i32>,
        /// The signal number that terminated the process, if killed by a signal.
        signal: Option<i32>,
    },
}

/// A summary record of a session — live or recently exited.
///
/// Returned by [`ExecProvider::list_sessions`] for enumeration by an
/// authorised peer.
#[derive(Debug, Clone)]
pub struct ExecSessionRecord {
    /// The session identifier.
    pub id: ExecSessionId,
    /// Whether this is a PTY or headless process session.
    pub kind: ExecKind,
    /// A short human-readable summary of what was spawned (argv or shell).
    pub command_summary: String,
    /// When the session was started.
    pub started: DateTime<Utc>,
}
