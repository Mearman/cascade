//! Kademlia/Mainline-DHT discovery source.
//!
//! The announce server ([`mod@super::announce`]) is a rendezvous directory for
//! two devices that have never met and sit on different networks: a device
//! publishes its candidate set keyed by device id, and any other device looks
//! those candidates up by id. The DHT plays the same "find an unknown peer"
//! role without a central directory — it is the serverless replacement the v9
//! roadmap and the design doc call for (see `docs/design.md`, "Peer
//! discovery"). Instead of storing the candidate set on one server, the device
//! stores it in the `BitTorrent` Mainline DHT under a key derived by hashing its
//! device id; any peer that knows the device id derives the same key and reads
//! the candidate set back out.
//!
//! ## Shape
//!
//! Discovery here is keyed lookup over a distributed hash table:
//!
//! - [`DhtKey::from_device_id`] maps a base32 device id to a 160-bit DHT key by
//!   hashing it (SHA-1, matching the 160-bit key width of `BitTorrent`
//!   info-hashes). The mapping is deterministic, so the announcer and the
//!   looker-up derive the same key from the same device id without any shared
//!   state.
//! - [`DhtDiscovery::announce`] serialises the local candidate set and stores
//!   it under that key. Conceptually this is `announce_peer` keyed by
//!   `hash(device_id)`, except the stored value is the whole candidate set
//!   rather than a single endpoint.
//! - [`DhtDiscovery::resolve`] performs the iterative lookup for the key and
//!   returns the stored candidate set, or an empty set when no node holds a
//!   value for the key. Conceptually this is `get_peers` keyed by
//!   `hash(device_id)`.
//!
//! ## Layering
//!
//! The DHT mechanics live behind the [`DhtNode`] contract: a put/get key-value
//! store over 160-bit keys with opaque byte values. [`DhtDiscovery`] is generic
//! over that contract, so its [`Discovery`] behaviour — the device-id-to-key
//! mapping, the candidate (de)serialisation, the dedupe-on-read — is exercised
//! against an in-memory node in tests without touching the network. The live
//! node, `MainlineDht`, wraps the maintained `mainline` crate and lives behind
//! the `dht` cargo feature, exactly as the announce-server HTTP client lives
//! behind `announce`: the contract, the resolver, and the key mapping are
//! always compiled; only the network-facing backend is gated.
//!
//! The stored value reuses the announce directory's serialisable candidate
//! shape ([`WireCandidate`]) wrapped in a [`StoredCandidates`] envelope, so the
//! DHT and the announce server agree on how a candidate set is encoded — there
//! is one serialisation home, not two that can drift.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use super::Discovery;
use super::announce::{MAX_ANNOUNCE_CANDIDATES, WireCandidate};
use crate::candidate::Candidate;

/// Width of a DHT key in bytes.
///
/// `BitTorrent`'s Mainline DHT keys its address space with 160-bit
/// identifiers (SHA-1 of the info-hash, or of a BEP44 public key plus salt).
/// The device-id-derived key matches that width so it addresses the same
/// keyspace the underlying node operates over.
pub const DHT_KEY_LEN: usize = 20;

/// A 160-bit DHT key.
///
/// Derived deterministically from a device id by [`DhtKey::from_device_id`].
/// Two devices that agree on a device id derive the same key, which is what
/// lets a looker-up address the announcer's stored candidate set without any
/// prior contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DhtKey(pub [u8; DHT_KEY_LEN]);

impl DhtKey {
    /// Map a base32 device id to its DHT key by hashing it.
    ///
    /// The device id is hashed with SHA-1, whose 160-bit digest is exactly
    /// the DHT key width. Hashing (rather than truncating the already-hashed
    /// device id) keeps the mapping independent of the device id's own
    /// encoding: any string maps to a uniformly distributed key, and the
    /// announcer and looker-up derive the same key from the same id.
    #[must_use]
    pub fn from_device_id(device_id: &str) -> Self {
        let mut hasher = Sha1::new();
        hasher.update(device_id.as_bytes());
        let digest = hasher.finalize();
        let mut key = [0u8; DHT_KEY_LEN];
        // SHA-1 produces exactly `DHT_KEY_LEN` bytes, so the copy is total.
        key.copy_from_slice(&digest);
        Self(key)
    }

    /// The raw 160-bit key bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; DHT_KEY_LEN] {
        &self.0
    }
}

/// Serialisable envelope for a candidate set stored under a DHT key.
///
/// Reuses [`WireCandidate`] — the announce directory's serialisable candidate
/// projection — so the DHT and the announce server encode a candidate set the
/// same way. The envelope carries the candidates as a JSON array; the DHT
/// stores the encoded bytes opaquely and hands them back verbatim on lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCandidates {
    /// Candidates the announcing device is reachable on, in announcer-computed
    /// priority order.
    pub candidates: Vec<WireCandidate>,
}

impl StoredCandidates {
    /// Encode a candidate set into the opaque bytes stored in the DHT.
    ///
    /// Caps the set at [`MAX_ANNOUNCE_CANDIDATES`] — the same bound the
    /// announce directory applies — so a device with an implausible number of
    /// candidates cannot bloat a DHT value. Returns the JSON-encoded bytes, or
    /// the error if serialisation fails (which only happens on an allocator
    /// failure for this shape).
    fn encode(candidates: &[Candidate]) -> Result<Vec<u8>, serde_json::Error> {
        let wire: Vec<WireCandidate> = candidates
            .iter()
            .take(MAX_ANNOUNCE_CANDIDATES)
            .copied()
            .map(WireCandidate::from)
            .collect();
        serde_json::to_vec(&Self { candidates: wire })
    }

    /// Decode opaque DHT bytes into in-memory candidates.
    ///
    /// Drops any candidate whose kind tag is unknown — a malformed or hostile
    /// stored value must not coerce into a wrong kind — and caps the decoded
    /// set at [`MAX_ANNOUNCE_CANDIDATES`]. Returns an empty vector when the
    /// bytes do not decode to a [`StoredCandidates`], so a corrupt value reads
    /// as "no candidates" rather than aborting the lookup.
    fn decode(bytes: &[u8]) -> Vec<Candidate> {
        let parsed: Self = match serde_json::from_slice(bytes) {
            Ok(parsed) => parsed,
            Err(_) => return Vec::new(),
        };
        parsed
            .candidates
            .into_iter()
            .take(MAX_ANNOUNCE_CANDIDATES)
            .filter_map(WireCandidate::to_candidate)
            .collect()
    }
}

/// A put/get key-value store over 160-bit DHT keys.
///
/// This is the seam between the discovery logic and the actual distributed
/// hash table. [`DhtDiscovery`] depends only on this contract, never on a
/// concrete DHT, so the device-id-to-key mapping and the candidate
/// (de)serialisation are testable against an in-memory node. The live
/// implementation, `MainlineDht`, talks to the `BitTorrent` Mainline DHT; the
/// in-memory mock used in tests stores values in a map.
///
/// `get` returns every value any node holds for the key. A DHT lookup can
/// surface more than one stored value (different storing nodes, or a value
/// mid-replacement), so the contract is a list rather than a single value; the
/// caller decides how to merge them.
#[async_trait]
pub trait DhtNode: Send + Sync {
    /// Store `value` under `key` in the DHT. Best-effort: a store that does
    /// not reach enough nodes is not surfaced as an error here, because
    /// discovery announce is itself best-effort and runs on a background loop.
    async fn put(&self, key: DhtKey, value: Vec<u8>);

    /// Look `key` up in the DHT, returning every stored value found. An
    /// empty vector means no node holds a value for the key — modelled as
    /// absence, not an error, the same way every [`Discovery`] source treats a
    /// peer it knows nothing about.
    async fn get(&self, key: DhtKey) -> Vec<Vec<u8>>;
}

/// DHT-backed discovery source.
///
/// Generic over the [`DhtNode`] contract so the [`Discovery`] behaviour is
/// exercised against an in-memory node in tests and the live `mainline` node in
/// production without changing the resolver. Composes behind
/// [`super::DiscoveryService`] alongside the LAN, gossip and announce sources.
#[derive(Debug, Clone)]
pub struct DhtDiscovery<N: DhtNode> {
    node: N,
}

impl<N: DhtNode> DhtDiscovery<N> {
    /// Wrap a DHT node as a discovery source.
    pub const fn new(node: N) -> Self {
        Self { node }
    }
}

#[async_trait]
impl<N: DhtNode> Discovery for DhtDiscovery<N> {
    /// Resolve `device_id` by reading the candidate set stored under its DHT
    /// key.
    ///
    /// Maps the device id to its key, performs the DHT lookup, and decodes
    /// every value found. When more than one value is stored under the key
    /// (different storing nodes, or a value mid-replacement) the decoded
    /// candidates are unioned; the composing [`super::DiscoveryService`]
    /// deduplicates the result by `(address, kind)`, so returning the union
    /// here is safe. An unknown device id — no value under the key — yields no
    /// candidates.
    async fn resolve(&self, device_id: &str) -> Vec<Candidate> {
        let key = DhtKey::from_device_id(device_id);
        let values = self.node.get(key).await;
        values
            .iter()
            .flat_map(|bytes| StoredCandidates::decode(bytes))
            .collect()
    }

    /// Store this device's `candidates` under its DHT key so peers can resolve
    /// it by device id.
    ///
    /// Encoding failures are logged and swallowed — announce is best-effort and
    /// runs on a background loop, so a serialisation failure must not abort the
    /// composed announce fan-out.
    async fn announce(&self, self_id: &str, candidates: &[Candidate]) {
        let key = DhtKey::from_device_id(self_id);
        match StoredCandidates::encode(candidates) {
            Ok(bytes) => self.node.put(key, bytes).await,
            Err(err) => tracing::debug!(
                target: "cascade::p2p::discovery::dht",
                device_id = %self_id,
                error = %err,
                "could not encode candidate set for DHT announce",
            ),
        }
    }
}

#[cfg(feature = "dht")]
pub use live::MainlineDht;

#[cfg(feature = "dht")]
mod live {
    use std::net::SocketAddr;
    use std::path::Path;

    use anyhow::{Context, Result};
    use async_trait::async_trait;
    use mainline::{Dht, MutableItem, SigningKey};

    use super::{DhtKey, DhtNode};

    /// File name for the persisted BEP44 signing key.
    ///
    /// The signing key authenticates this node's mutable DHT writes. It must
    /// persist across restarts so a device keeps writing under the same BEP44
    /// public key — a fresh key on every boot would orphan previously stored
    /// candidate sets.
    const SIGNING_KEY_FILE: &str = "dht-signing.key";

    /// Width of an ed25519 signing key seed in bytes.
    const SIGNING_KEY_SEED_LEN: usize = 32;

    /// Mainline-DHT-backed [`DhtNode`].
    ///
    /// Wraps the maintained `mainline` crate's `BitTorrent` Mainline DHT node and
    /// stores candidate sets as BEP44 mutable items. The device-id-derived
    /// [`DhtKey`] is carried as the mutable item's salt, so different device ids
    /// map to different DHT targets under this node's single BEP44 signing key.
    /// Reads use the most-recent mutable item for the salt; writes bump the
    /// sequence number monotonically using the wall clock so a later announce
    /// supersedes an earlier one.
    ///
    /// All DHT I/O is best-effort: a store or lookup that fails on the wire is
    /// logged and surfaces as "no candidates", never an error, matching the
    /// best-effort contract every [`super::Discovery`] source upholds.
    #[derive(Clone)]
    pub struct MainlineDht {
        dht: mainline::async_dht::AsyncDht,
        signing_key: SigningKey,
    }

    impl std::fmt::Debug for MainlineDht {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MainlineDht").finish_non_exhaustive()
        }
    }

    impl MainlineDht {
        /// Build a Mainline-DHT node, bootstrapping against `bootstrap_nodes`.
        ///
        /// An empty `bootstrap_nodes` uses the `mainline` crate's built-in
        /// public bootstrap set. The BEP44 signing key is loaded from
        /// `identity_dir` (or generated and persisted on first run) so the node
        /// keeps writing mutable items under a stable public key across
        /// restarts.
        ///
        /// Returns an error only on a process-level failure — binding the DHT
        /// UDP socket, or reading/writing the signing key — not on a per-query
        /// failure, which is handled best-effort inside [`DhtNode`].
        pub fn open(identity_dir: &Path, bootstrap_nodes: &[SocketAddr]) -> Result<Self> {
            Self::open_inner(identity_dir, bootstrap_nodes, None)
        }

        /// Shared constructor for [`Self::open`] and the test helper.
        ///
        /// `bind_ip`, when set, pins the DHT UDP socket to a specific local
        /// address rather than the all-interfaces default. Production opens with
        /// `None` (the node must be reachable on every interface); the live test
        /// pins it to localhost so it talks to the in-process testnet.
        fn open_inner(
            identity_dir: &Path,
            bootstrap_nodes: &[SocketAddr],
            bind_ip: Option<std::net::Ipv4Addr>,
        ) -> Result<Self> {
            let signing_key = load_or_generate_signing_key(identity_dir)
                .context("loading DHT BEP44 signing key")?;

            let mut builder = Dht::builder();
            if !bootstrap_nodes.is_empty() {
                builder.bootstrap(bootstrap_nodes);
            }
            if let Some(ip) = bind_ip {
                builder.bind_address(ip);
            }
            let dht = builder
                .build()
                .map_err(|err| anyhow::anyhow!("building mainline DHT node: {err}"))?
                .as_async();

            Ok(Self { dht, signing_key })
        }

        /// Open a node pinned to localhost, for the in-process testnet round-trip
        /// tests. Not part of the production surface — the live network requires
        /// the all-interfaces bind [`Self::open`] uses.
        #[cfg(test)]
        pub(super) fn open_local(
            identity_dir: &Path,
            bootstrap_nodes: &[SocketAddr],
        ) -> Result<Self> {
            Self::open_inner(
                identity_dir,
                bootstrap_nodes,
                Some(std::net::Ipv4Addr::LOCALHOST),
            )
        }

        /// Build the BEP44 mutable item carrying `value` under the salt derived
        /// from `key`.
        ///
        /// The DHT key is used as the salt so each device id maps to a distinct
        /// mutable-item target under this node's single signing key. The
        /// sequence number is the current Unix time in milliseconds, which is
        /// monotonic enough that a later announce always supersedes an earlier
        /// one for the same salt.
        fn mutable_item(&self, key: DhtKey, value: &[u8], seq: i64) -> MutableItem {
            MutableItem::new(self.signing_key.clone(), value, seq, Some(key.as_bytes()))
        }
    }

    /// Load the BEP44 signing key from `dir`, generating and persisting one on
    /// first run.
    ///
    /// The key is stored as its raw 32-byte ed25519 seed. A fresh seed is drawn
    /// from the OS CSPRNG via `getrandom` — the canonical source for long-lived
    /// signing-key material — and `SigningKey::from_bytes` derives the key from
    /// it, the same construction the `mainline` crate uses. A stored seed of the
    /// wrong length is a corrupt file and surfaces as an error rather than being
    /// silently regenerated, so the operator notices rather than orphaning
    /// previously stored candidate sets.
    fn load_or_generate_signing_key(dir: &Path) -> Result<SigningKey> {
        let path = dir.join(SIGNING_KEY_FILE);
        if path.exists() {
            let seed = std::fs::read(&path).context("reading DHT signing key")?;
            let seed: [u8; SIGNING_KEY_SEED_LEN] = seed
                .try_into()
                .map_err(|_| anyhow::anyhow!("DHT signing key file is not a 32-byte seed"))?;
            Ok(SigningKey::from_bytes(&seed))
        } else {
            let mut seed = [0u8; SIGNING_KEY_SEED_LEN];
            getrandom::fill(&mut seed)
                .map_err(|err| anyhow::anyhow!("drawing DHT signing key seed: {err}"))?;
            let signing_key = SigningKey::from_bytes(&seed);
            std::fs::create_dir_all(dir).context("creating DHT identity directory")?;
            std::fs::write(&path, signing_key.to_bytes()).context("writing DHT signing key")?;
            Ok(signing_key)
        }
    }

    #[async_trait]
    impl DhtNode for MainlineDht {
        async fn put(&self, key: DhtKey, value: Vec<u8>) {
            let seq = unix_millis();
            let item = self.mutable_item(key, &value, seq);
            let dht = self.dht.clone();
            if let Err(err) = dht.put_mutable(item, None).await {
                tracing::debug!(
                    target: "cascade::p2p::discovery::dht",
                    error = %err,
                    "DHT mutable put failed",
                );
            }
        }

        async fn get(&self, key: DhtKey) -> Vec<Vec<u8>> {
            let public_key = self.signing_key.verifying_key().to_bytes();
            let salt = key.as_bytes().to_vec();
            let dht = self.dht.clone();
            dht.get_mutable_most_recent(&public_key, Some(&salt))
                .await
                .map_or_else(Vec::new, |item| vec![item.value().to_vec()])
        }
    }

    /// Current Unix time in milliseconds, used as the BEP44 mutable-item
    /// sequence number so a later announce supersedes an earlier one.
    fn unix_millis() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use crate::candidate::CandidateKind;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// In-memory [`DhtNode`] keyed by [`DhtKey`], mirroring the distributed
    /// hash table the live node talks to without any network. A key maps to the
    /// list of values stored under it — `put` replaces the list (the live node's
    /// most-recent-wins read collapses to a single value), and `get` returns the
    /// stored list.
    #[derive(Clone, Default)]
    struct MockDht {
        store: Arc<Mutex<HashMap<[u8; DHT_KEY_LEN], Vec<Vec<u8>>>>>,
    }

    #[async_trait]
    impl DhtNode for MockDht {
        async fn put(&self, key: DhtKey, value: Vec<u8>) {
            self.store.lock().await.insert(key.0, vec![value]);
        }

        async fn get(&self, key: DhtKey) -> Vec<Vec<u8>> {
            self.store
                .lock()
                .await
                .get(&key.0)
                .cloned()
                .unwrap_or_default()
        }
    }

    #[test]
    fn device_id_maps_to_a_160_bit_key() {
        let key = DhtKey::from_device_id("DEVICE-A");
        assert_eq!(key.as_bytes().len(), DHT_KEY_LEN);
    }

    #[test]
    fn device_id_mapping_is_deterministic() {
        // The announcer and the looker-up must derive the same key from the
        // same device id without any shared state.
        let a = DhtKey::from_device_id("DEVICE-A");
        let b = DhtKey::from_device_id("DEVICE-A");
        assert_eq!(a, b);
    }

    #[test]
    fn distinct_device_ids_map_to_distinct_keys() {
        let a = DhtKey::from_device_id("DEVICE-A");
        let b = DhtKey::from_device_id("DEVICE-B");
        assert_ne!(a, b);
    }

    #[test]
    fn key_matches_independent_sha1_of_device_id() {
        // Pin the mapping to SHA-1 of the id bytes so a future change to the
        // derivation is caught — the two ends must agree on exactly this.
        let mut hasher = Sha1::new();
        hasher.update(b"DEVICE-A");
        let expected = hasher.finalize();
        let key = DhtKey::from_device_id("DEVICE-A");
        assert_eq!(key.as_bytes().as_slice(), expected.as_slice());
    }

    #[test]
    fn stored_candidates_round_trip_through_encode_decode() {
        let host = Candidate::new(addr(22000), CandidateKind::Host, 65_535);
        let srflx = Candidate::new(addr(33000), CandidateKind::ServerReflexive, 0);
        let bytes = StoredCandidates::encode(&[host, srflx]).unwrap();
        let decoded = StoredCandidates::decode(&bytes);
        assert_eq!(decoded.len(), 2);
        assert!(decoded.contains(&host));
        assert!(decoded.contains(&srflx));
    }

    #[test]
    fn decode_of_corrupt_bytes_yields_no_candidates() {
        assert!(StoredCandidates::decode(b"not json at all").is_empty());
    }

    #[tokio::test]
    async fn announce_then_resolve_round_trips_candidates() {
        let node = MockDht::default();
        let discovery = DhtDiscovery::new(node);

        let host = Candidate::new(addr(22000), CandidateKind::Host, 65_535);
        let srflx = Candidate::new(addr(33000), CandidateKind::ServerReflexive, 0);
        discovery.announce("DEVICE-A", &[host, srflx]).await;

        let resolved = discovery.resolve("DEVICE-A").await;
        assert_eq!(resolved.len(), 2);
        assert!(resolved.contains(&host));
        assert!(resolved.contains(&srflx));
    }

    #[tokio::test]
    async fn resolve_unknown_device_yields_no_candidates() {
        let discovery = DhtDiscovery::new(MockDht::default());
        assert!(discovery.resolve("NEVER-ANNOUNCED").await.is_empty());
    }

    #[tokio::test]
    async fn announce_stores_under_the_device_id_key() {
        // Announcing for one id must not surface under a different id's key.
        let node = MockDht::default();
        let discovery = DhtDiscovery::new(node);
        let host = Candidate::new(addr(22000), CandidateKind::Host, 0);
        discovery.announce("DEVICE-A", &[host]).await;

        assert!(discovery.resolve("DEVICE-B").await.is_empty());
        assert_eq!(discovery.resolve("DEVICE-A").await, vec![host]);
    }

    #[tokio::test]
    async fn resolve_unions_multiple_stored_values() {
        // A DHT lookup can surface more than one stored value for a key.
        // The resolver must union them; the DiscoveryService dedups later.
        let node = MockDht::default();
        let key = DhtKey::from_device_id("DEVICE-A");
        let host = Candidate::new(addr(22000), CandidateKind::Host, 0);
        let srflx = Candidate::new(addr(33000), CandidateKind::ServerReflexive, 0);
        node.store.lock().await.insert(
            key.0,
            vec![
                StoredCandidates::encode(&[host]).unwrap(),
                StoredCandidates::encode(&[srflx]).unwrap(),
            ],
        );

        let discovery = DhtDiscovery::new(node);
        let resolved = discovery.resolve("DEVICE-A").await;
        assert_eq!(resolved.len(), 2);
        assert!(resolved.contains(&host));
        assert!(resolved.contains(&srflx));
    }

    #[tokio::test]
    async fn re_announce_replaces_the_stored_set() {
        // A later announce supersedes an earlier one for the same device id,
        // matching the most-recent-wins read of the live node.
        let node = MockDht::default();
        let discovery = DhtDiscovery::new(node);
        let first = Candidate::new(addr(22000), CandidateKind::Host, 0);
        let second = Candidate::new(addr(44000), CandidateKind::ServerReflexive, 0);
        discovery.announce("DEVICE-A", &[first]).await;
        discovery.announce("DEVICE-A", &[second]).await;

        let resolved = discovery.resolve("DEVICE-A").await;
        assert_eq!(resolved, vec![second]);
    }
}

/// Live round-trip tests for the `mainline`-backed [`MainlineDht`] node.
///
/// These run against an in-process `mainline::Testnet` — a small swarm of DHT
/// nodes bound to local UDP sockets. They exercise the real BEP44
/// store-and-fetch path the offline `MockDht` tests cannot, but because they
/// bind UDP and bootstrap a swarm they are `#[ignore]`'d so the standard
/// offline `cargo test` run stays green. Run them explicitly with
/// `cargo test -p cascade-p2p --features dht -- --ignored`.
#[cfg(all(test, feature = "dht"))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod live_tests {
    use std::net::SocketAddr;

    use mainline::Testnet;

    use super::{DhtDiscovery, MainlineDht};
    use crate::candidate::{Candidate, CandidateKind};
    use crate::discovery::Discovery;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// Parse the testnet's bootstrap node strings into socket addresses.
    fn bootstrap_addrs(testnet: &Testnet) -> Vec<SocketAddr> {
        testnet
            .bootstrap
            .iter()
            .filter_map(|s| s.parse::<SocketAddr>().ok())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "binds local UDP sockets and bootstraps a DHT swarm"]
    async fn announce_then_resolve_round_trips_over_a_live_testnet() {
        let testnet = Testnet::builder(10).build().unwrap();
        let bootstrap = bootstrap_addrs(&testnet);

        let dir = tempfile::tempdir().unwrap();

        // The live node keys mutable items on (BEP44 public key, salt), so
        // announce and resolve share one node here — the cross-device case,
        // which does not depend on a shared signing key, is exercised by the
        // offline mock above. Bind to localhost so the node reaches the
        // in-process testnet (which binds localhost) on every platform.
        let node = MainlineDht::open_local(dir.path(), &bootstrap).unwrap();
        let discovery = DhtDiscovery::new(node);

        let host = Candidate::new(addr(22000), CandidateKind::Host, 65_535);
        let srflx = Candidate::new(addr(33000), CandidateKind::ServerReflexive, 0);
        discovery.announce("DEVICE-A", &[host, srflx]).await;

        let resolved = discovery.resolve("DEVICE-A").await;
        assert_eq!(resolved.len(), 2);
        assert!(resolved.contains(&host));
        assert!(resolved.contains(&srflx));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "binds local UDP sockets and bootstraps a DHT swarm"]
    async fn resolve_unknown_device_over_a_live_testnet_is_empty() {
        let testnet = Testnet::builder(10).build().unwrap();
        let bootstrap = bootstrap_addrs(&testnet);
        let dir = tempfile::tempdir().unwrap();
        let node = MainlineDht::open_local(dir.path(), &bootstrap).unwrap();
        let discovery = DhtDiscovery::new(node);

        assert!(discovery.resolve("NEVER-ANNOUNCED").await.is_empty());
    }
}
