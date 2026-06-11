//! Sync loop — polls backend changes, applies to state DB, notifies presenter.

pub mod conflict;
pub mod mount_path;
pub mod runner;

pub use runner::SyncRunner;
