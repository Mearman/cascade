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
pub mod cache;
pub mod changefeed;
pub mod config;
pub mod db;
pub mod engine;
pub mod manage;
pub mod p2p_bridge;
pub mod presenter;
pub mod protocol;
pub mod sync;
pub mod types;
pub mod vfs;

pub use engine::{Engine, EngineConfig, EngineHandle, EngineStatus};
