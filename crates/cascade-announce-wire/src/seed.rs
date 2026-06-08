//! Device-id-derived BEP44 signing seed.
//!
//! Both the announce server and the Mainline DHT address a device's stored
//! candidate set by a keypair *derived from the device id*: the announcer and
//! the looker-up independently hash the id to the same 32-byte ed25519 seed,
//! derive the same keypair, and (on the DHT) compute the same BEP44 target. This
//! is what lets a peer that has never met the announcer address and verify its
//! stored value with no shared secret.
//!
//! The seed and the seed→keypair construction live here so there is exactly one
//! derivation, shared by the announce signing path, the DHT live node's BEP44
//! `MutableItem` signing, and the Worker's write-time verification. One key, one
//! derivation, three carriers.

use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};

/// Width of the device-id-derived ed25519 seed in bytes.
///
/// An ed25519 keypair is built from a 32-byte seed
/// (`SigningKey::from_bytes`), and a BEP44 mutable item is signed by such a
/// keypair. The device-id mapping produces exactly that width so the seed feeds
/// the keypair directly, with no truncation or padding.
pub const DHT_KEY_LEN: usize = 32;

/// Domain-separation prefix mixed into the device-id hash before it becomes an
/// ed25519 seed.
///
/// The device id is hashed for several unrelated purposes across the codebase
/// (it is itself a SHA-256 of the TLS certificate). Prefixing the hash input
/// with a fixed, purpose-specific tag ensures the BEP44 seed cannot collide with
/// any other use of the same id, so deriving the signing key here never reuses
/// key material derived elsewhere.
const DHT_SEED_DOMAIN: &[u8] = b"cascade-dht-bep44-seed-v1";

/// A device-id-derived ed25519 seed for BEP44 addressing.
///
/// Derived deterministically from a device id by [`DhtKey::from_device_id`].
/// Two devices that agree on a device id derive the same seed, hence the same
/// BEP44 signing keypair and the same DHT target, which is what lets a looker-up
/// address the announcer's stored candidate set without any prior contact or
/// shared secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DhtKey(pub [u8; DHT_KEY_LEN]);

impl DhtKey {
    /// Map a base32 device id to its BEP44 ed25519 seed.
    ///
    /// The seed is `SHA-256(DHT_SEED_DOMAIN || device_id)`, whose 256-bit digest
    /// is exactly the ed25519 seed width. Hashing (rather than using the
    /// device-id bytes directly) keeps the seed independent of the id's own
    /// encoding and length, and the domain-separation prefix keeps it distinct
    /// from any other hash of the same id. The announcer and looker-up derive the
    /// same seed from the same id, so both compute the same BEP44 keypair and
    /// target.
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

/// Build the ed25519 signing keypair from a device-id-derived [`DhtKey`] seed.
///
/// The [`DhtKey`] *is* the 32-byte ed25519 seed, so this reconstructs the
/// identical keypair on every node that knows the device id. This is the single
/// seed→keypair site: the announce signing path (via
/// [`keypair_for_device`]) and the DHT live node's BEP44 `MutableItem` signing
/// both build their key here, so the two transports cannot drift onto different
/// keys.
#[must_use]
pub fn signing_key_for_seed(seed: &DhtKey) -> SigningKey {
    SigningKey::from_bytes(seed.as_bytes())
}

/// Derive the ed25519 keypair a device signs its announced candidate set with.
///
/// The seed is the device-id-derived [`DhtKey`] — the same 32-byte value the DHT
/// BEP44 path uses — so the keypair is identical whether the candidate set
/// travels over an announce server or the Mainline DHT, and any peer that knows
/// the device id derives the same verifying key to check the signature.
#[must_use]
pub fn keypair_for_device(device_id: &str) -> SigningKey {
    signing_key_for_seed(&DhtKey::from_device_id(device_id))
}

/// Derive the public verifying key for a device id.
///
/// The resolver derives this key from the id it is resolving and checks the
/// envelope's signature against it. The envelope carries no key of its own, so
/// there is nothing to substitute: a set signed by another device's key simply
/// fails to verify under the resolved id — the substitution defence.
#[must_use]
pub fn verifying_key_for_device(device_id: &str) -> ed25519_dalek::VerifyingKey {
    keypair_for_device(device_id).verifying_key()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_device_id_yields_the_same_seed() {
        assert_eq!(
            DhtKey::from_device_id("DEVICE-A"),
            DhtKey::from_device_id("DEVICE-A")
        );
    }

    #[test]
    fn different_device_ids_yield_different_seeds() {
        assert_ne!(
            DhtKey::from_device_id("DEVICE-A"),
            DhtKey::from_device_id("DEVICE-B")
        );
    }

    #[test]
    fn key_matches_independent_domain_separated_sha256_of_device_id() {
        // Pin the mapping to SHA-256 of the domain prefix and the id bytes so a
        // future change to the derivation is caught — both the announce and the
        // DHT transport, and the BEP44 keypair each builds from it, must agree on
        // exactly this.
        let mut hasher = Sha256::new();
        hasher.update(DHT_SEED_DOMAIN);
        hasher.update(b"DEVICE-A");
        let expected = hasher.finalize();
        let key = DhtKey::from_device_id("DEVICE-A");
        assert_eq!(key.as_bytes().as_slice(), expected.as_slice());
    }

    #[test]
    fn key_is_ed25519_seed_width() {
        assert_eq!(
            DhtKey::from_device_id("DEVICE-A").as_bytes().len(),
            DHT_KEY_LEN
        );
    }

    #[test]
    fn verifying_key_matches_signing_keypair() {
        let signing = keypair_for_device("DEVICE-A");
        assert_eq!(
            verifying_key_for_device("DEVICE-A").to_bytes(),
            signing.verifying_key().to_bytes()
        );
    }

    #[test]
    fn seed_derivation_is_collision_resistant_across_many_ids() {
        // Derive seeds for a large set of distinct device ids and confirm that
        // all seeds are unique.  The probability of any collision under SHA-256
        // is negligible; a collision here would indicate a derivation bug rather
        // than a birthday-paradox event.
        use std::collections::HashSet;
        let count = 1000usize;
        let seeds: HashSet<[u8; DHT_KEY_LEN]> = (0..count)
            .map(|i| *DhtKey::from_device_id(&format!("DEVICE-{i}")).as_bytes())
            .collect();
        assert_eq!(
            seeds.len(),
            count,
            "expected {count} distinct seeds, got collisions"
        );
    }

    #[test]
    fn seed_domain_separation_does_not_collide_with_raw_id_hash() {
        // The derivation mixes in a domain prefix before hashing. The seed
        // must therefore differ from a plain SHA-256 of the device id bytes,
        // confirming that the prefix is actually participating in the digest.
        use sha2::{Digest, Sha256};
        let id = "DEVICE-A";
        let seed = DhtKey::from_device_id(id);
        let raw_hash: [u8; 32] = Sha256::digest(id.as_bytes()).into();
        assert_ne!(
            seed.as_bytes(),
            &raw_hash,
            "seed should differ from the raw SHA-256 of the id (domain prefix must participate)"
        );
    }
}
