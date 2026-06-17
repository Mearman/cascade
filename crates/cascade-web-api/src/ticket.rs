//! Short-lived, single-use tickets that exchange a long-lived capability token
//! (presented over an authenticated HTTP path) for a one-shot credential the
//! websocket upgrade can accept.
//!
//! The browser's `WebSocket` API cannot send custom `Authorization` headers, so
//! the exec terminal previously passed the base64-encoded capability token as a
//! `?token=` query parameter. That put the long-lived token (which grants remote
//! code execution) into the websocket URL, where it lands in access logs, proxy
//! logs, and browser history. A ticket breaks that exposure: the browser
//! authenticates a normal `POST /v1/exec/ticket` with the `Authorization` header
//! (which fetch CAN set), receives an opaque ticket bound to the verified
//! authority, and presents only the ticket on the websocket URL.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cascade_engine::manage::{Capability, DeviceId, Scope};
use chrono::{DateTime, Duration, Utc};
use data_encoding::BASE64URL_NOPAD;
use ring::rand::{SecureRandom, SystemRandom};
use serde::Serialize;

/// How long an issued ticket remains valid after minting. Short enough that a
/// ticket intercepted from a log or history entry is almost certainly already
/// dead.
pub const TICKET_TTL_SECS: i64 = 30;

/// The number of random bytes a ticket encodes. 32 bytes (256 bits) is
/// unguessable by any feasible brute force.
const TICKET_RANDOM_BYTES: usize = 32;

/// The authority a ticket carries — captured at issue time from the verified
/// HTTP session, so the websocket handler never sees the long-lived token.
#[derive(Debug, Clone)]
pub struct TicketAuthority {
    /// The bearer device the issuing session authenticated as.
    pub bearer: DeviceId,
    /// The capability the ticket authorises (always `exec:pty` today).
    pub capability: Capability,
    /// The folder scope the capability was authorised over.
    pub scope: Scope,
}

/// The response body for `POST /v1/exec/ticket`.
#[derive(Debug, Serialize)]
pub struct TicketResponse {
    /// The opaque, unguessable ticket string. Present this as `?ticket=` on the
    /// websocket URL.
    pub ticket: String,
    /// The RFC 3339 instant the ticket expires.
    pub expires_at: DateTime<Utc>,
}

/// An in-memory store of pending tickets, swept on every access.
///
/// Each ticket is a 256-bit random opaque string mapping to the
/// [`TicketAuthority`] captured at issue time, plus an expiry. A ticket is
/// single-use: [`TicketStore::redeem`] removes it atomically, so a second
/// presentation always fails. Expired entries are swept opportunistically on
/// every issue and redeem.
#[derive(Debug)]
pub struct TicketStore {
    inner: Arc<Mutex<HashMap<String, StoredTicket>>>,
    rng: SystemRandom,
}

impl Default for TicketStore {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct StoredTicket {
    authority: TicketAuthority,
    expires_at: DateTime<Utc>,
}

impl TicketStore {
    /// Create an empty ticket store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            rng: SystemRandom::new(),
        }
    }

    /// Mint a fresh ticket bound to `authority`, valid for [`TICKET_TTL_SECS`]
    /// seconds. Sweeps expired entries before inserting.
    pub fn issue(&self, authority: TicketAuthority) -> Result<TicketResponse, TicketError> {
        let ticket = generate_ticket(&self.rng)?;
        let expires_at = Utc::now() + Duration::seconds(TICKET_TTL_SECS);

        let mut map = self.inner.lock().map_err(|_| TicketError::LockPoisoned)?;
        sweep(&mut map);

        map.insert(
            ticket.clone(),
            StoredTicket {
                authority,
                expires_at,
            },
        );

        Ok(TicketResponse { ticket, expires_at })
    }

    /// Look up `ticket`, reject if missing/expired, and remove it (single-use).
    /// Returns the authority captured at issue time on success.
    pub fn redeem(&self, ticket: &str) -> Result<TicketAuthority, TicketError> {
        let mut map = self.inner.lock().map_err(|_| TicketError::LockPoisoned)?;
        sweep(&mut map);

        match map.remove(ticket) {
            Some(stored) => {
                if stored.expires_at <= Utc::now() {
                    Err(TicketError::Expired)
                } else {
                    Ok(stored.authority)
                }
            }
            None => Err(TicketError::NotFound),
        }
    }
}

/// Why a ticket operation failed.
#[derive(Debug, thiserror::Error)]
pub enum TicketError {
    /// The ticket was not found (never issued, already redeemed, or swept as
    /// expired).
    #[error("ticket not found")]
    NotFound,
    /// The ticket existed but its expiry has passed.
    #[error("ticket has expired")]
    Expired,
    /// The system CSPRNG could not produce random bytes.
    #[error("could not generate random ticket: {0}")]
    Random(String),
    /// The internal mutex was poisoned.
    #[error("ticket store lock poisoned")]
    LockPoisoned,
}

/// Generate a 256-bit random ticket encoded as base64url (no padding).
fn generate_ticket(rng: &SystemRandom) -> Result<String, TicketError> {
    let mut bytes = [0u8; TICKET_RANDOM_BYTES];
    rng.fill(&mut bytes)
        .map_err(|e| TicketError::Random(e.to_string()))?;
    Ok(BASE64URL_NOPAD.encode(&bytes))
}

/// Remove every entry whose expiry has passed.
fn sweep(map: &mut HashMap<String, StoredTicket>) {
    let now = Utc::now();
    map.retain(|_, stored| stored.expires_at > now);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn dummy_authority() -> TicketAuthority {
        TicketAuthority {
            bearer: DeviceId::new("device-abc".to_owned()),
            capability: Capability::ExecPty,
            scope: Scope::folder("work"),
        }
    }

    #[test]
    fn issue_and_redeem_returns_authority() {
        let store = TicketStore::new();
        let resp = store.issue(dummy_authority()).unwrap();
        let auth = store.redeem(&resp.ticket).unwrap();
        assert_eq!(auth.bearer.as_str(), "device-abc");
        assert_eq!(auth.capability, Capability::ExecPty);
    }

    #[test]
    fn ticket_is_single_use() {
        let store = TicketStore::new();
        let resp = store.issue(dummy_authority()).unwrap();
        let _first = store.redeem(&resp.ticket).unwrap();
        let second = store.redeem(&resp.ticket);
        assert!(matches!(second, Err(TicketError::NotFound)));
    }

    #[test]
    fn unknown_ticket_is_not_found() {
        let store = TicketStore::new();
        let result = store.redeem("does-not-exist");
        assert!(matches!(result, Err(TicketError::NotFound)));
    }

    #[test]
    fn expired_ticket_is_rejected_and_swept() {
        let store = TicketStore::new();
        // Insert an already-expired ticket directly into the map.
        let ticket = "manually-inserted".to_owned();
        {
            let mut map = store.inner.lock().unwrap();
            map.insert(
                ticket.clone(),
                StoredTicket {
                    authority: dummy_authority(),
                    expires_at: Utc::now() - Duration::seconds(1),
                },
            );
        }
        let result = store.redeem(&ticket);
        // redeem sweeps before looking up, so the expired entry is removed and
        // the lookup returns NotFound (not Expired).
        assert!(matches!(result, Err(TicketError::NotFound)));
    }

    #[test]
    fn issued_tickets_are_unique() {
        let store = TicketStore::new();
        let a = store.issue(dummy_authority()).unwrap();
        let b = store.issue(dummy_authority()).unwrap();
        assert_ne!(a.ticket, b.ticket);
    }

    #[test]
    fn tickets_are_32_bytes_encoded() {
        let store = TicketStore::new();
        let resp = store.issue(dummy_authority()).unwrap();
        // 32 bytes encoded as base64url without padding = 43 characters.
        assert_eq!(resp.ticket.len(), 43);
    }
}
