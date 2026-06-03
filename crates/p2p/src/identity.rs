//! Device identity — TLS certificate generation and device ID derivation.
//!
//! Each device generates a self-signed TLS certificate on first run. The
//! device ID is the base32-encoded SHA-256 of the certificate DER bytes,
//! following the same convention as Syncthing.

use std::path::Path;

use anyhow::{Context, Result};
use data_encoding::{BASE32_NOPAD, BASE64};
use rcgen::KeyPair;
use ring::rand::SystemRandom;
use ring::signature::{
    ECDSA_P256_SHA256_FIXED, ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, UnparsedPublicKey,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use x509_parser::prelude::FromDer;

/// The byte length of a fixed-form ECDSA P-256 signature (`r ‖ s`, each a
/// 32-byte scalar). The device identity key is ECDSA P-256, so every capability
/// signature it produces is exactly this wide.
pub const DEVICE_SIGNATURE_LENGTH: usize = 64;

/// File name for the stored certificate.
pub const CERT_FILE: &str = "device.crt";

/// File name for the stored private key.
pub const KEY_FILE: &str = "device.key";

/// Device identity: a self-signed TLS certificate and its derived device ID.
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    /// Base32-encoded SHA-256 of the certificate DER.
    pub device_id: String,
    /// PEM-encoded certificate.
    pub cert_pem: String,
    /// PEM-encoded private key.
    pub key_pem: String,
}

impl DeviceIdentity {
    /// Generate a new device identity with a self-signed certificate.
    pub fn generate() -> Result<Self> {
        let key_pair = KeyPair::generate().context("generating TLS key pair")?;

        let cert = rcgen::CertificateParams::new(vec!["cascade-device".to_string()])
            .map_err(|e| anyhow::anyhow!("creating cert params: {e}"))?
            .self_signed(&key_pair)
            .map_err(|e| anyhow::anyhow!("signing certificate: {e}"))?;

        let cert_der = cert.der();
        let device_id = derive_device_id(cert_der.as_ref());

        Ok(Self {
            device_id,
            cert_pem: cert.pem(),
            key_pem: key_pair.serialize_pem(),
        })
    }

    /// Load identity from disk (certificate and key PEM files).
    pub fn load(dir: &Path) -> Result<Self> {
        let cert_pem =
            std::fs::read_to_string(dir.join(CERT_FILE)).context("reading device certificate")?;
        let key_pem = std::fs::read_to_string(dir.join(KEY_FILE)).context("reading device key")?;

        // Parse the PEM to get the DER bytes for device ID derivation.
        let cert_der = pem_to_der(&cert_pem)?;
        let device_id = derive_device_id(&cert_der);

        Ok(Self {
            device_id,
            cert_pem,
            key_pem,
        })
    }

    /// Save identity to disk.
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).context("creating identity directory")?;
        std::fs::write(dir.join(CERT_FILE), &self.cert_pem)
            .context("writing device certificate")?;
        std::fs::write(dir.join(KEY_FILE), &self.key_pem).context("writing device key")?;
        Ok(())
    }

    /// Load or generate identity. If the files exist on disk, loads them;
    /// otherwise generates a new identity and saves it.
    pub fn load_or_generate(dir: &Path) -> Result<Self> {
        if dir.join(CERT_FILE).exists() && dir.join(KEY_FILE).exists() {
            Self::load(dir)
        } else {
            let identity = Self::generate()?;
            identity.save(dir)?;
            Ok(identity)
        }
    }

    /// The DER bytes of this identity's certificate — the exact bytes the device
    /// id is the hash of, and the bytes a remote verifier hashes to bind the
    /// carried public key back to the issuer's device id.
    pub fn cert_der(&self) -> Result<Vec<u8>, DeviceKeyError> {
        pem_certificate_to_der(&self.cert_pem)
    }

    /// Sign `message` with this device's real identity private key — the secret
    /// behind the TLS certificate, not anything derivable from the public device
    /// id.
    ///
    /// This is the key step that makes a capability signature a genuine proof of
    /// node-key possession: only the holder of the private key persisted in
    /// `device.key` can produce a signature that verifies against the public key
    /// inside the certificate the device id commits to. A trusted-but-ungranted
    /// peer, which knows only the public device id, cannot.
    pub fn sign_capability(
        &self,
        message: &[u8],
    ) -> Result<[u8; DEVICE_SIGNATURE_LENGTH], DeviceKeyError> {
        let key_der = pem_pkcs8_to_der(&self.key_pem)?;
        let rng = SystemRandom::new();
        let key_pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &key_der, &rng)
            .map_err(|_| DeviceKeyError::MalformedPrivateKey)?;
        let signature = key_pair
            .sign(&rng, message)
            .map_err(|_| DeviceKeyError::Signing)?;
        <[u8; DEVICE_SIGNATURE_LENGTH]>::try_from(signature.as_ref())
            .map_err(|_| DeviceKeyError::Signing)
    }
}

/// Why signing with, or verifying against, a device identity key failed.
///
/// Every variant is a hard failure surfaced to the caller — never a silent
/// fallback. A verify failure means the signature does not prove possession of
/// the issuer's private key, or the carried certificate does not belong to the
/// claimed issuer; either way the credential is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DeviceKeyError {
    /// The stored private key PEM did not contain a usable PKCS#8 key.
    #[error("device private key is missing or malformed")]
    MalformedPrivateKey,

    /// Producing the signature failed inside the signing primitive.
    #[error("signing the capability payload failed")]
    Signing,

    /// The carried certificate could not be parsed as DER X.509.
    #[error("issuer certificate is not valid DER X.509")]
    MalformedCertificate,

    /// The certificate's device id (the hash of its DER bytes) is not the issuer
    /// the credential claims. A verifier rejects the signature before checking it
    /// so a forger cannot present its own certificate under a victim's id.
    #[error("issuer certificate does not match claimed device id {claimed}")]
    IssuerMismatch {
        /// The device id the credential claimed.
        claimed: String,
        /// The device id the carried certificate actually hashes to.
        actual: String,
    },

    /// The signature did not verify against the public key in the certificate.
    #[error("capability signature did not verify against the issuer certificate")]
    BadSignature,
}

/// Verify `signature` over `message` using the public key bound to
/// `issuer_cert_der`, after confirming that certificate belongs to
/// `claimed_issuer`.
///
/// The binding check is load-bearing: the device id is the hash of the
/// certificate DER, so re-deriving it from the carried certificate and demanding
/// it equal the claimed issuer is what stops a forger from presenting its own
/// certificate (and its own signature) under another node's device id. Only once
/// the certificate is bound to the issuer is its public key trusted to check the
/// signature.
pub fn verify_capability_signature(
    issuer_cert_der: &[u8],
    claimed_issuer: &str,
    message: &[u8],
    signature: &[u8],
) -> Result<(), DeviceKeyError> {
    let actual = derive_device_id(issuer_cert_der);
    if actual != claimed_issuer {
        return Err(DeviceKeyError::IssuerMismatch {
            claimed: claimed_issuer.to_owned(),
            actual,
        });
    }
    let public_key_point = certificate_public_key_point(issuer_cert_der)?;
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, public_key_point.as_slice())
        .verify(message, signature)
        .map_err(|_| DeviceKeyError::BadSignature)
}

/// Extract the raw ECDSA public-key point (`0x04 ‖ X ‖ Y`) from a DER X.509
/// certificate's `SubjectPublicKeyInfo`.
fn certificate_public_key_point(cert_der: &[u8]) -> Result<Vec<u8>, DeviceKeyError> {
    let (_, certificate) = x509_parser::certificate::X509Certificate::from_der(cert_der)
        .map_err(|_| DeviceKeyError::MalformedCertificate)?;
    Ok(certificate
        .public_key()
        .subject_public_key
        .data
        .as_ref()
        .to_vec())
}

/// Decode a PEM `CERTIFICATE` block to its DER bytes.
fn pem_certificate_to_der(cert_pem: &str) -> Result<Vec<u8>, DeviceKeyError> {
    let normalised = cert_pem
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    rustls_pemfile::certs(&mut normalised.as_bytes())
        .next()
        .and_then(Result::ok)
        .map(|der| der.as_ref().to_vec())
        .ok_or(DeviceKeyError::MalformedCertificate)
}

/// Decode a PEM PKCS#8 `PRIVATE KEY` block to its DER bytes.
fn pem_pkcs8_to_der(key_pem: &str) -> Result<Vec<u8>, DeviceKeyError> {
    let normalised = key_pem
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    rustls_pemfile::pkcs8_private_keys(&mut normalised.as_bytes())
        .next()
        .and_then(Result::ok)
        .map(|key| key.secret_pkcs8_der().to_vec())
        .ok_or(DeviceKeyError::MalformedPrivateKey)
}

/// Derive a device ID from certificate DER bytes.
///
/// The ID is the base32 encoding of the SHA-256 of the DER-encoded
/// certificate, matching Syncthing's convention.
#[must_use]
pub fn derive_device_id(cert_der: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    let hash = hasher.finalize();
    BASE32_NOPAD.encode(&hash)
}

/// Extract DER bytes from a PEM-encoded certificate.
fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    // Minimal PEM parser: strip header/footer and decode base64.
    let pem = pem.trim();
    let start = pem
        .find("-----BEGIN")
        .context("PEM start marker not found")?;
    let after_start = pem.get(start..).context("PEM start offset out of range")?;
    let header_end = after_start.find('\n').context("PEM header end not found")?;
    let end_marker = pem.find("-----END").context("PEM end marker not found")?;

    let b64_start = start + header_end + 1;
    let b64_data = pem
        .get(b64_start..end_marker)
        .context("PEM body out of range")?
        .replace(['\n', '\r', ' '], "");
    BASE64
        .decode(b64_data.as_bytes())
        .map_err(|e| anyhow::anyhow!("base64 decode error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_identity() {
        let id = DeviceIdentity::generate().unwrap();
        assert!(!id.device_id.is_empty());
        assert!(id.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(id.key_pem.contains("BEGIN"));
    }

    #[test]
    fn device_id_is_base32() {
        let id = DeviceIdentity::generate().unwrap();
        // Base32NOPAD uses A-Z and 2-7.
        assert!(
            id.device_id
                .chars()
                .all(|c| c.is_ascii_uppercase() || ('2'..='7').contains(&c))
        );
        // SHA-256 = 32 bytes → base32 = 52 characters.
        assert_eq!(id.device_id.len(), 52);
    }

    #[test]
    fn device_id_deterministic() {
        let id = DeviceIdentity::generate().unwrap();
        // Derive from the same cert DER should give the same ID.
        let cert_der = pem_to_der(&id.cert_pem).unwrap();
        let derived = derive_device_id(&cert_der);
        assert_eq!(derived, id.device_id);
    }

    #[test]
    fn different_identities_have_different_device_ids() {
        let id1 = DeviceIdentity::generate().unwrap();
        let id2 = DeviceIdentity::generate().unwrap();
        assert_ne!(id1.device_id, id2.device_id);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let original = DeviceIdentity::generate().unwrap();
        original.save(dir.path()).unwrap();

        let loaded = DeviceIdentity::load(dir.path()).unwrap();
        assert_eq!(loaded.device_id, original.device_id);
        assert_eq!(loaded.cert_pem, original.cert_pem);
        assert_eq!(loaded.key_pem, original.key_pem);
    }

    #[test]
    fn load_or_generate_creates_new() {
        let dir = tempfile::tempdir().unwrap();
        let id = DeviceIdentity::load_or_generate(dir.path()).unwrap();
        assert!(!id.device_id.is_empty());
        assert!(dir.path().join(CERT_FILE).exists());
        assert!(dir.path().join(KEY_FILE).exists());
    }

    #[test]
    fn load_or_generate_loads_existing() {
        let dir = tempfile::tempdir().unwrap();
        let id1 = DeviceIdentity::load_or_generate(dir.path()).unwrap();
        let id2 = DeviceIdentity::load_or_generate(dir.path()).unwrap();
        assert_eq!(id1.device_id, id2.device_id);
    }

    #[test]
    fn pem_to_der_works() {
        let id = DeviceIdentity::generate().unwrap();
        let der = pem_to_der(&id.cert_pem).unwrap();
        // DER should start with SEQUENCE tag (0x30).
        assert_eq!(der[0], 0x30);
        assert!(!der.is_empty());
    }

    #[test]
    fn sign_then_verify_round_trips_against_the_certificate() {
        let id = DeviceIdentity::generate().unwrap();
        let message = b"cascade capability payload";
        let signature = id.sign_capability(message).unwrap();
        let cert_der = id.cert_der().unwrap();
        verify_capability_signature(&cert_der, &id.device_id, message, &signature).unwrap();
    }

    #[test]
    fn a_different_devices_signature_does_not_verify() {
        // The key step: knowing the public device id is not enough. Another
        // device's signature, presented under the victim's certificate, must
        // fail — only the victim's private key produces a verifying signature.
        let victim = DeviceIdentity::generate().unwrap();
        let attacker = DeviceIdentity::generate().unwrap();
        let message = b"forge me";
        let forged = attacker.sign_capability(message).unwrap();
        let victim_cert = victim.cert_der().unwrap();
        assert_eq!(
            verify_capability_signature(&victim_cert, &victim.device_id, message, &forged),
            Err(DeviceKeyError::BadSignature)
        );
    }

    #[test]
    fn a_certificate_under_a_foreign_id_is_rejected_before_verification() {
        // An attacker presents its own (validly self-signed) certificate but
        // claims it belongs to the victim's id. The id binding rejects it.
        let victim = DeviceIdentity::generate().unwrap();
        let attacker = DeviceIdentity::generate().unwrap();
        let message = b"mismatched issuer";
        let signature = attacker.sign_capability(message).unwrap();
        let attacker_cert = attacker.cert_der().unwrap();
        let result =
            verify_capability_signature(&attacker_cert, &victim.device_id, message, &signature);
        assert!(matches!(result, Err(DeviceKeyError::IssuerMismatch { .. })));
    }

    #[test]
    fn a_tampered_message_does_not_verify() {
        let id = DeviceIdentity::generate().unwrap();
        let signature = id.sign_capability(b"original").unwrap();
        let cert_der = id.cert_der().unwrap();
        assert_eq!(
            verify_capability_signature(&cert_der, &id.device_id, b"tampered", &signature),
            Err(DeviceKeyError::BadSignature)
        );
    }
}
