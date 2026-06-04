//! The Cascade daemon's typed HTTP JSON API — the front door the PWA drives.
//!
//! This crate is the `v1` implementation of the contract in
//! `docs/pwa/api-contract.md`. The daemon wires it up only when the operator
//! passes `--web`; without that flag the `axum` dependency does not run.
//!
//! ## Shape
//!
//! - Authentication is bearer-token only: the same signed
//!   [`CapabilityToken`](cascade_engine::manage::CapabilityToken) the BEP
//!   management plane verifies, presented in `Authorization: Bearer <base64>`.
//!   The HTTP layer introduces no second credential format and no second
//!   authorisation path — every request re-runs
//!   [`authorises`](cascade_engine::manage::authorises), the exact decision the
//!   BEP dispatcher runs.
//! - Every error is one envelope shape ([`error::ApiError`]), every response
//!   carries an `X-Cascade-Request-Id`, and every schema serialises
//!   `snake_case`.
//! - The F1–F4 directional data-sharing fix is first-class: data verbs are
//!   folder-scoped at write and read time, node-wide data grants are refused,
//!   and data-plane routes gate on the F3 readiness bit.
//!
//! The entry points are [`router::build_router`] (compose the `axum` router
//! over an [`state::AppState`]) and [`router::serve`] (bind and serve it).

// The four restriction-group lints are denied workspace-wide for shipping code
// but are idiomatic in tests, where a panic is the intended failure signal.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]

pub mod auth;
pub mod error;
pub mod request_id;
pub mod router;
pub mod routes;
pub mod schemas;
pub mod state;

pub use error::{ApiError, ErrorCode};
pub use router::{RouterHandle, build_router, serve};
pub use state::{AppState, BindConfig, BindConfigError, NodeIdentity, Readiness};
