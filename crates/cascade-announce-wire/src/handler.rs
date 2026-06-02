//! Stateless announce-directory request handling.
//!
//! This is the host-testable core of the announce Worker: the routing, the HMAC
//! write authentication, the request-size bounds, and the blob round-trip
//! against a storage contract. It holds nothing between requests — the soft-state
//! directory lives entirely in the injected [`BlobStore`] (Workers KV in
//! production), so the same function answers each request independently. Keeping
//! it here, in a workspace crate, means it is exercised by `cargo test
//! --workspace` on the native target against an in-memory store double; the
//! wasm-only Worker crate is then a thin adapter that maps `worker::Request` onto
//! [`handle`] and the KV binding onto [`BlobStore`].
//!
//! The Worker is a *blind, untrusted carrier* of the stored
//! [`crate::signing::SignedCandidates`] envelope: it never inspects, verifies, or
//! vouches for the candidates inside — the looking-up client verifies the
//! signature on read, which is what makes the blob self-certifying. Storing a
//! blob therefore does not require trusting it. The Worker still authenticates
//! *writers* with the shared-secret HMAC (so only holders of the secret may
//! populate the directory) and rejects oversized or malformed input loudly,
//! because a blind carrier cannot assume a hostile poster capped its own request.

// The `BlobStore` contract must accommodate the announce Worker's production
// store, whose futures are `!Send` by construction: the Workers KV binding wraps
// single-threaded JS handles (`Rc`/`RefCell`) and the Workers runtime has no
// threads. A `Send` bound on the contract's futures would therefore make the
// production implementation impossible. `handle`/`register` are consequently
// `!Send` when instantiated with such a store, which `clippy::future_not_send`
// flags — but Send-ness here is a wasm-target FFI constraint, not a code smell,
// so it is relaxed for this module rather than worked around with a bound that
// breaks the only real consumer. The native store double in the tests is `Send`,
// so the host tests are unaffected.
#![allow(clippy::future_not_send)]

use crate::auth::{self, SHARED_SECRET_LEN};
use crate::wire::{AnnounceRequest, LookupResponse, MAX_ANNOUNCE_CANDIDATES};

/// Upper bound, in bytes, on a single JSON-encoded [`AnnounceRequest`] body the
/// register route will read off the wire.
///
/// Derived from the per-device candidate cap, not picked arbitrarily: it is the
/// cap times a generous per-candidate JSON budget, plus a fixed envelope budget
/// for the device id, the base64 signature, the expiry, and JSON punctuation. A
/// legitimately-capped set always fits; the precise count check below is the
/// exact gate, this is the coarse one that refuses a body too large to even
/// buffer.
pub const MAX_ANNOUNCE_REQUEST_BYTES: usize =
    MAX_ANNOUNCE_CANDIDATES * MAX_WIRE_CANDIDATE_JSON_BYTES + ANNOUNCE_REQUEST_ENVELOPE_JSON_BYTES;

/// Per-candidate JSON budget. A `WireCandidate` serialises as
/// `{"address":"[<ipv6>]:<port>","kind":<u8>,"priority":<u32>}`; the widest IPv6
/// socket-address string, the three-digit `kind`, the ten-digit `priority`, the
/// field names, and the punctuation all fit inside this budget with headroom.
const MAX_WIRE_CANDIDATE_JSON_BYTES: usize = 96;

/// Fixed JSON budget for everything that is not a candidate: the
/// `{"signed":{...}}` wrapping, the `device_id` string (a base32 SHA-256, 52
/// bytes), the `expires_at_unix_ms` integer, the 88-byte base64 signature
/// string, and all field names and punctuation. Sized with headroom so the limit
/// never clips an honest request.
const ANNOUNCE_REQUEST_ENVELOPE_JSON_BYTES: usize = 512;

/// Soft-state blob storage contract.
///
/// The directory stores the opaque [`crate::signing::SignedCandidates`] JSON blob
/// keyed by device id, with a per-write expiry. No durability is required: an
/// announcer republishes on its loop, so a lost entry simply forces a fresh
/// announce. The production implementation (in the Worker crate) is Workers KV
/// with `expiration_ttl`; the tests use an in-memory double. The contract is
/// async and trades only owned bytes, so it holds whether the store is a local
/// map or a remote KV namespace.
pub trait BlobStore {
    /// The store's own error type, surfaced verbatim so the Worker can map a KV
    /// failure to a `503` without this crate depending on the KV error type.
    type Error;

    /// Store `value` under `device_id`, expiring after `ttl_seconds`.
    ///
    /// Replaces any existing value for the id in full (the announce contract's
    /// replace-in-full semantics). The blob is stored verbatim — the store never
    /// inspects it.
    fn put(
        &self,
        device_id: &str,
        value: &[u8],
        ttl_seconds: u64,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>>;

    /// Read the blob last stored for `device_id`, or `None` when the id is
    /// unknown or its entry has expired.
    fn get(
        &self,
        device_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, Self::Error>>;
}

/// HTTP method the handler routes on. Only `GET` and `POST` are meaningful for
/// the two announce routes; anything else is [`Outcome::MethodNotAllowed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET /announce/<device_id>` — lookup.
    Get,
    /// `POST /announce/<device_id>` — register.
    Post,
    /// Any other method — rejected.
    Other,
}

/// The handler's decision, as a transport-agnostic value.
///
/// The Worker glue maps each variant to a `worker::Response`. Modelling the
/// outcome as data (rather than building a `worker::Response` here) is what keeps
/// the routing, auth, and size logic testable on the native target with no
/// `worker` dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Lookup result: the JSON-encoded [`LookupResponse`] body to return with
    /// `200 OK`. A known id carries its signed blob; an unknown id carries
    /// `signed: null`.
    LookupBody(Vec<u8>),
    /// A registration was accepted and stored. The Worker returns `204 No
    /// Content`.
    Registered,
    /// The request method is not `GET` or `POST` on the announce route.
    MethodNotAllowed,
    /// The path was not `/announce/<device_id>` with a non-empty id.
    NotFound,
    /// The register body was missing, malformed JSON, or not a valid
    /// `AnnounceRequest`. Carries a short reason for the `400` body.
    BadRequest(&'static str),
    /// The register body (or its candidate count) exceeded the cap. The Worker
    /// returns `413 Payload Too Large`.
    PayloadTooLarge,
    /// The register request failed HMAC write authentication: the header was
    /// missing, malformed, or did not verify. The Worker returns `401
    /// Unauthorized`. Modelled as one outcome (not distinguishing the three) so
    /// the response never reveals *why* auth failed to an unauthenticated caller.
    Unauthorized,
    /// The backing store failed (a KV error). The Worker returns `503 Service
    /// Unavailable`.
    StorageError,
}

/// Handle one announce-directory request statelessly against `store`.
///
/// Routing:
/// - `GET /announce/<id>` reads the stored blob and returns a [`LookupResponse`]
///   (`signed: null` for an unknown id — absence is modelled as "no candidates",
///   never a `404`).
/// - `POST /announce/<id>` authenticates the writer (HMAC over the path id and
///   the exact body), bounds the body size and candidate count, then stores the
///   signed blob verbatim with `ttl_seconds` expiry.
///
/// `device_id` is the path component (already URL-decoded by the caller).
/// `auth_header` is the raw [`crate::auth::ANNOUNCE_AUTH_HEADER`] value, if
/// present. `body` is
/// the raw request body bytes (empty for `GET`). Every rejection is explicit and
/// loud; nothing is silently skipped or defaulted.
pub async fn handle<S: BlobStore>(
    store: &S,
    method: Method,
    device_id: &str,
    auth_header: Option<&str>,
    body: &[u8],
    secret: &[u8; SHARED_SECRET_LEN],
    ttl_seconds: u64,
) -> Outcome {
    if device_id.is_empty() {
        return Outcome::NotFound;
    }
    match method {
        Method::Get => store
            .get(device_id)
            .await
            .map_or(Outcome::StorageError, |stored| {
                lookup_body(stored.as_deref())
            }),
        Method::Post => register(store, device_id, auth_header, body, secret, ttl_seconds).await,
        Method::Other => Outcome::MethodNotAllowed,
    }
}

/// Build the `200` lookup body from an optional stored blob.
///
/// A stored blob is wrapped verbatim in a [`LookupResponse`] (the carrier never
/// re-derives or re-orders it); an absent one yields `signed: null`. Serialising
/// the response cannot realistically fail for this fixed shape, but a failure is
/// surfaced as a storage error rather than panicking.
fn lookup_body(stored: Option<&[u8]>) -> Outcome {
    // A blob that no longer parses as a SignedCandidates envelope is treated as
    // absent rather than served as garbage — the only way a stored value is
    // malformed is a wire-format change, and a resolver would reject it anyway.
    let signed = stored.and_then(|bytes| serde_json::from_slice(bytes).ok());
    serde_json::to_vec(&LookupResponse { signed })
        .map_or(Outcome::StorageError, Outcome::LookupBody)
}

/// Authenticate, bound, and store a registration.
async fn register<S: BlobStore>(
    store: &S,
    device_id: &str,
    auth_header: Option<&str>,
    body: &[u8],
    secret: &[u8; SHARED_SECRET_LEN],
    ttl_seconds: u64,
) -> Outcome {
    // Refuse a body too large to buffer before doing any work — the coarse gate
    // backstopping the precise candidate-count check below.
    if body.len() > MAX_ANNOUNCE_REQUEST_BYTES {
        return Outcome::PayloadTooLarge;
    }

    // Authenticate the writer: the HMAC binds the path device id and the exact
    // body bytes, so neither a missing/forged tag nor a man-in-the-middle body
    // swap passes. A missing header, a malformed tag, and a non-verifying tag all
    // collapse to one Unauthorized outcome so an unauthenticated caller learns
    // nothing about why it failed.
    let Some(header) = auth_header else {
        return Outcome::Unauthorized;
    };
    match auth::verify_announce_write(secret, device_id, body, header) {
        Ok(true) => {}
        Ok(false) | Err(_) => return Outcome::Unauthorized,
    }

    // Parse only after authentication: an unauthenticated caller never reaches
    // the JSON parser.
    let request: AnnounceRequest = match serde_json::from_slice(body) {
        Ok(parsed) => parsed,
        Err(_) => return Outcome::BadRequest("body is not a valid AnnounceRequest"),
    };

    // Bound per-device storage by the candidate count. Reading the length is the
    // one structural fact the carrier may act on without trusting the envelope;
    // truncating would invalidate the signature, so an over-cap set is rejected,
    // not trimmed.
    if request.signed.candidates.len() > MAX_ANNOUNCE_CANDIDATES {
        return Outcome::PayloadTooLarge;
    }

    // Store the signed envelope (the inner `SignedCandidates`), which is what a
    // lookup serves back inside a `LookupResponse`. The candidates' signature
    // covers exactly this envelope, so a stored blob round-trips and still
    // verifies on the resolver. The HMAC authenticated the whole request body
    // (envelope + wrapper); peeling the wrapper here does not weaken that — the
    // wrapper carries no signed data of its own.
    let Ok(envelope) = serde_json::to_vec(&request.signed) else {
        return Outcome::BadRequest("could not re-encode the signed envelope");
    };
    store
        .put(device_id, &envelope, ttl_seconds)
        .await
        .map_or(Outcome::StorageError, |()| Outcome::Registered)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::convert::Infallible;
    use std::net::SocketAddr;

    use crate::auth::{announce_write_tag, encode_hex};
    use crate::candidate::WireCandidate;
    use crate::signing::SignedCandidates;

    /// In-memory [`BlobStore`] double. Single-threaded (the tests drive it
    /// directly), so a `RefCell` map suffices — no async runtime needed for the
    /// trivially-ready futures.
    #[derive(Default)]
    struct MemStore {
        entries: RefCell<HashMap<String, Vec<u8>>>,
        /// When set, every operation fails, exercising the storage-error path.
        fail: bool,
    }

    impl MemStore {
        fn failing() -> Self {
            Self {
                fail: true,
                ..Self::default()
            }
        }
    }

    impl BlobStore for MemStore {
        type Error = &'static str;

        async fn put(
            &self,
            device_id: &str,
            value: &[u8],
            _ttl_seconds: u64,
        ) -> Result<(), Self::Error> {
            if self.fail {
                return Err("storage down");
            }
            self.entries
                .borrow_mut()
                .insert(device_id.to_owned(), value.to_vec());
            Ok(())
        }

        async fn get(&self, device_id: &str) -> Result<Option<Vec<u8>>, Self::Error> {
            if self.fail {
                return Err("storage down");
            }
            Ok(self.entries.borrow().get(device_id).cloned())
        }
    }

    const TTL_SECONDS: u64 = 3600;

    fn secret() -> [u8; SHARED_SECRET_LEN] {
        let mut s = [0u8; SHARED_SECRET_LEN];
        for (idx, byte) in s.iter_mut().enumerate() {
            *byte = u8::try_from(idx).unwrap_or(0);
        }
        s
    }

    fn wire(port: u16) -> WireCandidate {
        WireCandidate {
            address: SocketAddr::from(([127, 0, 0, 1], port)),
            kind: 0,
            priority: 0,
        }
    }

    /// Build an authenticated register `(body, header)` pair for `device_id`.
    fn signed_request(device_id: &str, ports: &[u16]) -> (Vec<u8>, String) {
        let candidates = ports.iter().copied().map(wire).collect();
        let signed = SignedCandidates::sign(device_id, candidates, 1_700_000_000_000);
        let request = AnnounceRequest { signed };
        let body = serde_json::to_vec(&request).unwrap();
        let tag = announce_write_tag(&secret(), device_id, &body).unwrap();
        (body, encode_hex(&tag))
    }

    #[tokio::test]
    async fn register_then_lookup_round_trips_the_blob() {
        let store = MemStore::default();
        let (body, header) = signed_request("DEVICE-A", &[22000, 33000]);

        let registered = handle(
            &store,
            Method::Post,
            "DEVICE-A",
            Some(&header),
            &body,
            &secret(),
            TTL_SECONDS,
        )
        .await;
        assert_eq!(registered, Outcome::Registered);

        let lookup = handle(
            &store,
            Method::Get,
            "DEVICE-A",
            None,
            &[],
            &secret(),
            TTL_SECONDS,
        )
        .await;
        let Outcome::LookupBody(json) = lookup else {
            panic!("expected a lookup body, got {lookup:?}");
        };
        let response: LookupResponse = serde_json::from_slice(&json).unwrap();
        let signed = response.signed.expect("known id returns its blob");
        // The stored blob is the exact bytes the writer signed: it still verifies.
        assert!(signed.verify("DEVICE-A", 1_699_999_999_000).is_ok());
    }

    #[tokio::test]
    async fn lookup_unknown_id_yields_signed_null_not_404() {
        let store = MemStore::default();
        let lookup = handle(
            &store,
            Method::Get,
            "NEVER-REGISTERED",
            None,
            &[],
            &secret(),
            TTL_SECONDS,
        )
        .await;
        let Outcome::LookupBody(json) = lookup else {
            panic!("expected a lookup body, got {lookup:?}");
        };
        let response: LookupResponse = serde_json::from_slice(&json).unwrap();
        assert!(response.signed.is_none());
    }

    #[tokio::test]
    async fn register_without_an_auth_header_is_unauthorized_and_stores_nothing() {
        let store = MemStore::default();
        let (body, _header) = signed_request("DEVICE-A", &[22000]);
        let outcome = handle(
            &store,
            Method::Post,
            "DEVICE-A",
            None,
            &body,
            &secret(),
            TTL_SECONDS,
        )
        .await;
        assert_eq!(outcome, Outcome::Unauthorized);
        assert!(store.entries.borrow().is_empty());
    }

    #[tokio::test]
    async fn register_with_a_wrong_secret_is_unauthorized() {
        let store = MemStore::default();
        let (body, header) = signed_request("DEVICE-A", &[22000]);
        let mut wrong = secret();
        wrong[0] ^= 0xFF;
        let outcome = handle(
            &store,
            Method::Post,
            "DEVICE-A",
            Some(&header),
            &body,
            &wrong,
            TTL_SECONDS,
        )
        .await;
        assert_eq!(outcome, Outcome::Unauthorized);
        assert!(store.entries.borrow().is_empty());
    }

    #[tokio::test]
    async fn register_with_a_swapped_body_is_unauthorized() {
        // The tag authenticates DEVICE-A's body; a man-in-the-middle swaps the
        // body for a different (validly-signed) one. The recomputed tag fails.
        let store = MemStore::default();
        let (_body, header) = signed_request("DEVICE-A", &[22000]);
        let (other_body, _other_header) = signed_request("DEVICE-A", &[44000]);
        let outcome = handle(
            &store,
            Method::Post,
            "DEVICE-A",
            Some(&header),
            &other_body,
            &secret(),
            TTL_SECONDS,
        )
        .await;
        assert_eq!(outcome, Outcome::Unauthorized);
    }

    #[tokio::test]
    async fn register_for_a_different_path_id_than_the_tag_is_unauthorized() {
        // The tag binds DEVICE-A; posting it to the DEVICE-B path must fail, so a
        // captured tag cannot be replayed onto another id's key.
        let store = MemStore::default();
        let (body, header) = signed_request("DEVICE-A", &[22000]);
        let outcome = handle(
            &store,
            Method::Post,
            "DEVICE-B",
            Some(&header),
            &body,
            &secret(),
            TTL_SECONDS,
        )
        .await;
        assert_eq!(outcome, Outcome::Unauthorized);
    }

    #[tokio::test]
    async fn register_with_a_malformed_auth_header_is_unauthorized() {
        let store = MemStore::default();
        let (body, _header) = signed_request("DEVICE-A", &[22000]);
        let outcome = handle(
            &store,
            Method::Post,
            "DEVICE-A",
            Some("not-hex"),
            &body,
            &secret(),
            TTL_SECONDS,
        )
        .await;
        assert_eq!(outcome, Outcome::Unauthorized);
    }

    #[tokio::test]
    async fn authenticated_but_unparseable_body_is_a_bad_request() {
        // Authenticate over garbage bytes so auth passes but the JSON parse
        // fails — the parser only runs after authentication.
        let store = MemStore::default();
        let body = b"not json at all".to_vec();
        let tag = announce_write_tag(&secret(), "DEVICE-A", &body).unwrap();
        let outcome = handle(
            &store,
            Method::Post,
            "DEVICE-A",
            Some(&encode_hex(&tag)),
            &body,
            &secret(),
            TTL_SECONDS,
        )
        .await;
        assert!(matches!(outcome, Outcome::BadRequest(_)));
    }

    #[tokio::test]
    async fn register_over_the_candidate_cap_is_payload_too_large_and_stores_nothing() {
        let store = MemStore::default();
        let ports: Vec<u16> = (0..=u16::try_from(MAX_ANNOUNCE_CANDIDATES).unwrap())
            .map(|i| 22000u16.wrapping_add(i))
            .collect();
        assert_eq!(ports.len(), MAX_ANNOUNCE_CANDIDATES + 1);
        let (body, header) = signed_request("DEVICE-A", &ports);
        // The over-cap body is still under the coarse byte ceiling, so the
        // precise count check is what rejects it.
        assert!(body.len() <= MAX_ANNOUNCE_REQUEST_BYTES);
        let outcome = handle(
            &store,
            Method::Post,
            "DEVICE-A",
            Some(&header),
            &body,
            &secret(),
            TTL_SECONDS,
        )
        .await;
        assert_eq!(outcome, Outcome::PayloadTooLarge);
        assert!(store.entries.borrow().is_empty());
    }

    #[tokio::test]
    async fn register_at_the_candidate_cap_is_accepted() {
        let store = MemStore::default();
        let ports: Vec<u16> = (0..u16::try_from(MAX_ANNOUNCE_CANDIDATES).unwrap())
            .map(|i| 22000u16.wrapping_add(i))
            .collect();
        let (body, header) = signed_request("DEVICE-A", &ports);
        let outcome = handle(
            &store,
            Method::Post,
            "DEVICE-A",
            Some(&header),
            &body,
            &secret(),
            TTL_SECONDS,
        )
        .await;
        assert_eq!(outcome, Outcome::Registered);
    }

    #[tokio::test]
    async fn an_oversized_body_is_payload_too_large_before_any_auth() {
        let store = MemStore::default();
        let body = vec![0u8; MAX_ANNOUNCE_REQUEST_BYTES + 1];
        // No auth header at all: the size gate fires first, so this is a 413, not
        // a 401 — the coarse limit refuses a body too large to even buffer.
        let outcome = handle(
            &store,
            Method::Post,
            "DEVICE-A",
            None,
            &body,
            &secret(),
            TTL_SECONDS,
        )
        .await;
        assert_eq!(outcome, Outcome::PayloadTooLarge);
    }

    #[tokio::test]
    async fn an_unknown_method_is_method_not_allowed() {
        let store = MemStore::default();
        let outcome = handle(
            &store,
            Method::Other,
            "DEVICE-A",
            None,
            &[],
            &secret(),
            TTL_SECONDS,
        )
        .await;
        assert_eq!(outcome, Outcome::MethodNotAllowed);
    }

    #[tokio::test]
    async fn an_empty_device_id_is_not_found() {
        let store = MemStore::default();
        let outcome = handle(&store, Method::Get, "", None, &[], &secret(), TTL_SECONDS).await;
        assert_eq!(outcome, Outcome::NotFound);
    }

    #[tokio::test]
    async fn a_lookup_against_a_failing_store_is_a_storage_error() {
        let store = MemStore::failing();
        let outcome = handle(
            &store,
            Method::Get,
            "DEVICE-A",
            None,
            &[],
            &secret(),
            TTL_SECONDS,
        )
        .await;
        assert_eq!(outcome, Outcome::StorageError);
    }

    #[tokio::test]
    async fn a_register_against_a_failing_store_is_a_storage_error() {
        let store = MemStore::failing();
        let (body, header) = signed_request("DEVICE-A", &[22000]);
        let outcome = handle(
            &store,
            Method::Post,
            "DEVICE-A",
            Some(&header),
            &body,
            &secret(),
            TTL_SECONDS,
        )
        .await;
        assert_eq!(outcome, Outcome::StorageError);
    }

    // Exercises that the trait's associated error can be any type — `Infallible`
    // here — without the handler caring beyond mapping it to a storage error.
    struct InfallibleStore;

    impl BlobStore for InfallibleStore {
        type Error = Infallible;

        async fn put(&self, _: &str, _: &[u8], _: u64) -> Result<(), Infallible> {
            Ok(())
        }

        async fn get(&self, _: &str) -> Result<Option<Vec<u8>>, Infallible> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn handler_is_generic_over_the_store_error_type() {
        let store = InfallibleStore;
        let outcome = handle(
            &store,
            Method::Get,
            "DEVICE-A",
            None,
            &[],
            &secret(),
            TTL_SECONDS,
        )
        .await;
        assert!(matches!(outcome, Outcome::LookupBody(_)));
    }
}
