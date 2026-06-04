//! Health, readiness, and bundle-manifest response schemas.

use serde::{Deserialize, Serialize};

/// `GET /v1/health` — always 200 once the daemon is serving.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HealthResponse {
    /// Liveness marker; degraded state shows up in `/v1/ready`.
    pub status: String,
    /// The daemon version.
    pub version: String,
    /// This node's device id.
    pub node_device_id: String,
}

/// `GET /v1/ready` — readiness and the F3 data-plane bit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ReadyResponse {
    /// Whether the daemon is up.
    pub ready: bool,
    /// Whether the data plane has reported ready (the F3 bit).
    pub data_plane_ready: bool,
    /// The registered backend display strings.
    pub backends: Vec<String>,
    /// The daemon start instant.
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// `GET /v1/bundle` — the public PWA shell manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BundleResponse {
    /// The hosted PWA bundle URL; `null` renders a config-error screen.
    pub bundle_url: Option<String>,
    /// The base URL the PWA should call the API at.
    pub api_base_url: String,
    /// The daemon version.
    pub version: String,
    /// The build commit SHA, when known.
    pub build_sha: Option<String>,
}
