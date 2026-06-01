//! `cascade-relay` binary entry point.
//!
//! Parses CLI flags via clap, configures `tracing` for structured logging,
//! and hands the resulting [`RelayConfig`] to [`cascade_relay_server::run_relay`].

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use cascade_relay_server::{RelayConfig, run_relay};
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// `cascade-relay` — opaque byte-pipe relay for cascade peers behind NATs.
///
/// The relay shuttles already-encrypted bytes between two peers that have
/// authenticated with a shared HMAC secret. The relay never inspects the
/// payload — peers establish their own end-to-end TLS over the tunnel.
#[derive(Debug, Parser)]
#[command(name = "cascade-relay", version, about)]
struct Cli {
    /// Address the byte-pipe listener binds to.
    #[arg(long, default_value = "0.0.0.0:9999")]
    bind: SocketAddr,

    /// 64-character hexadecimal shared secret (32 bytes after decoding).
    /// Generate with `openssl rand -hex 32`. Both peers and the relay must
    /// hold the same secret. Pass via `--shared-secret "$(cat secret.hex)"`
    /// or inline; the workspace clap doesn't enable the `env` feature so
    /// there is no `CASCADE_RELAY_SHARED_SECRET` shortcut.
    #[arg(long)]
    shared_secret: String,

    /// How long the first peer of a session may wait for its partner
    /// before the server times the session out, in seconds.
    #[arg(long, default_value_t = 60)]
    session_timeout_seconds: u32,

    /// Maximum number of in-flight sessions (paired or parked). New
    /// sessions are rejected when full.
    #[arg(long, default_value_t = 1024)]
    max_sessions: u32,

    /// Optional address for a `/metrics` HTTP endpoint. When unset the
    /// metrics endpoint is disabled (counters are still kept in process
    /// memory but not exposed).
    #[arg(long)]
    metrics_bind: Option<SocketAddr>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Default to INFO; honour `RUST_LOG` when set. Structured logs go to
    // stderr so the relay can be combined with other tooling that uses
    // stdout for protocol data.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let shared_secret = RelayConfig::parse_shared_secret(&cli.shared_secret)
        .context("invalid --shared-secret")?;

    let config = RelayConfig {
        bind: cli.bind,
        shared_secret,
        session_timeout: Duration::from_secs(u64::from(cli.session_timeout_seconds)),
        max_sessions: cli.max_sessions,
        metrics_bind: cli.metrics_bind,
    };

    run_relay(config).await
}
