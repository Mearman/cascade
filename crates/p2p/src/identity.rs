//! Device identity — TLS certificate generation and device ID derivation.
//!
//! Each device generates a self-signed TLS certificate on first run. The
//! device ID is the base32-encoded SHA-256 of the certificate DER bytes,
//! following the same convention as Syncthing.

use std::path::Path;

use anyhow::{Context, Result};
use data_encoding::{BASE32_NOPAD, BASE64};
use rcgen::KeyPair;
use sha2::{Digest, Sha256};

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
}
