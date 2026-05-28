//! Cache manager — pinning, eviction, and lifecycle policy evaluation.
//!
//! The cache manager runs as a background worker that:
//! 1. Ensures pinned paths are fully cached (downloads missing files)
//! 2. Evicts LRU non-pinned files when cache exceeds size limits
//! 3. Applies lifecycle policies (max age, max file size)
//! 4. Updates file cache states in the state DB

pub mod lifecycle;
pub mod manager;
pub mod pin;

pub use lifecycle::LifecycleEvaluator;
pub use manager::CacheManager;
pub use pin::PinMatcher;
