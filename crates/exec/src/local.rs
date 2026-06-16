//! Production [`ExecProvider`] implementation backed by [`portable_pty`] and
//! [`tokio::process`].
//!
//! # PTY sessions
//!
//! A PTY session is opened with [`portable_pty::native_pty_system`]. The master
//! side is split into a reader and a writer via `try_clone_reader` and
//! `take_writer`. A dedicated, detached [`std::thread`] pumps bytes from the
//! reader into a bounded [`tokio::sync::mpsc`] channel; the bounded capacity
//! provides backpressure so a slow consumer throttles the producer rather than
//! buffering unboundedly in the node. The pump runs on a plain OS thread, not
//! the tokio blocking pool, because the master read blocks for the whole
//! session lifetime: a parked blocking-pool task would stall the runtime's
//! shutdown, whereas a detached thread is simply reclaimed at process exit. The
//! session owns its child and kills it on drop, so the master read sees EOF and
//! the pump thread exits when the session ends.
//!
//! # Headless process sessions
//!
//! A headless process is spawned with [`tokio::process::Command`] with piped
//! stdout and stderr and `kill_on_drop(true)`. Two reader tasks fan output into
//! the same bounded channel, tagging each chunk with its stream kind. A third
//! task awaits exit and emits [`ExecEvent::Exited`]; that task owns the child
//! for `wait`, while the registry keeps the child's pid so kill and signal can
//! reach the process without sharing the `Child` handle.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::mpsc;

use crate::error::ExecError;
use crate::session::{ExecEvent, ExecKind, ExecSessionId, ExecSessionRecord, ExecStreamKind};
use crate::{ExecProvider, ProcSpec, PtySpec};

/// Bounded channel capacity for session output events.
///
/// This is the node-side backpressure bound mandated by `exec-capability.md`:
/// the producer pauses when the consumer falls behind.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Internal state for one PTY session.
struct PtySession {
    /// The portable-pty master — used for resize and process-group lookup.
    master: Box<dyn portable_pty::MasterPty + Send>,
    /// The writable half of the master — used for stdin writes.
    writer: Box<dyn std::io::Write + Send>,
    /// The spawned child running on the slave side.
    ///
    /// Retained so the session owns the child's lifetime: dropping the session
    /// kills the child (see the [`Drop`] impl), which closes the slave and so
    /// returns EOF to the master-reader pump's blocking `read`, letting that
    /// thread exit. Without this the child would outlive the session, hold the
    /// PTY open, and park the pump thread forever.
    child: Box<dyn portable_pty::Child + Send>,
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Best-effort kill: if the child has already exited, `kill` returns an
        // error we can ignore — the goal is only to guarantee the slave end is
        // closed so the master-reader pump observes EOF and unparks.
        let _ = self.child.kill();
    }
}

/// Internal state for one headless process session.
///
/// The [`tokio::process::Child`] itself is owned by the session's exit-wait
/// task, which must hold `&mut Child` to call [`tokio::process::Child::wait`].
/// A second task cannot share that handle, so the kill/signal path does not go
/// through the `Child`: it signals the recorded process id directly (Unix) or
/// asks the exit-wait task to kill via [`ProcSession::kill_request`] (other
/// platforms). The registry entry is kept until exit is observed, so a kill or
/// signal arriving after the child has already self-reaped still resolves to a
/// live session rather than spuriously reporting it gone.
struct ProcSession {
    /// The child's process id, captured at spawn for signalling on Unix.
    ///
    /// `None` only if the child had already exited and been reaped before its
    /// id could be read — vanishingly rare, and handled as "nothing to signal".
    #[cfg_attr(not(unix), allow(dead_code))]
    pid: Option<u32>,
    /// A one-shot the exit-wait task awaits; firing it asks that task to kill
    /// the child it owns. Used on platforms without PID-based signalling.
    ///
    /// `Some` until consumed by the first kill request; subsequent kills on the
    /// same session are a no-op (the child is already being torn down).
    #[cfg_attr(unix, allow(dead_code))]
    kill_request: Option<tokio::sync::oneshot::Sender<()>>,
}

/// Shared state: the registry of live sessions and subscriber channels.
struct Inner {
    /// Monotonic counter for session ids. Bumped by [`Inner::alloc_id`].
    next_id: u64,
    /// Live PTY sessions.
    pty_sessions: HashMap<ExecSessionId, PtySession>,
    /// Live headless process sessions.
    proc_sessions: HashMap<ExecSessionId, ProcSession>,
    /// Sender halves for live sessions; used to check session liveness in
    /// [`ExecProvider::subscribe`].
    senders: HashMap<ExecSessionId, mpsc::Sender<ExecEvent>>,
    /// Output receivers parked at spawn time, awaiting their single consumer.
    ///
    /// The output channel is single-consumer (`tokio::sync::mpsc`): a session's
    /// receiver is created at spawn and parked here until [`ExecProvider::subscribe`]
    /// hands it to the data-plane pump. Taken at most once per session — a second
    /// subscribe returns `None`.
    pending_receivers: HashMap<ExecSessionId, mpsc::Receiver<ExecEvent>>,
    /// Records of all sessions (live and recently exited) for `list_sessions`.
    all_records: Vec<ExecSessionRecord>,
}

impl Inner {
    /// Allocate the next session id.
    const fn alloc_id(&mut self) -> ExecSessionId {
        let id = self.next_id;
        self.next_id += 1;
        ExecSessionId(id)
    }
}

/// The production [`ExecProvider`] implementation.
///
/// All state lives behind a shared `Arc<Mutex<Inner>>`. The mutex is held only
/// for brief bookkeeping; all blocking I/O runs in spawned tokio tasks.
pub struct LocalExecProvider {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for LocalExecProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalExecProvider").finish_non_exhaustive()
    }
}

impl LocalExecProvider {
    /// Construct a new provider with no sessions.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                next_id: 0,
                pty_sessions: HashMap::new(),
                proc_sessions: HashMap::new(),
                senders: HashMap::new(),
                pending_receivers: HashMap::new(),
                all_records: Vec::new(),
            })),
        }
    }
}

impl Default for LocalExecProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Lock the inner state, mapping a poisoned lock to an I/O error.
fn lock_inner(inner: &Arc<Mutex<Inner>>) -> Result<std::sync::MutexGuard<'_, Inner>, ExecError> {
    inner
        .lock()
        .map_err(|_| ExecError::Io(std::io::Error::other("exec state lock poisoned")))
}

/// Copy `n` bytes from `buf` into an owned `Vec<u8>`, using `.get(..n)` to
/// satisfy the `indexing_slicing` lint (which bans `buf[..n]`).
fn copy_bytes(buf: &[u8], n: usize) -> Vec<u8> {
    buf.get(..n).map_or_else(Vec::new, <[u8]>::to_vec)
}

#[async_trait]
impl ExecProvider for LocalExecProvider {
    async fn pty_spawn(&self, spec: PtySpec) -> Result<ExecSessionId, ExecError> {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};

        // Resolve the shell binary.
        let shell = spec
            .shell
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/sh".to_owned());

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: spec.rows,
                cols: spec.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| ExecError::Spawn(anyhow::anyhow!("{e}")))?;

        let mut cmd = CommandBuilder::new(&shell);
        for arg in &spec.argv {
            cmd.arg(arg);
        }
        if let Some(ref cwd) = spec.cwd {
            cmd.cwd(cwd);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        // Spawn the child on the slave side. The slave is consumed here. The
        // child handle is retained in the session (below) so the session owns
        // its lifetime and tears it down on drop.
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| ExecError::Spawn(anyhow::anyhow!("{e}")))?;

        // Split the master: writer for stdin/resize, reader for the pump task.
        // `take_writer` must be called before `try_clone_reader` on some
        // platforms because it consumes the master fd.
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| ExecError::Spawn(anyhow::anyhow!("{e}")))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| ExecError::Spawn(anyhow::anyhow!("{e}")))?;

        let args = spec.argv.join(" ");
        let command_summary = if args.is_empty() {
            shell
        } else {
            format!("{shell} {args}")
        };

        let (tx, rx) = mpsc::channel::<ExecEvent>(EVENT_CHANNEL_CAPACITY);

        let id = {
            let mut inner = lock_inner(&self.inner)?;
            let id = inner.alloc_id();
            let rec = ExecSessionRecord {
                id,
                kind: ExecKind::Pty,
                command_summary,
                started: Utc::now(),
            };
            inner.all_records.push(rec);
            inner.senders.insert(id, tx.clone());
            inner.pending_receivers.insert(id, rx);
            inner.pty_sessions.insert(
                id,
                PtySession {
                    master: pair.master,
                    writer,
                    child,
                },
            );
            id
        };

        // Pump thread: the synchronous master read is an unbounded-lifetime
        // blocking call (it returns only when the PTY closes), so it runs on a
        // dedicated, detached `std::thread` rather than the tokio blocking
        // pool. A blocking-pool task that is still parked in `read` when the
        // runtime shuts down would make `BlockingPool::shutdown` wait for it
        // forever; a detached OS thread is simply reclaimed at process exit and
        // cannot stall runtime teardown. `blocking_send` works from any thread,
        // so the channel still carries backpressure into the pump.
        {
            let pump_tx = tx;
            let inner_arc = Arc::clone(&self.inner);
            let mut master_reader = reader;
            std::thread::Builder::new()
                .name(format!("cascade-exec-pty-{}", id.0))
                .spawn(move || {
                    let mut buf = [0u8; 4096];
                    loop {
                        match std::io::Read::read(&mut *master_reader, &mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let bytes = copy_bytes(&buf, n);
                                if pump_tx
                                    .blocking_send(ExecEvent::Output {
                                        stream: ExecStreamKind::Stdout,
                                        bytes,
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    if let Ok(mut inner) = inner_arc.lock() {
                        inner.pty_sessions.remove(&id);
                        inner.senders.remove(&id);
                        // The parked output receiver is deliberately left in
                        // place: a consumer that subscribes after the session
                        // ends must still receive the buffered output. It is
                        // reclaimed when subscribed or when the provider is
                        // dropped.
                    }
                })
                .map_err(ExecError::Io)?;
        }

        tracing::debug!(target: "cascade::exec", session = id.0, "PTY session spawned");
        Ok(id)
    }

    async fn pty_write(&self, id: ExecSessionId, bytes: &[u8]) -> Result<(), ExecError> {
        let mut inner = lock_inner(&self.inner)?;
        let session = inner
            .pty_sessions
            .get_mut(&id)
            .ok_or(ExecError::NotFound(id))?;
        std::io::Write::write_all(&mut session.writer, bytes)?;
        Ok(())
    }

    async fn pty_resize(&self, id: ExecSessionId, cols: u16, rows: u16) -> Result<(), ExecError> {
        use portable_pty::PtySize;
        let inner = lock_inner(&self.inner)?;
        let session = inner.pty_sessions.get(&id).ok_or(ExecError::NotFound(id))?;
        session
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| ExecError::Io(std::io::Error::other(e.to_string())))?;
        Ok(())
    }

    async fn pty_kill(&self, id: ExecSessionId, signal: i32) -> Result<(), ExecError> {
        #[cfg(unix)]
        {
            use nix::sys::signal::{Signal, kill};
            use nix::unistd::Pid;

            let inner = lock_inner(&self.inner)?;
            let session = inner.pty_sessions.get(&id).ok_or(ExecError::NotFound(id))?;
            let raw_pid = session
                .master
                .process_group_leader()
                .ok_or(ExecError::SignalUnsupported(signal))?;
            let sig = Signal::try_from(signal).map_err(|_| ExecError::SignalUnsupported(signal))?;
            kill(Pid::from_raw(raw_pid), sig)
                .map_err(|e| ExecError::Io(std::io::Error::from_raw_os_error(e as i32)))?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = id;
            Err(ExecError::SignalUnsupported(signal))
        }
    }

    async fn proc_spawn(&self, spec: ProcSpec) -> Result<ExecSessionId, ExecError> {
        use tokio::io::AsyncReadExt;
        use tokio::process::Command;

        let mut argv_iter = spec.argv.iter();
        let bin = argv_iter
            .next()
            .ok_or_else(|| ExecError::Spawn(anyhow::anyhow!("proc argv must not be empty")))?;

        let mut cmd = Command::new(bin);
        cmd.args(argv_iter);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.stdin(std::process::Stdio::null());
        cmd.kill_on_drop(true);
        if let Some(ref cwd) = spec.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(|e| ExecError::Spawn(e.into()))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ExecError::Spawn(anyhow::anyhow!("stdout was not piped")))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ExecError::Spawn(anyhow::anyhow!("stderr was not piped")))?;

        // Capture the process id before the child is moved into the exit-wait
        // task, so the kill/signal path can reach the process without holding
        // the `Child` (which the waiter owns for `wait()`).
        let pid = child.id();

        let command_summary = spec.argv.join(" ");

        let (tx, rx) = mpsc::channel::<ExecEvent>(EVENT_CHANNEL_CAPACITY);
        let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();

        let id = {
            let mut inner = lock_inner(&self.inner)?;
            let id = inner.alloc_id();
            let rec = ExecSessionRecord {
                id,
                kind: ExecKind::Proc,
                command_summary,
                started: Utc::now(),
            };
            inner.all_records.push(rec);
            inner.senders.insert(id, tx.clone());
            inner.pending_receivers.insert(id, rx);
            inner.proc_sessions.insert(
                id,
                ProcSession {
                    pid,
                    kill_request: Some(kill_tx),
                },
            );
            id
        };

        // Stdout pump task.
        {
            let tx_out = tx.clone();
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stdout);
                let mut buf = vec![0u8; 4096];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let bytes = copy_bytes(&buf, n);
                            if tx_out
                                .send(ExecEvent::Output {
                                    stream: ExecStreamKind::Stdout,
                                    bytes,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            });
        }

        // Stderr pump task.
        {
            let tx_err = tx.clone();
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stderr);
                let mut buf = vec![0u8; 4096];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let bytes = copy_bytes(&buf, n);
                            if tx_err
                                .send(ExecEvent::Output {
                                    stream: ExecStreamKind::Stderr,
                                    bytes,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            });
        }

        // Exit-wait task: owns the child for the whole session, awaits exit
        // (racing a kill request from a non-Unix `proc_kill`), emits
        // `ExecEvent::Exited`, then removes the session from the registry.
        //
        // The child is owned here, never parked in the registry, because
        // `Child::wait` needs `&mut Child` and a concurrent kill/signal cannot
        // share that borrow. The registry instead holds the pid (Unix
        // signalling) and the kill-request sender (non-Unix kill), and the
        // session entry survives until `wait` returns — so a kill or signal
        // that arrives after the child self-reaps still finds a live session
        // and resolves cleanly rather than racing the entry's removal.
        {
            let inner_arc = Arc::clone(&self.inner);
            let tx_exit = tx;
            tokio::spawn(async move {
                let mut child = child;
                let status = tokio::select! {
                    status = child.wait() => status,
                    // A kill request fires only on platforms without PID-based
                    // signalling; on Unix `kill_rx`'s sender is never used and
                    // this branch resolves to `Err` (sender dropped) only once
                    // the session is being torn down, after which `wait`
                    // dominates. `biased` is unnecessary: both arms converge on
                    // awaiting the child's exit.
                    _ = kill_rx => {
                        let _ = child.start_kill();
                        child.wait().await
                    }
                };

                let (code, signal) = status.map_or((None, None), |status| {
                    #[cfg(unix)]
                    let sig = {
                        use std::os::unix::process::ExitStatusExt;
                        status.signal()
                    };
                    #[cfg(not(unix))]
                    let sig: Option<i32> = None;
                    (status.code(), sig)
                });

                let _ = tx_exit.send(ExecEvent::Exited { code, signal }).await;

                if let Ok(mut inner) = inner_arc.lock() {
                    inner.proc_sessions.remove(&id);
                    inner.senders.remove(&id);
                    // The parked output receiver is deliberately left in place:
                    // a consumer that subscribes after the process exits must
                    // still drain its buffered output. It is reclaimed when
                    // subscribed or when the provider is dropped.
                }
            });
        }

        tracing::debug!(target: "cascade::exec", session = id.0, "process session spawned");
        Ok(id)
    }

    async fn proc_signal(&self, id: ExecSessionId, signal: i32) -> Result<(), ExecError> {
        #[cfg(unix)]
        {
            use nix::sys::signal::{Signal, kill};
            use nix::unistd::Pid;

            let inner = lock_inner(&self.inner)?;
            let session = inner
                .proc_sessions
                .get(&id)
                .ok_or(ExecError::NotFound(id))?;
            let raw_pid = session.pid.ok_or(ExecError::NotFound(id))?;
            let pid = Pid::from_raw(
                i32::try_from(raw_pid).map_err(|_| ExecError::SignalUnsupported(signal))?,
            );
            let sig = Signal::try_from(signal).map_err(|_| ExecError::SignalUnsupported(signal))?;
            kill(pid, sig)
                .map_err(|e| ExecError::Io(std::io::Error::from_raw_os_error(e as i32)))?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = id;
            Err(ExecError::SignalUnsupported(signal))
        }
    }

    async fn proc_kill(&self, id: ExecSessionId) -> Result<(), ExecError> {
        // The session entry lives until its exit-wait task observes exit, so a
        // kill that lands after the child has self-reaped still finds it. On
        // Unix the kill is a SIGKILL to the recorded pid; elsewhere it hands a
        // kill request to the exit-wait task, which owns the child handle. The
        // task then reaps the child and removes the entry.
        #[cfg(unix)]
        {
            use nix::errno::Errno;
            use nix::sys::signal::{Signal, kill};
            use nix::unistd::Pid;

            let inner = lock_inner(&self.inner)?;
            let session = inner
                .proc_sessions
                .get(&id)
                .ok_or(ExecError::NotFound(id))?;
            let Some(raw_pid) = session.pid else {
                // No pid was ever captured: the child exited before its id
                // could be read. Nothing to kill; the exit-wait task will
                // remove the entry. Treat as success — the process is gone.
                return Ok(());
            };
            let pid =
                Pid::from_raw(i32::try_from(raw_pid).map_err(|_| {
                    ExecError::Io(std::io::Error::other("pid does not fit in i32"))
                })?);
            match kill(pid, Signal::SIGKILL) {
                // `ESRCH` means the child self-reaped between the lookup and the
                // signal: the pid is no longer a live process, so the teardown
                // the caller asked for has already happened. Both that and a
                // successful kill are success.
                Ok(()) | Err(Errno::ESRCH) => Ok(()),
                Err(e) => Err(ExecError::Io(std::io::Error::from_raw_os_error(e as i32))),
            }
        }
        #[cfg(not(unix))]
        {
            let mut inner = lock_inner(&self.inner)?;
            let session = inner
                .proc_sessions
                .get_mut(&id)
                .ok_or(ExecError::NotFound(id))?;
            // Fire the kill request once; a second kill is a no-op because the
            // child is already being torn down by the exit-wait task.
            if let Some(kill_tx) = session.kill_request.take() {
                let _ = kill_tx.send(());
            }
            Ok(())
        }
    }

    /// Subscribe to events from a session, taking ownership of its output
    /// receiver.
    ///
    /// The output channel is single-consumer ([`tokio::sync::mpsc`]): the
    /// receiver is parked at spawn time and handed to the first caller — the
    /// data-plane pump that forwards `stdout`/`stderr` to a peer. A second
    /// subscribe for the same session returns `None`, as does a subscribe for
    /// an unknown session.
    fn subscribe(&self, id: ExecSessionId) -> Option<mpsc::Receiver<ExecEvent>> {
        let mut inner = self.inner.lock().ok()?;
        inner.pending_receivers.remove(&id)
    }

    fn list_sessions(&self) -> Vec<ExecSessionRecord> {
        self.inner
            .lock()
            .map(|inner| inner.all_records.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[tokio::test]
    async fn proc_spawn_echo_records_session() {
        let provider = LocalExecProvider::new();
        let spec = ProcSpec {
            argv: vec!["echo".to_owned(), "hello exec".to_owned()],
            cwd: None,
            env: vec![],
        };
        let id = provider.proc_spawn(spec).await.unwrap();
        assert_eq!(id.0, 0);

        // Allow exit task to run.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let records = provider.list_sessions();
        assert!(!records.is_empty());
        let rec = records.iter().find(|r| r.id == id).unwrap();
        assert_eq!(rec.kind, ExecKind::Proc);
    }

    #[tokio::test]
    async fn proc_spawn_assigns_monotonic_ids() {
        let provider = LocalExecProvider::new();
        let id0 = provider
            .proc_spawn(ProcSpec {
                argv: vec!["true".to_owned()],
                cwd: None,
                env: vec![],
            })
            .await
            .unwrap();
        let id1 = provider
            .proc_spawn(ProcSpec {
                argv: vec!["true".to_owned()],
                cwd: None,
                env: vec![],
            })
            .await
            .unwrap();
        assert!(id1.0 > id0.0);
    }

    #[tokio::test]
    async fn proc_kill_stops_long_running_process() {
        let provider = LocalExecProvider::new();
        let id = provider
            .proc_spawn(ProcSpec {
                argv: vec!["sleep".to_owned(), "60".to_owned()],
                cwd: None,
                env: vec![],
            })
            .await
            .unwrap();

        provider.proc_kill(id).await.unwrap();

        // Wait for the exit task to clean up the proc_sessions entry. Generous
        // headroom for slow/loaded CI runners; a true failure to clean up still
        // trips this, just later.
        let inner_arc = Arc::clone(&provider.inner);
        let result = tokio::time::timeout(Duration::from_secs(30), async move {
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let inner = inner_arc.lock().unwrap();
                if !inner.proc_sessions.contains_key(&id) {
                    break;
                }
            }
        })
        .await;
        assert!(result.is_ok(), "process should exit within 3 s");
    }

    #[tokio::test]
    async fn proc_kill_unknown_id_returns_not_found() {
        let provider = LocalExecProvider::new();
        let err = provider.proc_kill(ExecSessionId(9999)).await.unwrap_err();
        assert!(matches!(err, ExecError::NotFound(_)));
    }

    #[test]
    fn list_sessions_empty_on_new_provider() {
        let provider = LocalExecProvider::new();
        assert!(provider.list_sessions().is_empty());
    }

    #[tokio::test]
    async fn subscribe_yields_process_output_then_exit() {
        let provider = LocalExecProvider::new();
        let id = provider
            .proc_spawn(ProcSpec {
                argv: vec!["echo".to_owned(), "subscribed".to_owned()],
                cwd: None,
                env: vec![],
            })
            .await
            .unwrap();

        let mut rx = provider
            .subscribe(id)
            .expect("first subscribe yields receiver");

        let mut collected = Vec::new();
        let mut exited = false;
        // Generous headroom: this drains in milliseconds locally, but a loaded
        // CI runner (Windows especially) can stall the child's spawn/exit
        // delivery for seconds. A true hang still fails, just later.
        let drain = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(event) = rx.recv().await {
                match event {
                    ExecEvent::Output { bytes, .. } => collected.extend_from_slice(&bytes),
                    ExecEvent::Exited { .. } => {
                        exited = true;
                        break;
                    }
                }
            }
        })
        .await;
        assert!(drain.is_ok(), "draining should not time out");
        assert!(exited, "an Exited event must arrive");
        assert!(
            collected
                .windows(b"subscribed".len())
                .any(|w| w == b"subscribed"),
            "stdout should carry the echoed text, got {collected:?}"
        );
    }

    #[tokio::test]
    async fn second_subscribe_returns_none() {
        let provider = LocalExecProvider::new();
        let id = provider
            .proc_spawn(ProcSpec {
                argv: vec!["true".to_owned()],
                cwd: None,
                env: vec![],
            })
            .await
            .unwrap();
        assert!(
            provider.subscribe(id).is_some(),
            "first subscribe yields the receiver"
        );
        assert!(
            provider.subscribe(id).is_none(),
            "second subscribe must not hand out a second receiver"
        );
    }

    #[tokio::test]
    async fn subscribe_unknown_session_returns_none() {
        let provider = LocalExecProvider::new();
        assert!(provider.subscribe(ExecSessionId(4242)).is_none());
    }

    #[tokio::test]
    async fn pty_spawn_returns_session_id_and_records_it() {
        let provider = LocalExecProvider::new();
        // `/bin/sh` is an absolute path that does not exist on Windows; use the
        // always-present `cmd` shell there so the PTY actually spawns.
        let shell = if cfg!(windows) { "cmd" } else { "/bin/sh" };
        let spec = PtySpec {
            shell: Some(shell.to_owned()),
            argv: vec![],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
        };
        let id = provider.pty_spawn(spec).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let records = provider.list_sessions();
        assert!(
            records
                .iter()
                .any(|r| r.id == id && r.kind == ExecKind::Pty)
        );
    }
}
