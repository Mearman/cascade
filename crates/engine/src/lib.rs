//! Cascade engine — core types, backend trait, VFS tree, state database.

pub mod backend;
pub mod cache;
pub mod config;
pub mod db;
pub mod engine;
pub mod p2p_bridge;
pub mod presenter;
pub mod protocol;
pub mod sync;
pub mod types;
pub mod vfs;

pub use engine::{Engine, EngineConfig, EngineHandle, EngineStatus};
