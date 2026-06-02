//! Relay server entry point.
//!
//! Accepts incoming `WebSocket` connections on the configured bind address,
//! extracts the session ID from the URL path (`/join/<session_id>`), runs
//! the `HMAC` handshake, and either parks the client awaiting a peer or
//! pairs it with an already-parked peer and hands both sockets to the
//! byte-pipe.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use http::{Response, StatusCode};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request};
use tracing::{info, warn};

use crate::auth::{HandshakeError, verify_handshake};
use crate::config::{RelayConfig, SHARED_SECRET_LEN};
use crate::metrics::Counters;
use crate::session::{RegisterOutcome, SessionRegistry};

/// Path prefix the relay accepts. Mirrors the existing client in
/// [`cascade-p2p`](../../../p2p/src/relay.rs).
pub const RELAY_JOIN_PATH: &str = "/join/";

/// Handle returned by [`spawn`]. Drop to stop the server.
#[derive(Debug)]
pub struct RelayHandle {
    /// Local address the relay is bound to.
    pub local_addr: SocketAddr,
    /// Optional local address the metrics endpoint is bound to.
    pub metrics_addr: Option<SocketAddr>,
    /// Shared counters — useful for tests asserting against metric values.
    pub counters: Arc<Counters>,
    /// Channel that, when signalled (sender dropped), instructs the server
    /// loop to terminate at the next accept boundary.
    _shutdown: mpsc::Sender<()>,
    listener_task: tokio::task::JoinHandle<()>,
    metrics_task: Option<tokio::task::JoinHandle<()>>,
    /// Kept alive so the listener task's reference to it stays valid;
    /// access goes through cloned references inside the listener.
    _registry: SessionRegistry,
}

impl RelayHandle {
    /// Block until the listener task exits. Mainly useful for the binary
    /// entry point — tests drop the handle and rely on `Drop` cleanup.
    pub async fn join(self) -> Result<()> {
        self.listener_task
            .await
            .context("relay listener task panicked")?;
        if let Some(metrics) = self.metrics_task {
            metrics.await.context("metrics task panicked")?;
        }
        Ok(())
    }
}

/// Run the relay until the returned handle is dropped or `join`'d.
pub async fn spawn(config: RelayConfig) -> Result<RelayHandle> {
    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("binding relay listener to {}", config.bind))?;
    let local_addr = listener
        .local_addr()
        .context("reading local address for bound relay listener")?;

    let registry = SessionRegistry::new();
    let counters = Counters::new();

    let (metrics_addr, metrics_task) = spawn_metrics_endpoint(&config, counters.clone()).await?;

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    let listener_registry = registry.clone();
    let listener_counters = counters.clone();
    let listener_config = config;

    let listener_task = tokio::spawn(async move {
        run_listener(
            listener,
            listener_config,
            listener_registry,
            listener_counters,
            &mut shutdown_rx,
        )
        .await;
    });

    info!(addr = %local_addr, metrics = ?metrics_addr, "cascade-relay listening");

    Ok(RelayHandle {
        local_addr,
        metrics_addr,
        counters,
        _shutdown: shutdown_tx,
        listener_task,
        metrics_task,
        _registry: registry,
    })
}

/// Convenience wrapper used by `main` — runs to completion.
pub async fn run_relay(config: RelayConfig) -> Result<()> {
    let handle = spawn(config).await?;
    handle.join().await
}

#[cfg(feature = "metrics")]
async fn spawn_metrics_endpoint(
    config: &RelayConfig,
    counters: Arc<Counters>,
) -> Result<(Option<SocketAddr>, Option<tokio::task::JoinHandle<()>>)> {
    if let Some(bind) = config.metrics_bind {
        let (addr, handle) = crate::metrics::serve_metrics(bind, counters).await?;
        Ok((Some(addr), Some(handle)))
    } else {
        Ok((None, None))
    }
}

// The `async` keyword is required here so this stub has the same signature as
// the `cfg(feature = "metrics")` variant above — the call site awaits the
// return value unconditionally regardless of which variant is selected.
#[cfg(not(feature = "metrics"))]
#[allow(clippy::unused_async)]
async fn spawn_metrics_endpoint(
    config: &RelayConfig,
    _counters: Arc<Counters>,
) -> Result<(Option<SocketAddr>, Option<tokio::task::JoinHandle<()>>)> {
    if config.metrics_bind.is_some() {
        warn!("metrics_bind set but cascade-relay-server was built without the metrics feature");
    }
    Ok((None, None))
}

async fn run_listener(
    listener: TcpListener,
    config: RelayConfig,
    registry: SessionRegistry,
    counters: Arc<Counters>,
    shutdown: &mut mpsc::Receiver<()>,
) {
    let reaper_registry = registry.clone();
    let reaper_counters = counters.clone();
    let reaper_interval = std::cmp::max(Duration::from_millis(250), config.session_timeout / 4);
    let reaper = tokio::spawn(async move {
        loop {
            tokio::time::sleep(reaper_interval).await;
            let reaped = reaper_registry.reap_expired().await;
            if reaped > 0 {
                reaper_counters
                    .sessions_timed_out_total
                    .fetch_add(reaped as u64, Ordering::Relaxed);
            }
        }
    });

    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                info!("relay shutdown signal received");
                break;
            }
            accept = listener.accept() => {
                match accept {
                    Ok((stream, peer)) => {
                        let registry = registry.clone();
                        let counters = counters.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_connection(stream, peer, &config, &registry, &counters).await {
                                warn!(error = %err, %peer, "relay connection ended with error");
                            }
                        });
                    }
                    Err(err) => {
                        warn!(error = %err, "accept failed");
                    }
                }
            }
        }
    }

    registry.shutdown().await;
    reaper.abort();
}

#[allow(clippy::result_large_err)]
// The `accept_hdr_async` callback is required to return
// `Result<Response<()>, ErrorResponse>` by the tungstenite API. The `Err`
// variant is a tungstenite `ErrorResponse` (an alias for
// `http::Response<Option<String>>`) whose size we cannot reduce without
// changing the third-party trait bound.
async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    config: &RelayConfig,
    registry: &SessionRegistry,
    counters: &Arc<Counters>,
) -> Result<()> {
    let mut path_holder: Option<String> = None;
    let path_ref = &mut path_holder;

    let ws = accept_hdr_async(stream, |req: &Request, response| {
        let path = req.uri().path().to_owned();
        parse_session_path(&path).map_or_else(
            || {
                let err_response = Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Some("relay session path must be /join/<session_id>".into()))
                    .unwrap_or_else(|_| {
                        let mut empty = Response::new(None);
                        *empty.status_mut() = StatusCode::NOT_FOUND;
                        empty
                    });
                Err(ErrorResponse::from(err_response))
            },
            |session| {
                *path_ref = Some(session.to_owned());
                Ok(response)
            },
        )
    })
    .await
    .with_context(|| format!("WebSocket handshake from {peer}"))?;

    let Some(session_id) = path_holder else {
        anyhow::bail!("WebSocket handshake succeeded without a session path");
    };

    info!(%peer, %session_id, "client connected");

    let outcome = run_session(ws, &session_id, config, registry, counters).await;
    if let Err(ref err) = outcome {
        warn!(%peer, %session_id, error = %err, "session ended with error");
    } else {
        info!(%peer, %session_id, "session ended");
    }
    outcome
}

async fn run_session(
    mut ws: WebSocketStream<TcpStream>,
    session_id: &str,
    config: &RelayConfig,
    registry: &SessionRegistry,
    counters: &Arc<Counters>,
) -> Result<()> {
    // First binary frame is the handshake.
    let handshake_frame = match ws.next().await {
        Some(Ok(Message::Binary(payload))) => payload,
        Some(Ok(_)) => {
            counters.auth_failures_total.fetch_add(1, Ordering::Relaxed);
            let _ = ws.send(Message::Close(None)).await;
            anyhow::bail!("first frame must be binary handshake");
        }
        Some(Err(err)) => {
            counters.auth_failures_total.fetch_add(1, Ordering::Relaxed);
            return Err(err).context("reading handshake frame");
        }
        None => {
            counters.auth_failures_total.fetch_add(1, Ordering::Relaxed);
            anyhow::bail!("client closed before sending handshake");
        }
    };

    let verified = match verify_handshake(&handshake_frame, session_id, &config.shared_secret) {
        Ok(v) => v,
        Err(err) => {
            counters.auth_failures_total.fetch_add(1, Ordering::Relaxed);
            let _ = ws.send(Message::Close(None)).await;
            return Err(handshake_to_anyhow(err));
        }
    };

    info!(device_id = %verified.device_id, session_id = %verified.session_id, "client authenticated");

    // Register against the session registry — either pair immediately or
    // park awaiting a peer.
    let outcome = registry
        .register(session_id, config.session_timeout, config.max_sessions)
        .await;

    match outcome {
        RegisterOutcome::AtCapacity => {
            counters
                .sessions_rejected_total
                .fetch_add(1, Ordering::Relaxed);
            warn!(%session_id, "rejecting session: registry at capacity");
            let _ = ws.send(Message::Close(None)).await;
            anyhow::bail!("relay at session capacity");
        }
        RegisterOutcome::Paired => {
            // We are the second peer to arrive — but we don't yet have the
            // first peer's `WebSocketStream`. The first peer's session-runner
            // is parked in the `pair` channel waiting for us. We use a tiny
            // companion mechanism in the registry: instead of duplicating
            // the channel plumbing, we re-park ourselves, send a "second"
            // marker through the channel, and let a coordinator pair us.
            //
            // The implementation uses the registry only as a rendezvous
            // signal — the actual byte-pipe is established here, by passing
            // the second peer's `WebSocketStream` over a side channel back
            // to the first parked peer's task.
            //
            // To keep the surface area small, the registry simply notifies
            // the parked peer with `Paired`; the parked peer then awaits a
            // second handoff on a fresh channel inserted by us. See
            // `pairing` module.
            pairing::pair_into(session_id, ws, counters).await
        }
        RegisterOutcome::Parked { wait_for_peer, .. } => {
            pairing::wait_for_peer(session_id, ws, wait_for_peer, registry, counters).await
        }
    }
}

fn handshake_to_anyhow(err: HandshakeError) -> anyhow::Error {
    anyhow::anyhow!(err).context("handshake verification failed")
}

fn parse_session_path(path: &str) -> Option<&str> {
    let stripped = path.strip_prefix(RELAY_JOIN_PATH)?;
    if stripped.is_empty() || stripped.contains('/') {
        None
    } else {
        Some(stripped)
    }
}

/// Validate a shared-secret length at compile time.
const _: () = {
    assert!(SHARED_SECRET_LEN == 32, "shared secret must be 32 bytes");
};

mod pairing {
    //! Side-channel that bridges the two halves of a session.
    //!
    //! When the second peer arrives the registry hands a `Paired` event
    //! to the first peer's task, but the first peer's task still needs to
    //! obtain the second peer's `WebSocketStream`. The pairing module
    //! keeps a single global map keyed by session ID where the second peer
    //! deposits its stream and the first peer collects it.

    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::OnceLock;
    use std::sync::atomic::Ordering;

    use anyhow::{Context, Result};
    use tokio::net::TcpStream;
    use tokio::sync::Mutex;
    use tokio::sync::oneshot;
    use tokio_tungstenite::WebSocketStream;

    use crate::metrics::Counters;
    use crate::pipe::run_pipe;
    use crate::session::{SessionEvent, SessionRegistry};

    type Slot = oneshot::Sender<WebSocketStream<TcpStream>>;

    static PAIRING: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();

    fn pairing_map() -> &'static Mutex<HashMap<String, Slot>> {
        PAIRING.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(super) async fn pair_into(
        session_id: &str,
        second_ws: WebSocketStream<TcpStream>,
        counters: &Arc<Counters>,
    ) -> Result<()> {
        let slot = {
            let mut map = pairing_map().lock().await;
            map.remove(session_id)
        };
        let Some(slot) = slot else {
            // The first peer disappeared between the registry notifying it
            // and us trying to hand off the stream. Drop our socket.
            anyhow::bail!("first peer dropped before hand-off");
        };
        slot.send(second_ws).map_err(|_| {
            anyhow::anyhow!("first peer task dropped before receiving second stream")
        })?;
        // The first peer's task is responsible for the byte-pipe; we're
        // done. Counters are updated by the first peer's task.
        let _ = counters;
        Ok(())
    }

    pub(super) async fn wait_for_peer(
        session_id: &str,
        first_ws: WebSocketStream<TcpStream>,
        wait_for_peer: oneshot::Receiver<SessionEvent>,
        registry: &SessionRegistry,
        counters: &Arc<Counters>,
    ) -> Result<()> {
        // Install our slot in the pairing map *before* awaiting the event
        // so the second peer never races us.
        let (slot_tx, slot_rx) = oneshot::channel();
        {
            let mut map = pairing_map().lock().await;
            map.insert(session_id.to_owned(), slot_tx);
        }

        let Ok(event) = wait_for_peer.await else {
            // Registry was dropped. Clean up our pairing slot.
            pairing_map().lock().await.remove(session_id);
            anyhow::bail!("session registry signalled an unrecoverable failure");
        };

        match event {
            SessionEvent::Paired => {
                // Receive the second peer's stream.
                let second_ws = slot_rx
                    .await
                    .context("waiting for second peer's stream handoff")?;
                counters.sessions_active.fetch_add(1, Ordering::Relaxed);
                counters
                    .sessions_paired_total
                    .fetch_add(1, Ordering::Relaxed);
                let (_a, _b) = run_pipe(first_ws, second_ws, counters.clone()).await;
                counters.sessions_active.fetch_sub(1, Ordering::Relaxed);
                Ok(())
            }
            SessionEvent::TimedOut => {
                pairing_map().lock().await.remove(session_id);
                let _ = first_ws;
                anyhow::bail!("session timed out before peer arrived");
            }
            SessionEvent::ServerShutdown => {
                pairing_map().lock().await.remove(session_id);
                registry.drop_parked(session_id).await;
                let _ = first_ws;
                anyhow::bail!("server shutting down");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_session_path() {
        assert_eq!(parse_session_path("/join/abc"), Some("abc"));
        assert_eq!(parse_session_path("/join/AbC-123"), Some("AbC-123"));
    }

    #[test]
    fn rejects_invalid_session_path() {
        assert_eq!(parse_session_path("/join/"), None);
        assert_eq!(parse_session_path("/join/a/b"), None);
        assert_eq!(parse_session_path("/other"), None);
        assert_eq!(parse_session_path("join/abc"), None);
    }
}
