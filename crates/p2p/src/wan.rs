//! WAN peer discovery via a global HTTPS discovery service.
//!
//! Devices announce the addresses where they can be reached and query the
//! service for other device IDs. The client intentionally treats the service
//! as a narrow REST contract so the P2P engine does not depend on any specific
//! server implementation.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const ANNOUNCE_PATH: &str = "announce";
const LOOKUP_PATH: &str = "lookup";

/// Global discovery server client.
#[derive(Debug, Clone)]
pub struct GlobalDiscovery {
    server_url: String,
    client: reqwest::Client,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DiscoveryAnnouncement {
    device_id: String,
    addresses: Vec<String>,
    timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DiscoveryLookupResponse {
    addresses: Vec<String>,
}

impl GlobalDiscovery {
    /// Create a discovery client pointing at a discovery server URL.
    pub fn new(server_url: &str) -> Self {
        Self {
            server_url: server_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Announce this device's addresses to the global discovery server.
    pub async fn announce(&self, device_id: &str, addresses: &[SocketAddr]) -> Result<()> {
        let payload = DiscoveryAnnouncement {
            device_id: device_id.to_string(),
            addresses: addresses.iter().map(SocketAddr::to_string).collect(),
            timestamp: Utc::now(),
        };

        self.client
            .post(self.announce_url())
            .json(&payload)
            .send()
            .await
            .context("sending global discovery announcement")?
            .error_for_status()
            .context("global discovery announcement rejected")?;

        Ok(())
    }

    /// Query the global discovery server for a specific device.
    pub async fn lookup(&self, device_id: &str) -> Result<Vec<SocketAddr>> {
        let response = self
            .client
            .get(self.lookup_url(device_id))
            .send()
            .await
            .context("querying global discovery server")?
            .error_for_status()
            .context("global discovery lookup rejected")?
            .json::<DiscoveryLookupResponse>()
            .await
            .context("decoding global discovery lookup response")?;

        response
            .addresses
            .iter()
            .map(|address| {
                address
                    .parse::<SocketAddr>()
                    .with_context(|| format!("parsing discovered address {address}"))
            })
            .collect()
    }

    fn announce_url(&self) -> String {
        format!("{}/{ANNOUNCE_PATH}", self.server_url)
    }

    fn lookup_url(&self, device_id: &str) -> String {
        format!("{}/{LOOKUP_PATH}/{device_id}", self.server_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Mutex;
    use tokio::task::JoinHandle;

    const HTTP_OK: &str =
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n";
    const HTTP_NOT_FOUND: &str =
        "HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 0\r\n\r\n";
    const HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";

    struct MockGlobalDiscoveryServer {
        address: SocketAddr,
        task: JoinHandle<()>,
    }

    impl MockGlobalDiscoveryServer {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let state = Arc::new(Mutex::new(HashMap::<String, Vec<String>>::new()));
            let task = tokio::spawn(run_server(listener, state));
            Self { address, task }
        }

        fn url(&self) -> String {
            format!("http://{}", self.address)
        }
    }

    impl Drop for MockGlobalDiscoveryServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn run_server(listener: TcpListener, state: Arc<Mutex<HashMap<String, Vec<String>>>>) {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                handle_request(stream, state).await.unwrap();
            });
        }
    }

    async fn handle_request(
        mut stream: TcpStream,
        state: Arc<Mutex<HashMap<String, Vec<String>>>>,
    ) -> Result<()> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];

        while !buffer
            .windows(HEADER_TERMINATOR.len())
            .any(|window| window == HEADER_TERMINATOR)
        {
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                anyhow::bail!("client closed before request headers completed");
            }
            buffer.extend_from_slice(&chunk[..read]);
        }

        let header_end = buffer
            .windows(HEADER_TERMINATOR.len())
            .position(|window| window == HEADER_TERMINATOR)
            .context("finding request header terminator")?
            + HEADER_TERMINATOR.len();
        let headers = String::from_utf8(buffer[..header_end].to_vec())?;
        let request_line = headers.lines().next().context("reading request line")?;
        let body_len = content_length(&headers)?;

        while buffer.len() < header_end + body_len {
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                anyhow::bail!("client closed before request body completed");
            }
            buffer.extend_from_slice(&chunk[..read]);
        }

        if request_line.starts_with("POST /announce ") {
            let body = &buffer[header_end..header_end + body_len];
            let announcement: DiscoveryAnnouncement = serde_json::from_slice(body)?;
            state
                .lock()
                .await
                .insert(announcement.device_id, announcement.addresses);
            write_json(&mut stream, &serde_json::json!({"ok": true})).await?;
            return Ok(());
        }

        if let Some(device_id) = request_line
            .strip_prefix("GET /lookup/")
            .and_then(|rest| rest.split_once(' '))
            .map(|(device_id, _)| device_id)
        {
            let addresses = state.lock().await.get(device_id).cloned();
            match addresses {
                Some(addresses) => {
                    write_json(&mut stream, &DiscoveryLookupResponse { addresses }).await?;
                }
                None => stream.write_all(HTTP_NOT_FOUND.as_bytes()).await?,
            }
            return Ok(());
        }

        stream.write_all(HTTP_NOT_FOUND.as_bytes()).await?;
        Ok(())
    }

    fn content_length(headers: &str) -> Result<usize> {
        for line in headers.lines() {
            if let Some(value) = line.strip_prefix("content-length: ") {
                return value.parse().context("parsing content length");
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                return value.parse().context("parsing content length");
            }
        }
        Ok(0)
    }

    async fn write_json<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<()> {
        let body = serde_json::to_vec(value)?;
        let headers = format!("{HTTP_OK}content-length: {}\r\n\r\n", body.len());
        stream.write_all(headers.as_bytes()).await?;
        stream.write_all(&body).await?;
        Ok(())
    }

    #[tokio::test]
    async fn announce_then_lookup_returns_registered_addresses() {
        let server = MockGlobalDiscoveryServer::start().await;
        let discovery = GlobalDiscovery::new(&server.url());
        let addresses = ["127.0.0.1:22000".parse::<SocketAddr>().unwrap()];

        discovery.announce("DEVICE", &addresses).await.unwrap();
        let discovered = discovery.lookup("DEVICE").await.unwrap();

        assert_eq!(discovered, addresses);
    }

    #[tokio::test]
    async fn lookup_unknown_device_returns_error() {
        let server = MockGlobalDiscoveryServer::start().await;
        let discovery = GlobalDiscovery::new(&server.url());

        let result = discovery.lookup("MISSING").await;

        assert!(result.is_err());
    }
}
