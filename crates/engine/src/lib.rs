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
#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
pub mod sync;
pub mod types;
#[cfg(feature = "native")]
pub mod vfs;

#[cfg(feature = "native")]
pub use engine::{Engine, EngineConfig, EngineHandle, EngineStatus};
