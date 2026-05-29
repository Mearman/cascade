//! Minimal `NFSv4` state management.
//!
//! Tracks open file state for OPEN/CLOSE operations. No delegations,
//! no lock manager, no client tracking beyond what's needed for basic
//! open/close semantics.

use std::collections::HashMap;
use std::sync::RwLock;

use super::xdr::StateId;

/// State for a single open file.
#[derive(Debug, Clone)]
pub struct OpenState {
    /// The path this open refers to.
    pub path: String,
    /// Share access mode (READ, WRITE, or BOTH).
    pub share_access: u32,
    /// Sequence number for this open.
    pub seqid: u32,
}

/// Manages `NFSv4` open file state.
#[derive(Debug)]
pub struct StateManager {
    /// Open files keyed by stateid counter.
    opens: RwLock<HashMap<u32, OpenState>>,
    /// Next stateid counter.
    next_id: RwLock<u32>,
}

impl StateManager {
    /// Create a new state manager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            opens: RwLock::new(HashMap::new()),
            next_id: RwLock::new(1),
        }
    }

    /// Allocate a new open state entry. Returns the stateid.
    pub fn create_open(&self, path: &str, share_access: u32) -> StateId {
        let mut next = self
            .next_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = *next;
        *next = id.saturating_add(1);
        drop(next);

        let state = OpenState {
            path: path.to_string(),
            share_access,
            seqid: 0,
        };

        let mut opens = self
            .opens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        opens.insert(id, state);

        StateId::from_counter(id)
    }

    /// Look up open state by stateid.
    pub fn lookup_open(&self, sid: &StateId) -> Option<OpenState> {
        let id = u32::from_be_bytes(sid.data.get(0..4)?.try_into().unwrap_or([0u8; 4]));
        self.opens
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    /// Close an open file by stateid.
    pub fn close_open(&self, sid: &StateId) {
        let id = u32::from_be_bytes(
            sid.data
                .get(0..4)
                .map_or([0u8; 4], |s| s.try_into().unwrap_or([0u8; 4])),
        );
        self.opens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }
}

impl Default for StateManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_lookup_open() {
        let mgr = StateManager::new();
        let sid = mgr.create_open("/test.txt", 1);
        let state = mgr.lookup_open(&sid).unwrap();
        assert_eq!(state.path, "/test.txt");
        assert_eq!(state.share_access, 1);
    }

    #[test]
    fn close_removes_open() {
        let mgr = StateManager::new();
        let sid = mgr.create_open("/test.txt", 1);
        assert!(mgr.lookup_open(&sid).is_some());
        mgr.close_open(&sid);
        assert!(mgr.lookup_open(&sid).is_none());
    }

    #[test]
    fn multiple_opens_unique_ids() {
        let mgr = StateManager::new();
        let sid1 = mgr.create_open("/a.txt", 1);
        let sid2 = mgr.create_open("/b.txt", 2);
        assert_ne!(sid1, sid2);
        assert!(mgr.lookup_open(&sid1).is_some());
        assert!(mgr.lookup_open(&sid2).is_some());
    }

    #[test]
    fn lookup_nonexistent_returns_none() {
        let mgr = StateManager::new();
        let sid = StateId::from_counter(999);
        assert!(mgr.lookup_open(&sid).is_none());
    }
}
