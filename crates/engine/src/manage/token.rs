//! Signed capability tokens — portable, offline-issuable grants.
//!
//! The on-node [`Grant`](crate::manage::Grant) list is the management plane's
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
//! A token is signed by the **issuing node's device-identity key** — the same
//! ed25519 keypair the DHT and announce paths derive from the device id (see
//! [`cascade_p2p::discovery::signing::keypair_for_device`]). There is no second
//! key and no new crypto: the node that issues a token signs it with the one
//! key its identity already implies, and any verifier re-derives the issuer's
//! verifying key from the issuer device id carried in the token.
//!
//! ## Threat model — inherited, deliberately
//!
//! The signing key is *derived from the device id*, which is public (it is a
//! hash of the device's TLS certificate, exchanged on every handshake). So the
//! signature proves "the author knew the issuer's device id", not "the author
//! holds the issuer's TLS private key" — the same limitation
//! [`cascade_announce_wire::signing`] documents for signed candidate sets. A
//! token is therefore **not** a bearer credential good on its own: presenting a
//! validly-signed token is necessary but not sufficient. The load-bearing
//! second factor is the connection. [`CapabilityToken::verify`] requires the
//! bearer field to equal the device id the transport authenticated by mutual
//! TLS, and the management plane only ever calls verify on a TLS-verified
//! session (relayed / post-hole-punch sessions, whose device id is merely
//! asserted, are refused before reaching the dispatcher). An attacker who knows
//! the issuer device id can forge a token, but cannot present it as a bearer it
//! does not control the TLS identity of.
//!
//! ## Bounded delegation
//!
//! A holder of [`Capability::GrantAdmin`](crate::manage::Capability::GrantAdmin)
//! may mint a token that delegates a strict *subset* of what it itself holds —
//! the same no-escalation rule the on-node delegation path enforces with
//! [`caller_can_delegate`](crate::manage::dispatch). A delegated token carries
//! its parent token inline, forming a chain; [`CapabilityToken::verify`] walks
//! the chain to a root signed by the verifying node and checks containment at
//! every hop, so a chain can never widen authority.

use chrono::{DateTime, Utc};
use ed25519_dalek::{SIGNATURE_LENGTH, Signature, Signer, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use cascade_p2p::discovery::signing::{keypair_for_device, verifying_key_for_device};

use crate::manage::{Capability, DeviceId, Grant, Scope};

/// Domain-separation tag prefixed to every signed token payload.
///
/// The device key is derived for, and used by, several unrelated purposes
/// (announce candidate sets, DHT BEP44 items). Prefixing the signed bytes with a
/// fixed, purpose-specific, versioned tag ensures a signature produced here can
/// never be mistaken for — or replayed as — a signature over some other
/// structure, and a future change to the signed layout is a clean break rather
/// than a silent reinterpretation of old bytes.
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

/// Why verifying a [`CapabilityToken`] failed.
///
/// Every variant is a hard rejection: the verifier discards the token and the
/// command it accompanies is refused. None is recoverable, and none is a panic —
/// a hostile bearer must never be able to crash the verifier.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TokenVerifyError {
    /// The token's signature did not verify against its signed bytes using the
    /// key derived from the token's `issuer` device id — the token was forged,
    /// tampered with, or signed by a different device than it claims.
    #[error("token {token_id}: signature verification failed")]
    BadSignature {
        /// The id of the token whose signature failed.
        token_id: String,
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
    /// [`Grant`](crate::manage::Grant) type, with `granted_by` set to the
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
    /// ed25519 signature (raw 64 bytes) over [`TokenClaims::signing_bytes`],
    /// produced by the issuer's device key. Carried as base64 on the wire
    /// because serde does not derive (de)serialisation for byte arrays this
    /// wide.
    #[serde(with = "base64_signature")]
    pub signature: [u8; SIGNATURE_LENGTH],
    /// The parent token this one was delegated from, when delegated. `None` for
    /// a node-issued token, which is itself a chain root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Box<Self>>,
}

impl CapabilityToken {
    /// Issue a node-signed token for `bearer`, signing with the issuing node's
    /// device key.
    ///
    /// `issuer` must be the device id whose key is used to sign; the caller
    /// holds the node identity and supplies its own id. The resulting token is a
    /// chain root: it has no parent and verifies against `issuer` directly.
    #[must_use]
    pub fn issue(
        token_id: impl Into<String>,
        issuer: &DeviceId,
        bearer: &DeviceId,
        capability: Capability,
        scope: Scope,
        expires: DateTime<Utc>,
    ) -> Self {
        let claims = TokenClaims {
            token_id: token_id.into(),
            issuer: issuer.clone(),
            bearer: bearer.clone(),
            capability,
            scope,
            expires,
        };
        let signature = sign_claims(&claims);
        Self {
            claims,
            signature,
            parent: None,
        }
    }

    /// Mint a delegated token from `self`, signed by the delegating bearer's own
    /// device key, conferring a *subset* of `self` onto `bearer`.
    ///
    /// The delegating party is the bearer of `self`: it signs the child with its
    /// own device key (so the child's `issuer` is the parent's bearer) and
    /// carries `self` as the child's parent. The subset rule is checked here so a
    /// minting party cannot escalate: the requested capability, scope, and
    /// expiry must be contained in `self`'s claims. Returns `None` when the
    /// requested grant exceeds what `self` confers.
    #[must_use]
    pub fn delegate(
        &self,
        token_id: impl Into<String>,
        bearer: &DeviceId,
        capability: Capability,
        scope: Scope,
        expires: DateTime<Utc>,
    ) -> Option<Self> {
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
        // authority. Refuse to mint a widening delegation rather than emit a
        // token that would fail verification later.
        if !self.claims.contains(&child_claims) {
            return None;
        }
        let signature = sign_claims(&child_claims);
        Some(Self {
            claims: child_claims,
            signature,
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
    /// them to a [`Grant`](crate::manage::Grant) and authorises the command
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

    /// Verify this single token's signature against its issuer's derived key.
    fn verify_signature(&self) -> Result<(), TokenVerifyError> {
        let verifying_key: VerifyingKey = verifying_key_for_device(self.claims.issuer.as_str());
        let message = self.claims.signing_bytes();
        let signature = Signature::from_bytes(&self.signature);
        verifying_key
            .verify_strict(&message, &signature)
            .map_err(|_| TokenVerifyError::BadSignature {
                token_id: self.claims.token_id.clone(),
            })
    }
}

/// Sign a set of claims with the issuer's device key.
fn sign_claims(claims: &TokenClaims) -> [u8; SIGNATURE_LENGTH] {
    let key = keypair_for_device(claims.issuer.as_str());
    key.sign(&claims.signing_bytes()).to_bytes()
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
    use data_encoding::BASE64;
    use ed25519_dalek::SIGNATURE_LENGTH;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        bytes: &[u8; SIGNATURE_LENGTH],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&BASE64.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<[u8; SIGNATURE_LENGTH], D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let decoded = BASE64
            .decode(encoded.as_bytes())
            .map_err(serde::de::Error::custom)?;
        <[u8; SIGNATURE_LENGTH]>::try_from(decoded.as_slice()).map_err(|_| {
            serde::de::Error::invalid_length(decoded.len(), &"64-byte ed25519 signature")
        })
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

    fn node() -> DeviceId {
        DeviceId::new("NODE-ISSUER")
    }

    fn bearer() -> DeviceId {
        DeviceId::new("BEARER-DEVICE")
    }

    fn never_revoked(_id: &str) -> bool {
        false
    }

    fn issued() -> CapabilityToken {
        CapabilityToken::issue(
            "tok-1",
            &node(),
            &bearer(),
            Capability::PinWrite,
            Scope::folder("/work"),
            at(2026, 12, 31),
        )
    }

    // ── Issue / verify round-trip ──

    #[test]
    fn issue_then_verify_round_trips() {
        let token = issued();
        let claims = token
            .verify(&node(), &bearer(), at(2026, 1, 1), &never_revoked)
            .expect("a freshly issued token must verify");
        assert_eq!(claims.capability, Capability::PinWrite);
        assert_eq!(claims.scope, Scope::folder("/work"));
        assert_eq!(claims.bearer, bearer());
    }

    #[test]
    fn verified_token_projects_to_a_grant() {
        let token = issued();
        let grant = token.claims.to_grant();
        assert_eq!(grant.grantee, bearer());
        assert_eq!(grant.granted_by, node());
        assert_eq!(grant.capability, Capability::PinWrite);
        assert_eq!(grant.scope, Scope::folder("/work"));
        assert_eq!(grant.expires, Some(at(2026, 12, 31)));
    }

    #[test]
    fn json_round_trip_preserves_a_verifiable_token() {
        let token = issued();
        let json = serde_json::to_string(&token).expect("serialise");
        let decoded: CapabilityToken = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(decoded, token);
        assert!(
            decoded
                .verify(&node(), &bearer(), at(2026, 1, 1), &never_revoked)
                .is_ok()
        );
    }

    // ── Hard rejections ──

    #[test]
    fn token_signed_by_a_different_node_is_rejected() {
        // A token whose root issuer is not the verifying node must be rejected:
        // it was signed by another node's key.
        let other = DeviceId::new("OTHER-NODE");
        let token = CapabilityToken::issue(
            "tok-2",
            &other,
            &bearer(),
            Capability::PinWrite,
            Scope::folder("/work"),
            at(2026, 12, 31),
        );
        let err = token
            .verify(&node(), &bearer(), at(2026, 1, 1), &never_revoked)
            .expect_err("a token from another issuer must be rejected");
        assert!(matches!(err, TokenVerifyError::WrongIssuer { .. }));
    }

    #[test]
    fn tampered_claims_break_the_signature() {
        let mut token = issued();
        // Widen the scope after signing: the signature no longer matches.
        token.claims.scope = Scope::Node;
        let err = token
            .verify(&node(), &bearer(), at(2026, 1, 1), &never_revoked)
            .expect_err("tampered claims must fail the signature check");
        assert!(matches!(err, TokenVerifyError::BadSignature { .. }));
    }

    #[test]
    fn tampered_signature_byte_is_rejected() {
        let mut token = issued();
        token.signature[0] ^= 0x01;
        let err = token
            .verify(&node(), &bearer(), at(2026, 1, 1), &never_revoked)
            .expect_err("a flipped signature byte must be rejected");
        assert!(matches!(err, TokenVerifyError::BadSignature { .. }));
    }

    #[test]
    fn expired_token_is_rejected() {
        let token = issued();
        // now == expiry is inclusive-past, and after expiry is stale.
        let err = token
            .verify(&node(), &bearer(), at(2026, 12, 31), &never_revoked)
            .expect_err("an expired token must be rejected");
        assert!(matches!(err, TokenVerifyError::Expired { .. }));
        assert!(matches!(
            token.verify(&node(), &bearer(), at(2027, 1, 1), &never_revoked),
            Err(TokenVerifyError::Expired { .. })
        ));
    }

    #[test]
    fn revoked_token_is_rejected() {
        let token = issued();
        let revoked = |id: &str| id == "tok-1";
        let err = token
            .verify(&node(), &bearer(), at(2026, 1, 1), &revoked)
            .expect_err("a revoked token must be rejected");
        assert!(matches!(err, TokenVerifyError::Revoked { .. }));
    }

    #[test]
    fn bearer_mismatch_is_rejected() {
        let token = issued();
        let stranger = DeviceId::new("STRANGER-DEVICE");
        let err = token
            .verify(&node(), &stranger, at(2026, 1, 1), &never_revoked)
            .expect_err("a token presented by the wrong bearer must be rejected");
        assert!(matches!(err, TokenVerifyError::BearerMismatch { .. }));
    }

    // ── Bounded delegation ──

    #[test]
    fn delegate_subset_round_trips_and_verifies() {
        // Bearer holds pin:write over /work and delegates a narrower /work/sub
        // to a sub-bearer with an earlier expiry. The chain must verify against
        // the original issuing node.
        let token = issued();
        let sub = DeviceId::new("SUB-BEARER");
        let delegated = token
            .delegate(
                "tok-1-a",
                &sub,
                Capability::PinWrite,
                Scope::folder("/work/sub"),
                at(2026, 6, 1),
            )
            .expect("a subset delegation must mint");
        let claims = delegated
            .verify(&node(), &sub, at(2026, 1, 1), &never_revoked)
            .expect("a valid delegated chain must verify");
        assert_eq!(claims.scope, Scope::folder("/work/sub"));
        assert_eq!(claims.bearer, sub);
    }

    #[test]
    fn delegate_widening_scope_is_refused_at_mint() {
        let token = issued();
        let sub = DeviceId::new("SUB-BEARER");
        // /work cannot delegate /personal — a sibling outside the subtree.
        assert!(
            token
                .delegate(
                    "tok-1-b",
                    &sub,
                    Capability::PinWrite,
                    Scope::folder("/personal"),
                    at(2026, 6, 1),
                )
                .is_none()
        );
        // /work cannot delegate the node-wide scope.
        assert!(
            token
                .delegate(
                    "tok-1-c",
                    &sub,
                    Capability::PinWrite,
                    Scope::Node,
                    at(2026, 6, 1),
                )
                .is_none()
        );
    }

    #[test]
    fn delegate_different_capability_is_refused_at_mint() {
        let token = issued();
        let sub = DeviceId::new("SUB-BEARER");
        assert!(
            token
                .delegate(
                    "tok-1-d",
                    &sub,
                    Capability::CacheManage,
                    Scope::folder("/work"),
                    at(2026, 6, 1),
                )
                .is_none()
        );
    }

    #[test]
    fn delegate_expiry_beyond_parent_is_refused_at_mint() {
        let token = issued();
        let sub = DeviceId::new("SUB-BEARER");
        // Parent expires 2026-12-31; a child expiring later over-reaches.
        assert!(
            token
                .delegate(
                    "tok-1-e",
                    &sub,
                    Capability::PinWrite,
                    Scope::folder("/work"),
                    at(2027, 1, 1),
                )
                .is_none()
        );
    }

    #[test]
    fn forged_widening_delegation_is_rejected_at_verify() {
        // A hostile bearer mints a legitimate subset child, then rewrites the
        // child's claims to widen scope and re-signs with its own (the child
        // issuer's) key. The signature is valid, but the containment check at
        // verify time must reject the chain.
        let token = issued();
        let sub = DeviceId::new("SUB-BEARER");
        let mut forged = token
            .delegate(
                "tok-1-f",
                &sub,
                Capability::PinWrite,
                Scope::folder("/work/sub"),
                at(2026, 6, 1),
            )
            .expect("subset child mints");
        // Widen the child's scope past the parent and re-sign with the child's
        // own issuer key so the leaf signature itself is valid.
        forged.claims.scope = Scope::Node;
        forged.signature = sign_claims(&forged.claims);
        let err = forged
            .verify(&node(), &sub, at(2026, 1, 1), &never_revoked)
            .expect_err("a widened delegation must be rejected at verify");
        assert!(matches!(
            err,
            TokenVerifyError::DelegationExceedsParent { .. }
        ));
    }

    #[test]
    fn revoked_ancestor_invalidates_the_chain() {
        let token = issued();
        let sub = DeviceId::new("SUB-BEARER");
        let delegated = token
            .delegate(
                "tok-1-g",
                &sub,
                Capability::PinWrite,
                Scope::folder("/work/sub"),
                at(2026, 6, 1),
            )
            .expect("subset child mints");
        // Revoking the root token must invalidate the delegated leaf.
        let revoked = |id: &str| id == "tok-1";
        let err = delegated
            .verify(&node(), &sub, at(2026, 1, 1), &revoked)
            .expect_err("a revoked ancestor must invalidate the chain");
        assert!(matches!(err, TokenVerifyError::ParentInvalid { .. }));
    }
}
