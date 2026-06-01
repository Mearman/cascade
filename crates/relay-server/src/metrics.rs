//! Lightweight relay-server metrics.
//!
//! Counters are stored as atomic integers and exposed through a plain-text
//! `Prometheus`-compatible endpoint when the `metrics` feature is enabled.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Shared counter handle.
#[derive(Debug, Default)]
pub struct Counters {
    /// Currently active (paired) sessions.
    pub sessions_active: AtomicI64,
    /// Total number of sessions that successfully paired since startup.
    pub sessions_paired_total: AtomicU64,
    /// Total number of sessions reaped due to peer-timeout.
    pub sessions_timed_out_total: AtomicU64,
    /// Total number of sessions rejected because the registry was full.
    pub sessions_rejected_total: AtomicU64,
    /// Total number of handshake failures (bad HMAC, malformed frame, etc).
    pub auth_failures_total: AtomicU64,
    /// Cumulative bytes ferried across all paired sessions.
    pub bytes_relayed_total: AtomicU64,
}

impl Counters {
    /// Construct a fresh counter set with all values at zero.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Render the counters as text in `Prometheus` exposition format.
    #[must_use]
    pub fn render_prometheus(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "# HELP cascade_relay_sessions_active Currently paired sessions."
        );
        let _ = writeln!(out, "# TYPE cascade_relay_sessions_active gauge");
        let _ = writeln!(
            out,
            "cascade_relay_sessions_active {}",
            self.sessions_active.load(Ordering::Relaxed)
        );

        let _ = writeln!(
            out,
            "# HELP cascade_relay_sessions_paired_total Sessions that successfully paired."
        );
        let _ = writeln!(out, "# TYPE cascade_relay_sessions_paired_total counter");
        let _ = writeln!(
            out,
            "cascade_relay_sessions_paired_total {}",
            self.sessions_paired_total.load(Ordering::Relaxed)
        );

        let _ = writeln!(
            out,
            "# HELP cascade_relay_sessions_timed_out_total Parked sessions reaped due to timeout."
        );
        let _ = writeln!(out, "# TYPE cascade_relay_sessions_timed_out_total counter");
        let _ = writeln!(
            out,
            "cascade_relay_sessions_timed_out_total {}",
            self.sessions_timed_out_total.load(Ordering::Relaxed)
        );

        let _ = writeln!(
            out,
            "# HELP cascade_relay_sessions_rejected_total Sessions rejected because the registry was full."
        );
        let _ = writeln!(out, "# TYPE cascade_relay_sessions_rejected_total counter");
        let _ = writeln!(
            out,
            "cascade_relay_sessions_rejected_total {}",
            self.sessions_rejected_total.load(Ordering::Relaxed)
        );

        let _ = writeln!(
            out,
            "# HELP cascade_relay_auth_failures_total Handshake verification failures."
        );
        let _ = writeln!(out, "# TYPE cascade_relay_auth_failures_total counter");
        let _ = writeln!(
            out,
            "cascade_relay_auth_failures_total {}",
            self.auth_failures_total.load(Ordering::Relaxed)
        );

        let _ = writeln!(
            out,
            "# HELP cascade_relay_bytes_relayed_total Total bytes shuttled between paired peers."
        );
        let _ = writeln!(out, "# TYPE cascade_relay_bytes_relayed_total counter");
        let _ = writeln!(
            out,
            "cascade_relay_bytes_relayed_total {}",
            self.bytes_relayed_total.load(Ordering::Relaxed)
        );

        out
    }
}

#[cfg(feature = "metrics")]
mod axum_endpoint {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use anyhow::{Context, Result};
    use axum::Router;
    use axum::extract::State;
    use axum::http::header::CONTENT_TYPE;
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use tokio::net::TcpListener;

    use super::Counters;

    /// Bind a small `HTTP` server on `bind` that exposes `/metrics`.
    /// Returns the actual bound address (useful when binding to port 0) and
    /// a handle that keeps the server alive until dropped.
    pub async fn serve_metrics(
        bind: SocketAddr,
        counters: Arc<Counters>,
    ) -> Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
        let listener = TcpListener::bind(bind)
            .await
            .with_context(|| format!("binding metrics endpoint to {bind}"))?;
        let local = listener
            .local_addr()
            .context("reading local address for bound metrics listener")?;

        let app = Router::new()
            .route("/metrics", get(metrics_handler))
            .with_state(counters);

        let join = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::error!(error = %err, "metrics server exited with error");
            }
        });
        Ok((local, join))
    }

    async fn metrics_handler(State(counters): State<Arc<Counters>>) -> Response {
        let body = counters.render_prometheus();
        ([(CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
    }
}

#[cfg(feature = "metrics")]
pub use axum_endpoint::serve_metrics;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn rendered_metrics_contain_each_counter_name() {
        let counters = Counters::new();
        counters.sessions_paired_total.store(7, Ordering::Relaxed);
        counters.bytes_relayed_total.store(1024, Ordering::Relaxed);
        let body = counters.render_prometheus();

        for needle in [
            "cascade_relay_sessions_active",
            "cascade_relay_sessions_paired_total 7",
            "cascade_relay_sessions_timed_out_total",
            "cascade_relay_sessions_rejected_total",
            "cascade_relay_auth_failures_total",
            "cascade_relay_bytes_relayed_total 1024",
        ] {
            assert!(body.contains(needle), "missing {needle} in {body}");
        }
    }
}
