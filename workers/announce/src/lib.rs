//! Stateless announce-directory Cloudflare Worker.
//!
//! This crate is the thin wasm-only glue that hosts cascade's announce-server
//! HTTP contract on Cloudflare's edge with zero durable state. All the routing,
//! HMAC write authentication, size bounds, and blob round-trip logic live in
//! [`cascade_announce_wire::handler`], a workspace crate exercised on the native
//! target by `cargo test --workspace`; this file only adapts a `worker::Request`
//! onto [`cascade_announce_wire::handler::handle`] and a Workers KV namespace
//! onto its [`cascade_announce_wire::handler::BlobStore`] contract.
//!
//! ## Why soft state is enough
//!
//! The directory is a rendezvous hint, not a source of truth: announcers
//! republish on a loop, so a dropped entry simply forces a fresh announce on the
//! next tick. KV's eventual-consistency and per-key expiry are therefore exactly
//! the right substrate — every write carries an `expiration_ttl` matching the
//! republish cadence, so an entry whose device stops announcing ages out on its
//! own and the Worker holds nothing between requests.
//!
//! ## The Worker authenticates writers but does not trust the blob
//!
//! The stored [`cascade_announce_wire::SignedCandidates`] envelope is
//! self-certifying — the looking-up client verifies its signature on read — so
//! the Worker is a blind carrier of the candidates and never inspects them.
//! Storing a blob does not require trusting it. The Worker still gates *who* may
//! write with the shared-secret HMAC (bound to the device id and the exact body)
//! and rejects oversized or malformed input loudly, because a blind carrier
//! cannot assume a hostile poster capped its own request.
//!
//! ## Bindings
//!
//! - KV namespace `ANNOUNCE` — the soft-state blob store.
//! - Secret `ANNOUNCE_SHARED_SECRET` — the 64-char hex `HMAC` key writers
//!   authenticate with. A missing or malformed secret fails every write closed
//!   (`503`), never open.
//! - Optional var `ANNOUNCE_TTL_SECONDS` — the per-write KV expiry; defaults to
//!   an hour when unset.

use cascade_announce_wire::auth::{
    ANNOUNCE_AUTH_HEADER, SHARED_SECRET_LEN, parse_shared_secret_hex,
};
use cascade_announce_wire::handler::{self, BlobStore, Method, Outcome};
use worker::{Env, Request, Response, Result, event, kv::KvStore};

/// Name of the KV namespace binding holding the soft-state directory.
const KV_BINDING: &str = "ANNOUNCE";

/// Name of the secret binding holding the hex `HMAC` write key.
const SECRET_BINDING: &str = "ANNOUNCE_SHARED_SECRET";

/// Name of the optional var binding overriding the per-write KV expiry.
const TTL_VAR: &str = "ANNOUNCE_TTL_SECONDS";

/// Default per-write KV expiry, in seconds.
///
/// Cloudflare KV enforces a 60-second floor on `expiration_ttl`; an hour sits
/// well above the announcer's republish cadence so a live device's entry is
/// always replaced before it lapses, while a stale capture ages out within the
/// hour. The announce client's own TTL is an hour, so the two agree.
const DEFAULT_TTL_SECONDS: u64 = 3600;

/// Path prefix every announce route is rooted at: `/announce/<device_id>`.
const ANNOUNCE_PREFIX: &str = "/announce/";

/// Workers KV implementation of the soft-state [`BlobStore`] contract.
///
/// Every write sets `expiration_ttl` so the directory ages itself out with no
/// background job and no durable state.
struct KvBlobStore {
    kv: KvStore,
    ttl_seconds: u64,
}

impl BlobStore for KvBlobStore {
    type Error = worker::kv::KvError;

    async fn put(
        &self,
        device_id: &str,
        value: &[u8],
        ttl_seconds: u64,
    ) -> std::result::Result<(), Self::Error> {
        self.kv
            .put_bytes(device_id, value)?
            .expiration_ttl(ttl_seconds)
            .execute()
            .await
    }

    async fn get(&self, device_id: &str) -> std::result::Result<Option<Vec<u8>>, Self::Error> {
        self.kv.get(device_id).bytes().await
    }
}

/// Worker entrypoint. Resolves the bindings, routes the request through the
/// shared stateless handler, and maps the [`Outcome`] back to a
/// `worker::Response`.
#[event(fetch)]
pub async fn fetch(mut req: Request, env: Env, _ctx: worker::Context) -> Result<Response> {
    // A missing or malformed secret means the Worker cannot authenticate any
    // writer; it must fail closed (every write 503) rather than admit unsigned
    // writes. Resolve it once per request.
    let Some(secret) = resolve_secret(&env) else {
        return Response::error("announce shared secret not configured", 503);
    };

    let Ok(kv) = env.kv(KV_BINDING) else {
        return Response::error("announce KV namespace not bound", 503);
    };
    let store = KvBlobStore {
        kv,
        ttl_seconds: resolve_ttl(&env),
    };

    let Some(device_id) = device_id_from_path(&req.path()) else {
        return Response::error("not found", 404);
    };

    let method = method_of(&req);
    let header = req.headers().get(ANNOUNCE_AUTH_HEADER).ok().flatten();
    // Only POST carries a body; reading it for GET would be wasteful, and the
    // handler ignores the body on GET anyway.
    let body = match method {
        Method::Post => req.bytes().await.unwrap_or_default(),
        Method::Get | Method::Other => Vec::new(),
    };

    let outcome = handler::handle(
        &store,
        method,
        &device_id,
        header.as_deref(),
        &body,
        &secret,
        store.ttl_seconds,
    )
    .await;

    outcome_to_response(outcome)
}

/// Resolve the hex `HMAC` secret from the secret binding, or `None` when it is
/// absent or not a valid 32-byte hex key.
fn resolve_secret(env: &Env) -> Option<[u8; SHARED_SECRET_LEN]> {
    let hex = env.secret(SECRET_BINDING).ok()?.to_string();
    parse_shared_secret_hex(&hex).ok()
}

/// Resolve the per-write TTL from the optional var, falling back to
/// [`DEFAULT_TTL_SECONDS`] when the var is unset or not a positive integer.
fn resolve_ttl(env: &Env) -> u64 {
    let Ok(var) = env.var(TTL_VAR) else {
        return DEFAULT_TTL_SECONDS;
    };
    var.to_string()
        .parse::<u64>()
        .unwrap_or(DEFAULT_TTL_SECONDS)
}

/// Extract the `<device_id>` component of a `/announce/<device_id>` path.
///
/// Returns `None` for any other path, or when the id component is empty, so a
/// malformed path is a `404` rather than an empty-id lookup. A trailing segment
/// containing a further slash is rejected — the contract is a single id, not a
/// nested path.
fn device_id_from_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix(ANNOUNCE_PREFIX)?;
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some(rest.to_owned())
}

/// Map a `worker::Request` method onto the handler's [`Method`].
fn method_of(req: &Request) -> Method {
    match req.method() {
        worker::Method::Get => Method::Get,
        worker::Method::Post => Method::Post,
        _ => Method::Other,
    }
}

/// Map the handler's transport-agnostic [`Outcome`] onto a `worker::Response`.
///
/// The status codes mirror the relay-server's axum announce endpoint so the two
/// hosts of the same contract behave identically: `204` on a stored
/// registration, `200` with the JSON body on a lookup, `404`/`405`/`400`/`401`/
/// `413`/`503` for the respective rejections.
fn outcome_to_response(outcome: Outcome) -> Result<Response> {
    match outcome {
        Outcome::LookupBody(json) => {
            let mut response = Response::from_bytes(json)?;
            response
                .headers_mut()
                .set("content-type", "application/json")?;
            Ok(response)
        }
        Outcome::Registered => Ok(Response::empty()?.with_status(204)),
        Outcome::MethodNotAllowed => Response::error("method not allowed", 405),
        Outcome::NotFound => Response::error("not found", 404),
        Outcome::BadRequest(reason) => Response::error(reason, 400),
        Outcome::PayloadTooLarge => Response::error("payload too large", 413),
        Outcome::Unauthorized => Response::error("unauthorized", 401),
        Outcome::StorageError => Response::error("storage unavailable", 503),
    }
}
