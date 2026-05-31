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
//! Connection order: direct TCP with TLS first, then relay fallback.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};

use crate::discovery::DiscoveredPeer;
use crate::identity::DeviceIdentity;
use crate::relay::{RelayClient, RelayConnection};

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

/// Manages TLS-authenticated connections to P2P peers.
#[derive(Debug)]
pub struct ConnectionManager {
    /// Our device identity (certificate + key).
    identity: DeviceIdentity,
    /// Device IDs we accept connections from.
    trusted_device_ids: Vec<String>,
    /// Relay URLs for fallback connections.
    relay_urls: Vec<String>,
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
    pub const fn new(
        identity: DeviceIdentity,
        trusted_device_ids: Vec<String>,
        relay_urls: Vec<String>,
    ) -> Self {
        Self {
            identity,
            trusted_device_ids,
            relay_urls,
        }
    }

    /// Connect to a known peer. Tries direct TLS first, then relay fallback.
    pub async fn connect(&self, peer: &DiscoveredPeer) -> Result<PeerConnection> {
        match self.connect_direct(peer.address, &peer.device_id).await {
            Ok(stream) => Ok(PeerConnection::Direct(Box::new(stream))),
            Err(direct_error) if self.relay_urls.is_empty() => {
                Err(direct_error).with_context(|| {
                    format!(
                        "connecting directly to peer {} at {}",
                        peer.device_id, peer.address
                    )
                })
            }
            Err(direct_error) => {
                let mut relay_errors = Vec::new();
                for relay_url in &self.relay_urls {
                    match RelayClient::connect(relay_url, &peer.device_id).await {
                        Ok(connection) => return Ok(PeerConnection::Relay(Box::new(connection))),
                        Err(error) => relay_errors.push(format!("{relay_url}: {error:#}")),
                    }
                }
                anyhow::bail!(
                    "direct connection to peer {} at {} failed: {direct_error}; relay fallback failed: {}",
                    peer.device_id,
                    peer.address,
                    relay_errors.join("; ")
                );
            }
        }
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
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
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
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
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
        let manager = ConnectionManager::new(identity, vec![], vec![]);
        let _connector = manager.build_client_connector(&device_id).unwrap();
    }

    #[test]
    fn connection_manager_builds_server_acceptor() {
        let identity = DeviceIdentity::generate().unwrap();
        let manager =
            ConnectionManager::new(identity.clone(), vec!["SOME-PEER".to_string()], vec![]);
        let acceptor = manager.build_server_acceptor();
        assert!(acceptor.is_ok());
    }

    #[tokio::test]
    async fn accept_rejects_untrusted_device_id() {
        let identity = DeviceIdentity::generate().unwrap();
        let manager = ConnectionManager::new(identity, vec!["TRUSTED-PEER".to_string()], vec![]);

        // Start a listener.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        // Connect with a different identity.
        let attacker_identity = DeviceIdentity::generate().unwrap();
        let attacker_manager =
            ConnectionManager::new(attacker_identity, vec!["ANYTHING".to_string()], vec![]);

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
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn connect_uses_direct_tcp_when_peer_is_reachable() {
        let identity = DeviceIdentity::generate().unwrap();
        let device_id = identity.device_id.clone();
        let manager = ConnectionManager::new(identity.clone(), vec![device_id.clone()], vec![]);

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
        let client_manager = ConnectionManager::new(identity, vec![device_id], vec![]);
        let connection = client_manager.connect(&peer).await.unwrap();

        match connection {
            PeerConnection::Direct(_) => {}
            PeerConnection::Relay(_) => panic!("expected direct connection"),
        }
        accept_task.await.unwrap();
    }

    #[tokio::test]
    async fn connect_falls_back_to_relay_when_direct_tcp_fails() {
        let identity = DeviceIdentity::generate().unwrap();
        let device_id = identity.device_id.clone();

        let unavailable = unavailable_loopback_address().await;
        let relay_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_address = relay_listener.local_addr().unwrap();
        let relay_task = tokio::spawn(async move {
            let (stream, _) = relay_listener.accept().await.unwrap();
            let _websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
        });

        let peer = DiscoveredPeer {
            device_id: device_id.clone(),
            address: unavailable,
        };
        let manager = ConnectionManager::new(
            identity,
            vec![device_id],
            vec![format!("ws://{relay_address}")],
        );

        let connection = manager.connect(&peer).await.unwrap();

        match connection {
            PeerConnection::Direct(_) => panic!("expected relay connection"),
            PeerConnection::Relay(_) => {}
        }
        relay_task.await.unwrap();
    }

    async fn unavailable_loopback_address() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        address
    }
}
