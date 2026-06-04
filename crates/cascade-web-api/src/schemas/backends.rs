//! Backend schemas.

use serde::{Deserialize, Serialize};

/// One registered backend, as the PWA's folder picker reads it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BackendView {
    /// The backend id (its user-facing name).
    pub id: String,
    /// The backend type (`gdrive`, `s3`, `local`, `p2p`).
    pub backend_type: String,
    /// The backend's display name.
    pub display_name: String,
    /// The mount path, when configured.
    pub mount_path: Option<String>,
    /// The canonical BEP folder id (`p2p-<id>`); `null` for non-P2P backends.
    pub folder_id: Option<String>,
}

/// `GET /v1/backends` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BackendsResponse {
    /// The registered backends.
    pub backends: Vec<BackendView>,
}
