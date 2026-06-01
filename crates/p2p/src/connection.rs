//! TLS-authenticated peer connections with device ID verification.
//!
//! Every peer connection is wrapped in TLS. Trusted device fingerprints
//! are pinned at the rustls verifier layer, so the TLS handshake itself
//! fails for any peer whose SHA-256 certificate fingerprint is not in
//! the approved set. No data is exchanged until identity is confirmed.
//!
//! As belt-and-braces, the cert that survives the handshake is also
//! checked once more post-handshake. The handshake-time verifier is
//! authoritative; the post-handshake check guards against a future
//! misconfiguration where the wrong verifier is wired in.
//!
//! Direct TCP+TLS only — relay fallback is driven by
//! [`crate::relay::RelayClient::connect_with_secret`] from the
//! `cascade-backend-p2p` sync engine, which knows the connectivity
//! strategy and the shared secret. The connection manager itself does
//! not attempt relay fallback any more.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};

use crate::discovery::DiscoveredPeer;
use crate::identity::DeviceIdentity;
use crate::relay::RelayConnection;

/// Ensure a process-level `CryptoProvider` is installed.
///
/// rustls 0.23 requires explicit crypto provider installation.
/// This must be called before any TLS operations. Calling it multiple
/// times is safe — it only installs once.
fn ensure_crypto_provider() {
    let _ = tokio_rustls::rustls::crypto::CryptoProvider::install_default(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    );
}

/// Manages TLS-authenticated direct connections to P2P peers.
///
/// Relay fallback lives one layer up in the backend's sync engine, which
/// has the connectivity strategy and the shared secret needed for the
/// HMAC handshake the relay server requires.
#[derive(Debug)]
pub struct ConnectionManager {
    /// Our device identity (certificate + key).
    identity: DeviceIdentity,
    /// Device IDs we accept connections from.
    trusted_device_ids: Vec<String>,
}

/// A TLS-authenticated connection to a peer. Wraps either a client or
/// server TLS stream — both carry the same encrypted data.
#[derive(Debug)]
pub enum PeerConnection {
    /// Direct TLS connection.
    Direct(Box<TlsStream<TcpStream>>),
    /// TLS-through-relay connection.
    Relay(Box<RelayConnection>),
}

impl ConnectionManager {
    /// Create a connection manager with our identity and trusted peers.
    #[must_use]
    pub const fn new(identity: DeviceIdentity, trusted_device_ids: Vec<String>) -> Self {
        Self {
            identity,
            trusted_device_ids,
        }
    }

    /// Connect to a known peer over direct TCP+TLS.
    pub async fn connect(&self, peer: &DiscoveredPeer) -> Result<PeerConnection> {
        let stream = self
            .connect_direct(peer.address, &peer.device_id)
            .await
            .with_context(|| {
                format!(
                    "connecting directly to peer {} at {}",
                    peer.device_id, peer.address
                )
            })?;
        Ok(PeerConnection::Direct(Box::new(stream)))
    }

    /// Accept an incoming connection and verify the peer's device ID.
    ///
    /// Returns the verified device ID and the TLS stream on success.
    /// The TLS handshake fails for any peer whose certificate fingerprint
    /// is not in the trusted list; the post-handshake recheck below is a
    /// belt-and-braces assertion that should be unreachable in practice.
    pub async fn accept(&self, stream: TcpStream) -> Result<(String, TlsStream<TcpStream>)> {
        let acceptor = self.build_server_acceptor()?;
        let tls_stream = acceptor
            .accept(stream)
            .await
            .context("TLS handshake with incoming peer")?;

        let (_, connection) = tls_stream.get_ref();
        let peer_cert = connection
            .peer_certificates()
            .context("peer did not present a certificate")?;
        let cert = peer_cert
            .first()
            .context("peer certificate chain is empty")?;

        let peer_device_id = crate::identity::derive_device_id(cert.as_ref());

        // Belt-and-braces: the cert that survived the TLS handshake should
        // match a trusted device, but verify once more in case a verifier
        // was misconfigured. This branch is unreachable in normal operation.
        if !self.trusted_device_ids.contains(&peer_device_id) {
            anyhow::bail!(
                "post-handshake mismatch: peer device ID {peer_device_id} is not in the trusted list (verifier misconfiguration?)"
            );
        }

        Ok((peer_device_id, TlsStream::Server(tls_stream)))
    }

    /// Initiate a direct TLS connection to an address. The `expected_device_id`
    /// is pinned at the verifier layer so the handshake fails for any peer
    /// whose certificate fingerprint does not match.
    async fn connect_direct(
        &self,
        address: SocketAddr,
        expected_device_id: &str,
    ) -> Result<TlsStream<TcpStream>> {
        let tcp = TcpStream::connect(address)
            .await
            .with_context(|| format!("TCP connect to {address}"))?;

        let connector = self.build_client_connector(expected_device_id)?;
        let server_name = ServerName::try_from("cascade-device")
            .map_err(|e| anyhow::anyhow!("invalid server name: {e}"))?;

        let tls_stream = connector
            .connect(server_name, tcp)
            .await
            .context("TLS handshake with peer")?;

        // Belt-and-braces: the cert that survived the TLS handshake should
        // match the expected device, but verify once more in case a verifier
        // was misconfigured. This branch is unreachable in normal operation.
        let (_, connection) = tls_stream.get_ref();
        let peer_cert = connection
            .peer_certificates()
            .context("peer did not present a certificate")?;
        let cert = peer_cert
            .first()
            .context("peer certificate chain is empty")?;
        let peer_device_id = crate::identity::derive_device_id(cert.as_ref());

        if peer_device_id != expected_device_id {
            anyhow::bail!(
                "post-handshake mismatch: peer presented device {peer_device_id}, expected {expected_device_id} (verifier misconfiguration?)"
            );
        }

        Ok(TlsStream::Client(tls_stream))
    }

    /// Build a TLS client connector that presents our identity certificate
    /// and pins the expected server device ID at the verifier layer.
    fn build_client_connector(&self, expected_device_id: &str) -> Result<TlsConnector> {
        ensure_crypto_provider();
        let certs = load_certs(&self.identity.cert_pem)?;
        let key = load_key(&self.identity.key_pem)?;
        let verifier = TrustedServerCertVerifier {
            expected_device_id: expected_device_id.to_string(),
        };
        let config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_client_auth_cert(certs, key)
            .map_err(|e| anyhow::anyhow!("building TLS client config: {e}"))?;
        Ok(TlsConnector::from(Arc::new(config)))
    }

    /// Build a TLS server acceptor that pins the trusted client device IDs
    /// at the verifier layer — untrusted clients fail the handshake.
    fn build_server_acceptor(&self) -> Result<TlsAcceptor> {
        ensure_crypto_provider();
        let certs = load_certs(&self.identity.cert_pem)?;
        let key = load_key(&self.identity.key_pem)?;

        let verifier = TrustedClientCertVerifier {
            trusted_device_ids: self.trusted_device_ids.clone(),
        };
        let config = ServerConfig::builder()
            .with_client_cert_verifier(Arc::new(verifier))
            .with_single_cert(certs, key)
            .map_err(|e| anyhow::anyhow!("building TLS server config: {e}"))?;

        Ok(TlsAcceptor::from(Arc::new(config)))
    }
}

/// Client cert verifier that pins a set of trusted device fingerprints.
///
/// The TLS handshake fails for any client whose certificate fingerprint
/// (SHA-256, base32-encoded as a device ID) is not in `trusted_device_ids`.
/// Signature verification is delegated through — the certificate identity
/// is what we authenticate against, not a CA-signed chain.
#[derive(Debug)]
struct TrustedClientCertVerifier {
    trusted_device_ids: Vec<String>,
}

impl tokio_rustls::rustls::server::danger::ClientCertVerifier for TrustedClientCertVerifier {
    fn root_hint_subjects(&self) -> &[tokio_rustls::rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> std::result::Result<
        tokio_rustls::rustls::server::danger::ClientCertVerified,
        tokio_rustls::rustls::Error,
    > {
        let observed = crate::identity::derive_device_id(end_entity.as_ref());
        if self.trusted_device_ids.iter().any(|id| id == &observed) {
            Ok(tokio_rustls::rustls::server::danger::ClientCertVerified::assertion())
        } else {
            Err(tokio_rustls::rustls::Error::General(format!(
                "untrusted client device {observed}"
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        let provider =
            tokio_rustls::rustls::crypto::CryptoProvider::get_default().ok_or_else(|| {
                tokio_rustls::rustls::Error::General("no rustls CryptoProvider installed".into())
            })?;
        tokio_rustls::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        let provider =
            tokio_rustls::rustls::crypto::CryptoProvider::get_default().ok_or_else(|| {
                tokio_rustls::rustls::Error::General("no rustls CryptoProvider installed".into())
            })?;
        tokio_rustls::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        vec![
            tokio_rustls::rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            tokio_rustls::rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            tokio_rustls::rustls::SignatureScheme::ED25519,
            tokio_rustls::rustls::SignatureScheme::RSA_PSS_SHA256,
            tokio_rustls::rustls::SignatureScheme::RSA_PSS_SHA384,
            tokio_rustls::rustls::SignatureScheme::RSA_PKCS1_SHA256,
            tokio_rustls::rustls::SignatureScheme::RSA_PKCS1_SHA384,
        ]
    }
}

/// Server cert verifier that pins the expected server device fingerprint.
///
/// Set at connect time because the client always knows which peer it is
/// dialling. The TLS handshake fails if the server presents any cert
/// whose fingerprint does not match `expected_device_id`.
#[derive(Debug)]
struct TrustedServerCertVerifier {
    expected_device_id: String,
}

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for TrustedServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::ServerCertVerified,
        tokio_rustls::rustls::Error,
    > {
        let observed = crate::identity::derive_device_id(end_entity.as_ref());
        if observed == self.expected_device_id {
            Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(tokio_rustls::rustls::Error::General(format!(
                "untrusted server device {observed}; expected {}",
                self.expected_device_id
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        let provider =
            tokio_rustls::rustls::crypto::CryptoProvider::get_default().ok_or_else(|| {
                tokio_rustls::rustls::Error::General("no rustls CryptoProvider installed".into())
            })?;
        tokio_rustls::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        let provider =
            tokio_rustls::rustls::crypto::CryptoProvider::get_default().ok_or_else(|| {
                tokio_rustls::rustls::Error::General("no rustls CryptoProvider installed".into())
            })?;
        tokio_rustls::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        vec![
            tokio_rustls::rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            tokio_rustls::rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            tokio_rustls::rustls::SignatureScheme::ED25519,
            tokio_rustls::rustls::SignatureScheme::RSA_PSS_SHA256,
            tokio_rustls::rustls::SignatureScheme::RSA_PSS_SHA384,
            tokio_rustls::rustls::SignatureScheme::RSA_PKCS1_SHA256,
            tokio_rustls::rustls::SignatureScheme::RSA_PKCS1_SHA384,
        ]
    }
}

/// Load certificates from PEM text.
fn load_certs(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    let rustls_pem = pem
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let certs = rustls_pemfile::certs(&mut rustls_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("parsing certificate PEM")?;
    Ok(certs)
}

/// Load a private key from PEM text.
fn load_key(pem: &str) -> Result<PrivateKeyDer<'static>> {
    let rustls_pem = pem
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    // Try PKCS8 first (covers ECDSA and RSA keys).
    if let Some(key) = rustls_pemfile::pkcs8_private_keys(&mut rustls_pem.as_bytes())
        .next()
        .transpose()
        .context("parsing PKCS8 key")?
    {
        return Ok(key.into());
    }

    // Try EC.
    let rustls_pem = pem
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(key) = rustls_pemfile::ec_private_keys(&mut rustls_pem.as_bytes())
        .next()
        .transpose()
        .context("parsing EC key")?
    {
        return Ok(key.into());
    }

    // Try RSA.
    let rustls_pem = pem
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(key) = rustls_pemfile::rsa_private_keys(&mut rustls_pem.as_bytes())
        .next()
        .transpose()
        .context("parsing RSA key")?
    {
        return Ok(key.into());
    }

    Err(anyhow::anyhow!("no usable private key found in PEM"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::DeviceIdentity;

    #[allow(dead_code)]
    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn connection_manager_builds_client_connector() {
        let identity = DeviceIdentity::generate().unwrap();
        let device_id = identity.device_id.clone();
        let manager = ConnectionManager::new(identity, vec![]);
        let _connector = manager.build_client_connector(&device_id).unwrap();
    }

    #[test]
    fn connection_manager_builds_server_acceptor() {
        let identity = DeviceIdentity::generate().unwrap();
        let manager = ConnectionManager::new(identity.clone(), vec!["SOME-PEER".to_string()]);
        let acceptor = manager.build_server_acceptor();
        assert!(acceptor.is_ok());
    }

    #[tokio::test]
    async fn accept_rejects_untrusted_device_id() {
        let identity = DeviceIdentity::generate().unwrap();
        let manager = ConnectionManager::new(identity, vec!["TRUSTED-PEER".to_string()]);

        // Start a listener.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        // Connect with a different identity.
        let attacker_identity = DeviceIdentity::generate().unwrap();
        let attacker_manager =
            ConnectionManager::new(attacker_identity, vec!["ANYTHING".to_string()]);

        let accept_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            manager.accept(stream).await
        });

        // The attacker connects with an untrusted cert. The expected
        // server device the attacker pins doesn't matter here — the
        // server will reject the attacker's client cert at handshake
        // time before the client's own pin is ever checked.
        let _ = attacker_manager
            .connect_direct(address, "TRUSTED-PEER")
            .await;

        let result = accept_task.await.unwrap();
        let error = result.expect_err("expected accept to reject the untrusted peer");
        // The rejection now happens during the TLS handshake itself,
        // not as a post-handshake fingerprint check. The accept call
        // wraps the rustls error with this context message.
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("TLS handshake with incoming peer"),
            "expected handshake-time rejection, got: {rendered}"
        );
    }

    #[tokio::test]
    async fn connect_direct_rejects_untrusted_server() {
        // The client pins an expected device ID that the server does
        // not match. The TLS handshake must fail at the client's
        // ServerCertVerifier — no application data flows.
        let server_identity = DeviceIdentity::generate().unwrap();
        let server_device_id = server_identity.device_id.clone();
        let server_manager =
            ConnectionManager::new(server_identity.clone(), vec![server_device_id.clone()]);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        // The server side may or may not surface its own error first;
        // we don't care here because we are testing the client.
        let _accept_task = tokio::spawn(async move {
            let acceptor = server_manager.build_server_acceptor()?;
            let (stream, _) = listener.accept().await?;
            let _ = acceptor.accept(stream).await;
            Ok::<(), anyhow::Error>(())
        });

        let client_identity = DeviceIdentity::generate().unwrap();
        let client_manager = ConnectionManager::new(client_identity, vec![server_device_id]);

        let result = client_manager
            .connect_direct(address, "SOME-OTHER-DEVICE-WE-DO-NOT-TRUST")
            .await;

        let error = result.expect_err("expected client to reject server with wrong device ID");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("TLS handshake with peer"),
            "expected handshake-time rejection, got: {rendered}"
        );
    }

    #[tokio::test]
    async fn connect_uses_direct_tcp_when_peer_is_reachable() {
        let identity = DeviceIdentity::generate().unwrap();
        let device_id = identity.device_id.clone();
        let manager = ConnectionManager::new(identity.clone(), vec![device_id.clone()]);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let accept_task = tokio::spawn(async move {
            let acceptor = manager.build_server_acceptor().unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            acceptor.accept(stream).await.unwrap();
        });

        let peer = DiscoveredPeer {
            device_id: device_id.clone(),
            address,
        };
        let client_manager = ConnectionManager::new(identity, vec![device_id]);
        let connection = client_manager.connect(&peer).await.unwrap();

        match connection {
            PeerConnection::Direct(_) => {}
            PeerConnection::Relay(_) => panic!("expected direct connection"),
        }
        accept_task.await.unwrap();
    }

    /// Build a DigitallySignedStruct via the public Codec path —
    /// rustls keeps its constructor `pub(crate)`, so we synthesise the
    /// on-wire form and parse it back.
    fn forged_dss(
        scheme: tokio_rustls::rustls::SignatureScheme,
        signature: &[u8],
    ) -> tokio_rustls::rustls::DigitallySignedStruct {
        use tokio_rustls::rustls::internal::msgs::codec::{Codec, Reader};

        let mut encoded = Vec::new();
        scheme.encode(&mut encoded);
        // length-prefixed signature, big-endian u16.
        let len = u16::try_from(signature.len()).unwrap();
        encoded.extend_from_slice(&len.to_be_bytes());
        encoded.extend_from_slice(signature);

        let mut reader = Reader::init(&encoded);
        tokio_rustls::rustls::DigitallySignedStruct::read(&mut reader).unwrap()
    }

    /// A corrupted DigitallySignedStruct must be rejected by the signature
    /// verifier — the previous implementation accepted everything blindly.
    ///
    /// This exercises the verifier method directly rather than driving a
    /// real handshake. Producing a forged-but-syntactically-valid signed
    /// handshake transcript would require reimplementing pieces of the TLS
    /// state machine; testing the verifier itself with garbage signature
    /// bytes is sufficient to prove that the fix calls into the real
    /// rustls crypto path (which rejects the bytes) rather than returning
    /// an unconditional assertion.
    #[test]
    fn server_cert_verifier_rejects_forged_signature() {
        use tokio_rustls::rustls::SignatureScheme;
        use tokio_rustls::rustls::client::danger::ServerCertVerifier;

        ensure_crypto_provider();

        let identity = DeviceIdentity::generate().unwrap();
        let certs = load_certs(&identity.cert_pem).unwrap();
        let cert = certs.into_iter().next().unwrap();

        let verifier = TrustedServerCertVerifier {
            expected_device_id: identity.device_id.clone(),
        };

        // Garbage signature bytes that cannot have come from any real key.
        let dss = forged_dss(SignatureScheme::ECDSA_NISTP256_SHA256, &[0u8; 64]);
        let message = b"not the real handshake transcript";

        let result12 = verifier.verify_tls12_signature(message, &cert, &dss);
        assert!(
            result12.is_err(),
            "expected TLS1.2 signature verification to reject forged bytes"
        );

        let result13 = verifier.verify_tls13_signature(message, &cert, &dss);
        assert!(
            result13.is_err(),
            "expected TLS1.3 signature verification to reject forged bytes"
        );
    }

    /// Same as above for the server-side ClientCertVerifier — exercising
    /// the client-cert signature path on the server.
    #[test]
    fn client_cert_verifier_rejects_forged_signature() {
        use tokio_rustls::rustls::SignatureScheme;
        use tokio_rustls::rustls::server::danger::ClientCertVerifier;

        ensure_crypto_provider();

        let identity = DeviceIdentity::generate().unwrap();
        let certs = load_certs(&identity.cert_pem).unwrap();
        let cert = certs.into_iter().next().unwrap();

        let verifier = TrustedClientCertVerifier {
            trusted_device_ids: vec![identity.device_id.clone()],
        };

        let dss = forged_dss(SignatureScheme::ECDSA_NISTP256_SHA256, &[0u8; 64]);
        let message = b"not the real handshake transcript";

        let result12 = verifier.verify_tls12_signature(message, &cert, &dss);
        assert!(
            result12.is_err(),
            "expected TLS1.2 signature verification to reject forged bytes"
        );

        let result13 = verifier.verify_tls13_signature(message, &cert, &dss);
        assert!(
            result13.is_err(),
            "expected TLS1.3 signature verification to reject forged bytes"
        );
    }
}
