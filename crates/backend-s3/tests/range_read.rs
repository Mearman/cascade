//! Integration tests for `S3Backend::read_range`.
//!
//! These drive a mockito HTTP server standing in for S3 and assert the
//! `Backend::read_range` contract: a `206` body is used as-is, a `200`
//! response (server ignored `Range`) is sliced client-side, mid-range reads
//! return the requested window, and an offset at or past end-of-file yields
//! an empty result.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]

use cascade_backend_s3::create_backend;
use cascade_engine::types::{FileEntry, ItemId};

const BUCKET: &str = "test-bucket";
const KEY: &str = "data.bin";
const BODY: &[u8] = b"hello world";

/// Build a backend pointing at the given mock endpoint.
fn backend(endpoint: &str) -> Box<dyn cascade_engine::backend::Backend> {
    let mut table = toml::map::Map::new();
    table.insert(
        "endpoint".to_string(),
        toml::Value::String(endpoint.to_string()),
    );
    table.insert(
        "bucket".to_string(),
        toml::Value::String(BUCKET.to_string()),
    );
    table.insert(
        "region".to_string(),
        toml::Value::String("us-east-1".to_string()),
    );
    table.insert(
        "access_key_id".to_string(),
        toml::Value::String("AKIAIOSFODNN7EXAMPLE".to_string()),
    );
    table.insert(
        "secret_access_key".to_string(),
        toml::Value::String("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string()),
    );
    create_backend(&toml::Value::Table(table)).unwrap()
}

/// A `FileEntry` whose native id is the mock object key.
fn entry() -> FileEntry {
    let id = ItemId::new("s3", KEY);
    let parent = ItemId::new("s3", "");
    let size = u64::try_from(BODY.len()).unwrap();
    FileEntry::file(id, parent, KEY.to_string()).with_size(Some(size))
}

/// The object path the backend requests: `/{bucket}/{key}`.
fn object_path() -> String {
    format!("/{BUCKET}/{KEY}")
}

#[tokio::test]
async fn read_range_uses_206_body_as_is() {
    let mut server = mockito::Server::new_async().await;

    // Mid-range request: bytes 6..=10 -> "world".
    let mock = server
        .mock("GET", object_path().as_str())
        .match_header("range", "bytes=6-10")
        .with_status(206)
        .with_header("content-range", "bytes 6-10/11")
        .with_body(&BODY[6..11])
        .create_async()
        .await;

    let backend = backend(&server.url());
    let out = backend.read_range(&entry(), 6, 5).await.unwrap();

    assert_eq!(out, b"world");
    mock.assert_async().await;
}

#[tokio::test]
async fn read_range_slices_when_server_ignores_range_200() {
    let mut server = mockito::Server::new_async().await;

    // Server ignores Range and returns the whole object with 200.
    let mock = server
        .mock("GET", object_path().as_str())
        .with_status(200)
        .with_body(BODY)
        .create_async()
        .await;

    let backend = backend(&server.url());
    // Request bytes 6..=10; client must slice the full body down to "world".
    let out = backend.read_range(&entry(), 6, 5).await.unwrap();

    assert_eq!(out, b"world");
    mock.assert_async().await;
}

#[tokio::test]
async fn read_range_200_clamps_length_past_eof() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("GET", object_path().as_str())
        .with_status(200)
        .with_body(BODY)
        .create_async()
        .await;

    let backend = backend(&server.url());
    // Length runs past EOF: from offset 6, only "world" (5 bytes) remains.
    let out = backend.read_range(&entry(), 6, 999).await.unwrap();

    assert_eq!(out, b"world");
    mock.assert_async().await;
}

#[tokio::test]
async fn read_range_mid_range_206() {
    let mut server = mockito::Server::new_async().await;

    // bytes 0..=4 -> "hello".
    let mock = server
        .mock("GET", object_path().as_str())
        .match_header("range", "bytes=0-4")
        .with_status(206)
        .with_header("content-range", "bytes 0-4/11")
        .with_body(&BODY[0..5])
        .create_async()
        .await;

    let backend = backend(&server.url());
    let out = backend.read_range(&entry(), 0, 5).await.unwrap();

    assert_eq!(out, b"hello");
    mock.assert_async().await;
}

#[tokio::test]
async fn read_range_offset_past_eof_returns_empty() {
    let mut server = mockito::Server::new_async().await;

    // Offset past EOF -> S3 responds 416 -> empty result.
    let mock = server
        .mock("GET", object_path().as_str())
        .with_status(416)
        .with_body("<Error><Code>InvalidRange</Code></Error>")
        .create_async()
        .await;

    let backend = backend(&server.url());
    let out = backend.read_range(&entry(), 1000, 10).await.unwrap();

    assert!(out.is_empty());
    mock.assert_async().await;
}

#[tokio::test]
async fn read_range_zero_length_skips_request() {
    let server = mockito::Server::new_async().await;
    // No mock registered: any HTTP request would fail to match and the test
    // would error. A zero-length read must short-circuit before any request.
    let backend = backend(&server.url());
    let out = backend.read_range(&entry(), 0, 0).await.unwrap();
    assert!(out.is_empty());
}
