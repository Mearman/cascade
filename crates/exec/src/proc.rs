//! Headless process session specification.

/// Parameters for spawning a headless process session.
#[derive(Debug, Clone)]
pub struct ProcSpec {
    /// The command and its arguments. `argv[0]` is the binary path; at least
    /// one element is required.
    pub argv: Vec<String>,
    /// Working directory for the spawned process. When `None`, the current
    /// working directory of the daemon is inherited.
    pub cwd: Option<String>,
    /// Environment variables to set in addition to (or overriding) the
    /// inherited environment. Each entry is `(name, value)`.
    pub env: Vec<(String, String)>,
}
