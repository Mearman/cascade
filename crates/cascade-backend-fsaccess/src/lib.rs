#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
// Every async method here awaits a `wasm_bindgen_futures::JsFuture`, which is
// `Rc`-backed and so fundamentally `!Send`. The browser's WASM context is
// single-threaded, so a `Send` future is neither achievable nor meaningful;
// `future_not_send` cannot apply on this target.
#![cfg_attr(target_arch = "wasm32", allow(clippy::future_not_send))]
//! Browser File System Access backend for Cascade.
//!
//! This crate is the Rust side of the File System Access API bridge. It compiles
//! to `wasm32-unknown-unknown` and calls into the browser-side JavaScript module
//! at `apps/web/src/wasm/fsaccess.ts` through `wasm-bindgen`. The JS module owns
//! every DOM and File System Access API call (the directory picker, file reads
//! and writes, change detection); this crate marshals data across the boundary
//! and presents a small, deterministic Rust surface.
//!
//! The data types (`FsAccessError`, `DirectoryChanges`) and the struct accessors
//! (`id`, `display_name`) are portable and host-testable under
//! `cfg(any(target_arch = "wasm32", test))`. The async bridge methods
//! (`changes`, `download`, `upload`) and the `js` module are wasm32-only
//! because they depend on `js-sys` and `wasm-bindgen-futures`.
//!
//! Verify the WASM build with:
//!
//! ```text
//! cargo check -p cascade-backend-fsaccess --target wasm32-unknown-unknown
//! ```
//!
//! Run the wasm-bindgen-test suite (exercises the Rust decode paths against a
//! JS stub module):
//!
//! ```text
//! wasm-pack test --node crates/cascade-backend-fsaccess -- --features js-test-stub
//! ```
//!
//! ## Why not `cascade_engine::backend::Backend`
//!
//! The engine's `Backend` trait lives in `cascade-engine`, which depends on
//! `cascade-p2p` (→ `ring`), `tokio`, and `rusqlite` — none of which build for
//! `wasm32-unknown-unknown`. The project's v2 design
//! (`docs/pwa-v2-rust-wasm.md`) keeps `cascade-engine` and the backend crates
//! native-only and runs the browser's storage surface as a separate concern.
//! This crate therefore exposes its own self-contained surface
//! (`id`, `display_name`, `changes`, `download`, `upload`) shaped after the
//! backend contract rather than implementing the native trait directly.

#[cfg(any(target_arch = "wasm32", test))]
mod backend;
#[cfg(target_arch = "wasm32")]
mod js;

#[cfg(target_arch = "wasm32")]
pub use backend::create_backend;
#[cfg(any(target_arch = "wasm32", test))]
pub use backend::{DirectoryChanges, FsAccessBackend, FsAccessError};

// wasm-bindgen-test suite: bridge exercises against the JS stub.
// The `test` gate ensures the module is only compiled as part of a `cargo test`
// invocation (where dev-dependencies like `wasm-bindgen-test` are available),
// never into the production lib. The `js-test-stub` feature gate ensures the
// stub-specific extern block and inspector API are present. The `wasm32` gate
// keeps it invisible to native `cargo test`.
#[cfg(all(test, target_arch = "wasm32", feature = "js-test-stub"))]
mod wasm_tests;
