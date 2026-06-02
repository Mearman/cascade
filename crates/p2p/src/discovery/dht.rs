//! Kademlia/Mainline-DHT discovery source.
//!
//! The announce server ([`mod@super::announce`]) is a rendezvous directory for
//! two devices that have never met and sit on different networks: a device
//! publishes its candidate set keyed by device id, and any other device looks
//! those candidates up by id. The DHT plays the same "find an unknown peer"
//! role without a central directory — it is the serverless replacement the v9
//! roadmap and the design doc call for (see `docs/design.md`, "Peer
//! discovery"). Instead of storing the candidate set on one server, the device
//! stores it in the `BitTorrent` Mainline DHT as a BEP44 mutable item whose
//! signing keypair is derived deterministically from its device id; any peer
//! that knows the device id derives the same keypair, computes the same BEP44
//! target, and reads the candidate set back out.
//!
//! ## Why the keypair is derived from the device id
//!
//! BEP44 mutable items are addressed by `target = SHA-1(public_key || salt)`,
//! where `public_key` is the *writer's* ed25519 verifying key (see
//! `mainline::MutableItem::target_from_key`). A naive design that signs with a
//! random per-node key cannot work as a rendezvous: device B resolving device
//! A would compute `SHA-1(B_pubkey || ...)`, a different target from where A
//! wrote under `SHA-1(A_pubkey || ...)`, so B could never read A's value. The
//! whole point of the source — "any peer that knows the device id reads the
//! candidate set" — requires that the *public key half* of the target be
//! derivable from the device id alone. So the ed25519 signing key is seeded
//! from the device id with a domain-separated hash: the announcer and the
//! looker-up independently derive the identical keypair, hence the identical
//! target, with no shared secret and no per-node persisted key.
//!
//! ## Shape
//!
//! Discovery here is keyed lookup over a distributed hash table:
//!
//! - [`DhtKey::from_device_id`] maps a base32 device id to a 32-byte ed25519
//!   seed by hashing it (SHA-256 over a domain-separation prefix and the id).
//!   The mapping is deterministic, so the announcer and the looker-up derive
//!   the same seed — and therefore the same BEP44 keypair and target — from the
//!   same device id without any shared state.
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
use sha2::{Digest, Sha256};

use super::Discovery;
use super::announce::WireCandidate;
use crate::candidate::Candidate;

/// Width of the device-id-derived ed25519 seed in bytes.
///
/// A BEP44 mutable item is signed by an ed25519 keypair, and ed25519 keys are
/// built from a 32-byte seed (`SigningKey::from_bytes`). The device-id mapping
/// produces exactly that width so the seed feeds the keypair directly, with no
/// truncation or padding.
pub const DHT_KEY_LEN: usize = 32;

/// Domain-separation prefix mixed into the device-id hash before it becomes an
/// ed25519 seed.
///
/// The device id is hashed for several unrelated purposes across the codebase
/// (it is itself a SHA-256 of the TLS certificate). Prefixing the hash input
/// with a fixed, purpose-specific tag ensures the BEP44 seed cannot collide
/// with any other use of the same id, so deriving the signing key here never
/// reuses key material derived elsewhere.
const DHT_SEED_DOMAIN: &[u8] = b"cascade-dht-bep44-seed-v1";

/// A device-id-derived ed25519 seed for BEP44 addressing.
///
/// Derived deterministically from a device id by [`DhtKey::from_device_id`].
/// Two devices that agree on a device id derive the same seed, hence the same
/// BEP44 signing keypair and the same DHT target, which is what lets a
/// looker-up address the announcer's stored candidate set without any prior
/// contact or shared secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DhtKey(pub [u8; DHT_KEY_LEN]);

impl DhtKey {
    /// Map a base32 device id to its BEP44 ed25519 seed.
    ///
    /// The seed is `SHA-256(DHT_SEED_DOMAIN || device_id)`, whose 256-bit
    /// digest is exactly the ed25519 seed width. Hashing (rather than using the
    /// device-id bytes directly) keeps the seed independent of the id's own
    /// encoding and length, and the domain-separation prefix keeps it distinct
    /// from any other hash of the same id. The announcer and looker-up derive
    /// the same seed from the same id, so both compute the same BEP44 keypair
    /// and target.
    #[must_use]
    pub fn from_device_id(device_id: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(DHT_SEED_DOMAIN);
        hasher.update(device_id.as_bytes());
        let digest = hasher.finalize();
        let mut key = [0u8; DHT_KEY_LEN];
        // SHA-256 produces exactly `DHT_KEY_LEN` bytes, so the copy is total.
        key.copy_from_slice(&digest);
        Self(key)
    }

    /// The raw ed25519 seed bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; DHT_KEY_LEN] {
        &self.0
    }
}

/// How long the resolver waits for a BEP44 `get` before giving up.
///
/// A Mainline DHT `get` walks the network iteratively, hop by hop, towards the
/// nodes closest to the target — several round-trips, not one. The `mainline`
/// crate's own per-request timeout (`mainline::DEFAULT_REQUEST_TIMEOUT`) is
/// two seconds *per hop*; a full iterative lookup against the live network
/// routinely takes several of those in series before it either yields the value
/// or exhausts the closest nodes. Twenty seconds is a generous ceiling over that
/// real BEP44 get latency: long enough that a slow-but-live lookup completes,
/// short enough that a wedged or partitioned lookup fails the resolve rather
/// than hanging the management-plane dial that waits on it. The live node treats
/// a lookup that overruns this as "no value found" — the same absence every
/// [`Discovery`] source reports for a peer it cannot place — and logs the
/// timeout distinctly from a clean not-found so the two are tellable apart in
/// the field.
pub const DHT_RESOLVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Conservative estimate, in seconds, of how long a storing node keeps a BEP44
/// mutable item before expiring it.
///
/// [BEP44](https://www.bittorrent.org/beps/bep_0044.html) leaves the exact
/// retention to the storing node, but the `BitTorrent` network's de-facto figure
/// — the same two hours BEP5 uses for an `announce_peer` lease — is what storing
/// implementations converge on: a value not refreshed within roughly two hours
/// is dropped. The republish cadence is derived from this so an announced
/// candidate set never silently lapses out of the DHT between refreshes. Held in
/// seconds (not a `Duration`) so the republish interval below derives from it by
/// plain const integer arithmetic.
const BEP44_VALUE_EXPIRY_SECS: u64 = 2 * 60 * 60;

/// Divisor applied to the BEP44 expiry to set the republish cadence.
///
/// Republishing every *half* the expiry window keeps the value continuously
/// present with a full window of slack: one missed refresh (a transient network
/// blip) still leaves the previous value live until the next tick. Named so the
/// "refresh twice per expiry window" intent is explicit rather than a bare `2`.
const DHT_REPUBLISH_DIVISOR: u64 = 2;

/// How often a device republishes its candidate set into the DHT.
///
/// BEP44 mutable items are soft state: a storing node drops a value it has not
/// seen refreshed within roughly the BEP44 expiry window. Refreshing twice per
/// expiry window keeps the value present without hammering the DHT the way a
/// minute-scale cadence would, and the interval is derived from the expiry
/// estimate rather than picked arbitrarily, so the two move together: widen the
/// expiry belief and the cadence widens with it.
pub const DHT_REPUBLISH_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(BEP44_VALUE_EXPIRY_SECS / DHT_REPUBLISH_DIVISOR);

/// Maximum length, in bytes, of a BEP44 mutable-item value.
///
/// [BEP44](https://www.bittorrent.org/beps/bep_0044.html) caps a stored value
/// at 1000 bytes, and storing nodes on the live Mainline DHT enforce this:
/// `mainline` does not pre-check client-side, so an oversized `put_mutable`
/// reports success locally while the network silently drops the value. The
/// encoder therefore bounds the *encoded byte length* against this ceiling, not
/// the candidate count, so a value that is put is a value that can actually be
/// stored.
const BEP44_MAX_VALUE_LEN: usize = 1000;

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
    /// Encode a candidate set into the opaque bytes stored in the DHT,
    /// guaranteed to fit the BEP44 1000-byte value ceiling.
    ///
    /// The candidate count is the wrong thing to cap against: the announce
    /// directory's HTTP path has no size limit, but a BEP44 value does, and
    /// even a couple of dozen candidates serialise past 1000 bytes. So this
    /// bounds the *encoded length* instead. Candidates are sorted by descending
    /// priority (highest first) and the lowest-priority entries are dropped one
    /// at a time until the JSON encoding fits [`BEP44_MAX_VALUE_LEN`], so the
    /// most useful candidates survive and the stored value is one the network
    /// will actually accept rather than silently reject.
    ///
    /// Returns the JSON-encoded bytes, or the serialisation error (which only
    /// arises on an allocator failure for this shape). An empty candidate set
    /// encodes to the empty-array envelope, which is well within the ceiling.
    fn encode(candidates: &[Candidate]) -> Result<Vec<u8>, serde_json::Error> {
        let mut wire: Vec<WireCandidate> = candidates
            .iter()
            .copied()
            .map(WireCandidate::from)
            .collect();
        // Highest priority first, so dropping from the tail removes the
        // least-useful candidates. The input is already announcer-ordered, but
        // sorting here makes the byte-budget trim correct regardless of caller.
        wire.sort_unstable_by_key(|c| std::cmp::Reverse(c.priority));

        loop {
            let encoded = serde_json::to_vec(&Self {
                candidates: wire.clone(),
            })?;
            if encoded.len() <= BEP44_MAX_VALUE_LEN || wire.is_empty() {
                if encoded.len() > BEP44_MAX_VALUE_LEN {
                    // Only reachable when even the empty-array envelope exceeds
                    // the ceiling, which cannot happen for this fixed shape;
                    // surfacing it loudly beats silently announcing a value the
                    // network will drop.
                    tracing::warn!(
                        target: "cascade::p2p::discovery::dht",
                        encoded_len = encoded.len(),
                        limit = BEP44_MAX_VALUE_LEN,
                        "DHT candidate value exceeds the BEP44 ceiling even when empty",
                    );
                }
                return Ok(encoded);
            }
            // Drop the lowest-priority candidate and retry.
            wire.pop();
        }
    }

    /// Decode opaque DHT bytes into in-memory candidates.
    ///
    /// Drops any candidate whose kind tag is unknown — a malformed or hostile
    /// stored value must not coerce into a wrong kind. Returns an empty vector
    /// when the bytes do not decode to a [`StoredCandidates`], so a corrupt
    /// value reads as "no candidates" rather than aborting the lookup.
    fn decode(bytes: &[u8]) -> Vec<Candidate> {
        let parsed: Self = match serde_json::from_slice(bytes) {
            Ok(parsed) => parsed,
            Err(_) => return Vec::new(),
        };
        parsed
            .candidates
            .into_iter()
            .filter_map(WireCandidate::to_candidate)
            .collect()
    }
}

/// Outcome of a [`DhtNode::get`] lookup.
///
/// A DHT lookup has three meaningfully different endings, and collapsing them
/// to a bare `Vec<Vec<u8>>` would lose the one that matters operationally:
///
/// - [`Found`](DhtGetOutcome::Found) — at least one node returned a value. The
///   list carries every value seen (a lookup can surface more than one: two
///   storing nodes, or a value mid-replacement), and the caller merges them.
/// - [`NotFound`](DhtGetOutcome::NotFound) — the lookup completed and no node
///   holds a value for the key. The peer simply has not announced (or its entry
///   has expired). This is the normal "unknown device" ending.
/// - [`TimedOut`](DhtGetOutcome::TimedOut) — the iterative lookup did not finish
///   within [`DHT_RESOLVE_TIMEOUT`]. The peer *might* have a value we never
///   reached. This is a transport-health signal, not absence, and the resolver
///   logs it distinctly so a partitioned or wedged DHT is diagnosable rather
///   than masquerading as "every peer is offline".
///
/// [`DhtDiscovery::resolve`] folds both `NotFound` and `TimedOut` to "no
/// candidates" — the [`Discovery`] contract cannot surface a per-source error,
/// and a single source coming up empty is normal when others may still succeed
/// — but it logs the two endings differently so the distinction survives where
/// it is useful: in the logs of a node that cannot reach anyone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DhtGetOutcome {
    /// The lookup found one or more stored values for the key.
    Found(Vec<Vec<u8>>),
    /// The lookup completed cleanly and no node holds a value for the key.
    NotFound,
    /// The lookup did not complete within [`DHT_RESOLVE_TIMEOUT`].
    TimedOut,
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
/// `get` returns a [`DhtGetOutcome`] that distinguishes a clean not-found from a
/// timed-out lookup, because the two mean different things — one peer is absent,
/// the other the network is unreachable — and a node that can place no peers at
/// all wants that distinction in its logs.
#[async_trait]
pub trait DhtNode: Send + Sync {
    /// Store `value` under `key` in the DHT. Best-effort: a store that does
    /// not reach enough nodes is not surfaced as an error here, because
    /// discovery announce is itself best-effort and runs on a background loop.
    async fn put(&self, key: DhtKey, value: Vec<u8>);

    /// Look `key` up in the DHT, distinguishing a clean not-found from a
    /// timed-out lookup (see [`DhtGetOutcome`]). Implementations bound the
    /// lookup at [`DHT_RESOLVE_TIMEOUT`] so a wedged iterative search reports
    /// [`DhtGetOutcome::TimedOut`] rather than hanging the resolver.
    async fn get(&self, key: DhtKey) -> DhtGetOutcome;
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
    /// here is safe.
    ///
    /// A clean not-found and a timed-out lookup both yield no candidates — the
    /// [`Discovery`] contract has no per-source error channel, and one source
    /// coming up empty is normal — but the two are logged distinctly so a node
    /// that can reach no peers can tell "this device has not announced" from
    /// "the DHT is unreachable from here".
    async fn resolve(&self, device_id: &str) -> Vec<Candidate> {
        let key = DhtKey::from_device_id(device_id);
        match self.node.get(key).await {
            DhtGetOutcome::Found(values) => values
                .iter()
                .flat_map(|bytes| StoredCandidates::decode(bytes))
                .collect(),
            DhtGetOutcome::NotFound => {
                tracing::debug!(
                    target: "cascade::p2p::discovery::dht",
                    device_id = %device_id,
                    "DHT lookup completed with no stored value for device",
                );
                Vec::new()
            }
            DhtGetOutcome::TimedOut => {
                tracing::warn!(
                    target: "cascade::p2p::discovery::dht",
                    device_id = %device_id,
                    timeout_secs = DHT_RESOLVE_TIMEOUT.as_secs(),
                    "DHT lookup timed out before completing; treating as no candidates",
                );
                Vec::new()
            }
        }
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
pub use live::{DEFAULT_DHT_BOOTSTRAP_NODES, MainlineDht};

#[cfg(feature = "dht")]
mod live {
    use std::net::SocketAddr;

    use anyhow::Result;
    use async_trait::async_trait;
    use mainline::{DEFAULT_BOOTSTRAP_NODES, Dht, MutableItem, SigningKey};

    use super::{DHT_RESOLVE_TIMEOUT, DhtGetOutcome, DhtKey, DhtNode};

    /// The default Mainline-DHT bootstrap nodes used when an operator supplies
    /// no `dht_bootstrap_nodes` of their own.
    ///
    /// These are the long-standing public DHT routers — `router.bittorrent.com`,
    /// `dht.transmissionbt.com`, `dht.libtorrent.org`, and `relay.pkarr.org` —
    /// that the wider `BitTorrent` ecosystem bootstraps against, surfaced here as
    /// a named constant rather than left implicit in the `mainline` crate so an
    /// operator can see exactly what an out-of-the-box DHT join talks to. The set
    /// is the `mainline` crate's own [`mainline::DEFAULT_BOOTSTRAP_NODES`],
    /// re-exposed so the default is documented at the cascade layer and stays in
    /// lockstep with the crate rather than being hand-copied (which would rot if
    /// the crate's set changed).
    ///
    /// Each entry is a `host:port` string resolved at join time. An operator who
    /// wants to pin their own nodes sets `dht_bootstrap_nodes` in the backend
    /// TOML; an omitted or explicitly empty override falls back to exactly this
    /// set, so the DHT is usable without supplying any bootstrap nodes.
    pub const DEFAULT_DHT_BOOTSTRAP_NODES: &[&str] = &DEFAULT_BOOTSTRAP_NODES;

    /// Mainline-DHT-backed [`DhtNode`].
    ///
    /// Wraps the maintained `mainline` crate's `BitTorrent` Mainline DHT node and
    /// stores candidate sets as BEP44 mutable items. Each item is signed by an
    /// ed25519 keypair derived deterministically from the device id being
    /// announced or resolved (the [`DhtKey`] carries that key's seed), so the
    /// BEP44 target `SHA-1(public_key)` is the same on every node that knows the
    /// id — that is what makes cross-device resolution work without a shared
    /// secret or a persisted per-node key. The salt is left empty: the device id
    /// already lives in the keypair, so it must not also live in the salt, and
    /// the announcer and looker-up agree on "no salt" trivially.
    ///
    /// Reads take the most-recent mutable item for the derived public key;
    /// writes bump the sequence number monotonically using the wall clock so a
    /// later announce supersedes an earlier one.
    ///
    /// All DHT I/O is best-effort: a store or lookup that fails on the wire is
    /// logged and surfaces as "no candidates", never an error, matching the
    /// best-effort contract every [`super::Discovery`] source upholds.
    #[derive(Clone)]
    pub struct MainlineDht {
        dht: mainline::async_dht::AsyncDht,
    }

    impl std::fmt::Debug for MainlineDht {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MainlineDht").finish_non_exhaustive()
        }
    }

    impl MainlineDht {
        /// Build a Mainline-DHT node, bootstrapping against `bootstrap_nodes`.
        ///
        /// An empty `bootstrap_nodes` (the operator supplied none, or supplied
        /// an explicitly empty `dht_bootstrap_nodes`) joins the public default
        /// set, [`DEFAULT_DHT_BOOTSTRAP_NODES`] — which is the `mainline` crate's
        /// own default, re-exposed here so the out-of-the-box join is documented
        /// at the cascade layer. The default set is left for the crate to resolve
        /// on its own DHT actor thread rather than being handed to
        /// `builder.bootstrap()`: the crate resolves the default-set *hostnames*
        /// with blocking `getaddrinfo`, and the builder's `bootstrap()` does that
        /// synchronously on the caller's thread, so deferring it keeps the
        /// resolution off whatever thread opens the node and avoids the
        /// crate's empty-result trap (a `bootstrap()` set that all fails to
        /// resolve is stored verbatim as an empty set and is *not* replaced by
        /// the crate default). A non-empty `bootstrap_nodes` is an operator
        /// override of already-resolved [`SocketAddr`]s, passed verbatim — no
        /// name resolution, so it is safe to hand straight to `bootstrap()`.
        ///
        /// There is no persisted signing key: every BEP44 keypair is derived
        /// from the device id at put/get time, so the node holds no long-lived
        /// secret of its own.
        ///
        /// Returns an error only on a process-level failure — binding the DHT
        /// UDP socket — not on a per-query failure, which is handled best-effort
        /// inside [`DhtNode`].
        pub fn open(bootstrap_nodes: &[SocketAddr]) -> Result<Self> {
            Self::open_inner(bootstrap_nodes, None)
        }

        /// Shared constructor for [`Self::open`] and the test helper.
        ///
        /// `bind_ip`, when set, pins the DHT UDP socket to a specific local
        /// address rather than the all-interfaces default. Production opens with
        /// `None` (the node must be reachable on every interface); the live test
        /// pins it to localhost so it talks to the in-process testnet.
        ///
        /// An empty `bootstrap_nodes` defers to the crate's own resolution of
        /// [`DEFAULT_DHT_BOOTSTRAP_NODES`] (see [`Self::open`]); a non-empty list
        /// of already-resolved addresses is passed to `bootstrap()` verbatim, so
        /// the live testnet helper — which always passes concrete localhost
        /// bootstrap addresses — pins its swarm exactly.
        fn open_inner(
            bootstrap_nodes: &[SocketAddr],
            bind_ip: Option<std::net::Ipv4Addr>,
        ) -> Result<Self> {
            let mut builder = Dht::builder();
            // Only override the crate's bootstrap set when the operator supplied
            // concrete, already-resolved addresses. The empty (default) case is
            // deliberately left untouched so the crate resolves
            // DEFAULT_DHT_BOOTSTRAP_NODES on its own actor thread — see the
            // `open` doc for why handing the default hostnames to `bootstrap()`
            // would both block this thread on `getaddrinfo` and risk a stored
            // empty set.
            if let Some(override_addrs) = bootstrap_override(bootstrap_nodes) {
                builder.bootstrap(&override_addrs);
            }
            if let Some(ip) = bind_ip {
                builder.bind_address(ip);
            }
            let dht = builder
                .build()
                .map_err(|err| anyhow::anyhow!("building mainline DHT node: {err}"))?
                .as_async();

            Ok(Self { dht })
        }

        /// Open a node pinned to localhost, for the in-process testnet round-trip
        /// tests. Not part of the production surface — the live network requires
        /// the all-interfaces bind [`Self::open`] uses.
        #[cfg(test)]
        pub(super) fn open_local(bootstrap_nodes: &[SocketAddr]) -> Result<Self> {
            Self::open_inner(bootstrap_nodes, Some(std::net::Ipv4Addr::LOCALHOST))
        }
    }

    /// Decide whether to override the crate's bootstrap set.
    ///
    /// This is the default-vs-override decision, isolated from socket binding so
    /// it is unit-testable. An empty `configured` list returns [`None`]: the
    /// caller leaves `builder.bootstrap()` uncalled so the crate joins its own
    /// [`DEFAULT_DHT_BOOTSTRAP_NODES`] (resolving those hostnames on its actor
    /// thread). A non-empty list returns [`Some`] of the operator's
    /// already-resolved addresses, to be passed to `bootstrap()` verbatim. The
    /// `host:port` strings the `mainline` builder ultimately wants are produced
    /// by its own `ToSocketAddrs` impl for [`SocketAddr`], so no DNS happens for
    /// the override case.
    fn bootstrap_override(configured: &[SocketAddr]) -> Option<Vec<SocketAddr>> {
        if configured.is_empty() {
            None
        } else {
            Some(configured.to_vec())
        }
    }

    /// Build the BEP44 signing keypair from a device-id-derived [`DhtKey`].
    ///
    /// The [`DhtKey`] *is* the ed25519 seed (`SHA-256` of a domain prefix and
    /// the device id), so `SigningKey::from_bytes` reconstructs the identical
    /// keypair on every node that knows the id. This is the whole mechanism that
    /// makes the BEP44 target `SHA-1(public_key)` shared across devices.
    fn signing_key_for(key: DhtKey) -> SigningKey {
        SigningKey::from_bytes(key.as_bytes())
    }

    #[async_trait]
    impl DhtNode for MainlineDht {
        async fn put(&self, key: DhtKey, value: Vec<u8>) {
            let seq = unix_millis();
            // No salt: the device id is already encoded in the keypair, so the
            // target is SHA-1(public_key), which the resolver reproduces.
            let item = MutableItem::new(signing_key_for(key), &value, seq, None);
            let dht = self.dht.clone();
            if let Err(err) = dht.put_mutable(item, None).await {
                tracing::debug!(
                    target: "cascade::p2p::discovery::dht",
                    error = %err,
                    "DHT mutable put failed",
                );
            }
        }

        async fn get(&self, key: DhtKey) -> DhtGetOutcome {
            // Derive the announcer's public key from the queried device id and
            // read the most-recent item under SHA-1(public_key) — the exact
            // target the announcer wrote to. No salt, matching the put path.
            let public_key = signing_key_for(key).verifying_key().to_bytes();
            let dht = self.dht.clone();
            // Bound the iterative lookup: `get_mutable_most_recent` returns
            // `None` for a clean not-found, but a partitioned or wedged lookup
            // could otherwise run far longer than the resolver's caller is
            // willing to wait. A timeout reads as `TimedOut`, distinct from the
            // `None`-driven `NotFound`, so the two endings are tellable apart.
            match tokio::time::timeout(
                DHT_RESOLVE_TIMEOUT,
                dht.get_mutable_most_recent(&public_key, None),
            )
            .await
            {
                Ok(Some(item)) => DhtGetOutcome::Found(vec![item.value().to_vec()]),
                Ok(None) => DhtGetOutcome::NotFound,
                Err(_elapsed) => DhtGetOutcome::TimedOut,
            }
        }
    }

    /// Current Unix time in milliseconds, used as the BEP44 mutable-item
    /// sequence number so a later announce supersedes an earlier one.
    fn unix_millis() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    mod tests {
        use std::net::SocketAddr;

        use super::{DEFAULT_DHT_BOOTSTRAP_NODES, bootstrap_override};

        #[test]
        fn empty_override_defers_to_the_crate_default() {
            // No operator-supplied bootstrap nodes → no override, so the node
            // leaves `bootstrap()` uncalled and the crate resolves its own
            // DEFAULT_DHT_BOOTSTRAP_NODES on its actor thread. This is what keeps
            // the default hostnames off the opening thread and avoids the crate's
            // empty-result trap. The default set must still be non-empty so the
            // crate has something to resolve.
            assert_eq!(bootstrap_override(&[]), None);
            assert!(
                !DEFAULT_DHT_BOOTSTRAP_NODES.is_empty(),
                "the crate default set must be non-empty",
            );
        }

        #[test]
        fn non_empty_override_is_passed_through_verbatim() {
            // Operator-pinned nodes are returned as-is for the builder to use
            // verbatim, with no DNS and no mixing-in of the public default.
            let pinned: Vec<SocketAddr> = vec![
                "127.0.0.1:6881".parse().unwrap(),
                "10.0.0.1:6882".parse().unwrap(),
            ];
            let resolved = bootstrap_override(&pinned);
            assert_eq!(resolved, Some(pinned));
        }
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

        async fn get(&self, key: DhtKey) -> DhtGetOutcome {
            // A key with stored values reads as `Found`; a key never written
            // reads as a clean `NotFound`. The mock has no network, so it never
            // times out — the `TimedOut` ending is exercised by `TimeoutDht`.
            self.store
                .lock()
                .await
                .get(&key.0)
                .cloned()
                .map_or(DhtGetOutcome::NotFound, DhtGetOutcome::Found)
        }
    }

    /// A [`DhtNode`] double whose `get` always reports the timed-out ending,
    /// standing in for an iterative lookup that never completes. Lets the
    /// resolver's not-found-vs-timeout handling be exercised without a network.
    #[derive(Clone, Default)]
    struct TimeoutDht;

    #[async_trait]
    impl DhtNode for TimeoutDht {
        async fn put(&self, _key: DhtKey, _value: Vec<u8>) {}

        async fn get(&self, _key: DhtKey) -> DhtGetOutcome {
            DhtGetOutcome::TimedOut
        }
    }

    #[test]
    fn republish_interval_refreshes_strictly_inside_the_bep44_expiry_window() {
        // The republish cadence exists so an announced value never lapses: it
        // must refresh strictly faster than the expiry window, with slack for a
        // missed tick. Pin the derivation so a change to either constant that
        // would let the value expire between refreshes is caught.
        let expiry = std::time::Duration::from_secs(BEP44_VALUE_EXPIRY_SECS);
        assert!(
            DHT_REPUBLISH_INTERVAL < expiry,
            "republish interval {DHT_REPUBLISH_INTERVAL:?} must be shorter than the BEP44 expiry {expiry:?}",
        );
        // A missed refresh (one whole interval) must still leave the previous
        // value live, i.e. two intervals must not exceed the expiry window.
        assert!(
            DHT_REPUBLISH_INTERVAL.saturating_mul(2) <= expiry,
            "two republish intervals must fit inside the expiry window so one missed tick is survivable",
        );
    }

    #[test]
    fn device_id_maps_to_an_ed25519_seed_width_key() {
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
    fn key_matches_independent_domain_separated_sha256_of_device_id() {
        // Pin the mapping to SHA-256 of the domain prefix and the id bytes so a
        // future change to the derivation is caught — both ends, and the BEP44
        // keypair they each build from it, must agree on exactly this.
        let mut hasher = Sha256::new();
        hasher.update(DHT_SEED_DOMAIN);
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

    #[test]
    fn encode_keeps_a_large_set_within_the_bep44_value_ceiling() {
        // Far more candidates than will fit in 1000 bytes. The encoder must
        // drop the lowest-priority ones until the value fits, never emit a
        // value the network would silently reject.
        let many: Vec<Candidate> = (0..256u32)
            .map(|i| {
                let port = u16::try_from(20_000 + i).unwrap_or(u16::MAX);
                Candidate::new(addr(port), CandidateKind::Host, port)
            })
            .collect();
        let bytes = StoredCandidates::encode(&many).unwrap();
        assert!(
            bytes.len() <= BEP44_MAX_VALUE_LEN,
            "encoded value {} exceeds BEP44 ceiling {BEP44_MAX_VALUE_LEN}",
            bytes.len(),
        );
    }

    #[test]
    fn encode_drops_lowest_priority_candidates_when_over_budget() {
        // When the set will not fit, the highest-priority candidates must
        // survive and the lowest-priority ones must be dropped. A host
        // candidate with maximum local preference outranks relayed candidates
        // with zero local preference (host type preference dominates), so the
        // host must be the one that survives the byte-budget trim.
        let top = Candidate::new(addr(20000), CandidateKind::Host, u16::MAX);
        let lesser: Vec<Candidate> = (0..256u32)
            .map(|i| {
                let port = u16::try_from(30_000 + i).unwrap_or(u16::MAX);
                Candidate::new(addr(port), CandidateKind::Relayed, 0)
            })
            .collect();
        let mut set = vec![top];
        set.extend(lesser);

        let bytes = StoredCandidates::encode(&set).unwrap();
        assert!(bytes.len() <= BEP44_MAX_VALUE_LEN);
        let decoded = StoredCandidates::decode(&bytes);
        assert!(
            decoded.contains(&top),
            "the highest-priority candidate must survive the byte-budget trim",
        );
        assert!(
            decoded.len() < set.len(),
            "some low-priority candidates must have been dropped",
        );
    }

    #[test]
    fn encode_of_empty_set_is_well_within_the_ceiling() {
        let bytes = StoredCandidates::encode(&[]).unwrap();
        assert!(bytes.len() <= BEP44_MAX_VALUE_LEN);
        assert!(StoredCandidates::decode(&bytes).is_empty());
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
    async fn get_of_unknown_key_is_a_clean_not_found() {
        // A key never written reads as the not-found ending, distinct from a
        // timeout — the resolver leans on this distinction for its logging.
        let node = MockDht::default();
        let outcome = node.get(DhtKey::from_device_id("NEVER-ANNOUNCED")).await;
        assert_eq!(outcome, DhtGetOutcome::NotFound);
    }

    #[tokio::test]
    async fn get_of_known_key_is_found_with_the_stored_value() {
        let node = MockDht::default();
        let host = Candidate::new(addr(22000), CandidateKind::Host, 0);
        let bytes = StoredCandidates::encode(&[host]).unwrap();
        node.put(DhtKey::from_device_id("DEVICE-A"), bytes.clone())
            .await;
        let outcome = node.get(DhtKey::from_device_id("DEVICE-A")).await;
        assert_eq!(outcome, DhtGetOutcome::Found(vec![bytes]));
    }

    #[tokio::test]
    async fn resolve_treats_not_found_as_no_candidates() {
        // The clean not-found ending collapses to "no candidates" at the
        // Discovery boundary — a source coming up empty is normal.
        let discovery = DhtDiscovery::new(MockDht::default());
        assert!(discovery.resolve("NEVER-ANNOUNCED").await.is_empty());
    }

    #[tokio::test]
    async fn resolve_treats_timeout_as_no_candidates() {
        // A timed-out lookup also collapses to "no candidates" — the Discovery
        // contract has no per-source error channel — but the resolver logs it
        // distinctly from not-found. Here we only assert the candidate-set
        // behaviour; the distinction lives in the DhtGetOutcome the node
        // reports, exercised by `get_*` above and `TimeoutDht`.
        let discovery = DhtDiscovery::new(TimeoutDht);
        assert!(discovery.resolve("DEVICE-A").await.is_empty());
    }

    #[tokio::test]
    async fn timeout_and_not_found_are_distinct_outcomes() {
        // The two empty endings must not be the same value: a node that can
        // place no peers needs to tell "device has not announced" from "the
        // DHT is unreachable from here".
        assert_ne!(DhtGetOutcome::NotFound, DhtGetOutcome::TimedOut);
        let timed_out = TimeoutDht.get(DhtKey::from_device_id("DEVICE-A")).await;
        let not_found = MockDht::default()
            .get(DhtKey::from_device_id("DEVICE-A"))
            .await;
        assert_eq!(timed_out, DhtGetOutcome::TimedOut);
        assert_eq!(not_found, DhtGetOutcome::NotFound);
        assert_ne!(timed_out, not_found);
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
/// store-and-fetch and republish-supersession paths the offline `MockDht` tests
/// cannot, but because they bind UDP and bootstrap a swarm they are
/// `#[ignore]`'d so the standard offline `cargo test` run stays green. Run them
/// explicitly with `cargo test -p cascade-p2p --features dht -- --ignored`; the
/// `DHT live tests (local mainline testnet)` CI job runs exactly this.
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

        // Bind to localhost so the node reaches the in-process testnet (which
        // binds localhost) on every platform.
        let node = MainlineDht::open_local(&bootstrap).unwrap();
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
    async fn one_node_announces_and_a_separate_node_resolves_over_a_live_testnet() {
        // The actual purpose of the source: a device announces on one node and
        // a *different* device, knowing only the device id, resolves it. This
        // is the cross-device path that the offline mock cannot exercise (the
        // mock keys on the DhtKey directly and never builds a BEP44 keypair) —
        // it is the only test that catches a regression to per-node keying,
        // where the announcer and resolver would compute different targets.
        let testnet = Testnet::builder(10).build().unwrap();
        let bootstrap = bootstrap_addrs(&testnet);

        let writer = DhtDiscovery::new(MainlineDht::open_local(&bootstrap).unwrap());
        let reader = DhtDiscovery::new(MainlineDht::open_local(&bootstrap).unwrap());

        let host = Candidate::new(addr(22000), CandidateKind::Host, 65_535);
        let srflx = Candidate::new(addr(33000), CandidateKind::ServerReflexive, 0);
        writer.announce("DEVICE-A", &[host, srflx]).await;

        let found = reader.resolve("DEVICE-A").await;
        assert_eq!(found.len(), 2);
        assert!(found.contains(&host));
        assert!(found.contains(&srflx));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "binds local UDP sockets and bootstraps a DHT swarm"]
    async fn resolve_unknown_device_over_a_live_testnet_is_empty() {
        let testnet = Testnet::builder(10).build().unwrap();
        let bootstrap = bootstrap_addrs(&testnet);
        let node = MainlineDht::open_local(&bootstrap).unwrap();
        let discovery = DhtDiscovery::new(node);

        assert!(discovery.resolve("NEVER-ANNOUNCED").await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "binds local UDP sockets and bootstraps a DHT swarm"]
    async fn republishing_supersedes_the_earlier_value_over_a_live_testnet() {
        // Republish is what keeps an announced candidate set continuously
        // present: BEP44 mutable items are soft state that a storing node drops
        // if it is not refreshed within the expiry window (see
        // `DHT_REPUBLISH_INTERVAL`'s derivation from `BEP44_VALUE_EXPIRY_SECS`).
        // Each refresh is a `put_mutable` carrying a higher sequence number, and
        // a resolver must read back the *most recent* value, not a stale one.
        //
        // This is the live counterpart of the offline `re_announce_replaces_
        // the_stored_set` mock test: it drives the real BEP44 sequence-number
        // path against the swarm, so a regression where republish stopped
        // bumping the seq (or the read stopped preferring the highest seq) — a
        // class of bug the mock cannot see, because the mock simply overwrites
        // its map entry — fails here. The announcer republishes and a *separate*
        // resolver reads, so the supersession is observed across nodes exactly
        // as a real refresh-then-lookup would be.
        //
        // The two announces bracket a full live resolve, so the wall-clock
        // sequence numbers (`unix_millis`) are milliseconds apart and the second
        // strictly exceeds the first — the republish supersedes rather than
        // racing the earlier write.
        let testnet = Testnet::builder(10).build().unwrap();
        let bootstrap = bootstrap_addrs(&testnet);

        let announcer = DhtDiscovery::new(MainlineDht::open_local(&bootstrap).unwrap());
        let resolver = DhtDiscovery::new(MainlineDht::open_local(&bootstrap).unwrap());

        let first = Candidate::new(addr(22000), CandidateKind::Host, 65_535);
        announcer.announce("DEVICE-A", &[first]).await;

        // The first publication is readable before the refresh.
        let before = resolver.resolve("DEVICE-A").await;
        assert_eq!(
            before,
            vec![first],
            "the first announced value must be readable before the republish",
        );

        // Republish a different candidate set under the same device id. The
        // higher wall-clock sequence number must make this value win.
        let refreshed = Candidate::new(addr(44000), CandidateKind::ServerReflexive, 0);
        announcer.announce("DEVICE-A", &[refreshed]).await;

        let after = resolver.resolve("DEVICE-A").await;
        assert_eq!(
            after,
            vec![refreshed],
            "the republished value must supersede the earlier one on resolve",
        );
    }
}
