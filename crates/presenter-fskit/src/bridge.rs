use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};
#[cfg(any(target_os = "macos", test))]
use cascade_engine::protocol::encode_message;
use cascade_engine::protocol::{Request, Response};
use serde::de::DeserializeOwned;
use serde_json::Value;

#[cfg(target_os = "macos")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(target_os = "macos")]
use tokio::net::UnixStream;

const SOCKET_RELATIVE_PATH: &str = ".config/cascade/fskit.sock";

/// Resolve the default `FSKit` bridge socket path.
pub fn default_socket_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is required to locate the FSKit socket")?;
    Ok(PathBuf::from(home).join(SOCKET_RELATIVE_PATH))
}

/// Client for the Swift `FSKit` extension bridge.
///
/// Sends length-prefixed JSON messages over a Unix domain socket using the
/// same wire protocol as the File Provider bridge. Each request carries a
/// monotonically increasing ID; the response echoes it back.
#[derive(Debug)]
pub struct FSKitBridge {
    socket_path: PathBuf,
    next_id: AtomicU32,
}

impl FSKitBridge {
    /// Create a bridge client pointing at the given socket path.
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            next_id: AtomicU32::new(1),
        }
    }

    /// Create using the default socket path.
    pub fn from_default_socket() -> Result<Self> {
        Ok(Self::new(default_socket_path()?))
    }

    /// Return the socket path this bridge is configured to use.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Build a protocol request with an auto-incrementing ID.
    pub fn build_request(&self, method: impl Into<String>, params: Value) -> Request {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        Request {
            id,
            method: method.into(),
            params,
        }
    }

    /// Send a request and decode a typed response.
    pub async fn request<T>(&self, method: impl Into<String>, params: Value) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let request = self.build_request(method, params);
        let response = self.send_request(&request).await?;

        if let Some(error) = response.error {
            bail!("FSKit bridge request {} failed: {error}", request.method);
        }

        let result = response.result.with_context(|| {
            format!("FSKit bridge request {} returned no result", request.method)
        })?;
        serde_json::from_value(result)
            .with_context(|| format!("invalid FSKit bridge response for {}", request.method))
    }

    /// Send a request that returns no payload.
    pub async fn request_empty(&self, method: impl Into<String>, params: Value) -> Result<()> {
        let _: Value = self.request(method, params).await?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    async fn send_request(&self, request: &Request) -> Result<Response> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("connect to FSKit socket {}", self.socket_path.display()))?;

        let encoded = encode_message(request)?;
        stream
            .write_all(&encoded)
            .await
            .context("write FSKit request")?;

        let mut len = [0u8; 4];
        stream
            .read_exact(&mut len)
            .await
            .context("read FSKit response length")?;
        let body_len = u32::from_be_bytes(len) as usize;
        let mut body = vec![0u8; body_len];
        stream
            .read_exact(&mut body)
            .await
            .context("read FSKit response body")?;
        serde_json::from_slice(&body).context("decode FSKit response")
    }

    #[cfg(not(target_os = "macos"))]
    #[allow(clippy::unused_async)]
    async fn send_request(&self, request: &Request) -> Result<Response> {
        bail!(
            "FSKit bridge request {} requires macOS; socket {} is not available on this platform",
            request.method,
            self.socket_path.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cascade_engine::protocol::decode_message;
    use serde_json::json;

    #[test]
    fn build_request_increments_request_ids() {
        let bridge = FSKitBridge::new("/tmp/cascade-fskit-test.sock");

        let first = bridge.build_request("upsertItem", json!({"id": "one"}));
        let second = bridge.build_request("deleteItem", json!({"id": "two"}));

        assert_eq!(first.id + 1, second.id);
        assert_eq!(first.method, "upsertItem");
        assert_eq!(second.params, json!({"id": "two"}));
    }

    #[test]
    fn encoded_request_uses_engine_protocol() {
        let bridge = FSKitBridge::new("/tmp/cascade-fskit-test.sock");
        let request = bridge.build_request("evictItem", json!({"id": "gdrive:file1"}));
        let encoded = encode_message(&request).unwrap();
        let (consumed, decoded): (usize, Request) = decode_message(&encoded).unwrap().unwrap();

        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded.id, request.id);
        assert_eq!(decoded.method, "evictItem");
        assert_eq!(decoded.params, json!({"id": "gdrive:file1"}));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn request_round_trips_over_unix_socket() {
        use cascade_engine::protocol::Response;
        use tokio::net::UnixListener;

        let socket_path = std::env::temp_dir().join(format!(
            "cascade-fskit-{}-roundtrip.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut len = [0u8; 4];
            stream.read_exact(&mut len).await.unwrap();
            let body_len = u32::from_be_bytes(len) as usize;
            let mut body = vec![0u8; body_len];
            stream.read_exact(&mut body).await.unwrap();
            let request: Request = serde_json::from_slice(&body).unwrap();
            assert_eq!(request.method, "fetchContents");

            let response = Response::ok(request.id, json!({"path": "/tmp/downloaded.txt"}));
            let encoded = encode_message(&response).unwrap();
            stream.write_all(&encoded).await.unwrap();
        });

        let bridge = FSKitBridge::new(&socket_path);
        let result: Value = bridge
            .request("fetchContents", json!({"id": "gdrive:file1"}))
            .await
            .unwrap();

        assert_eq!(result, json!({"path": "/tmp/downloaded.txt"}));
        server.await.unwrap();
        std::fs::remove_file(&socket_path).unwrap();
    }
}
