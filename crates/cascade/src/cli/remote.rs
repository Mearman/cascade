//! CLI implementations for the manager-side management plane: administering a
//! remote node by device id.
//!
//! `cascade remote <device-id> <subcommand>` resolves the target through the
//! configured P2P backend's discovery sources, opens an authenticated session
//! over the connectivity ladder, sends a
//! [`ManageRequest`](cascade_p2p::protocol::BepMessage::ManageRequest), and
//! renders the [`ManageResponse`](cascade_p2p::protocol::BepMessage::ManageResponse).
//!
//! The transport is the P2P backend's own discovery + connection plumbing —
//! [`cascade_backend_p2p::P2pBackend::manage_remote`] — so a manager never opens
//! a parallel transport. A grant the target has not conferred surfaces as a
//! typed authorisation denial from the node, distinct from a transport failure.

use std::path::Path;

use anyhow::{Context as _, Result};
use cascade_backend_p2p::P2pBackend;
use cascade_p2p::protocol::{
    ManageCommand, ManageConfigFormat, ManageErrorKind, ManageGrant, ManageResult, ManageScope,
};

use super::CliContext;
use super::init::CascadeConfig;

/// The wildcard scope token an operator may pass to mean "node-wide" — every
/// path on the node. Mirrors the local `grant add --scope *` spelling so the
/// manager and managed sides accept the same vocabulary.
const SCOPE_WILDCARD: &str = "*";

/// The leading path component used to confine a glob, or root when the glob
/// has no fixed prefix. The managed node re-derives the same prefix from the
/// command payload; advertising it here keeps the wire scope aligned with what
/// the node authorises over, but the node's value is the one that binds.
const ROOT_SCOPE: &str = "/";

/// A remote-administration subcommand, parsed from the clap command tree into
/// the wire [`ManageCommand`] plus the [`ManageScope`] it targets.
///
/// Kept as a CLI-side enum (rather than threading clap's types into the
/// transport) so the mapping from user-facing verbs to wire frames lives in one
/// place. `cache warm` maps to a recursive pin — warming a path is pinning it so
/// the matching files download — mirroring the local `cache warm` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteCommand {
    /// Read the node's status snapshot.
    Status,
    /// Pin a path, keeping matching files offline on the node.
    Pin {
        /// The path to pin.
        path: String,
    },
    /// Remove a pin rule from the node.
    Unpin {
        /// The path whose pin rule to remove.
        path: String,
    },
    /// Run one cache eviction sweep on the node.
    CacheEvict,
    /// Warm a path on the node by pinning it so the files download.
    CacheWarm {
        /// The path to warm.
        path: String,
    },
    /// Push a `.cascade` config fragment to merge into the node's rule set,
    /// rooted at `folder`.
    ConfigPush {
        /// The folder the fragment applies to — the scope the push targets.
        folder: String,
        /// The serialisation format of `body`, derived from the source file's
        /// extension.
        format: ManageConfigFormat,
        /// The raw config fragment.
        body: String,
    },
    /// Set a lifecycle policy on the node over a path glob.
    PolicySet {
        /// The path glob the policy applies to — also the scope it targets.
        path_glob: String,
        /// Maximum file age before eviction, in seconds. Absent leaves the
        /// dimension unbounded.
        max_age_secs: Option<i64>,
        /// Maximum file size before eviction, in bytes. Absent leaves the
        /// dimension unbounded.
        max_file_size: Option<i64>,
        /// Priority — higher wins when policies overlap.
        priority: i32,
    },
    /// Register a backend on the node, mounted at `mount_path`.
    BackendAdd {
        /// The backend name (its identifier and config file stem).
        name: String,
        /// The backend type (`gdrive`, `s3`, `p2p`, …).
        backend_type: String,
        /// The VFS mount path the backend is mounted at — the scope this
        /// command targets.
        mount_path: String,
        /// The backend's TOML config fragment, as a literal TOML document.
        config_toml: String,
    },
    /// Remove a registered backend by name.
    BackendRemove {
        /// The backend name to remove.
        name: String,
        /// The VFS mount path the backend occupied — the scope this command
        /// targets.
        mount_path: String,
    },
    /// Restart the node's background workers, confined to a folder scope.
    Restart {
        /// The folder scope the dangerous `lifecycle:control` capability is
        /// authorised over. A dangerous capability is never satisfied by a
        /// node-wide grant, so this names an explicit folder.
        scope: String,
    },
    /// Stop the node's background workers, confined to a folder scope.
    Stop {
        /// The folder scope the dangerous `lifecycle:control` capability is
        /// authorised over.
        scope: String,
    },
    /// Delegate a grant to another device. Advertises `grant:admin`; the node
    /// enforces the subset rule, refusing any attempt to escalate beyond the
    /// caller's own authority.
    GrantAdd {
        /// The device the grant authorises, by device ID.
        grantee: String,
        /// The capability conferred, in its colon-delimited wire form.
        capability: String,
        /// The scope the capability applies over, as a folder path or the
        /// wildcard token.
        scope: String,
        /// When the grant expires, as an RFC 3339 timestamp. Absent means
        /// never.
        expires: Option<String>,
    },
    /// Revoke a grant by its row id, advertising the folder scope the caller
    /// holds `grant:admin` over. The node re-resolves the revoked grant's real
    /// stored scope and authorises over that, so this advertised scope cannot
    /// widen the revocation.
    GrantRevoke {
        /// The row id of the grant to revoke (as shown by the node's
        /// `grant list`).
        grant_id: i64,
        /// The folder scope the caller's `grant:admin` grant covers.
        scope: String,
    },
    /// Run a one-shot command on the remote node under a PTY. The command's
    /// stdout and stderr are streamed back and written to the terminal; the
    /// CLI exits when the node signals the session ended, propagating the
    /// remote process's exit code as its own exit status. A process killed by
    /// a signal maps to `128 + signal` per the shell convention.
    ///
    /// Requires the dangerous `exec:pty` capability over the session's
    /// working directory, granted explicitly for a folder scope.
    Exec {
        /// The working directory the command runs in — the scope the
        /// `exec:pty` grant covers.
        cwd: Option<String>,
        /// The command argv, passed after `--`.
        argv: Vec<String>,
    },
    /// Open an interactive shell on the remote node under a PTY. Local stdin
    /// is forwarded to the remote shell, output is rendered to the terminal,
    /// and terminal resizes are forwarded.
    ///
    /// Requires the dangerous `exec:pty` capability over the session's
    /// working directory, granted explicitly for a folder scope.
    Shell {
        /// The working directory the shell starts in — the scope the
        /// `exec:pty` grant covers.
        cwd: Option<String>,
        /// The shell to launch (absent uses the node's default).
        shell: Option<String>,
    },
}

impl RemoteCommand {
    /// The wire [`ManageCommand`] this subcommand sends.
    ///
    /// `cache warm` maps to a recursive pin — the same wire command the local
    /// `cache warm` produces — rather than a [`ManageCommand::CacheWarm`], so a
    /// warmed path is kept offline by a pin rule. Every other verb maps to its
    /// matching `ManageCommand` variant one-to-one.
    #[must_use]
    pub fn to_wire(&self) -> ManageCommand {
        match self {
            Self::Status => ManageCommand::StatusRead,
            Self::Pin { path } | Self::CacheWarm { path } => ManageCommand::Pin {
                path_glob: path.clone(),
                recursive: true,
            },
            Self::Unpin { path } => ManageCommand::Unpin {
                path_glob: path.clone(),
            },
            Self::CacheEvict => ManageCommand::CacheEvict,
            Self::ConfigPush {
                folder,
                format,
                body,
            } => ManageCommand::ConfigPush {
                format: *format,
                folder: folder.clone(),
                body: body.clone(),
            },
            Self::PolicySet {
                path_glob,
                max_age_secs,
                max_file_size,
                priority,
            } => ManageCommand::PolicySet {
                path_glob: path_glob.clone(),
                max_age_secs: *max_age_secs,
                max_file_size: *max_file_size,
                priority: *priority,
            },
            Self::BackendAdd {
                name,
                backend_type,
                mount_path,
                config_toml,
            } => ManageCommand::BackendAdd {
                name: name.clone(),
                backend_type: backend_type.clone(),
                mount_path: mount_path.clone(),
                config_toml: config_toml.clone(),
            },
            Self::BackendRemove { name, mount_path } => ManageCommand::BackendRemove {
                name: name.clone(),
                mount_path: mount_path.clone(),
            },
            Self::Restart { .. } => ManageCommand::Restart,
            Self::Stop { .. } => ManageCommand::Stop,
            Self::GrantAdd {
                grantee,
                capability,
                scope,
                expires,
            } => ManageCommand::GrantAdd {
                grant: ManageGrant {
                    grantee: grantee.clone(),
                    capability: capability.clone(),
                    scope: scope_from_arg(scope),
                    expires: expires.clone(),
                },
            },
            Self::GrantRevoke { grant_id, scope } => ManageCommand::GrantRevoke {
                grant_id: *grant_id,
                scope: scope_from_arg(scope),
            },
            // Exec and Shell do not ride the simple `manage_remote` round-trip
            // path: they spawn a PTY session, then pump stdin/stdout/stderr
            // over the exec data plane. Their `to_wire` is the initial
            // `PtySpawn` command; the exec/shell handlers in `run` drive the
            // subsequent PtyWrite/PtyResize/PtyKill verbs and the stream loop
            // directly.
            Self::Exec { cwd, argv } => ManageCommand::PtySpawn {
                shell: None,
                argv: argv.clone(),
                cwd: cwd.clone(),
                env: Vec::new(),
                cols: exec_terminal_cols(),
                rows: exec_terminal_rows(),
            },
            Self::Shell { cwd, shell } => ManageCommand::PtySpawn {
                shell: shell.clone(),
                argv: Vec::new(),
                cwd: cwd.clone(),
                env: Vec::new(),
                cols: exec_terminal_cols(),
                rows: exec_terminal_rows(),
            },
        }
    }

    /// The [`ManageScope`] the request advertises.
    ///
    /// A path-bearing command advertises the path itself as a folder scope; a
    /// node-wide command ([`Self::Status`], [`Self::CacheEvict`]) advertises
    /// [`ManageScope::Node`]. The managed node independently re-derives the
    /// scope the command's payload actually touches and authorises over both,
    /// so a path advertised here cannot widen what the command may do — it is a
    /// best-effort declaration the node cross-checks, not a source of authority.
    ///
    /// The dangerous-capability commands ([`Self::Restart`], [`Self::Stop`],
    /// [`Self::BackendAdd`], [`Self::BackendRemove`], [`Self::GrantAdd`],
    /// [`Self::GrantRevoke`]) advertise an explicit folder scope: a dangerous
    /// capability is never satisfied by a node-wide grant, so a node-wide scope
    /// could never authorise them.
    #[must_use]
    pub fn wire_scope(&self) -> ManageScope {
        match self {
            Self::Status | Self::CacheEvict => ManageScope::Node,
            Self::Pin { path } | Self::Unpin { path } | Self::CacheWarm { path } => {
                ManageScope::Folder { path: path.clone() }
            }
            Self::ConfigPush { folder, .. } => ManageScope::Folder {
                path: folder.clone(),
            },
            Self::PolicySet { path_glob, .. } => ManageScope::Folder {
                path: path_glob.clone(),
            },
            Self::BackendAdd { mount_path, .. } | Self::BackendRemove { mount_path, .. } => {
                ManageScope::Folder {
                    path: mount_path.clone(),
                }
            }
            Self::Restart { scope }
            | Self::Stop { scope }
            | Self::GrantAdd { scope, .. }
            | Self::GrantRevoke { scope, .. } => scope_from_arg(scope),
            // Exec/Shell: the dangerous `exec:pty` capability is never
            // node-wide, so the advertised scope is the explicit `cwd` folder.
            // When `cwd` is absent the node picks its default and authorises
            // over that; advertise the root so a node-wide `exec:pty` grant
            // (if one somehow existed) would not be the only thing matching.
            Self::Exec { cwd, .. } | Self::Shell { cwd, .. } => {
                scope_from_arg(cwd.as_deref().unwrap_or(ROOT_SCOPE))
            }
        }
    }
}

/// Default terminal column count for a PTY spawn when the local terminal size
/// cannot be probed (non-TTY stdout or a platform without a size ioctl).
const DEFAULT_TERMINAL_COLS: u16 = 80;
/// Default terminal row count for a PTY spawn.
const DEFAULT_TERMINAL_ROWS: u16 = 24;

/// Probe the current terminal's column count for a PTY spawn.
fn exec_terminal_cols() -> u16 {
    terminal_size().map_or(DEFAULT_TERMINAL_COLS, |(cols, _)| cols)
}

/// Probe the current terminal's row count for a PTY spawn.
fn exec_terminal_rows() -> u16 {
    terminal_size().map_or(DEFAULT_TERMINAL_ROWS, |(_, rows)| rows)
}

/// Read the terminal size from the controlling terminal, returning `None`
/// when stdout is not a TTY or the platform has no size probe. crossterm wraps
/// the platform ioctl safely, so this replaces the prior fixed 80x24 fallback
/// and lets the spawned PTY open at the real local size.
fn terminal_size() -> Option<(u16, u16)> {
    crossterm::terminal::size().ok()
}

/// Map a `--scope` argument string to a wire [`ManageScope`].
///
/// The wildcard token [`SCOPE_WILDCARD`] maps to [`ManageScope::Node`]; any
/// other value is a folder path prefix. Mirrors the local `grant add` scope
/// parsing so the two sides share one spelling.
#[must_use]
fn scope_from_arg(raw: &str) -> ManageScope {
    if raw == SCOPE_WILDCARD {
        ManageScope::Node
    } else {
        ManageScope::Folder {
            path: raw.to_owned(),
        }
    }
}

/// Derive the [`ManageConfigFormat`] of a `.cascade` fragment from its source
/// file's extension.
///
/// A `.toml`, `.yaml`/`.yml`, or `.json` extension selects the matching
/// structured format; anything else — including a bare `.cascade` file with no
/// extension — is treated as the gitignore-style format, matching the parser's
/// own default for an extensionless `.cascade` file.
#[must_use]
fn config_format_from_path(path: &Path) -> ManageConfigFormat {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("toml") => ManageConfigFormat::Toml,
        Some("yaml" | "yml") => ManageConfigFormat::Yaml,
        Some("json") => ManageConfigFormat::Json,
        _ => ManageConfigFormat::Gitignore,
    }
}

/// Read a `.cascade` config fragment from `path`, returning the body and the
/// format inferred from its extension.
///
/// The file must exist and be readable; a missing or unreadable file fails
/// loudly rather than pushing an empty fragment.
fn read_config_fragment(path: &Path) -> Result<(ManageConfigFormat, String)> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading config fragment {}", path.display()))?;
    Ok((config_format_from_path(path), body))
}

/// Build a [`RemoteCommand::ConfigPush`] from a local fragment file.
///
/// Reads `file` and infers its format from the extension. `scope` is the folder
/// the fragment applies to; when absent it defaults to the node root, matching
/// the engine's rooting of an unscoped fragment.
///
/// # Errors
///
/// Returns an error when the fragment file cannot be read.
pub fn config_push(file: &Path, scope: Option<&str>) -> Result<RemoteCommand> {
    let (format, body) = read_config_fragment(file)?;
    let folder = scope.unwrap_or(ROOT_SCOPE).to_owned();
    Ok(RemoteCommand::ConfigPush {
        folder,
        format,
        body,
    })
}

/// Build a [`RemoteCommand::BackendAdd`] from a local backend config file.
///
/// Reads the backend's TOML config fragment from `config` so the managed node
/// registers it exactly as the local wizard would.
///
/// # Errors
///
/// Returns an error when the config file cannot be read.
pub fn backend_add(
    name: String,
    backend_type: String,
    mount_path: String,
    config: &Path,
) -> Result<RemoteCommand> {
    let config_toml = std::fs::read_to_string(config)
        .with_context(|| format!("reading backend config {}", config.display()))?;
    Ok(RemoteCommand::BackendAdd {
        name,
        backend_type,
        mount_path,
        config_toml,
    })
}

/// Resolve and open the first configured P2P backend, returning the typed
/// [`P2pBackend`] so its manager-side entry point is in reach.
///
/// The management plane rides the P2P transport, so a node with no P2P backend
/// cannot administer a remote node — the command fails loudly rather than
/// inventing a transport.
fn open_p2p_backend(ctx: &CliContext) -> Result<P2pBackend> {
    let main_config_path = ctx.config_dir.join("config.toml");
    if !main_config_path.exists() {
        anyhow::bail!(
            "no config.toml at {} — run `cascade init` before administering a remote node",
            main_config_path.display()
        );
    }
    let raw = std::fs::read_to_string(&main_config_path)
        .with_context(|| format!("reading {}", main_config_path.display()))?;
    let main_config: CascadeConfig =
        toml::from_str(&raw).with_context(|| format!("parsing {}", main_config_path.display()))?;

    let p2p_name = main_config
        .backends
        .iter()
        .find_map(|(name, value)| {
            value
                .get("type")
                .and_then(toml::Value::as_str)
                .filter(|t| *t == "p2p")
                .map(|_| name.clone())
        })
        .context(
            "no P2P backend configured — remote administration rides the P2P transport. \
             Add one with `cascade backend-add p2p`.",
        )?;

    let backend_config_path = ctx.config_dir.join(format!("{p2p_name}.toml"));
    let backend_raw = std::fs::read_to_string(&backend_config_path)
        .with_context(|| format!("reading {}", backend_config_path.display()))?;
    let backend_config: toml::Value = toml::from_str(&backend_raw)
        .with_context(|| format!("parsing {}", backend_config_path.display()))?;
    cascade_backend_p2p::open_from_config(&backend_config)
        .context("opening P2P backend for remote administration")
}

/// `cascade remote <device-id> <subcommand>`.
///
/// Drives the management round-trip: open the P2P backend, send the command to
/// `device_id` over the connectivity ladder, and render the node's reply. An
/// authorisation denial is reported as such and the process exits with a
/// non-zero status via the returned `Err`, distinguishing "the node refused
/// you" from "the command ran and failed".
///
/// `Exec` and `Shell` take a separate path: they spawn a PTY session, then
/// pump stdin/stdout/stderr over the exec data plane until the session exits.
pub async fn run(
    ctx: &CliContext,
    device_id: &str,
    command: RemoteCommand,
    token: Option<String>,
) -> Result<()> {
    if device_id.trim().is_empty() {
        anyhow::bail!("remote requires a non-empty device id");
    }
    let backend = open_p2p_backend(ctx)?;
    match &command {
        RemoteCommand::Exec { .. } | RemoteCommand::Shell { .. } => {
            run_exec(ctx, device_id, command, token).await
        }
        _ => {
            let result = backend
                .manage_remote(device_id, command.to_wire(), command.wire_scope(), token)
                .await
                .with_context(|| format!("administering remote node {device_id}"))?;
            render(device_id, &result)
        }
    }
}

/// `cascade remote <device-id> exec` / `shell`.
///
/// Spawns a PTY session on the remote node, then pumps I/O:
///
/// - The node's stdout/stderr (delivered as exec-stream frames) are written
///   to the local terminal.
/// - For `shell`, local stdin is forwarded to the node as `PtyWrite` commands
///   via a `tokio::select!` loop multiplexed with the output stream.
/// - For `exec` (one-shot), stdin is not forwarded; the command's output is
///   drained and the CLI exits when the stream closes.
///
/// The session is always cleaned up: the consumer is unsubscribed and a
/// `PtyKill` with SIGTERM is sent on exit, so a cancelled CLI does not leave
/// an orphaned session on the node.
async fn run_exec(
    ctx: &CliContext,
    device_id: &str,
    command: RemoteCommand,
    token: Option<String>,
) -> Result<()> {
    let backend = std::sync::Arc::new(open_p2p_backend(ctx)?);
    let is_shell = matches!(command, RemoteCommand::Shell { .. });

    // Send the PtySpawn, learn the session id. Clone the token here so the
    // same credential can be re-presented to the session verbs (PtyWrite on
    // stdin, the cleanup PtyKill) — a token-only caller has no on-node grant,
    // so every follow-up request must carry it.
    let spawn_result = backend
        .manage_remote(
            device_id,
            command.to_wire(),
            command.wire_scope(),
            token.clone(),
        )
        .await
        .with_context(|| format!("spawning exec session on {device_id}"))?;
    let session_id = match spawn_result {
        ManageResult::ExecSpawned { session } => session,
        ManageResult::Ok { summary } => {
            anyhow::bail!("unexpected Ok reply for exec spawn: {summary}");
        }
        ManageResult::Err { kind, message } => {
            return Err(anyhow::anyhow!(
                "exec spawn on {device_id} failed ({kind:?}): {message}"
            ));
        }
    };

    // Register the consumer now that the session id is known.
    let mut stream = backend.subscribe_exec_stream(device_id, session_id).await;

    // Drive the session: multiplex output draining with stdin forwarding
    // (shell mode only) using tokio::select. For a one-shot `exec` the pump
    // returns the remote process's exit code; the CLI propagates it as its own
    // exit status. For an interactive `shell` the code is irrelevant (the
    // session ends on stream close).
    let pump_result = pump_exec_session(
        &backend,
        device_id,
        session_id,
        &mut stream,
        is_shell,
        token.clone(),
    )
    .await;

    // Cleanup: unsubscribe the consumer and best-effort SIGTERM so a cancelled
    // shell does not leave the PTY running on the node. The token is re-presented
    // so a token-authenticated caller's cleanup signal is authorised too.
    backend.unsubscribe_exec_stream(device_id, session_id).await;
    let _ = backend
        .send_pty_signal(device_id, session_id, 15, token)
        .await;

    let exit_code = pump_result?;
    if !is_shell && let Some(code) = exit_code {
        // Propagate the remote process's exit status as this process's own.
        // std::process::exit runs no further destructors, but the session
        // cleanup above already ran and stdout is flushed by the OS on exit.
        std::process::exit(code);
    }
    Ok(())
}

/// Pump the exec session: forward output to the terminal and (for shell mode)
/// forward local stdin to the remote PTY. Returns when the session's
/// [`cascade_p2p::exec_stream::ExecStreamEvent::Exited`] arrives (the process
/// exited) or, for a shell, when stdin reaches EOF.
///
/// Returns `Ok(Some(code))` when a one-shot `exec` received the remote
/// process's exit status (a shell-style code: `code` for a normal exit,
/// `128 + signal` for a signal kill, `1` for an indeterminate exit). Returns
/// `Ok(None)` for an interactive shell — its exit status is not the remote
/// process's.
///
/// `token` is re-presented with every `PtyWrite` so a caller authenticated
/// only by a capability token (no on-node grant) can drive an interactive
/// session.
async fn pump_exec_session(
    backend: &std::sync::Arc<cascade_backend_p2p::P2pBackend>,
    device_id: &str,
    session_id: u64,
    stream: &mut tokio::sync::mpsc::UnboundedReceiver<cascade_p2p::exec_stream::ExecStreamEvent>,
    is_shell: bool,
    token: Option<String>,
) -> Result<Option<i32>> {
    use std::io::IsTerminal as _;
    use std::io::Write as _;
    use tokio::io::AsyncReadExt as _;

    if !is_shell {
        // One-shot exec: drain output to the terminal. The remote process's
        // exit code arrives as an ExecStreamEvent::Exited control frame after
        // the last output frame; the CLI propagates it as its own exit status.
        let stdout = std::io::stdout();
        while let Some(event) = stream.recv().await {
            match event {
                cascade_p2p::exec_stream::ExecStreamEvent::Output(frame) => {
                    let mut handle = stdout.lock();
                    handle.write_all(&frame.bytes)?;
                    handle.flush()?;
                }
                cascade_p2p::exec_stream::ExecStreamEvent::Exited { .. } => {
                    return Ok(event.to_exit_code());
                }
            }
        }
        // The stream closed without an Exited event (the node went away before
        // delivering the exit frame). Treat it as a generic failure.
        return Ok(Some(1));
    }

    // Interactive shell: drive a real raw-mode PTY. The local terminal is put
    // into raw mode so keystrokes (including control bytes such as ^C as 0x03)
    // are forwarded byte-for-byte and the local line discipline does not echo
    // or line-buffer; the remote PTY interprets them. Output is rendered to the
    // terminal and local resizes are forwarded, with raw mode restored on exit.
    // When stdin is not a TTY (piped input) raw mode is skipped and stdin is
    // forwarded as a byte stream instead.
    let raw = if std::io::stdin().is_terminal() {
        crossterm::terminal::enable_raw_mode().context("enabling raw mode for the remote shell")?;
        Some(RawModeGuard)
    } else {
        None
    };

    // Forward local terminal resizes. `terminal::size()` reads the current size
    // via ioctl without contending with the raw-byte stdin reader — unlike
    // crossterm's event loop, which would consume keystrokes meant for the byte
    // forwarder — so a light poll detects a resize shortly after it happens on
    // every platform.
    let mut last_size = terminal_size();
    let mut size_tick = tokio::time::interval(std::time::Duration::from_millis(250));
    // The interval's first tick fires immediately; consume it so we do not echo
    // a redundant initial resize (the spawn already opened at the live size).
    size_tick.tick().await;

    let mut stdin = tokio::io::stdin();
    let mut in_buf = [0u8; 4096];
    let stdout = std::io::stdout();

    loop {
        tokio::select! {
            // Output from the remote session.
            event = stream.recv() => match event {
                Some(cascade_p2p::exec_stream::ExecStreamEvent::Output(frame)) => {
                    let mut handle = stdout.lock();
                    handle.write_all(&frame.bytes)?;
                    handle.flush()?;
                }
                // The remote process exited: end the shell session. The exit
                // code is ignored for the interactive shell — its own exit
                // status is not the remote process's.
                Some(cascade_p2p::exec_stream::ExecStreamEvent::Exited { .. }) | None => break,
            },
            // Local stdin bytes -> remote PtyWrite. In raw mode each keystroke
            // arrives immediately; ^C arrives as 0x03 and the remote PTY raises
            // SIGINT for the child, so interrupt works without local handling.
            n = stdin.read(&mut in_buf) => match n {
                // Local stdin closed, or an unreadable stdin: end the session.
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    if let Some(chunk) = in_buf.get(..count) {
                        backend
                            .send_pty_write(device_id, session_id, chunk.to_vec(), token.clone())
                            .await
                            .context("forwarding stdin to remote PTY")?;
                    }
                }
            },
            // Local terminal resized -> forward the new size.
            _ = size_tick.tick() => {
                if let Some(now) = terminal_size()
                    && Some(now) != last_size
                {
                    last_size = Some(now);
                    let _ = backend
                        .send_pty_resize(device_id, session_id, now.0, now.1, token.clone())
                        .await;
                }
            }
        }
    }

    drop(raw);
    Ok(None)
}

/// Restores the local terminal from raw mode when the shell session ends,
/// including on early returns or errors. crossterm's raw mode is a process-wide
/// terminal attribute, so leaving it enabled would hand the operator back a
/// broken terminal.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Read a capability token's JSON from `path`, validating it parses as a token
/// before it travels — a malformed token file fails loudly here rather than
/// earning an opaque rejection from the remote node.
pub fn read_token_file(path: &std::path::Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading capability token file {}", path.display()))?;
    let _: cascade_engine::manage::token::CapabilityToken = serde_json::from_str(&raw)
        .with_context(|| format!("parsing capability token file {}", path.display()))?;
    Ok(raw)
}

/// Render a [`ManageResult`] to stdout, returning an `Err` for any non-`Ok`
/// outcome so the CLI exits non-zero on a denial or a failed command.
fn render(device_id: &str, result: &ManageResult) -> Result<()> {
    match result {
        ManageResult::Ok { summary } => {
            println!("{summary}");
            Ok(())
        }
        ManageResult::Err {
            kind: ManageErrorKind::Unauthorised,
            message,
        } => anyhow::bail!("node {device_id} refused the command (unauthorised): {message}"),
        ManageResult::Err {
            kind: ManageErrorKind::Failed,
            message,
        } => anyhow::bail!("command on node {device_id} failed: {message}"),
        ManageResult::ExecSpawned { session } => {
            println!("spawned exec session {session}");
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn status_maps_to_status_read_over_node() {
        let cmd = RemoteCommand::Status;
        assert_eq!(cmd.to_wire(), ManageCommand::StatusRead);
        assert_eq!(cmd.wire_scope(), ManageScope::Node);
    }

    #[test]
    fn pin_maps_to_recursive_pin_over_its_folder() {
        let cmd = RemoteCommand::Pin {
            path: "/work/reports".to_owned(),
        };
        assert_eq!(
            cmd.to_wire(),
            ManageCommand::Pin {
                path_glob: "/work/reports".to_owned(),
                recursive: true,
            }
        );
        assert_eq!(
            cmd.wire_scope(),
            ManageScope::Folder {
                path: "/work/reports".to_owned(),
            }
        );
    }

    #[test]
    fn unpin_maps_to_unpin() {
        let cmd = RemoteCommand::Unpin {
            path: "/work".to_owned(),
        };
        assert_eq!(
            cmd.to_wire(),
            ManageCommand::Unpin {
                path_glob: "/work".to_owned(),
            }
        );
    }

    #[test]
    fn cache_evict_maps_to_cache_evict_over_node() {
        let cmd = RemoteCommand::CacheEvict;
        assert_eq!(cmd.to_wire(), ManageCommand::CacheEvict);
        assert_eq!(cmd.wire_scope(), ManageScope::Node);
    }

    #[test]
    fn cache_warm_maps_to_recursive_pin() {
        // Warming a path is pinning it so the files download — the same wire
        // command the local `cache warm` produces.
        let cmd = RemoteCommand::CacheWarm {
            path: "/media".to_owned(),
        };
        assert_eq!(
            cmd.to_wire(),
            ManageCommand::Pin {
                path_glob: "/media".to_owned(),
                recursive: true,
            }
        );
        assert_eq!(
            cmd.wire_scope(),
            ManageScope::Folder {
                path: "/media".to_owned(),
            }
        );
    }

    #[test]
    fn render_ok_prints_summary() {
        let result = ManageResult::Ok {
            summary: "all good".to_owned(),
        };
        assert!(render("PEER", &result).is_ok());
    }

    #[test]
    fn render_unauthorised_is_error() {
        let result = ManageResult::Err {
            kind: ManageErrorKind::Unauthorised,
            message: "no grant".to_owned(),
        };
        let err = render("PEER", &result).unwrap_err();
        assert!(format!("{err:#}").contains("unauthorised"));
    }

    #[test]
    fn render_failed_is_error() {
        let result = ManageResult::Err {
            kind: ManageErrorKind::Failed,
            message: "disk full".to_owned(),
        };
        let err = render("PEER", &result).unwrap_err();
        assert!(format!("{err:#}").contains("failed"));
    }

    #[test]
    fn scope_from_arg_maps_wildcard_to_node() {
        assert_eq!(scope_from_arg("*"), ManageScope::Node);
        assert_eq!(
            scope_from_arg("/work"),
            ManageScope::Folder {
                path: "/work".to_owned(),
            }
        );
    }

    #[test]
    fn config_format_from_path_infers_by_extension() {
        assert_eq!(
            config_format_from_path(Path::new("rules.toml")),
            ManageConfigFormat::Toml
        );
        assert_eq!(
            config_format_from_path(Path::new("rules.yaml")),
            ManageConfigFormat::Yaml
        );
        assert_eq!(
            config_format_from_path(Path::new("rules.yml")),
            ManageConfigFormat::Yaml
        );
        assert_eq!(
            config_format_from_path(Path::new("rules.json")),
            ManageConfigFormat::Json
        );
        // Case-insensitive.
        assert_eq!(
            config_format_from_path(Path::new("RULES.TOML")),
            ManageConfigFormat::Toml
        );
        // An extensionless `.cascade` file — and anything unrecognised — is the
        // gitignore-style default.
        assert_eq!(
            config_format_from_path(Path::new(".cascade")),
            ManageConfigFormat::Gitignore
        );
        assert_eq!(
            config_format_from_path(Path::new("rules.txt")),
            ManageConfigFormat::Gitignore
        );
    }

    #[test]
    fn config_push_reads_body_and_defaults_scope_to_root() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rules.yaml");
        std::fs::write(&file, "ignore:\n  - \"*.tmp\"\n").unwrap();

        let cmd = config_push(&file, None).unwrap();
        assert_eq!(
            cmd,
            RemoteCommand::ConfigPush {
                folder: ROOT_SCOPE.to_owned(),
                format: ManageConfigFormat::Yaml,
                body: "ignore:\n  - \"*.tmp\"\n".to_owned(),
            }
        );
    }

    #[test]
    fn config_push_missing_file_is_error() {
        assert!(config_push(Path::new("/nonexistent/rules.toml"), Some("/work")).is_err());
    }

    #[test]
    fn backend_add_reads_config_toml() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("s3.toml");
        std::fs::write(&config, "type = \"s3\"\n").unwrap();

        let cmd = backend_add(
            "store".to_owned(),
            "s3".to_owned(),
            "/Archive".to_owned(),
            &config,
        )
        .unwrap();
        assert_eq!(
            cmd,
            RemoteCommand::BackendAdd {
                name: "store".to_owned(),
                backend_type: "s3".to_owned(),
                mount_path: "/Archive".to_owned(),
                config_toml: "type = \"s3\"\n".to_owned(),
            }
        );
    }

    #[test]
    fn backend_add_missing_config_is_error() {
        assert!(
            backend_add(
                "store".to_owned(),
                "s3".to_owned(),
                "/Archive".to_owned(),
                Path::new("/nonexistent/s3.toml"),
            )
            .is_err()
        );
    }

    #[test]
    fn exec_maps_argv_to_pty_spawn_over_cwd_scope() {
        let cmd = RemoteCommand::Exec {
            cwd: Some("/work".to_owned()),
            argv: vec!["ls".to_owned(), "-la".to_owned()],
        };
        // Exec maps to a PtySpawn with the argv, the cwd as both the spawn
        // working directory and the authorised scope, no shell override, and
        // a default terminal size.
        match cmd.to_wire() {
            ManageCommand::PtySpawn {
                shell,
                argv,
                cwd,
                env,
                ..
            } => {
                assert!(shell.is_none(), "exec does not override the shell");
                assert_eq!(argv, vec!["ls".to_owned(), "-la".to_owned()]);
                assert_eq!(cwd.as_deref(), Some("/work"));
                assert!(env.is_empty(), "exec carries no extra env");
            }
            other => panic!("exec should map to PtySpawn, got {other:?}"),
        }
        // The dangerous exec:pty capability is never node-wide, so the
        // advertised scope is the explicit cwd folder.
        assert_eq!(
            cmd.wire_scope(),
            ManageScope::Folder {
                path: "/work".to_owned(),
            }
        );
    }

    #[test]
    fn exec_without_cwd_advertises_root_scope() {
        let cmd = RemoteCommand::Exec {
            cwd: None,
            argv: vec!["uptime".to_owned()],
        };
        match cmd.to_wire() {
            ManageCommand::PtySpawn { cwd, .. } => {
                assert!(cwd.is_none());
            }
            other => panic!("exec should map to PtySpawn, got {other:?}"),
        }
        // No cwd given: advertise the root so the node's own scope resolution
        // decides authorisation (the node will use its default cwd).
        assert_eq!(
            cmd.wire_scope(),
            ManageScope::Folder {
                path: ROOT_SCOPE.to_owned()
            }
        );
    }

    #[test]
    fn shell_maps_to_pty_spawn_with_shell_override() {
        let cmd = RemoteCommand::Shell {
            cwd: Some("/home/user".to_owned()),
            shell: Some("bash".to_owned()),
        };
        match cmd.to_wire() {
            ManageCommand::PtySpawn {
                shell,
                argv,
                cwd,
                env,
                ..
            } => {
                assert_eq!(shell.as_deref(), Some("bash"));
                assert!(argv.is_empty(), "shell carries no argv");
                assert_eq!(cwd.as_deref(), Some("/home/user"));
                assert!(env.is_empty());
            }
            other => panic!("shell should map to PtySpawn, got {other:?}"),
        }
        assert_eq!(
            cmd.wire_scope(),
            ManageScope::Folder {
                path: "/home/user".to_owned(),
            }
        );
    }
}
