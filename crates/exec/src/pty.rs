//! PTY session specification.

/// Parameters for spawning a PTY session.
#[derive(Debug, Clone)]
pub struct PtySpec {
    /// The shell binary to run. When `None`, the implementation picks a
    /// platform default (e.g. `$SHELL` or `/bin/sh`).
    pub shell: Option<String>,
    /// Arguments to pass to the shell. Typically empty for an interactive
    /// session.
    pub argv: Vec<String>,
    /// Working directory for the spawned process. When `None`, the current
    /// working directory of the daemon is inherited.
    pub cwd: Option<String>,
    /// Environment variables to set in addition to (or overriding) the
    /// inherited environment. Each entry is `(name, value)`.
    pub env: Vec<(String, String)>,
    /// Initial terminal width in columns.
    pub cols: u16,
    /// Initial terminal height in rows.
    pub rows: u16,
}
