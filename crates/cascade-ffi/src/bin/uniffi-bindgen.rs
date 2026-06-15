//! `UniFFI` bindings generator entry point.
//!
//! Mobile build scripts invoke this binary to emit the Swift and Kotlin glue
//! from the proc-macro scaffolding compiled into the `cascade-ffi` library, for
//! example:
//!
//! ```text
//! cargo run --bin uniffi-bindgen -- generate --library \
//!     target/aarch64-apple-ios/release/libcascade_ffi.a --language swift \
//!     --out-dir generated/
//! ```

fn main() {
    uniffi::uniffi_bindgen_main();
}
