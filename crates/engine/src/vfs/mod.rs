//! VFS tree — composes multiple backends with longest-prefix routing.

pub mod tree;

pub use tree::{NEUTRAL_ROOT_ID, VfsTree, derive_sync_cursor, merge_listing, neutral_root_item_id};
