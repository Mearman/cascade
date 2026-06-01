#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! `.cascade` config parser — four formats, merge, directory walk.
//!
//! Supports gitignore-style, TOML, YAML, and JSON formats.
//! All deserialise to [`CascadeConfig`]. Directory walk produces
//! [`ResolvedConfig`] with child-overrides-parent precedence.

pub mod merge;
pub mod parse;
pub mod types;

pub use types::{CascadeConfig, IgnoreRule, ResolvedConfig};
