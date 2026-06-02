//! Announce-server discovery source.
//!
//! The LAN multicast ([`super::lan`]) and introducer-gossip
//! ([`super::gossip`]) sources reach peers that are either on the same
//! network segment or transitively known through a trusted device. Neither
//! helps two devices that have never met and sit on different networks. The
//! announce server closes that gap: it is an optional rendezvous directory
//! where a device publishes ("registers") the candidates it is reachable on,
//! keyed by its device id, and any other device can look those candidates up
//! by id.
//!
//! Unlike the relay ([`crate::relay`]), the announce server never carries
//! payload traffic — it only stores and serves candidate sets. Once a
//! looker-up has the candidates it connects directly (or via a relay) using
//! the existing connectivity stack. The server is therefore a thin,
//! stateless-from-the-client's-view JSON directory.
//!
//! ## Wire contract
//!
//! Two JSON endpoints, both rooted at the configured base URL:
//!
//! - `POST <base>/announce/<device_id>` with an [`AnnounceRequest`] body —
//!   replaces the stored candidate set for `device_id`.
//! - `GET  <base>/announce/<device_id>` — returns a [`LookupResponse`] with
//!   the most recently registered candidate set, or nothing when the id is
//!   unknown.
//!
//! ## Self-certifying candidate sets
//!
//! The candidate set is carried inside a
//! [`SignedCandidates`](crate::discovery::signing::SignedCandidates) envelope:
//! the announcing device signs its candidates, the claimed device id, and an
//! expiry with the ed25519 key derived from its device id (the same key the
//! DHT BEP44 path signs with). The announce server is therefore a *blind,
//! untrusted carrier* — it stores and serves the signed blob verbatim and never
//! inspects or vouches for it. The client verifies the signature on read: the
//! signer must be the device being resolved, the payload must be untampered, and
//! the expiry must be unexpired, otherwise the result is rejected. A malicious
//! server (or a man in the middle) can withhold or corrupt a set, and it cannot
//! *substitute* one device's set for another or silently relabel a stored
//! envelope. It can, however, mint a fresh valid envelope for any device id it
//! knows — the signing key is derived from the public device id, not the
//! device's TLS private key (see [`super::signing`] for the full threat model),
//! so this construction is a substitution/relabel/replay defence, not a defence
//! against forgery by a party that knows the id.
//!
//! ## Write authentication
//!
//! The announce directory is a soft-state rendezvous that only holders of a
//! shared secret may write to: every `POST /announce/<device_id>` carries an
//! `HMAC-SHA256` tag over the device id and the exact request body in the
//! [`ANNOUNCE_AUTH_HEADER`](cascade_announce_wire::auth::ANNOUNCE_AUTH_HEADER)
//! header, and both carriers — the relay-server's directory endpoint and the
//! Cloudflare Worker — reject a write whose tag is missing or does not verify.
//! The client therefore holds the same 32-byte secret and stamps the header on
//! every register. The HMAC gates *who* may write; it is orthogonal to the
//! self-certifying signature inside the envelope, which is what a *reader*
//! verifies. Binding the body into the tag stops a man-in-the-middle swapping
//! the stored blob for a different (even validly-signed) one in flight.
//!
//! The wire types (`WireCandidate`, `AnnounceRequest`, `LookupResponse`), the
//! per-device candidate cap (`MAX_ANNOUNCE_CANDIDATES`), and the write-auth
//! primitive (`auth::announce_write_tag`, `ANNOUNCE_AUTH_HEADER`) are owned by
//! the wasm-safe [`cascade_announce_wire`] crate so the announce client, the
//! relay-server's directory endpoint, the DHT, and the Cloudflare Worker all
//! share one definition and cannot drift. This module re-exports them and adds
//! the `cascade-p2p`-side HTTP client (behind the `announce` cargo feature, which
//! pulls in `reqwest`).

pub use cascade_announce_wire::WireCandidate;
pub use cascade_announce_wire::auth::{SHARED_SECRET_LEN, parse_shared_secret_hex};
pub use cascade_announce_wire::wire::{AnnounceRequest, LookupResponse, MAX_ANNOUNCE_CANDIDATES};

#[cfg(feature = "announce")]
pub use client::AnnounceDiscovery;

#[cfg(feature = "announce")]
mod client {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use cascade_announce_wire::auth::{self, ANNOUNCE_AUTH_HEADER};
    use reqwest::Client;

    use super::{
        AnnounceRequest, LookupResponse, MAX_ANNOUNCE_CANDIDATES, SHARED_SECRET_LEN, WireCandidate,
    };
    use crate::candidate::Candidate;
    use crate::discovery::Discovery;
    use crate::discovery::signing::{self, SignedCandidates};
    use crate::traversal::{Clock, SystemClock};

    /// Wall-clock ceiling on a single announce or lookup round-trip.
    ///
    /// Discovery is best-effort and runs on a background loop, so a slow or
    /// unreachable announce server must not wedge the caller. Ten seconds is
    /// generous for a JSON round-trip while still bounding how long a hung
    /// server can hold the discovery task.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// How long an announced candidate set stays valid after it is signed.
    ///
    /// The expiry bounds how long a captured signed set can be replayed by a
    /// hostile carrier: once it lapses, the resolver rejects the envelope and
    /// the device must have re-announced a fresh one. The window is set well
    /// above the announce loop's republish cadence so a live device's set is
    /// always replaced before it expires, but short enough that a stale capture
    /// is useless within an hour.
    const ANNOUNCE_TTL: Duration = Duration::from_hours(1);

    /// Announce-server discovery client.
    ///
    /// Registers the local device's candidates with, and resolves peers by
    /// device id against, a cascade announce server reachable at `base_url`.
    /// Implements [`Discovery`] so it composes behind
    /// [`crate::discovery::DiscoveryService`] alongside the LAN and gossip
    /// sources.
    ///
    /// All network failures (unreachable server, timeout, malformed body) are
    /// best-effort: `resolve` returns no candidates and `announce` logs and
    /// moves on, exactly as the LAN source treats a missed multicast. A
    /// single source failing must never abort the composed resolution.
    #[derive(Clone)]
    pub struct AnnounceDiscovery {
        base_url: String,
        client: Client,
        clock: Arc<dyn Clock>,
        /// Shared secret authenticating this device's writes to the announce
        /// directory. Every `POST` carries the `HMAC-SHA256` tag over the device
        /// id and the body keyed by this secret; both carriers reject a write
        /// whose tag is absent or wrong, so the client cannot register without
        /// it. Read-only lookups do not use it.
        secret: [u8; SHARED_SECRET_LEN],
    }

    impl std::fmt::Debug for AnnounceDiscovery {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("AnnounceDiscovery")
                .field("base_url", &self.base_url)
                .finish_non_exhaustive()
        }
    }

    impl AnnounceDiscovery {
        /// Create a client targeting the announce server at `base_url`.
        ///
        /// `base_url` is the scheme-and-authority root (e.g.
        /// `https://announce.example`); the `/announce/<device_id>` path is
        /// appended per request. A trailing slash is tolerated. `secret` is the
        /// 32-byte shared secret this device authenticates its registrations
        /// with — it is stamped as the `HMAC` write tag on every `POST`, which
        /// both carriers require. The wall clock used to stamp announce expiries
        /// and check freshness on lookup is the real [`SystemClock`]; tests
        /// inject a virtualised clock via [`Self::with_clock`].
        ///
        /// Returns an error only if the underlying HTTP client cannot be
        /// constructed (TLS backend initialisation), which is a process-level
        /// failure rather than a per-request one.
        pub fn new(
            base_url: impl Into<String>,
            secret: [u8; SHARED_SECRET_LEN],
        ) -> anyhow::Result<Self> {
            Self::with_clock(base_url, secret, Arc::new(SystemClock))
        }

        /// Create a client with an injected [`Clock`].
        ///
        /// Identical to [`Self::new`] but lets a caller (the tests) supply a
        /// deterministic clock so signing expiries and freshness checks are
        /// reproducible.
        pub fn with_clock(
            base_url: impl Into<String>,
            secret: [u8; SHARED_SECRET_LEN],
            clock: Arc<dyn Clock>,
        ) -> anyhow::Result<Self> {
            let client = Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .map_err(|err| anyhow::anyhow!("building announce HTTP client: {err}"))?;
            Ok(Self {
                base_url: base_url.into(),
                client,
                clock,
                secret,
            })
        }

        /// Build the per-device endpoint URL, tolerating a trailing slash on
        /// the configured base.
        fn endpoint(&self, device_id: &str) -> String {
            format!(
                "{}/announce/{device_id}",
                self.base_url.trim_end_matches('/')
            )
        }
    }

    #[async_trait]
    impl Discovery for AnnounceDiscovery {
        async fn resolve(&self, device_id: &str) -> Vec<Candidate> {
            let url = self.endpoint(device_id);
            let response = match self.client.get(&url).send().await {
                Ok(response) => response,
                Err(err) => {
                    tracing::debug!(
                        target: "cascade::p2p::discovery::announce",
                        %url,
                        error = %err,
                        "announce lookup request failed",
                    );
                    return Vec::new();
                }
            };
            if !response.status().is_success() {
                tracing::debug!(
                    target: "cascade::p2p::discovery::announce",
                    %url,
                    status = %response.status(),
                    "announce lookup returned non-success status",
                );
                return Vec::new();
            }
            let body: LookupResponse = match response.json().await {
                Ok(body) => body,
                Err(err) => {
                    tracing::debug!(
                        target: "cascade::p2p::discovery::announce",
                        %url,
                        error = %err,
                        "announce lookup body was not a valid LookupResponse",
                    );
                    return Vec::new();
                }
            };
            // Unknown id: nothing stored. Modelled as "no candidates".
            let Some(signed) = body.signed else {
                return Vec::new();
            };
            // The server is an untrusted carrier: verify the signature before
            // trusting a single address. The signer must be the device being
            // resolved, the payload untampered, and the expiry unexpired. Any
            // failure is rejected loudly and yields no candidates — never a
            // silent acceptance, never a panic.
            let now = signing::now_unix_ms(self.clock.as_ref());
            match signing::verify_to_candidates(&signed, device_id, now) {
                Ok(candidates) => candidates
                    .into_iter()
                    .take(MAX_ANNOUNCE_CANDIDATES)
                    .collect(),
                Err(err) => {
                    tracing::warn!(
                        target: "cascade::p2p::discovery::announce",
                        %url,
                        device_id = %device_id,
                        error = %err,
                        "rejecting announce lookup: signature verification failed",
                    );
                    Vec::new()
                }
            }
        }

        async fn announce(&self, self_id: &str, candidates: &[Candidate]) {
            let wire: Vec<WireCandidate> = candidates
                .iter()
                .take(MAX_ANNOUNCE_CANDIDATES)
                .copied()
                .map(WireCandidate::from)
                .collect();
            let expires_at = signing::expiry_from_now(self.clock.as_ref(), ANNOUNCE_TTL);
            let signed = SignedCandidates::sign(self_id, wire, expires_at);
            let url = self.endpoint(self_id);
            let request = AnnounceRequest { signed };
            // Serialise the body once and reuse those exact bytes for both the
            // write-auth tag and the request payload, so the HMAC binds exactly
            // what is sent. Both carriers recompute the tag over the path id and
            // the body they received; a mismatch (a swapped body, a wrong
            // secret, a missing header) is rejected.
            let body = match serde_json::to_vec(&request) {
                Ok(body) => body,
                Err(err) => {
                    tracing::debug!(
                        target: "cascade::p2p::discovery::announce",
                        %url,
                        error = %err,
                        "could not serialise announce request body",
                    );
                    return;
                }
            };
            let tag = match auth::announce_write_tag(&self.secret, self_id, &body) {
                Ok(tag) => tag,
                Err(err) => {
                    tracing::debug!(
                        target: "cascade::p2p::discovery::announce",
                        %url,
                        error = %err,
                        "could not compute announce write tag",
                    );
                    return;
                }
            };
            let response = self
                .client
                .post(&url)
                .header(ANNOUNCE_AUTH_HEADER, auth::encode_hex(&tag))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {}
                Ok(response) => tracing::debug!(
                    target: "cascade::p2p::discovery::announce",
                    %url,
                    status = %response.status(),
                    "announce register returned non-success status",
                ),
                Err(err) => tracing::debug!(
                    target: "cascade::p2p::discovery::announce",
                    %url,
                    error = %err,
                    "announce register request failed",
                ),
            }
        }
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    mod tests {
        use std::collections::HashMap;
        use std::net::SocketAddr;
        use std::sync::Arc;

        use cascade_announce_wire::auth::{self, ANNOUNCE_AUTH_HEADER, SHARED_SECRET_LEN};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio::sync::Mutex;

        use super::super::{AnnounceRequest, LookupResponse, WireCandidate};
        use super::AnnounceDiscovery;
        use crate::candidate::{Candidate, CandidateKind};
        use crate::discovery::Discovery;
        use crate::discovery::signing::SignedCandidates;
        use crate::traversal::Clock;

        fn addr(port: u16) -> SocketAddr {
            SocketAddr::from(([127, 0, 0, 1], port))
        }

        /// Deterministic 32-byte shared secret the client and the mock carrier
        /// agree on, so the produced write tag verifies against the carrier.
        fn secret() -> [u8; SHARED_SECRET_LEN] {
            let mut s = [0u8; SHARED_SECRET_LEN];
            for (idx, byte) in s.iter_mut().enumerate() {
                *byte = u8::try_from(idx).unwrap_or(0);
            }
            s
        }

        /// A fixed-time [`Clock`] so signing expiries and freshness checks are
        /// deterministic across the round-trip. The monotonic `now` is unused
        /// by the announce path, so it returns a constant instant.
        struct FixedClock {
            unix_ms: u64,
        }

        impl Clock for FixedClock {
            fn now(&self) -> std::time::Instant {
                std::time::Instant::now()
            }

            fn now_unix_ms(&self) -> u64 {
                self.unix_ms
            }
        }

        const NOW_MS: u64 = 1_700_000_000_000;

        fn fixed_clock() -> Arc<dyn Clock> {
            Arc::new(FixedClock { unix_ms: NOW_MS })
        }

        /// In-memory store of the opaque signed blob keyed by device id,
        /// mirroring the directory the real announce server keeps. The server
        /// stores and serves the [`SignedCandidates`] verbatim — it never
        /// inspects it.
        type Store = Arc<Mutex<HashMap<String, SignedCandidates>>>;

        /// A parsed mock request: method, device id from the path, the raw
        /// write-auth header value if present, and the body.
        struct MockRequest {
            method: String,
            device_id: String,
            auth_header: Option<String>,
            body: Vec<u8>,
        }

        /// Read one HTTP request off `stream` and return a [`MockRequest`]. The
        /// mock parses only the request line, the `Content-Length` and
        /// write-auth headers, and the body — enough for the two announce
        /// routes, nothing more.
        async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<MockRequest> {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            // Read until we have the full header block (terminated by a blank
            // line). The bodies here are tiny so a single growth loop is fine.
            loop {
                let read = stream.read(&mut chunk).await.ok()?;
                if read == 0 {
                    return None;
                }
                buf.extend_from_slice(&chunk[..read]);
                if let Some(pos) = find_header_end(&buf) {
                    let header = &buf[..pos];
                    let header_str = std::str::from_utf8(header).ok()?;
                    let mut lines = header_str.split("\r\n");
                    let request_line = lines.next()?;
                    let mut parts = request_line.split(' ');
                    let method = parts.next()?.to_owned();
                    let path = parts.next()?;
                    let device_id = path.rsplit('/').next()?.to_owned();
                    let content_length = header_str
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length: ")
                                .or_else(|| l.strip_prefix("Content-Length: "))
                        })
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    // The header name is matched case-insensitively because
                    // `reqwest` may normalise the casing on the wire.
                    let auth_header = header_str.lines().find_map(|l| {
                        let (name, value) = l.split_once(':')?;
                        name.trim()
                            .eq_ignore_ascii_case(ANNOUNCE_AUTH_HEADER)
                            .then(|| value.trim().to_owned())
                    });
                    let body_start = pos + 4;
                    while buf.len() < body_start + content_length {
                        let read = stream.read(&mut chunk).await.ok()?;
                        if read == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..read]);
                    }
                    let body = buf
                        .get(body_start..body_start + content_length)
                        .unwrap_or_default()
                        .to_vec();
                    return Some(MockRequest {
                        method,
                        device_id,
                        auth_header,
                        body,
                    });
                }
            }
        }

        /// Index of the byte before the `\r\n\r\n` header terminator.
        fn find_header_end(buf: &[u8]) -> Option<usize> {
            buf.windows(4).position(|w| w == b"\r\n\r\n")
        }

        async fn write_json_response(stream: &mut tokio::net::TcpStream, body: &[u8]) {
            let header = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(body).await;
            let _ = stream.flush().await;
        }

        /// Reject an unauthenticated write the way both real carriers do: a bare
        /// `401` with no body, so the client treats it as a non-success status.
        async fn write_unauthorized(stream: &mut tokio::net::TcpStream) {
            let header =
                "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.flush().await;
        }

        /// Spawn a mock announce server backed by `store` that authenticates
        /// writers with `secret` exactly as the real carriers do. Returns the
        /// bound base URL. Each accepted connection serves exactly one request.
        ///
        /// The `POST` path runs the shared
        /// [`auth::verify_announce_write`] over the path device id and the exact
        /// received body, rejecting a missing or non-verifying header with
        /// `401`. This is what proves the *real* client produces a header that
        /// verifies against an auth-requiring carrier, rather than a mock that
        /// ignores auth.
        async fn spawn_mock_server(store: Store, secret: [u8; SHARED_SECRET_LEN]) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let bound = listener.local_addr().unwrap();
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let store = store.clone();
                    tokio::spawn(async move {
                        let Some(request) = read_request(&mut stream).await else {
                            return;
                        };
                        match request.method.as_str() {
                            "POST" => {
                                // Authenticate the writer over the path id and
                                // the exact body, the way the relay endpoint and
                                // the Worker do. A missing or non-verifying tag
                                // is a 401 and stores nothing.
                                let authenticated =
                                    request.auth_header.as_deref().is_some_and(|header| {
                                        auth::verify_announce_write(
                                            &secret,
                                            &request.device_id,
                                            &request.body,
                                            header,
                                        )
                                        .unwrap_or(false)
                                    });
                                if !authenticated {
                                    write_unauthorized(&mut stream).await;
                                    return;
                                }
                                let parsed: AnnounceRequest =
                                    serde_json::from_slice(&request.body).unwrap();
                                // Store the signed blob verbatim — the carrier
                                // never inspects or validates it.
                                store.lock().await.insert(request.device_id, parsed.signed);
                                write_json_response(&mut stream, b"{}").await;
                            }
                            "GET" => {
                                let signed = store.lock().await.get(&request.device_id).cloned();
                                let response = LookupResponse { signed };
                                let json = serde_json::to_vec(&response).unwrap();
                                write_json_response(&mut stream, &json).await;
                            }
                            _ => {}
                        }
                    });
                }
            });
            format!("http://{bound}")
        }

        #[tokio::test]
        async fn register_then_lookup_round_trips_candidates() {
            // The mock carrier authenticates the write with the same secret the
            // client holds, so a successful round-trip proves the client
            // produced a header that verifies — not merely that a mock ignored
            // auth.
            let store: Store = Arc::new(Mutex::new(HashMap::new()));
            let base = spawn_mock_server(store, secret()).await;
            let client = AnnounceDiscovery::with_clock(base, secret(), fixed_clock()).unwrap();

            let host = Candidate::new(addr(22000), CandidateKind::Host, 65_535);
            let srflx = Candidate::new(addr(33000), CandidateKind::ServerReflexive, 0);
            client.announce("DEVICE-A", &[host, srflx]).await;

            let resolved = client.resolve("DEVICE-A").await;
            assert_eq!(resolved.len(), 2);
            assert!(resolved.contains(&host));
            assert!(resolved.contains(&srflx));
        }

        #[tokio::test]
        async fn register_with_a_mismatched_secret_is_rejected_and_stores_nothing() {
            // The client holds a different secret than the carrier, so its write
            // tag does not verify: the carrier 401s and the directory stays
            // empty, so the subsequent lookup yields nothing. This is the exact
            // failure mode the producer/consumer fix prevents when the secrets
            // *do* match.
            let store: Store = Arc::new(Mutex::new(HashMap::new()));
            let base = spawn_mock_server(store, secret()).await;
            let mut wrong = secret();
            wrong[0] ^= 0xFF;
            let client = AnnounceDiscovery::with_clock(base, wrong, fixed_clock()).unwrap();

            let host = Candidate::new(addr(22000), CandidateKind::Host, 1);
            client.announce("DEVICE-A", &[host]).await;

            assert!(client.resolve("DEVICE-A").await.is_empty());
        }

        #[tokio::test]
        async fn lookup_unknown_device_yields_no_candidates() {
            let store: Store = Arc::new(Mutex::new(HashMap::new()));
            let base = spawn_mock_server(store, secret()).await;
            let client = AnnounceDiscovery::with_clock(base, secret(), fixed_clock()).unwrap();

            assert!(client.resolve("NEVER-REGISTERED").await.is_empty());
        }

        #[tokio::test]
        async fn resolve_against_unreachable_server_is_empty() {
            // Bind then drop so the port has no listener — the request must
            // fail and surface as "no candidates", never an error.
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let bound = listener.local_addr().unwrap();
            drop(listener);
            let client =
                AnnounceDiscovery::with_clock(format!("http://{bound}"), secret(), fixed_clock())
                    .unwrap();
            assert!(client.resolve("DEVICE-A").await.is_empty());
        }

        #[tokio::test]
        async fn carrier_serving_a_tampered_blob_is_rejected_on_resolve() {
            // The carrier holds a valid set, then a byte is flipped after the
            // device signed it. The client must reject the lookup, not surface
            // a forged address.
            let store: Store = Arc::new(Mutex::new(HashMap::new()));
            let host = WireCandidate::from(Candidate::new(addr(22000), CandidateKind::Host, 1));
            let mut signed = SignedCandidates::sign(
                "DEVICE-A",
                vec![host],
                i64::try_from(NOW_MS).unwrap() + 1000,
            );
            signed.candidates[0].priority ^= 0x01;
            store.lock().await.insert("DEVICE-A".to_owned(), signed);
            let base = spawn_mock_server(store, secret()).await;

            let client = AnnounceDiscovery::with_clock(base, secret(), fixed_clock()).unwrap();
            assert!(client.resolve("DEVICE-A").await.is_empty());
        }

        #[tokio::test]
        async fn carrier_serving_a_blob_for_another_device_is_rejected() {
            // The carrier stores DEVICE-B's validly-signed set under DEVICE-A's
            // key (substitution). Resolving DEVICE-A must reject it.
            let store: Store = Arc::new(Mutex::new(HashMap::new()));
            let host = WireCandidate::from(Candidate::new(addr(22000), CandidateKind::Host, 1));
            let signed = SignedCandidates::sign(
                "DEVICE-B",
                vec![host],
                i64::try_from(NOW_MS).unwrap() + 1000,
            );
            store.lock().await.insert("DEVICE-A".to_owned(), signed);
            let base = spawn_mock_server(store, secret()).await;

            let client = AnnounceDiscovery::with_clock(base, secret(), fixed_clock()).unwrap();
            assert!(client.resolve("DEVICE-A").await.is_empty());
        }

        #[tokio::test]
        async fn expired_blob_from_the_carrier_is_rejected() {
            // The carrier replays a set whose expiry has lapsed. Even though it
            // is correctly signed for DEVICE-A, the client must reject it.
            let store: Store = Arc::new(Mutex::new(HashMap::new()));
            let host = WireCandidate::from(Candidate::new(addr(22000), CandidateKind::Host, 1));
            // Expiry one second before the fixed clock's "now".
            let signed = SignedCandidates::sign(
                "DEVICE-A",
                vec![host],
                i64::try_from(NOW_MS).unwrap() - 1000,
            );
            store.lock().await.insert("DEVICE-A".to_owned(), signed);
            let base = spawn_mock_server(store, secret()).await;

            let client = AnnounceDiscovery::with_clock(base, secret(), fixed_clock()).unwrap();
            assert!(client.resolve("DEVICE-A").await.is_empty());
        }
    }
}

// The `WireCandidate` ⇄ `Candidate` conversions are tested in
// [`crate::candidate`], where they live; the `AnnounceRequest` /
// `LookupResponse` JSON round-trips are tested in the `cascade-announce-wire`
// crate that owns those types. This module re-exports them and is exercised by
// the `announce`-feature client tests below.
