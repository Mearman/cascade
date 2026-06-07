#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! Cascade engine — core types, backend trait, VFS tree, state database.

pub mod backend;
// The cache manager and sync runner are daemon-level concerns: they schedule
// background work, flush dirty files, and run eviction sweeps. A wasm32 build
// uses the storage `_sync` helpers directly and has no need for these modules.
// Compile them only for native, or for a non-wasm32 portable build (e.g. a
// test harness running on a native host with the portable feature enabled).
#[cfg(any(
    feature = "native",
    all(feature = "portable", not(target_arch = "wasm32"))
))]
pub mod cache;
#[cfg(feature = "native")]
pub mod changefeed;
pub mod config;
pub mod db;
#[cfg(feature = "native")]
pub mod engine;
pub mod manage;
#[cfg(feature = "p2p")]
pub mod p2p_bridge;
#[cfg(any(feature = "native", feature = "portable"))]
pub mod portable;
pub mod presenter;
pub mod protocol;
#[cfg(any(
    feature = "native",
    all(feature = "portable", not(target_arch = "wasm32"))
))]
pub mod sync;
pub mod types;
#[cfg(feature = "native")]
pub mod vfs;

#[cfg(feature = "native")]
pub use engine::{Engine, EngineConfig, EngineHandle, EngineStatus};
