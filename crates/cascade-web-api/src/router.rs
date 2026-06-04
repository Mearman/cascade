//! Router assembly, CORS, and the serve entry point.
//!
//! [`build_router`] composes every resource's routes, wraps them in the
//! request-id middleware, the CORS layer, and the body-size limit, and binds the
//! [`AppState`]. [`serve`] binds the configured socket and spawns the server,
//! returning a [`RouterHandle`] the daemon stops on shutdown.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderName, HeaderValue, Method, header};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::auth::BEARER_DEVICE_HEADER;
use crate::request_id::{self, REQUEST_ID_HEADER};
use crate::routes;
use crate::state::AppState;

/// The CORS preflight cache lifetime, in seconds.
const CORS_MAX_AGE_SECS: u64 = 600;

/// Compose the full `/v1` router over `state`, with CORS, request-id, and
/// body-limit layers applied.
pub fn build_router(state: AppState) -> Router {
    let max_body = state.bind.max_body_bytes;
    let cors = build_cors(&state);

    Router::new()
        .merge(routes::health::routes())
        .merge(routes::session::routes())
        .merge(routes::files::routes())
        .merge(routes::shares::routes())
        .merge(routes::tokens::routes())
        .merge(routes::grants::routes())
        .merge(routes::audit::routes())
        .merge(routes::peers::routes())
        .merge(routes::pins::routes())
        .merge(routes::policies::routes())
        .merge(routes::backends::routes())
        .merge(routes::cache::routes())
        .merge(routes::config::routes())
        .layer(DefaultBodyLimit::max(max_body))
        .layer(cors)
        .layer(axum::middleware::from_fn(request_id::middleware))
        .with_state(state)
}

/// Whether an `Origin` header value is a loopback origin on any port.
fn is_loopback_origin(origin: &str) -> bool {
    // Strip the scheme, then check the host (ignoring any `:port`).
    let host_port = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    let Some(host_port) = host_port else {
        return false;
    };
    // An IPv6 literal is bracketed (`[::1]:port`); split on the closing bracket.
    if let Some(rest) = host_port.strip_prefix('[') {
        return rest.starts_with("::1]");
    }
    let host = host_port.split(':').next().unwrap_or(host_port);
    host == "localhost" || host == "127.0.0.1"
}

/// Build the CORS layer: loopback always, plus the operator allowlist (which
/// provably contains no wildcard — that is refused at config-parse time).
fn build_cors(state: &AppState) -> CorsLayer {
    let allowlist = state.bind.cors_origins.clone();
    let allow_origin = AllowOrigin::predicate(move |origin: &HeaderValue, _parts| {
        let Ok(origin) = origin.to_str() else {
            return false;
        };
        is_loopback_origin(origin) || allowlist.iter().any(|allowed| allowed == origin)
    });

    CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods([
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            HeaderName::from_static(BEARER_DEVICE_HEADER),
            HeaderName::from_static(REQUEST_ID_HEADER),
        ])
        .allow_credentials(false)
        .max_age(Duration::from_secs(CORS_MAX_AGE_SECS))
}

/// A handle to a running server, returned by [`serve`].
#[derive(Debug)]
pub struct RouterHandle {
    local_addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    join: JoinHandle<std::io::Result<()>>,
}

impl RouterHandle {
    /// The address the server bound (resolves an ephemeral `:0` port to the
    /// concrete one).
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Signal a graceful shutdown and await the server task.
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.join.await;
    }

    /// Abort the server task immediately, without a graceful drain.
    pub fn abort(&self) {
        self.join.abort();
    }
}

/// Bind the configured socket and spawn the server, returning a handle.
///
/// Runs in the caller's `tokio` runtime as a second task alongside the
/// presenter, exactly as the contract's process model prescribes.
pub async fn serve(state: AppState) -> std::io::Result<RouterHandle> {
    let bind = state.bind.bind;
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let local_addr = listener.local_addr()?;

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let join = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                // Resolve when the shutdown flag flips to true, or when the
                // sender is dropped.
                while shutdown_rx.changed().await.is_ok() {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
            })
            .await
    });

    Ok(RouterHandle {
        local_addr,
        shutdown: shutdown_tx,
        join,
    })
}
