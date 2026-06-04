//! Per-request id middleware.
//!
//! Every request reaches a handler with a request id — echoed from the caller's
//! `X-Cascade-Request-Id` header when it parses as a valid 26-character base32
//! (Crockford) token, or minted by the daemon otherwise. The id is stamped on
//! every response header and is the value support tickets quote against the
//! daemon log.
//!
//! This middleware also *finalises* error responses: a handler returns an
//! [`ApiError`](crate::error::ApiError) carrying an
//! [`ApiErrorPayload`] carried in the response
//! extensions, and this layer folds the request id into the envelope body so no
//! handler has to thread the id through itself.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use data_encoding::{Encoding, Specification};
use sha2::{Digest, Sha256};

use crate::error::ApiErrorPayload;

/// The response (and request) header carrying the per-request id.
pub const REQUEST_ID_HEADER: &str = "x-cascade-request-id";

/// The fixed character length of a request id: 16 bytes (128 bits) encoded as
/// base32 with no padding is exactly 26 symbols.
pub const REQUEST_ID_LEN: usize = 26;

/// The number of random-ish bytes a minted id encodes. 128 bits is collision-
/// resistant for the lifetime of any daemon process.
const REQUEST_ID_BYTES: usize = 16;

/// The verified request id carried in request extensions for handlers that want
/// it (none of the success bodies include it, but the error finaliser reads it).
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

impl<S: Sync> FromRequestParts<S> for RequestId {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // The middleware inserts the id into the extensions before any handler
        // runs; fall back to a fresh id only if the layer was somehow bypassed.
        Ok(parts
            .extensions
            .get::<Self>()
            .cloned()
            .unwrap_or_else(|| Self(mint_request_id())))
    }
}

/// The Crockford base32 alphabet (no `I`, `L`, `O`, `U`), with case-insensitive
/// decoding and the Crockford `I`/`L`→`1`, `O`→`0` aliases folded in, built once.
fn crockford() -> &'static Encoding {
    static ENCODING: OnceLock<Encoding> = OnceLock::new();
    ENCODING.get_or_init(|| {
        let mut spec = Specification::new();
        spec.symbols.push_str("0123456789ABCDEFGHJKMNPQRSTVWXYZ");
        // Decode is case-insensitive and accepts the Crockford ambiguity
        // aliases: lowercase maps to its uppercase symbol, and I/L→1, O→0.
        spec.translate.from.push_str("abcdefghjkmnpqrstvwxyzILilOo");
        spec.translate.to.push_str("ABCDEFGHJKMNPQRSTVWXYZ111100");
        // A valid specification by construction (32 distinct symbols, no
        // padding); a malformed spec is a programming error caught in tests.
        spec.encoding()
            .unwrap_or_else(|e| panic!("Crockford base32 specification is invalid: {e}"))
    })
}

/// Whether `candidate` is a syntactically valid request id: exactly
/// [`REQUEST_ID_LEN`] characters that decode under the Crockford alphabet.
#[must_use]
pub fn is_valid_request_id(candidate: &str) -> bool {
    candidate.len() == REQUEST_ID_LEN && crockford().decode(candidate.as_bytes()).is_ok()
}

/// Mint a fresh request id.
///
/// Uniqueness within a daemon process comes from a monotonic counter folded with
/// the wall clock in nanoseconds and hashed; the id is opaque and carries no
/// authority, so a cryptographic source is unnecessary — only non-collision
/// within the process matters.
#[must_use]
pub fn mint_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());

    let mut hasher = Sha256::new();
    hasher.update(counter.to_be_bytes());
    hasher.update(nanos.to_be_bytes());
    let digest = hasher.finalize();
    let prefix = digest.get(..REQUEST_ID_BYTES).unwrap_or(&digest);
    crockford().encode(prefix)
}

/// Resolve the request id for an incoming request: echo a valid caller-supplied
/// `X-Cascade-Request-Id`, otherwise mint one.
fn resolve_request_id(req: &Request) -> String {
    req.headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|candidate| is_valid_request_id(candidate))
        .map_or_else(mint_request_id, ToOwned::to_owned)
}

/// The middleware: resolve the id, expose it to handlers, run the request, then
/// stamp the id on the response and finalise any error envelope.
pub async fn middleware(mut req: Request, next: Next) -> Response {
    let id = resolve_request_id(&req);
    req.extensions_mut().insert(RequestId(id.clone()));

    let response = next.run(req).await;
    let mut response = finalise_error_body(response, &id);

    if let Ok(header_value) = HeaderValue::from_str(&id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), header_value);
    }
    response
}

/// If the response carries an [`ApiErrorPayload`] (placed there by
/// [`ApiError::into_response`](crate::error::ApiError)), rebuild the body as the
/// full error envelope with the request id folded in. Otherwise pass it through
/// unchanged.
fn finalise_error_body(response: Response, request_id: &str) -> Response {
    let Some(payload) = response.extensions().get::<ApiErrorPayload>().cloned() else {
        return response;
    };

    let mut error = serde_json::Map::new();
    error.insert("code".to_owned(), payload.code.wire().into());
    error.insert("message".to_owned(), payload.message.into());
    error.insert("request_id".to_owned(), request_id.into());
    // Details are omitted from the envelope when absent, per the contract.
    if let Some(details) = payload.details {
        error.insert("details".to_owned(), details);
    }
    let body = serde_json::json!({ "error": error });

    (payload.code.status(), axum::Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_ids_are_valid_and_unique() {
        let a = mint_request_id();
        let b = mint_request_id();
        assert_eq!(a.len(), REQUEST_ID_LEN);
        assert!(is_valid_request_id(&a));
        assert!(is_valid_request_id(&b));
        assert_ne!(a, b, "consecutive ids must differ");
    }

    #[test]
    fn rejects_wrong_length_and_alphabet() {
        assert!(!is_valid_request_id("too-short"));
        assert!(!is_valid_request_id(&"A".repeat(REQUEST_ID_LEN - 1)));
        // `U` is not in the Crockford alphabet.
        assert!(!is_valid_request_id(&"U".repeat(REQUEST_ID_LEN)));
    }

    #[test]
    fn accepts_a_minted_id_round_trip() {
        let id = mint_request_id();
        assert!(is_valid_request_id(&id));
    }
}
