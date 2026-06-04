//! Request and response schemas — one module per resource.
//!
//! Every struct serialises `snake_case` (`#[serde(rename_all = "snake_case")]`)
//! so the JSON field names match the contract 1:1, and the hand-maintained
//! TypeScript types on the PWA side line up without a codegen step. The one
//! deliberate exception is [`shares::SharePosture`], whose variants are
//! `kebab-case` because the contract's posture values are `read-only`,
//! `write-only`, `read-write`; it still uses a `rename_all` (not per-field
//! renames), satisfying the contract's "no per-field rename" rule.
//!
//! Capability and scope fields reuse the engine's
//! [`Capability`](cascade_engine::manage::Capability) and
//! [`Scope`](cascade_engine::manage::Scope) types directly, whose serde forms
//! are already the contract's wire forms (`"status:read"`, `{ "kind": "node" }`).

pub mod audit;
pub mod backends;
pub mod cache;
pub mod common;
pub mod config;
pub mod files;
pub mod grants;
pub mod health;
pub mod peers;
pub mod pins;
pub mod policies;
pub mod session;
pub mod shares;
pub mod tokens;
