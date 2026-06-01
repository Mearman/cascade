//! VFS tree — composes multiple backends with longest-prefix routing.

pub mod tree;

pub use tree::{VfsTree, derive_sync_cursor};
