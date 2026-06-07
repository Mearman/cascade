//! Test module for `sync.rs`, split out to keep the
//! parent file under the source-length cap. Declared from there via
//! `#[cfg(test)] #[path = "sync_sync_engine_send_check.rs"] mod sync_engine_send_check;`, so it stays a child
//! module with full access to the parent's private items.

use super::*;
#[allow(dead_code)]
fn assert_traits() {
    fn is_send_sync<T: Send + Sync>() {}
    is_send_sync::<SyncEngine>();
}
