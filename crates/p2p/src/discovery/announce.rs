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
//! The candidate set is carried inside a [`SignedCandidates`] envelope:
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
//! The wire types ([`WireCandidate`], [`AnnounceRequest`], [`LookupResponse`])
//! are serde-serialisable and shared by the relay-server's announce endpoint,
//! so the two sides cannot drift. The HTTP client lives behind the `announce`
//! cargo feature because it pulls in `reqwest`; the wire types are always
//! compiled so the server can depend on them without the client weight.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::candidate::{Candidate, CandidateKind};
use crate::discovery::signing::SignedCandidates;

/// Maximum number of candidates accepted in a single announce request or
/// returned from a lookup.
///
/// Mirrors the `MAX_CANDIDATES_PER_FRAME` cap the BEP `Candidates` frame
/// uses (a device with more than a handful of host, server-reflexive and
/// relayed addresses is unrealistic), so the announce directory bounds its
/// per-device storage the same way the wire protocol bounds a frame.
pub const MAX_ANNOUNCE_CANDIDATES: usize = 64;

/// Serialisable form of a [`Candidate`].
///
/// [`Candidate`] itself is the in-memory connectivity type and deliberately
/// carries no serde derives — its wire form on the BEP path is the
/// hand-rolled XDR encoding in [`crate::protocol`]. The announce directory is
/// a JSON API, so it needs an explicit serialisable projection. The `kind` is
/// carried as its stable wire tag (`0` host, `1` server-reflexive, `2`
/// relayed) rather than the in-memory enum so the JSON shape is stable across
/// releases, exactly as the BEP encoding does.
///
/// `priority` is carried so the looker-up sees the same RFC 8445 ordering the
/// announcer computed; an announcer that lies about its priority only
/// reorders its own candidates, which the [`super::DiscoveryService`] merge
/// already tolerates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCandidate {
    /// Reachable address (`IPv4` or `IPv6`) plus port.
    pub address: SocketAddr,
    /// Candidate kind as the stable wire tag — `0` host, `1`
    /// server-reflexive, `2` relayed.
    pub kind: u8,
    /// Precomputed RFC 8445 priority, carried so the recipient need not
    /// re-derive it.
    pub priority: u32,
}

impl From<Candidate> for WireCandidate {
    fn from(candidate: Candidate) -> Self {
        Self {
            address: candidate.address,
            kind: candidate.kind.wire_tag(),
            priority: candidate.priority,
        }
    }
}

impl WireCandidate {
    /// Convert back to an in-memory [`Candidate`].
    ///
    /// Returns `None` when the `kind` tag is not one of the three known
    /// values so a malformed or hostile directory entry is rejected rather
    /// than silently coerced. The stored `priority` is preserved exactly —
    /// the recipient honours the announcer's claimed priority, which the
    /// merge in [`super::DiscoveryService`] is designed to tolerate.
    #[must_use]
    pub fn to_candidate(self) -> Option<Candidate> {
        let kind = CandidateKind::from_wire_tag(self.kind)?;
        Some(Candidate {
            address: self.address,
            kind,
            priority: self.priority,
        })
    }
}

/// Body of a `POST <base>/announce/<device_id>` request.
///
/// Carries the signed candidate set the announcing device is currently
/// reachable on. A subsequent announce for the same id replaces the set in
/// full — candidates are not accumulated, matching the replace-in-full
/// semantics of [`crate::wan::PeerBook::set_remote_candidates`].
///
/// The server stores the [`SignedCandidates`] verbatim and never inspects it;
/// the looking-up client is the only party that verifies the signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnounceRequest {
    /// The device's self-signed candidate set.
    pub signed: SignedCandidates,
}

/// Body of a `GET <base>/announce/<device_id>` response.
///
/// An unknown device id yields `signed: None` rather than a `404`, so the
/// client models absence as "no candidates" — the same way every
/// [`super::Discovery`] source treats a peer it knows nothing about. A known
/// id returns the signed blob exactly as it was registered, for the client to
/// verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupResponse {
    /// The signed candidate set last registered for the looked-up device id,
    /// or `None` when the id is unknown.
    pub signed: Option<SignedCandidates>,
}

#[cfg(feature = "announce")]
pub use client::AnnounceDiscovery;

#[cfg(feature = "announce")]
mod client {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use reqwest::Client;

    use super::{AnnounceRequest, LookupResponse, MAX_ANNOUNCE_CANDIDATES, WireCandidate};
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
        /// appended per request. A trailing slash is tolerated. The wall clock
        /// used to stamp announce expiries and check freshness on lookup is the
        /// real [`SystemClock`]; tests inject a virtualised clock via
        /// [`Self::with_clock`].
        ///
        /// Returns an error only if the underlying HTTP client cannot be
        /// constructed (TLS backend initialisation), which is a process-level
        /// failure rather than a per-request one.
        pub fn new(base_url: impl Into<String>) -> anyhow::Result<Self> {
            Self::with_clock(base_url, Arc::new(SystemClock))
        }

        /// Create a client with an injected [`Clock`].
        ///
        /// Identical to [`Self::new`] but lets a caller (the tests) supply a
        /// deterministic clock so signing expiries and freshness checks are
        /// reproducible.
        pub fn with_clock(
            base_url: impl Into<String>,
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
            match signed.verify_to_candidates(device_id, now) {
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
            let body = AnnounceRequest { signed };
            match self.client.post(&url).json(&body).send().await {
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

        /// Read one HTTP request off `stream` and return
        /// `(method, device_id, body)`. The mock parses only the request
        /// line, the `Content-Length` header, and the body — enough for the
        /// two announce routes, nothing more.
        async fn read_request(
            stream: &mut tokio::net::TcpStream,
        ) -> Option<(String, String, Vec<u8>)> {
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
                    return Some((method, device_id, body));
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

        /// Spawn a mock announce server backed by `store`. Returns the bound
        /// base URL. Each accepted connection serves exactly one request.
        async fn spawn_mock_server(store: Store) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let bound = listener.local_addr().unwrap();
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let store = store.clone();
                    tokio::spawn(async move {
                        let Some((method, device_id, body)) = read_request(&mut stream).await
                        else {
                            return;
                        };
                        match method.as_str() {
                            "POST" => {
                                let request: AnnounceRequest =
                                    serde_json::from_slice(&body).unwrap();
                                // Store the signed blob verbatim — the carrier
                                // never inspects or validates it.
                                store.lock().await.insert(device_id, request.signed);
                                write_json_response(&mut stream, b"{}").await;
                            }
                            "GET" => {
                                let signed = store.lock().await.get(&device_id).cloned();
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
            let store: Store = Arc::new(Mutex::new(HashMap::new()));
            let base = spawn_mock_server(store).await;
            let client = AnnounceDiscovery::with_clock(base, fixed_clock()).unwrap();

            let host = Candidate::new(addr(22000), CandidateKind::Host, 65_535);
            let srflx = Candidate::new(addr(33000), CandidateKind::ServerReflexive, 0);
            client.announce("DEVICE-A", &[host, srflx]).await;

            let resolved = client.resolve("DEVICE-A").await;
            assert_eq!(resolved.len(), 2);
            assert!(resolved.contains(&host));
            assert!(resolved.contains(&srflx));
        }

        #[tokio::test]
        async fn lookup_unknown_device_yields_no_candidates() {
            let store: Store = Arc::new(Mutex::new(HashMap::new()));
            let base = spawn_mock_server(store).await;
            let client = AnnounceDiscovery::with_clock(base, fixed_clock()).unwrap();

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
                AnnounceDiscovery::with_clock(format!("http://{bound}"), fixed_clock()).unwrap();
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
            let base = spawn_mock_server(store).await;

            let client = AnnounceDiscovery::with_clock(base, fixed_clock()).unwrap();
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
            let base = spawn_mock_server(store).await;

            let client = AnnounceDiscovery::with_clock(base, fixed_clock()).unwrap();
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
            let base = spawn_mock_server(store).await;

            let client = AnnounceDiscovery::with_clock(base, fixed_clock()).unwrap();
            assert!(client.resolve("DEVICE-A").await.is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn wire_candidate_round_trips_every_kind() {
        for kind in [
            CandidateKind::Host,
            CandidateKind::ServerReflexive,
            CandidateKind::Relayed,
        ] {
            let candidate = Candidate::new(addr(22000), kind, 1024);
            let wire = WireCandidate::from(candidate);
            assert_eq!(wire.to_candidate(), Some(candidate));
        }
    }

    #[test]
    fn wire_candidate_preserves_priority_exactly() {
        let candidate = Candidate::new(addr(33000), CandidateKind::ServerReflexive, u16::MAX);
        let wire = WireCandidate::from(candidate);
        assert_eq!(wire.priority, candidate.priority);
        assert_eq!(
            wire.to_candidate().map(|c| c.priority),
            Some(candidate.priority)
        );
    }

    #[test]
    fn wire_candidate_rejects_unknown_kind_tag() {
        let wire = WireCandidate {
            address: addr(22000),
            kind: 3,
            priority: 0,
        };
        assert_eq!(wire.to_candidate(), None);
    }

    #[test]
    fn announce_request_round_trips_through_json() {
        let request = AnnounceRequest {
            signed: SignedCandidates::sign(
                "DEVICE-A",
                vec![
                    WireCandidate::from(Candidate::new(addr(22000), CandidateKind::Host, 65_535)),
                    WireCandidate::from(Candidate::new(
                        addr(33000),
                        CandidateKind::ServerReflexive,
                        0,
                    )),
                ],
                1_700_000_000_000,
            ),
        };
        let json = serde_json::to_string(&request).expect("serialise");
        let decoded: AnnounceRequest = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(decoded, request);
    }

    #[test]
    fn lookup_response_round_trips_through_json() {
        let response = LookupResponse {
            signed: Some(SignedCandidates::sign(
                "DEVICE-A",
                vec![WireCandidate::from(Candidate::new(
                    addr(22000),
                    CandidateKind::Host,
                    1,
                ))],
                1_700_000_000_000,
            )),
        };
        let json = serde_json::to_string(&response).expect("serialise");
        let decoded: LookupResponse = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(decoded, response);
    }

    #[test]
    fn lookup_response_models_unknown_id_as_none() {
        let response = LookupResponse { signed: None };
        let json = serde_json::to_string(&response).expect("serialise");
        let decoded: LookupResponse = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(decoded, response);
        assert!(decoded.signed.is_none());
    }
}
