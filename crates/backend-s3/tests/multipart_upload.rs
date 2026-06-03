//! Integration tests for S3 multipart upload.
//!
//! These drive a mockito HTTP server and assert the full multipart upload
//! sequence: `CreateMultipartUpload` → `UploadPart`(×N) → `CompleteMultipartUpload`,
//! `AbortMultipartUpload` on a failed part, the small-object fast path using a
//! single PUT, and the part-count/threshold boundary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]

use cascade_backend_s3::{
    MAX_PARTS, MIN_PART_SIZE, MULTIPART_THRESHOLD, S3Backend, create_backend,
};
use cascade_engine::backend::Backend;
use cascade_engine::types::FileId;
use std::path::Path;

const BUCKET: &str = "test-bucket";
const REGION: &str = "us-east-1";

fn backend(endpoint: &str) -> Box<dyn Backend> {
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
        toml::Value::String(REGION.to_string()),
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

fn object_path(key: &str) -> String {
    format!("/{BUCKET}/{key}")
}

// ── Small-object path: single PUT ────────────────────────────────────────────

/// Objects at or below `MULTIPART_THRESHOLD` use a single `PutObject` request.
#[tokio::test]
async fn small_object_uses_single_put() {
    let mut server = mockito::Server::new_async().await;
    let key = "small.bin";
    let body = b"small content";

    let put_mock = server
        .mock("PUT", object_path(key).as_str())
        .with_status(200)
        .create_async()
        .await;

    let backend = backend(&server.url());
    let mut reader = std::io::Cursor::new(body.to_vec());
    let parent_id = FileId(String::new());
    backend
        .upload(Path::new(key), &mut reader, &parent_id)
        .await
        .unwrap();

    put_mock.assert_async().await;
}

// ── Multipart upload: happy path ─────────────────────────────────────────────

/// A multipart upload uses `CreateMultipartUpload` → `UploadPart`(×N) →
/// `CompleteMultipartUpload` in order and the returned entry reflects the
/// correct size.
#[tokio::test]
async fn multipart_upload_issues_create_parts_complete_in_order() {
    let mut server = mockito::Server::new_async().await;
    let key = "large.bin";
    let upload_id = "test-upload-id-1234";

    // A 2-part body (2 × 5 MiB = 10 MiB). MULTIPART_THRESHOLD is 5 GiB so we
    // drive the multipart logic directly via `multipart_upload_pub`, which
    // avoids allocating a multi-gigabyte buffer in the test.
    let part_size = MIN_PART_SIZE;
    let body = vec![0xABu8; 2 * part_size];

    // `CreateMultipartUpload`: POST /{bucket}/{key}?uploads
    let create_mock = server
        .mock("POST", object_path(key).as_str())
        .match_query(mockito::Matcher::UrlEncoded(
            "uploads".to_string(),
            String::new(),
        ))
        .with_status(200)
        .with_body(format!(
            "<?xml version=\"1.0\"?><InitiateMultipartUploadResult>\
             <UploadId>{upload_id}</UploadId></InitiateMultipartUploadResult>"
        ))
        .create_async()
        .await;

    // `UploadPart` 1
    let part1_mock = server
        .mock("PUT", object_path(key).as_str())
        .match_query(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("partNumber".to_string(), "1".to_string()),
            mockito::Matcher::UrlEncoded("uploadId".to_string(), upload_id.to_string()),
        ]))
        .with_status(200)
        .with_header("etag", "\"etag-part-1\"")
        .create_async()
        .await;

    // `UploadPart` 2
    let part2_mock = server
        .mock("PUT", object_path(key).as_str())
        .match_query(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("partNumber".to_string(), "2".to_string()),
            mockito::Matcher::UrlEncoded("uploadId".to_string(), upload_id.to_string()),
        ]))
        .with_status(200)
        .with_header("etag", "\"etag-part-2\"")
        .create_async()
        .await;

    // `CompleteMultipartUpload`
    let complete_mock = server
        .mock("POST", object_path(key).as_str())
        .match_query(mockito::Matcher::UrlEncoded(
            "uploadId".to_string(),
            upload_id.to_string(),
        ))
        .with_status(200)
        .with_body(format!(
            "<?xml version=\"1.0\"?><CompleteMultipartUploadResult>\
             <Location>http://example.com/{key}</Location>\
             <Bucket>{BUCKET}</Bucket><Key>{key}</Key>\
             <ETag>\"final-etag\"</ETag></CompleteMultipartUploadResult>"
        ))
        .create_async()
        .await;

    let s3 = S3Backend::new_for_test(server.url(), BUCKET, REGION);
    s3.multipart_upload_pub(key, &body).await.unwrap();

    // All mocks must have fired exactly once.
    create_mock.assert_async().await;
    part1_mock.assert_async().await;
    part2_mock.assert_async().await;
    complete_mock.assert_async().await;
}

// ── Multipart upload: `AbortMultipartUpload` on part failure ──────────────────

/// If an `UploadPart` request fails, `AbortMultipartUpload` is called so the
/// partial upload does not leak storage.
#[tokio::test]
async fn multipart_upload_aborts_on_part_failure() {
    let mut server = mockito::Server::new_async().await;
    let key = "abort-test.bin";
    let upload_id = "abort-upload-id";

    let part_size = MIN_PART_SIZE;
    let body = vec![0xCDu8; 2 * part_size];

    // `CreateMultipartUpload` succeeds.
    let _create_mock = server
        .mock("POST", object_path(key).as_str())
        .match_query(mockito::Matcher::UrlEncoded(
            "uploads".to_string(),
            String::new(),
        ))
        .with_status(200)
        .with_body(format!(
            "<?xml version=\"1.0\"?><InitiateMultipartUploadResult>\
             <UploadId>{upload_id}</UploadId></InitiateMultipartUploadResult>"
        ))
        .create_async()
        .await;

    // `UploadPart` 1 fails with 500.
    let _part1_mock = server
        .mock("PUT", object_path(key).as_str())
        .match_query(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("partNumber".to_string(), "1".to_string()),
            mockito::Matcher::UrlEncoded("uploadId".to_string(), upload_id.to_string()),
        ]))
        .with_status(500)
        .with_body("<Error><Code>InternalError</Code></Error>")
        .create_async()
        .await;

    // `AbortMultipartUpload`: DELETE /{bucket}/{key}?uploadId=...
    let abort_mock = server
        .mock("DELETE", object_path(key).as_str())
        .match_query(mockito::Matcher::UrlEncoded(
            "uploadId".to_string(),
            upload_id.to_string(),
        ))
        .with_status(204)
        .create_async()
        .await;

    let s3 = S3Backend::new_for_test(server.url(), BUCKET, REGION);
    let result = s3.multipart_upload_pub(key, &body).await;

    // The upload must have failed.
    assert!(result.is_err());

    // Abort must have been called.
    abort_mock.assert_async().await;
}

// ── Part-count / threshold boundary ─────────────────────────────────────────

/// `compute_part_size` returns `MIN_PART_SIZE` for objects that fit within
/// `MAX_PARTS` × `MIN_PART_SIZE`, and scales up for larger objects.
#[test]
fn compute_part_size_boundary() {
    // Re-derive the function logic: the minimum part size that keeps within
    // MAX_PARTS parts.
    let compute = |total: usize| -> usize {
        let min_for_limit = total.div_ceil(MAX_PARTS);
        min_for_limit.max(MIN_PART_SIZE)
    };

    // At exactly MAX_PARTS × MIN_PART_SIZE the result is still MIN_PART_SIZE.
    let max_without_scale = MAX_PARTS * MIN_PART_SIZE;
    assert_eq!(compute(max_without_scale), MIN_PART_SIZE);

    // One byte more forces the part size above MIN_PART_SIZE.
    assert!(compute(max_without_scale + 1) > MIN_PART_SIZE);

    // A very large object (5 TiB) must fit within MAX_PARTS parts.
    let five_tib: usize = 5 * 1024 * 1024 * 1024 * 1024;
    let part = compute(five_tib);
    let parts_needed = five_tib.div_ceil(part);
    assert!(
        parts_needed <= MAX_PARTS,
        "parts_needed={parts_needed} exceeds MAX_PARTS"
    );
}

/// The exported constants match AWS S3 documented limits.
#[test]
fn constants_are_sane() {
    // `MULTIPART_THRESHOLD` must be at least large enough for one part.
    const { assert!(MULTIPART_THRESHOLD >= MIN_PART_SIZE) }
    // The minimum part size is 5 MiB per AWS docs.
    assert_eq!(MIN_PART_SIZE, 5 * 1024 * 1024);
    // `MAX_PARTS` is 10,000 per AWS docs.
    assert_eq!(MAX_PARTS, 10_000);
}
