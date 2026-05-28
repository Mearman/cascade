//! Sync loop — polls backend changes, applies to state DB, notifies presenter.

pub mod conflict;
pub mod runner;

pub use runner::SyncRunner;
