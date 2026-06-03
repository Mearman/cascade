//! Signed capability tokens — portable, offline-issuable grants.
//!
//! The on-node [`Grant`] list is the management plane's
//! authority of record: a grant lives on the managed node, and a manager's
//! command is [authorised](crate::manage::authorises) against it. A
//! [`CapabilityToken`] is the *portable* form of the same authority. Instead of
//! a row on the node, the node's own key signs a statement — "device *bearer*
//! may exercise *capability* over *scope* until *expiry*" — that the bearer
//! carries and presents when it issues a command. The node verifies the
//! signature, the expiry, the revocation list, and that the bearer matches the
//! authenticated connection, then authorises the carried grant through the
//! *exact same* [`authorises`](crate::manage::authorises) path an on-node grant
//! takes. A token is a grant that travels.
//!
//! ## What signs a token
//!
//! A token is signed by the **issuing node's real device-identity private key** —
//! the secret behind its TLS certificate (see
//! [`cascade_p2p::identity::DeviceIdentity::sign_capability`]). The token carries
//! the issuer's certificate; a verifier re-derives the issuer device id from that
//! certificate (the id *is* the hash of the certificate), demands it equal the
//! issuer the token names, and only then checks the signature against the public
//! key inside the certificate. The signature is therefore a genuine proof that
//! the author held the issuer's private key, not merely that it knew the public
//! device id.
//!
//! ## Threat model
//!
//! Because the signature requires the issuer's private key, a peer that knows
//! only the public device id — which every trusted peer learns on the TLS
//! handshake — cannot forge a node-issued token. This is the property the device
//! identity exists to provide: possession of the private key is what
//! distinguishes the node from a peer that has merely seen its id. Conflating the
//! data-plane trust set (peers trusted to sync) with the management-plane
//! authority set (peers a node has granted authority) is exactly what this
//! closes: a trusted-but-ungranted peer can no longer mint itself a node-signed
//! grant.
//!
//! The bearer binding remains a second, independent factor. [`CapabilityToken::verify`]
//! requires the bearer field to equal the device id the transport authenticated
//! by mutual TLS, and the management plane only ever calls verify on a
//! TLS-verified session (relayed / post-hole-punch sessions, whose device id is
//! merely asserted, are refused before reaching the dispatcher). A token is bound
//! both to the node that signed it and to the device that may present it.
//!
//! ## Bounded delegation
//!
//! A holder of [`Capability::GrantAdmin`]
//! may mint a token that delegates a strict *subset* of what it itself holds —
//! the same no-escalation rule the on-node delegation path enforces with
//! [`caller_can_delegate`](crate::manage::dispatch). A delegated token carries
//! its parent token inline, forming a chain; [`CapabilityToken::verify`] walks
//! the chain to a root signed by the verifying node and checks containment at
//! every hop, so a chain can never widen authority. Each hop carries the
//! delegating party's certificate too, so every hop's signature is proven
//! against a private key, not a public id.

use chrono::{DateTime, Utc};
use data_encoding::BASE32_NOPAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use cascade_p2p::identity::{
    DEVICE_SIGNATURE_LENGTH, DeviceIdentity, DeviceKeyError, verify_capability_signature,
};

use crate::manage::{Capability, DeviceId, Grant, Scope};

/// Domain-separation tag prefixed to every signed token payload.
///
/// The device identity key signs for more than one purpose. Prefixing the signed
/// bytes with a fixed, purpose-specific, versioned tag ensures a signature
/// produced here can never be mistaken for — or replayed as — a signature over
/// some other structure, and a future change to the signed layout is a clean
/// break rather than a silent reinterpretation of old bytes.
const TOKEN_SIGNING_DOMAIN: &[u8] = b"cascade-manage-capability-token-v1";

/// The maximum depth of a delegation chain a verifier will walk.
///
/// Each delegation hop carries its parent token inline, so a chain is a linked
/// list the verifier follows to its root. The bound caps the work a presented
/// chain can force and rules out a self-referential or pathologically deep
/// chain crafted by a hostile bearer. A direct (node-issued) token is depth 1;
/// every delegation adds one. Eight hops is far beyond any administrative
/// delegation a human builds, while staying a hard, cheap ceiling.
pub const MAX_DELEGATION_DEPTH: usize = 8;

/// Domain-separation prefix mixed into a derived token id, keeping it distinct
/// from any other hash of the same fields.
const TOKEN_ID_DOMAIN: &[u8] = b"cascade-manage-token-id-v1";

/// Derive a collision-resistant token id from the claim fields and an issuance
/// instant.
///
/// The id is `BASE32(SHA-256(domain || issuer || bearer || capability || scope
/// || expiry || issued_at_nanos))`, truncated to a short, copy-pasteable
/// handle. Folding the nanosecond issuance instant in makes two otherwise
/// identical tokens distinct; the store's `token_id` primary key is the
/// backstop that turns any residual clash into a hard error rather than a silent
/// overwrite. The id is opaque — it carries no authority of its own; it is only
/// the handle a revocation names.
#[must_use]
pub fn derive_token_id(
    issuer: &DeviceId,
    bearer: &DeviceId,
    capability: Capability,
    scope: &Scope,
    expires: DateTime<Utc>,
    issued_at: DateTime<Utc>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(TOKEN_ID_DOMAIN);
    hasher.update(issuer.as_str().as_bytes());
    hasher.update(bearer.as_str().as_bytes());
    hasher.update(capability.as_wire().as_bytes());
    let (scope_kind, scope_path) = scope.to_columns();
    hasher.update(scope_kind.as_bytes());
    hasher.update(scope_path.unwrap_or("").as_bytes());
    hasher.update(expires.timestamp_millis().to_be_bytes());
    hasher.update(issued_at.timestamp_nanos_opt().unwrap_or(0).to_be_bytes());
    let digest = hasher.finalize();
    // A 16-byte (128-bit) prefix is ample for a handle: base32 of it is 26
    // characters, collision-resistant in practice, and the store's primary key
    // catches the vanishing remainder.
    let prefix = digest.get(..16).unwrap_or(&digest);
    BASE32_NOPAD.encode(prefix)
}

/// Why verifying a [`CapabilityToken`] failed.
///
/// Every variant is a hard rejection: the verifier discards the token and the
/// command it accompanies is refused. None is recoverable, and none is a panic —
/// a hostile bearer must never be able to crash the verifier.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TokenVerifyError {
    /// The token's signature did not verify against its signed bytes using the
    /// public key bound to the carried issuer certificate, or that certificate
    /// did not belong to the issuer the token names — the token was forged,
    /// tampered with, or signed without the issuer's private key.
    #[error("token {token_id}: signature verification failed: {source}")]
    BadSignature {
        /// The id of the token whose signature failed.
        token_id: String,
        /// The underlying device-key failure (mismatched issuer certificate, a
        /// malformed certificate, or a signature that did not verify).
        source: DeviceKeyError,
    },

    /// The root of the delegation chain was not signed by the node verifying it.
    /// A token only authorises against a node if it (or the root of its chain)
    /// was issued by that node's own key.
    #[error("token {token_id}: chain root issuer {root_issuer} is not this node {verifying_node}")]
    WrongIssuer {
        /// The id of the presented (leaf) token.
        token_id: String,
        /// The issuer at the root of the chain.
        root_issuer: String,
        /// The node performing verification.
        verifying_node: String,
    },

    /// The bearer named in the token is not the device id the connection
    /// authenticated. A token is bound to the device that presents it; a third
    /// party cannot replay another bearer's token.
    #[error("token {token_id}: bearer {bearer} does not match connected device {connected}")]
    BearerMismatch {
        /// The id of the token.
        token_id: String,
        /// The bearer the token names.
        bearer: String,
        /// The device id the connection authenticated.
        connected: String,
    },

    /// The token has expired relative to the verifier's clock.
    #[error("token {token_id}: expired at {expires} (now {now})")]
    Expired {
        /// The id of the expired token.
        token_id: String,
        /// The token's expiry.
        expires: DateTime<Utc>,
        /// The verifier's current wall clock.
        now: DateTime<Utc>,
    },

    /// The token (or one of its ancestors) is on the verifying node's revocation
    /// list.
    #[error("token {token_id}: revoked")]
    Revoked {
        /// The id of the revoked token (the leaf, or an ancestor whose
        /// revocation invalidates the chain).
        token_id: String,
    },

    /// A delegation hop widened authority: the child token claims a capability,
    /// scope, or expiry its parent does not contain. A chain can only narrow.
    #[error("token {token_id}: delegation exceeds parent authority")]
    DelegationExceedsParent {
        /// The id of the child token that over-reached.
        token_id: String,
    },

    /// The parent token a delegation links to could not itself be authenticated
    /// as a valid step toward the root (its own signature, expiry, or chain
    /// failed). Carried so the leaf error names the broken hop.
    #[error("token {token_id}: parent token invalid: {reason}")]
    ParentInvalid {
        /// The id of the child token whose parent failed.
        token_id: String,
        /// The underlying reason the parent failed to verify.
        reason: Box<Self>,
    },

    /// The delegation chain is deeper than [`MAX_DELEGATION_DEPTH`].
    #[error("token {token_id}: delegation chain exceeds maximum depth {max}")]
    ChainTooDeep {
        /// The id of the leaf token whose chain was too deep.
        token_id: String,
        /// The depth ceiling that was exceeded.
        max: usize,
    },
}

/// The claims a [`CapabilityToken`] asserts, independent of its signature.
///
/// Split from the signature so the canonical signing bytes are derived from the
/// claims alone, and so a token's content can be inspected (for listing or
/// audit) without re-checking the signature each time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenClaims {
    /// A stable, unique identifier for this token — the handle a revocation
    /// names. Chosen by the issuer; carried in the signed bytes so it cannot be
    /// relabelled.
    pub token_id: String,
    /// The device id of the node that issued (and signed) this token. The
    /// verifier derives the verifying key from this id.
    pub issuer: DeviceId,
    /// The device authorised to exercise this token — and the only device that
    /// may present it, checked against the authenticated connection.
    pub bearer: DeviceId,
    /// The capability the token confers.
    pub capability: Capability,
    /// The scope over which the capability applies.
    pub scope: Scope,
    /// When the token expires. A token always expires; there is no
    /// never-expiring token, because an offline-issued credential with no time
    /// bound is unrevocable in practice for any node that loses its revocation
    /// list.
    pub expires: DateTime<Utc>,
}

impl TokenClaims {
    /// The grant this token confers, as the management plane's
    /// [`Grant`] type, with `granted_by` set to the
    /// issuer. Once a token verifies, the dispatcher feeds this grant through
    /// the same [`authorises`](crate::manage::authorises) path an on-node grant
    /// takes — the token is just a portable grant.
    #[must_use]
    pub fn to_grant(&self) -> Grant {
        Grant {
            grantee: self.bearer.clone(),
            capability: self.capability,
            scope: self.scope.clone(),
            granted_by: self.issuer.clone(),
            expires: Some(self.expires),
        }
    }

    /// Whether `self` (a parent token's claims) contains `child` — the
    /// no-escalation predicate for one delegation hop.
    ///
    /// A child may delegate only authority the parent itself holds: the same
    /// capability, a scope the parent's scope [covers](Scope::covers), and an
    /// expiry no later than the parent's. This is the token-level mirror of the
    /// on-node [`caller_can_delegate`](crate::manage::dispatch) subset rule.
    #[must_use]
    fn contains(&self, child: &Self) -> bool {
        self.capability == child.capability
            && self.scope.covers(&child.scope)
            && child.expires <= self.expires
    }

    /// Build the canonical, length-prefixed byte encoding the signature covers.
    ///
    /// The signature is over these bytes, not any JSON form, so verification is
    /// independent of serialiser whitespace or field ordering. Every variable
    /// field is length-prefixed and every fixed field is written at fixed width,
    /// so no two distinct claim tuples can collide on the same bytes.
    fn signing_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(TOKEN_SIGNING_DOMAIN);
        push_field(&mut bytes, self.token_id.as_bytes());
        push_field(&mut bytes, self.issuer.as_str().as_bytes());
        push_field(&mut bytes, self.bearer.as_str().as_bytes());
        push_field(&mut bytes, self.capability.as_wire().as_bytes());
        let (scope_kind, scope_path) = self.scope.to_columns();
        push_field(&mut bytes, scope_kind.as_bytes());
        push_field(&mut bytes, scope_path.unwrap_or("").as_bytes());
        bytes.extend_from_slice(&self.expires.timestamp_millis().to_be_bytes());
        bytes
    }
}

/// A signed capability token: claims plus the issuer's signature over them, and
/// the parent token when the claims were delegated rather than node-issued.
///
/// A node-issued token has no `parent`; the chain root is the token itself,
/// signed by the issuing node. A delegated token carries its parent inline so a
/// verifier can walk to the root and check containment at every hop without any
/// out-of-band lookup — the chain is self-contained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityToken {
    /// The claims this token asserts.
    pub claims: TokenClaims,
    /// ECDSA P-256 signature (raw 64-byte fixed form) over the claims' canonical
    /// signing bytes, produced by the issuer's real device-identity private key.
    /// Carried as base64 on the wire because serde does not derive
    /// (de)serialisation for byte arrays this wide.
    #[serde(with = "base64_signature")]
    pub signature: [u8; DEVICE_SIGNATURE_LENGTH],
    /// The DER bytes of the issuer's certificate — the public half of the key
    /// that signed this token, carried so a verifier can both bind the
    /// certificate to the issuer device id (the id is the hash of these bytes)
    /// and check the signature against the public key inside it.
    #[serde(with = "base64_cert")]
    pub issuer_cert_der: Vec<u8>,
    /// The parent token this one was delegated from, when delegated. `None` for
    /// a node-issued token, which is itself a chain root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Box<Self>>,
}

impl CapabilityToken {
    /// Issue a node-signed token for `bearer`, signing with the issuing node's
    /// real device-identity private key.
    ///
    /// `issuer` is the node's full [`DeviceIdentity`]: its `device_id` becomes the
    /// token's issuer, its private key signs the claims, and its certificate is
    /// carried so a verifier can prove the signature against the public key the
    /// device id commits to. The resulting token is a chain root: it has no parent
    /// and verifies against the issuer's id directly.
    ///
    /// Fails only if the identity's key or certificate is malformed — a
    /// programming or on-disk-corruption error, never a routine outcome.
    pub fn issue(
        token_id: impl Into<String>,
        issuer: &DeviceIdentity,
        bearer: &DeviceId,
        capability: Capability,
        scope: Scope,
        expires: DateTime<Utc>,
    ) -> Result<Self, DeviceKeyError> {
        let claims = TokenClaims {
            token_id: token_id.into(),
            issuer: DeviceId::new(issuer.device_id.clone()),
            bearer: bearer.clone(),
            capability,
            scope,
            expires,
        };
        let signature = issuer.sign_capability(&claims.signing_bytes())?;
        Ok(Self {
            claims,
            signature,
            issuer_cert_der: issuer.cert_der()?,
            parent: None,
        })
    }

    /// Mint a delegated token from `self`, signed by the delegating party's own
    /// device-identity private key, conferring a *subset* of `self` onto
    /// `bearer`.
    ///
    /// The delegating party is the bearer of `self`: `delegator` must be that
    /// device's full [`DeviceIdentity`]. It signs the child with its own private
    /// key (so the child's `issuer` is the parent's bearer, proven by the
    /// delegator's certificate carried on the child) and carries `self` as the
    /// child's parent. Two guards refuse a bad mint up front rather than emit a
    /// token that would only fail verification later:
    /// - [`DelegateError::NotDelegator`] if `delegator` is not the bearer of
    ///   `self` — only the device a token authorises may delegate it; and
    /// - [`DelegateError::Exceeds`] if the requested capability, scope, or expiry
    ///   is not wholly contained in `self`'s claims.
    ///
    /// A malformed delegator key or certificate surfaces as
    /// [`DelegateError::Key`].
    pub fn delegate(
        &self,
        token_id: impl Into<String>,
        delegator: &DeviceIdentity,
        bearer: &DeviceId,
        capability: Capability,
        scope: Scope,
        expires: DateTime<Utc>,
    ) -> Result<Self, DelegateError> {
        if delegator.device_id != self.claims.bearer.as_str() {
            return Err(DelegateError::NotDelegator);
        }
        let child_claims = TokenClaims {
            token_id: token_id.into(),
            // The delegating bearer issues (and signs) the child.
            issuer: self.claims.bearer.clone(),
            bearer: bearer.clone(),
            capability,
            scope,
            expires,
        };
        // No-escalation: the child must be wholly contained in this token's
        // authority.
        if !self.claims.contains(&child_claims) {
            return Err(DelegateError::Exceeds);
        }
        let signature = delegator.sign_capability(&child_claims.signing_bytes())?;
        Ok(Self {
            claims: child_claims,
            signature,
            issuer_cert_der: delegator.cert_der()?,
            parent: Some(Box::new(self.clone())),
        })
    }

    /// Verify this token for presentation by `connected_device` on the node
    /// `verifying_node`, at `now`, against `is_revoked`.
    ///
    /// Every defence is a hard rejection, never a panic and never a silent
    /// acceptance:
    /// - the chain root must be signed by `verifying_node` (a token only
    ///   authorises against the node that issued its root);
    /// - every token in the chain must carry a signature that verifies under its
    ///   own `issuer`'s derived key;
    /// - every token in the chain must be unexpired at `now`;
    /// - no token in the chain may be on the revocation list (`is_revoked`);
    /// - each delegation hop must narrow, never widen, authority; and
    /// - the leaf token's `bearer` must equal `connected_device` — the device
    ///   the transport authenticated.
    ///
    /// On success the verified leaf claims are returned; the caller converts
    /// them to a [`Grant`] and authorises the command
    /// against it exactly as for an on-node grant.
    pub fn verify(
        &self,
        verifying_node: &DeviceId,
        connected_device: &DeviceId,
        now: DateTime<Utc>,
        is_revoked: &dyn Fn(&str) -> bool,
    ) -> Result<&TokenClaims, TokenVerifyError> {
        // The bearer binding is checked against the leaf only: the presenter
        // must be the device the leaf names. Ancestors bind their own bearers
        // (the delegating parties), not the presenter.
        if self.claims.bearer != *connected_device {
            return Err(TokenVerifyError::BearerMismatch {
                token_id: self.claims.token_id.clone(),
                bearer: self.claims.bearer.to_string(),
                connected: connected_device.to_string(),
            });
        }
        self.verify_chain(verifying_node, now, is_revoked, 1)?;
        Ok(&self.claims)
    }

    /// Walk the delegation chain from this token to its root, checking the
    /// signature, expiry, revocation, and (for a parent) the containment of this
    /// token within it. `depth` is the current chain depth, starting at 1 for
    /// the leaf.
    fn verify_chain(
        &self,
        verifying_node: &DeviceId,
        now: DateTime<Utc>,
        is_revoked: &dyn Fn(&str) -> bool,
        depth: usize,
    ) -> Result<(), TokenVerifyError> {
        if depth > MAX_DELEGATION_DEPTH {
            return Err(TokenVerifyError::ChainTooDeep {
                token_id: self.claims.token_id.clone(),
                max: MAX_DELEGATION_DEPTH,
            });
        }

        // Signature: every token must be validly signed by its claimed issuer.
        self.verify_signature()?;

        // Expiry: every token in the chain must be live. An expired ancestor
        // invalidates everything delegated from it.
        if now >= self.claims.expires {
            return Err(TokenVerifyError::Expired {
                token_id: self.claims.token_id.clone(),
                expires: self.claims.expires,
                now,
            });
        }

        // Revocation: a revoked token anywhere in the chain is fatal.
        if is_revoked(&self.claims.token_id) {
            return Err(TokenVerifyError::Revoked {
                token_id: self.claims.token_id.clone(),
            });
        }

        match &self.parent {
            None => {
                // Chain root: it must have been issued by the verifying node.
                if self.claims.issuer != *verifying_node {
                    return Err(TokenVerifyError::WrongIssuer {
                        token_id: self.claims.token_id.clone(),
                        root_issuer: self.claims.issuer.to_string(),
                        verifying_node: verifying_node.to_string(),
                    });
                }
                Ok(())
            }
            Some(parent) => {
                // The parent must itself verify as a step toward the root.
                parent
                    .verify_chain(verifying_node, now, is_revoked, depth + 1)
                    .map_err(|reason| TokenVerifyError::ParentInvalid {
                        token_id: self.claims.token_id.clone(),
                        reason: Box::new(reason),
                    })?;
                // The delegating party (this token's issuer) must be the bearer
                // of the parent — a token can only be delegated by the device it
                // authorises.
                if self.claims.issuer != parent.claims.bearer {
                    return Err(TokenVerifyError::DelegationExceedsParent {
                        token_id: self.claims.token_id.clone(),
                    });
                }
                // No-escalation: this token must be contained in its parent.
                if parent.claims.contains(&self.claims) {
                    Ok(())
                } else {
                    Err(TokenVerifyError::DelegationExceedsParent {
                        token_id: self.claims.token_id.clone(),
                    })
                }
            }
        }
    }

    /// Verify this single token's signature against the public key bound to its
    /// carried issuer certificate, after binding that certificate to the issuer
    /// the token names.
    ///
    /// The binding (the issuer device id must equal the hash of the carried
    /// certificate) is what makes carrying the certificate safe: a forger cannot
    /// substitute its own certificate (and its own valid signature) under a
    /// victim's issuer id, because the substituted certificate hashes to the
    /// forger's id, not the victim's.
    fn verify_signature(&self) -> Result<(), TokenVerifyError> {
        verify_capability_signature(
            &self.issuer_cert_der,
            self.claims.issuer.as_str(),
            &self.claims.signing_bytes(),
            &self.signature,
        )
        .map_err(|source| TokenVerifyError::BadSignature {
            token_id: self.claims.token_id.clone(),
            source,
        })
    }
}

/// Why minting a delegated token failed.
///
/// Each variant is a refusal at mint time, so a widening or improperly-signed
/// delegation is never emitted to fail verification later.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DelegateError {
    /// The delegating identity is not the bearer of the parent token. Only the
    /// device a token authorises may delegate from it.
    #[error("delegator is not the bearer of the parent token")]
    NotDelegator,

    /// The requested grant is not wholly contained in the parent token's
    /// authority — a delegation may only narrow, never widen.
    #[error("delegated grant exceeds the parent token's authority")]
    Exceeds,

    /// The delegating identity's key or certificate is malformed.
    #[error("delegator device key error: {0}")]
    Key(#[from] DeviceKeyError),
}

/// Append a length-prefixed variable-width field to the signing bytes.
///
/// The length is a `u64` big-endian prefix. Token ids, device ids, and
/// capability/scope strings are bounded well below `u64::MAX`, so the
/// `usize`→`u64` widening is total on every supported platform.
fn push_field(bytes: &mut Vec<u8>, field: &[u8]) {
    let len = u64::try_from(field.len()).unwrap_or(u64::MAX);
    bytes.extend_from_slice(&len.to_be_bytes());
    bytes.extend_from_slice(field);
}

/// Base64 (de)serialisation for the fixed-width signature byte array.
///
/// serde derives array (de)serialisation only up to 32 bytes by value, and JSON
/// arrays-of-numbers are wasteful and fragile; encoding as base64 gives a
/// compact, stable string field. The decode rejects a wrong-length blob loudly,
/// which the signature check then catches.
mod base64_signature {
    use cascade_p2p::identity::DEVICE_SIGNATURE_LENGTH;
    use data_encoding::BASE64;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        bytes: &[u8; DEVICE_SIGNATURE_LENGTH],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&BASE64.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<[u8; DEVICE_SIGNATURE_LENGTH], D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let decoded = BASE64
            .decode(encoded.as_bytes())
            .map_err(serde::de::Error::custom)?;
        <[u8; DEVICE_SIGNATURE_LENGTH]>::try_from(decoded.as_slice()).map_err(|_| {
            serde::de::Error::invalid_length(decoded.len(), &"64-byte ECDSA P-256 signature")
        })
    }
}

/// Base64 (de)serialisation for the issuer certificate DER bytes.
///
/// The certificate is binary DER; base64 gives a compact, stable string field on
/// the wire, the same shape the signature uses.
mod base64_cert {
    use data_encoding::BASE64;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&BASE64.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        BASE64
            .decode(encoded.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
            .single()
            .expect("valid date")
    }

    /// A freshly generated device identity. Its `device_id` is the hash of its
    /// own certificate, so signing with it produces a signature that verifies
    /// against the certificate the token carries — exactly the real-key property
    /// under test.
    fn identity() -> DeviceIdentity {
        DeviceIdentity::generate().expect("generate a device identity")
    }

    fn id_of(identity: &DeviceIdentity) -> DeviceId {
        DeviceId::new(identity.device_id.clone())
    }

    fn never_revoked(_id: &str) -> bool {
        false
    }

    /// Issue a `pin:write` over `/work` token from `node` to `bearer`.
    fn issued_by(node: &DeviceIdentity, bearer: &DeviceId) -> CapabilityToken {
        CapabilityToken::issue(
            "tok-1",
            node,
            bearer,
            Capability::PinWrite,
            Scope::folder("/work"),
            at(2026, 12, 31),
        )
        .expect("issue a token with a fresh identity")
    }

    // ── Issue / verify round-trip ──

    #[test]
    fn issue_then_verify_round_trips() {
        let node = identity();
        let bearer = identity();
        let token = issued_by(&node, &id_of(&bearer));
        let claims = token
            .verify(
                &id_of(&node),
                &id_of(&bearer),
                at(2026, 1, 1),
                &never_revoked,
            )
            .expect("a freshly issued token must verify");
        assert_eq!(claims.capability, Capability::PinWrite);
        assert_eq!(claims.scope, Scope::folder("/work"));
        assert_eq!(claims.bearer, id_of(&bearer));
    }

    #[test]
    fn verified_token_projects_to_a_grant() {
        let node = identity();
        let bearer = identity();
        let token = issued_by(&node, &id_of(&bearer));
        let grant = token.claims.to_grant();
        assert_eq!(grant.grantee, id_of(&bearer));
        assert_eq!(grant.granted_by, id_of(&node));
        assert_eq!(grant.capability, Capability::PinWrite);
        assert_eq!(grant.scope, Scope::folder("/work"));
        assert_eq!(grant.expires, Some(at(2026, 12, 31)));
    }

    #[test]
    fn json_round_trip_preserves_a_verifiable_token() {
        let node = identity();
        let bearer = identity();
        let token = issued_by(&node, &id_of(&bearer));
        let json = serde_json::to_string(&token).expect("serialise");
        let decoded: CapabilityToken = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(decoded, token);
        assert!(
            decoded
                .verify(
                    &id_of(&node),
                    &id_of(&bearer),
                    at(2026, 1, 1),
                    &never_revoked
                )
                .is_ok()
        );
    }

    // ── Hard rejections ──

    #[test]
    fn token_signed_by_a_different_node_is_rejected() {
        // A token whose root issuer is not the verifying node must be rejected:
        // it was signed by another node's key and carries that node's cert.
        let node = identity();
        let other = identity();
        let bearer = identity();
        let token = issued_by(&other, &id_of(&bearer));
        let err = token
            .verify(
                &id_of(&node),
                &id_of(&bearer),
                at(2026, 1, 1),
                &never_revoked,
            )
            .expect_err("a token from another issuer must be rejected");
        assert!(matches!(err, TokenVerifyError::WrongIssuer { .. }));
    }

    #[test]
    fn a_token_carrying_a_substituted_certificate_is_rejected() {
        // An attacker keeps the victim node's issuer id in the claims but swaps in
        // its own certificate and re-signs with its own key. The cert no longer
        // hashes to the claimed issuer, so the binding check rejects it before the
        // signature is even trusted.
        let node = identity();
        let attacker = identity();
        let bearer = identity();
        let mut token = issued_by(&node, &id_of(&bearer));
        token.issuer_cert_der = attacker.cert_der().expect("attacker cert");
        token.signature = attacker
            .sign_capability(&token.claims.signing_bytes())
            .expect("attacker signs");
        let err = token
            .verify(
                &id_of(&node),
                &id_of(&bearer),
                at(2026, 1, 1),
                &never_revoked,
            )
            .expect_err("a substituted certificate must be rejected");
        assert!(matches!(err, TokenVerifyError::BadSignature { .. }));
    }

    #[test]
    fn tampered_claims_break_the_signature() {
        let node = identity();
        let bearer = identity();
        let mut token = issued_by(&node, &id_of(&bearer));
        // Widen the scope after signing: the signature no longer matches.
        token.claims.scope = Scope::Node;
        let err = token
            .verify(
                &id_of(&node),
                &id_of(&bearer),
                at(2026, 1, 1),
                &never_revoked,
            )
            .expect_err("tampered claims must fail the signature check");
        assert!(matches!(err, TokenVerifyError::BadSignature { .. }));
    }

    #[test]
    fn tampered_signature_byte_is_rejected() {
        let node = identity();
        let bearer = identity();
        let mut token = issued_by(&node, &id_of(&bearer));
        token.signature[0] ^= 0x01;
        let err = token
            .verify(
                &id_of(&node),
                &id_of(&bearer),
                at(2026, 1, 1),
                &never_revoked,
            )
            .expect_err("a flipped signature byte must be rejected");
        assert!(matches!(err, TokenVerifyError::BadSignature { .. }));
    }

    #[test]
    fn expired_token_is_rejected() {
        let node = identity();
        let bearer = identity();
        let token = issued_by(&node, &id_of(&bearer));
        // now == expiry is inclusive-past, and after expiry is stale.
        let err = token
            .verify(
                &id_of(&node),
                &id_of(&bearer),
                at(2026, 12, 31),
                &never_revoked,
            )
            .expect_err("an expired token must be rejected");
        assert!(matches!(err, TokenVerifyError::Expired { .. }));
        assert!(matches!(
            token.verify(
                &id_of(&node),
                &id_of(&bearer),
                at(2027, 1, 1),
                &never_revoked
            ),
            Err(TokenVerifyError::Expired { .. })
        ));
    }

    #[test]
    fn revoked_token_is_rejected() {
        let node = identity();
        let bearer = identity();
        let token = issued_by(&node, &id_of(&bearer));
        let revoked = |id: &str| id == "tok-1";
        let err = token
            .verify(&id_of(&node), &id_of(&bearer), at(2026, 1, 1), &revoked)
            .expect_err("a revoked token must be rejected");
        assert!(matches!(err, TokenVerifyError::Revoked { .. }));
    }

    #[test]
    fn bearer_mismatch_is_rejected() {
        let node = identity();
        let bearer = identity();
        let stranger = DeviceId::new("STRANGER-DEVICE");
        let token = issued_by(&node, &id_of(&bearer));
        let err = token
            .verify(&id_of(&node), &stranger, at(2026, 1, 1), &never_revoked)
            .expect_err("a token presented by the wrong bearer must be rejected");
        assert!(matches!(err, TokenVerifyError::BearerMismatch { .. }));
    }

    // ── Bounded delegation ──

    #[test]
    fn delegate_subset_round_trips_and_verifies() {
        // Bearer holds pin:write over /work and delegates a narrower /work/sub
        // to a sub-bearer with an earlier expiry. The chain must verify against
        // the original issuing node.
        let node = identity();
        let bearer = identity();
        let sub = identity();
        let token = issued_by(&node, &id_of(&bearer));
        let delegated = token
            .delegate(
                "tok-1-a",
                &bearer,
                &id_of(&sub),
                Capability::PinWrite,
                Scope::folder("/work/sub"),
                at(2026, 6, 1),
            )
            .expect("a subset delegation must mint");
        let claims = delegated
            .verify(&id_of(&node), &id_of(&sub), at(2026, 1, 1), &never_revoked)
            .expect("a valid delegated chain must verify");
        assert_eq!(claims.scope, Scope::folder("/work/sub"));
        assert_eq!(claims.bearer, id_of(&sub));
    }

    #[test]
    fn delegate_by_a_non_bearer_is_refused() {
        // Only the bearer of the parent token may delegate from it; a different
        // identity attempting to mint a child is refused.
        let node = identity();
        let bearer = identity();
        let imposter = identity();
        let sub = identity();
        let token = issued_by(&node, &id_of(&bearer));
        assert_eq!(
            token.delegate(
                "tok-1-x",
                &imposter,
                &id_of(&sub),
                Capability::PinWrite,
                Scope::folder("/work/sub"),
                at(2026, 6, 1),
            ),
            Err(DelegateError::NotDelegator)
        );
    }

    #[test]
    fn delegate_widening_scope_is_refused_at_mint() {
        let node = identity();
        let bearer = identity();
        let sub = identity();
        let token = issued_by(&node, &id_of(&bearer));
        // /work cannot delegate /personal — a sibling outside the subtree.
        assert_eq!(
            token.delegate(
                "tok-1-b",
                &bearer,
                &id_of(&sub),
                Capability::PinWrite,
                Scope::folder("/personal"),
                at(2026, 6, 1),
            ),
            Err(DelegateError::Exceeds)
        );
        // /work cannot delegate the node-wide scope.
        assert_eq!(
            token.delegate(
                "tok-1-c",
                &bearer,
                &id_of(&sub),
                Capability::PinWrite,
                Scope::Node,
                at(2026, 6, 1),
            ),
            Err(DelegateError::Exceeds)
        );
    }

    #[test]
    fn delegate_different_capability_is_refused_at_mint() {
        let node = identity();
        let bearer = identity();
        let sub = identity();
        let token = issued_by(&node, &id_of(&bearer));
        assert_eq!(
            token.delegate(
                "tok-1-d",
                &bearer,
                &id_of(&sub),
                Capability::CacheManage,
                Scope::folder("/work"),
                at(2026, 6, 1),
            ),
            Err(DelegateError::Exceeds)
        );
    }

    #[test]
    fn delegate_expiry_beyond_parent_is_refused_at_mint() {
        let node = identity();
        let bearer = identity();
        let sub = identity();
        let token = issued_by(&node, &id_of(&bearer));
        // Parent expires 2026-12-31; a child expiring later over-reaches.
        assert_eq!(
            token.delegate(
                "tok-1-e",
                &bearer,
                &id_of(&sub),
                Capability::PinWrite,
                Scope::folder("/work"),
                at(2027, 1, 1),
            ),
            Err(DelegateError::Exceeds)
        );
    }

    #[test]
    fn forged_widening_delegation_is_rejected_at_verify() {
        // A hostile bearer mints a legitimate subset child, then rewrites the
        // child's claims to widen scope and re-signs with its own (the child
        // issuer's) key. The leaf signature itself is valid, but the containment
        // check at verify time must reject the chain.
        let node = identity();
        let bearer = identity();
        let sub = identity();
        let token = issued_by(&node, &id_of(&bearer));
        let mut forged = token
            .delegate(
                "tok-1-f",
                &bearer,
                &id_of(&sub),
                Capability::PinWrite,
                Scope::folder("/work/sub"),
                at(2026, 6, 1),
            )
            .expect("subset child mints");
        // Widen the child's scope past the parent and re-sign with the child's
        // own issuer key (the delegating bearer) so the leaf signature itself is
        // valid. The containment check, not the signature, must catch it.
        forged.claims.scope = Scope::Node;
        forged.signature = bearer
            .sign_capability(&forged.claims.signing_bytes())
            .expect("the delegating bearer re-signs the forged claims");
        let err = forged
            .verify(&id_of(&node), &id_of(&sub), at(2026, 1, 1), &never_revoked)
            .expect_err("a widened delegation must be rejected at verify");
        assert!(matches!(
            err,
            TokenVerifyError::DelegationExceedsParent { .. }
        ));
    }

    #[test]
    fn revoked_ancestor_invalidates_the_chain() {
        let node = identity();
        let bearer = identity();
        let sub = identity();
        let token = issued_by(&node, &id_of(&bearer));
        let delegated = token
            .delegate(
                "tok-1-g",
                &bearer,
                &id_of(&sub),
                Capability::PinWrite,
                Scope::folder("/work/sub"),
                at(2026, 6, 1),
            )
            .expect("subset child mints");
        // Revoking the root token must invalidate the delegated leaf.
        let revoked = |id: &str| id == "tok-1";
        let err = delegated
            .verify(&id_of(&node), &id_of(&sub), at(2026, 1, 1), &revoked)
            .expect_err("a revoked ancestor must invalidate the chain");
        assert!(matches!(err, TokenVerifyError::ParentInvalid { .. }));
    }
}
