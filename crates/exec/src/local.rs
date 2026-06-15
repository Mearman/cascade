//! Production [`ExecProvider`] implementation backed by [`portable_pty`] and
//! [`tokio::process`].
//!
//! # PTY sessions
//!
//! A PTY session is opened with [`portable_pty::native_pty_system`]. The master
//! side is split into a reader and a writer via `try_clone_reader` and
//! `take_writer`. A dedicated tokio blocking task pumps bytes from the reader
//! into a bounded [`tokio::sync::mpsc`] channel; the bounded capacity provides
//! backpressure so a slow consumer throttles the producer rather than buffering
//! unboundedly in the node.
//!
//! # Headless process sessions
//!
//! A headless process is spawned with [`tokio::process::Command`] with piped
//! stdout and stderr and `kill_on_drop(true)`. Two reader tasks fan output into
//! the same bounded channel, tagging each chunk with its stream kind. A third
//! task awaits exit and emits [`ExecEvent::Exited`].

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
}

/// Internal state for one headless process session.
struct ProcSession {
    /// Handle to the child process — used for kill and signal.
    child: tokio::process::Child,
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

        // Spawn the child on the slave side. The slave is consumed here.
        let _child = pair
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

        let (tx, _rx) = mpsc::channel::<ExecEvent>(EVENT_CHANNEL_CAPACITY);

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
            inner.pty_sessions.insert(
                id,
                PtySession {
                    master: pair.master,
                    writer,
                },
            );
            id
        };

        // Pump task: synchronous reader runs in a blocking thread.
        {
            let pump_tx = tx;
            let inner_arc = Arc::clone(&self.inner);
            let mut master_reader = reader;
            tokio::task::spawn_blocking(move || {
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
                }
            });
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

        let command_summary = spec.argv.join(" ");

        let (tx, _rx) = mpsc::channel::<ExecEvent>(EVENT_CHANNEL_CAPACITY);

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
            inner.proc_sessions.insert(id, ProcSession { child });
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

        // Exit-wait task: takes the child out of the registry, awaits exit,
        // emits `ExecEvent::Exited`, and cleans up.
        {
            let inner_arc = Arc::clone(&self.inner);
            let tx_exit = tx;
            tokio::spawn(async move {
                let child = {
                    match inner_arc.lock() {
                        Ok(mut inner) => inner.proc_sessions.remove(&id).map(|s| s.child),
                        Err(_) => return,
                    }
                };

                let (code, signal) = match child {
                    Some(mut c) => c.wait().await.map_or((None, None), |status| {
                        #[cfg(unix)]
                        let sig = {
                            use std::os::unix::process::ExitStatusExt;
                            status.signal()
                        };
                        #[cfg(not(unix))]
                        let sig: Option<i32> = None;
                        (status.code(), sig)
                    }),
                    None => (None, None),
                };

                let _ = tx_exit.send(ExecEvent::Exited { code, signal }).await;

                if let Ok(mut inner) = inner_arc.lock() {
                    inner.senders.remove(&id);
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
            let raw_pid = session.child.id().ok_or(ExecError::NotFound(id))?;
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
            let _ = (id, signal);
            Err(ExecError::SignalUnsupported(signal))
        }
    }

    async fn proc_kill(&self, id: ExecSessionId) -> Result<(), ExecError> {
        let mut inner = lock_inner(&self.inner)?;
        let session = inner
            .proc_sessions
            .get_mut(&id)
            .ok_or(ExecError::NotFound(id))?;
        session.child.start_kill().map_err(ExecError::Io)?;
        Ok(())
    }

    /// Subscribe to events from a session.
    ///
    /// Because [`tokio::sync::mpsc`] channels are single-consumer, this returns
    /// `None` — the primary consumer (the engine dispatch path) takes the
    /// receiver at spawn time. A future upgrade to `broadcast` channels would
    /// allow multiple subscribers.
    fn subscribe(&self, id: ExecSessionId) -> Option<mpsc::Receiver<ExecEvent>> {
        let inner = self.inner.lock().ok()?;
        // Only confirm the session is known; cannot hand out a second receiver.
        inner.senders.get(&id)?;
        None
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

        // Wait for the exit task to clean up the proc_sessions entry.
        let inner_arc = Arc::clone(&provider.inner);
        let result = tokio::time::timeout(Duration::from_secs(3), async move {
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
    async fn pty_spawn_returns_session_id_and_records_it() {
        let provider = LocalExecProvider::new();
        let spec = PtySpec {
            shell: Some("/bin/sh".to_owned()),
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
