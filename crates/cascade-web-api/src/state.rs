//! The shared application state every handler is given.
//!
//! [`AppState`] is `Clone` (it is the `axum` router state) and holds only cheap
//! handles: an `Arc<Engine>` for the data and management planes, the node's
//! signing identity, the immutable bind configuration, and the F3 readiness
//! flag.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cascade_engine::engine::NativeEngine;
use cascade_engine::manage::DeviceId;
use cascade_p2p::identity::DeviceIdentity;
use chrono::{DateTime, Utc};

/// Default bind address — loopback only, the contract's documented default.
pub const DEFAULT_BIND: &str = "127.0.0.1:7842";
/// Default server-side request timeout in seconds (one hour) — bounds the only
/// streaming route, `/v1/folders/{folder}/archive`.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 3600;
/// Default maximum request body size in bytes (1 GiB), overriding `axum`'s 2 MiB
/// default so large `PUT`s are accepted.
pub const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024 * 1024;

/// The node's signing identity — the key a capability token's delegation chain
/// roots in, and the device id the HTTP transport compares against the
/// `X-Cascade-Bearer-Device` header.
#[derive(Debug, Clone)]
pub struct NodeIdentity {
    identity: DeviceIdentity,
    device_id: DeviceId,
}

impl NodeIdentity {
    /// Wrap the node's device identity.
    #[must_use]
    pub fn new(identity: DeviceIdentity) -> Self {
        let device_id = DeviceId::new(identity.device_id.clone());
        Self {
            identity,
            device_id,
        }
    }

    /// This node's device id — the issuer a token's chain root must name and the
    /// value the bearer-device header is checked against.
    #[must_use]
    pub const fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// The full identity, including the private key, for signing issued tokens.
    #[must_use]
    pub const fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }
}

/// Why a [`BindConfig`] could not be built.
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum BindConfigError {
    /// A `cors_origins` entry was the wildcard `*`. A wildcard CORS allowlist
    /// combined with bearer auth is a credential-leak footgun, so it is refused
    /// at config-parse time and the runtime never sees it.
    #[error(
        "wildcard `*` is not permitted in cors_origins: a wildcard CORS allowlist with bearer \
         auth leaks credentials; name explicit origins instead"
    )]
    WildcardCors,
}

/// The immutable bind-time configuration the server reads.
#[derive(Debug, Clone)]
pub struct BindConfig {
    /// The socket the server binds.
    pub bind: SocketAddr,
    /// The PWA bundle URL the daemon advertises in `/v1/bundle`. `None` renders
    /// a config-error screen in the PWA.
    pub bundle_url: Option<String>,
    /// Operator-configured CORS origins, in addition to loopback (which is
    /// always allowed). Never contains `*` — that is refused at construction.
    pub cors_origins: Vec<String>,
    /// Server-side request timeout, bounding the streaming archive route.
    pub request_timeout_secs: u64,
    /// Maximum request body size in bytes.
    pub max_body_bytes: usize,
    /// The daemon version reported in `/v1/health` and `/v1/bundle`.
    pub version: String,
    /// The build commit SHA reported in `/v1/bundle`, when known.
    pub build_sha: Option<String>,
}

impl BindConfig {
    /// Build a bind configuration, rejecting a wildcard in `cors_origins`.
    ///
    /// The wildcard check is the single enforcement point the contract's "No
    /// wildcard CORS" rule names: a `*` here is a hard error, so the CORS layer
    /// is built from a list that provably contains none.
    pub fn new(
        bind: SocketAddr,
        bundle_url: Option<String>,
        cors_origins: Vec<String>,
        request_timeout_secs: u64,
        max_body_bytes: usize,
        version: String,
        build_sha: Option<String>,
    ) -> Result<Self, BindConfigError> {
        if cors_origins.iter().any(|origin| origin.trim() == "*") {
            return Err(BindConfigError::WildcardCors);
        }
        Ok(Self {
            bind,
            bundle_url,
            cors_origins,
            request_timeout_secs,
            max_body_bytes,
            version,
            build_sha,
        })
    }
}

/// The F3 readiness state.
///
/// Holds the data-plane readiness bit and the daemon start instant. Cloning
/// shares the same bit (it is behind an `Arc`), so the daemon flips it on one
/// handle and every request observes the change.
#[derive(Debug, Clone)]
pub struct Readiness {
    data_plane_ready: Arc<AtomicBool>,
    started_at: DateTime<Utc>,
}

impl Readiness {
    /// Create a readiness state that starts not-ready, stamped with the daemon
    /// start instant.
    #[must_use]
    pub fn new(started_at: DateTime<Utc>) -> Self {
        Self {
            data_plane_ready: Arc::new(AtomicBool::new(false)),
            started_at,
        }
    }

    /// Whether the data plane has reported ready (the F3 bit).
    #[must_use]
    pub fn data_plane_ready(&self) -> bool {
        self.data_plane_ready.load(Ordering::Acquire)
    }

    /// Flip the data-plane readiness bit. The daemon calls this once the
    /// presenter and data plane have come up.
    pub fn set_data_plane_ready(&self, ready: bool) {
        self.data_plane_ready.store(ready, Ordering::Release);
    }

    /// The daemon start instant, reported by `/v1/ready`.
    #[must_use]
    pub const fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }
}

/// The shared state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    /// The engine — the single data-plane and management-plane authority.
    pub engine: Arc<NativeEngine>,
    /// The node's signing identity.
    pub identity: Arc<NodeIdentity>,
    /// Immutable bind-time configuration.
    pub bind: Arc<BindConfig>,
    /// The F3 readiness state.
    pub readiness: Readiness,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("node_device_id", &self.identity.device_id())
            .field("bind", &self.bind.bind)
            .field("data_plane_ready", &self.readiness.data_plane_ready())
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Construct application state.
    #[must_use]
    pub fn new(
        engine: Arc<NativeEngine>,
        identity: NodeIdentity,
        bind: BindConfig,
        readiness: Readiness,
    ) -> Self {
        Self {
            engine,
            identity: Arc::new(identity),
            bind: Arc::new(bind),
            readiness,
        }
    }
}
