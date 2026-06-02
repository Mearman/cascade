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
//!   the most recently registered candidate set, or an empty set when the id
//!   is unknown.
//!
//! The wire types ([`WireCandidate`], [`AnnounceRequest`], [`LookupResponse`])
//! are serde-serialisable and shared by the relay-server's announce endpoint,
//! so the two sides cannot drift. The HTTP client lives behind the `announce`
//! cargo feature because it pulls in `reqwest`; the wire types are always
//! compiled so the server can depend on them without the client weight.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::candidate::{Candidate, CandidateKind};

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
/// Carries the candidate set the announcing device is currently reachable
/// on. A subsequent announce for the same id replaces the set in full —
/// candidates are not accumulated, matching the replace-in-full semantics of
/// [`crate::wan::PeerBook::set_remote_candidates`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnounceRequest {
    /// Candidates the device is reachable on, in announcer-computed priority
    /// order (the server preserves order but does not rely on it).
    pub candidates: Vec<WireCandidate>,
}

/// Body of a `GET <base>/announce/<device_id>` response.
///
/// An unknown device id yields an empty `candidates` vector rather than a
/// `404`, so the client models absence as "no candidates" — the same way
/// every [`super::Discovery`] source treats a peer it knows nothing about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupResponse {
    /// Candidates last registered for the looked-up device id, or empty when
    /// the id is unknown.
    pub candidates: Vec<WireCandidate>,
}

#[cfg(feature = "announce")]
pub use client::AnnounceDiscovery;

#[cfg(feature = "announce")]
mod client {
    use std::time::Duration;

    use async_trait::async_trait;
    use reqwest::Client;

    use super::{AnnounceRequest, LookupResponse, MAX_ANNOUNCE_CANDIDATES, WireCandidate};
    use crate::candidate::Candidate;
    use crate::discovery::Discovery;

    /// Wall-clock ceiling on a single announce or lookup round-trip.
    ///
    /// Discovery is best-effort and runs on a background loop, so a slow or
    /// unreachable announce server must not wedge the caller. Ten seconds is
    /// generous for a JSON round-trip while still bounding how long a hung
    /// server can hold the discovery task.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

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
    #[derive(Debug, Clone)]
    pub struct AnnounceDiscovery {
        base_url: String,
        client: Client,
    }

    impl AnnounceDiscovery {
        /// Create a client targeting the announce server at `base_url`.
        ///
        /// `base_url` is the scheme-and-authority root (e.g.
        /// `https://announce.example`); the `/announce/<device_id>` path is
        /// appended per request. A trailing slash is tolerated.
        ///
        /// Returns an error only if the underlying HTTP client cannot be
        /// constructed (TLS backend initialisation), which is a process-level
        /// failure rather than a per-request one.
        pub fn new(base_url: impl Into<String>) -> anyhow::Result<Self> {
            let client = Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .map_err(|err| anyhow::anyhow!("building announce HTTP client: {err}"))?;
            Ok(Self {
                base_url: base_url.into(),
                client,
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
            // Drop any candidate whose kind tag is unknown — a hostile or
            // buggy directory entry must not coerce into a wrong kind.
            body.candidates
                .into_iter()
                .take(MAX_ANNOUNCE_CANDIDATES)
                .filter_map(WireCandidate::to_candidate)
                .collect()
        }

        async fn announce(&self, self_id: &str, candidates: &[Candidate]) {
            let wire: Vec<WireCandidate> = candidates
                .iter()
                .take(MAX_ANNOUNCE_CANDIDATES)
                .copied()
                .map(WireCandidate::from)
                .collect();
            let url = self.endpoint(self_id);
            let body = AnnounceRequest { candidates: wire };
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

        fn addr(port: u16) -> SocketAddr {
            SocketAddr::from(([127, 0, 0, 1], port))
        }

        /// In-memory store keyed by device id, mirroring the directory the
        /// real announce server keeps.
        type Store = Arc<Mutex<HashMap<String, Vec<WireCandidate>>>>;

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
                                store.lock().await.insert(device_id, request.candidates);
                                write_json_response(&mut stream, b"{}").await;
                            }
                            "GET" => {
                                let candidates = store
                                    .lock()
                                    .await
                                    .get(&device_id)
                                    .cloned()
                                    .unwrap_or_default();
                                let response = LookupResponse { candidates };
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
            let client = AnnounceDiscovery::new(base).unwrap();

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
            let client = AnnounceDiscovery::new(base).unwrap();

            assert!(client.resolve("NEVER-REGISTERED").await.is_empty());
        }

        #[tokio::test]
        async fn resolve_against_unreachable_server_is_empty() {
            // Bind then drop so the port has no listener — the request must
            // fail and surface as "no candidates", never an error.
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let bound = listener.local_addr().unwrap();
            drop(listener);
            let client = AnnounceDiscovery::new(format!("http://{bound}")).unwrap();
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
            candidates: vec![
                WireCandidate::from(Candidate::new(addr(22000), CandidateKind::Host, 65_535)),
                WireCandidate::from(Candidate::new(
                    addr(33000),
                    CandidateKind::ServerReflexive,
                    0,
                )),
            ],
        };
        let json = serde_json::to_string(&request).expect("serialise");
        let decoded: AnnounceRequest = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(decoded, request);
    }

    #[test]
    fn lookup_response_round_trips_through_json() {
        let response = LookupResponse {
            candidates: vec![WireCandidate::from(Candidate::new(
                addr(22000),
                CandidateKind::Host,
                1,
            ))],
        };
        let json = serde_json::to_string(&response).expect("serialise");
        let decoded: LookupResponse = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(decoded, response);
    }
}
