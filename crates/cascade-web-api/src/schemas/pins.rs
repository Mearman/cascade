//! Pin schemas.

use serde::{Deserialize, Serialize};

/// One pin rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PinView {
    /// The pin rule row id.
    pub id: i64,
    /// The path glob the rule pins.
    pub path_glob: String,
    /// Whether the rule applies recursively.
    pub recursive: bool,
}

/// `GET /v1/pins` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PinsResponse {
    /// The active pin rules.
    pub pins: Vec<PinView>,
}

/// `POST /v1/pins` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PinPost {
    /// The path glob to pin.
    pub path_glob: String,
    /// Whether to pin recursively (defaults to `true`).
    #[serde(default = "default_recursive")]
    pub recursive: bool,
}

/// Pins default to recursive, matching the CLI's pin behaviour.
const fn default_recursive() -> bool {
    true
}
