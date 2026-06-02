//! Token persistence behind an injectable contract.
//!
//! The Google Drive backend refreshes expired access tokens and writes the
//! result back to durable storage. In production that storage is the macOS
//! Keychain or a per-account JSON file (see [`crate::auth`]); under test it is
//! an in-memory map so the refresh path can be exercised without touching the
//! host's Keychain or config directory.
//!
//! Modelling persistence as a port keeps the backend's token-refresh logic
//! independent of where the tokens actually live, matching the storage-boundary
//! rule the workspace follows elsewhere.

use async_trait::async_trait;

use crate::auth::{self, AuthTokens};

/// Durable storage for a single account's `OAuth2` tokens.
///
/// Implementations are keyed by account name so one backend instance maps to
/// one account's slot in the underlying store.
#[async_trait]
pub trait TokenStore: Send + Sync + std::fmt::Debug {
    /// Load the stored tokens for `account`, or `None` if none are stored.
    async fn load(&self, account: &str) -> anyhow::Result<Option<AuthTokens>>;

    /// Persist `tokens` for `account`, replacing any existing entry.
    async fn save(&self, account: &str, tokens: &AuthTokens) -> anyhow::Result<()>;
}

/// Production token store: delegates to the platform persistence in
/// [`crate::auth`] (macOS Keychain, or a per-account JSON file elsewhere).
#[derive(Debug, Default, Clone, Copy)]
pub struct PlatformTokenStore;

#[async_trait]
impl TokenStore for PlatformTokenStore {
    async fn load(&self, account: &str) -> anyhow::Result<Option<AuthTokens>> {
        auth::load_tokens(account)
    }

    async fn save(&self, account: &str, tokens: &AuthTokens) -> anyhow::Result<()> {
        auth::save_tokens(account, tokens)
    }
}
