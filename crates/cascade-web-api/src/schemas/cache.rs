//! Cache operation schemas.

use serde::{Deserialize, Serialize};

/// `POST /v1/cache/warm` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CacheWarmPost {
    /// The path glob to pre-warm.
    pub path_glob: String,
}

/// The short summary string returned by `cache/evict` and `cache/warm`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CacheResponse {
    /// The engine's summary of the operation.
    pub summary: String,
}
