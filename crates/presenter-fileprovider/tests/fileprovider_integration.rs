//! Integration tests for the macOS File Provider presenter.
//!
//! Exercises the bridge protocol, VfsItem↔FileProviderItem round-trips,
//! request building, and error handling. macOS-specific socket operations
//! are gated behind #[cfg(target_os = "macos")].

use cascade_engine::protocol::{Request, Response, decode_message, encode_message};
use cascade_engine::types::{CacheState, ItemId, VfsItem};
use cascade_presenter_fileprovider::bridge::FileProviderBridge;
use cascade_presenter_fileprovider::items::FileProviderItem;
use chrono::{TimeZone, Utc};
use serde_json::json;

// ---------------------------------------------------------------------------
// Task 2: Bridge round-trip tests
// ---------------------------------------------------------------------------

#[test]
fn bridge_builds_valid_protocol_request() {
    let bridge = FileProviderBridge::new("/tmp/cascade-test.sock");
    let request = bridge.build_request("upsertItem", json!({"id": "gdrive:abc"}));

    assert_eq!(request.method, "upsertItem");
    assert_eq!(request.params, json!({"id": "gdrive:abc"}));
}

#[test]
fn bridge_request_ids_monotonically_increase() {
    let bridge = FileProviderBridge::new("/tmp/cascade-test.sock");
    let r1 = bridge.build_request("method1", json!({}));
    let r2 = bridge.build_request("method2", json!({}));
    let r3 = bridge.build_request("method3", json!({}));

    assert!(r1.id < r2.id);
    assert!(r2.id < r3.id);
}

#[test]
fn bridge_request_encodes_with_length_prefix() {
    let bridge = FileProviderBridge::new("/tmp/cascade-test.sock");
    let request = bridge.build_request("deleteItem", json!({"id": "local:test"}));
    let encoded = encode_message(&request).unwrap();

    // First 4 bytes are big-endian length.
    assert!(encoded.len() > 4);
    let len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
    assert_eq!(encoded.len(), 4 + len as usize);

    // Decode round-trips.
    let (consumed, decoded): (usize, Request) = decode_message(&encoded).unwrap().unwrap();
    assert_eq!(consumed, encoded.len());
    assert_eq!(decoded.id, request.id);
    assert_eq!(decoded.method, "deleteItem");
    assert_eq!(decoded.params, json!({"id": "local:test"}));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn bridge_round_trips_over_unix_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    let socket_path = std::env::temp_dir().join(format!(
        "cascade-fp-integration-{}-roundtrip.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).unwrap();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.unwrap();
        let body_len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; body_len];
        stream.read_exact(&mut body).await.unwrap();

        let request: Request = serde_json::from_slice(&body).unwrap();
        assert_eq!(request.method, "upsertItem");

        let response = Response::ok(request.id, json!({"status": "ok"}));
        let encoded = encode_message(&response).unwrap();
        stream.write_all(&encoded).await.unwrap();
    });

    let bridge = FileProviderBridge::new(&socket_path);
    let result: serde_json::Value = bridge
        .request("upsertItem", json!({"item": {"id": "test:file1"}}))
        .await
        .unwrap();

    assert_eq!(result, json!({"status": "ok"}));
    server.await.unwrap();
    std::fs::remove_file(&socket_path).unwrap();
}

// ---------------------------------------------------------------------------
// VfsItem ↔ FileProviderItem round-trip
// ---------------------------------------------------------------------------

fn sample_vfs_item() -> VfsItem {
    VfsItem {
        id: ItemId::new("gdrive", "file123"),
        parent_id: ItemId::new("gdrive", "root"),
        name: "report.pdf".to_string(),
        is_dir: false,
        size: Some(8192),
        mod_time: Some(Utc.with_ymd_and_hms(2026, 5, 28, 10, 30, 0).unwrap()),
        cache_state: CacheState::Cached,
        mime_type: Some("application/pdf".to_string()),
    }
}

#[test]
fn vfs_item_to_file_provider_item_preserves_fields() {
    let item = sample_vfs_item();
    let fp_item = FileProviderItem::from(item);

    assert_eq!(fp_item.id, "gdrive:file123");
    assert_eq!(fp_item.parent_id, "gdrive:root");
    assert_eq!(fp_item.filename, "report.pdf");
    assert!(!fp_item.is_directory);
    assert_eq!(fp_item.size, Some(8192));
    assert_eq!(fp_item.content_type, Some("application/pdf".to_string()));
    assert_eq!(fp_item.cache_state, CacheState::Cached);
    assert!(fp_item.last_modified.is_some());
}

#[test]
fn file_provider_item_round_trips_to_vfs_item() {
    let original = sample_vfs_item();
    let fp_item = FileProviderItem::from(original.clone());
    let round_tripped = VfsItem::try_from(fp_item).unwrap();

    assert_eq!(round_tripped.id, original.id);
    assert_eq!(round_tripped.parent_id, original.parent_id);
    assert_eq!(round_tripped.name, original.name);
    assert_eq!(round_tripped.is_dir, original.is_dir);
    assert_eq!(round_tripped.size, original.size);
    assert_eq!(round_tripped.mod_time, original.mod_time);
    assert_eq!(round_tripped.cache_state, original.cache_state);
    assert_eq!(round_tripped.mime_type, original.mime_type);
}

#[test]
fn directory_vfs_item_round_trips() {
    let dir_item = VfsItem {
        id: ItemId::new("local", "photos"),
        parent_id: ItemId::new("local", "/"),
        name: "photos".to_string(),
        is_dir: true,
        size: None,
        mod_time: Some(Utc.with_ymd_and_hms(2026, 1, 15, 0, 0, 0).unwrap()),
        cache_state: CacheState::Online,
        mime_type: None,
    };

    let fp_item = FileProviderItem::from(dir_item.clone());
    assert!(fp_item.is_directory);
    assert_eq!(fp_item.size, None);

    let round_tripped = VfsItem::try_from(fp_item).unwrap();
    assert!(round_tripped.is_dir);
    assert_eq!(round_tripped.name, "photos");
}

#[test]
fn file_provider_item_rejects_invalid_date() {
    let mut fp_item = FileProviderItem::from(sample_vfs_item());
    fp_item.last_modified = Some("not-a-valid-date".to_string());

    let result = VfsItem::try_from(fp_item);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("invalid File Provider modification date"),
    );
}

// ---------------------------------------------------------------------------
// Enumerator simulation via protocol
// ---------------------------------------------------------------------------

#[test]
fn enumerate_request_builds_correct_protocol_message() {
    let bridge = FileProviderBridge::new("/tmp/cascade-test.sock");
    let request = bridge.build_request(
        "enumerate",
        json!({
            "parent_id": "gdrive:root",
            "page_size": 100
        }),
    );

    assert_eq!(request.method, "enumerate");
    assert_eq!(
        request.params,
        json!({"parent_id": "gdrive:root", "page_size": 100}),
    );
}

#[test]
fn enumerate_response_decodes_children() {
    let response = Response::ok(
        1,
        json!({
            "items": [
                {"id": "gdrive:file1", "name": "a.txt", "is_dir": false},
                {"id": "gdrive:dir1", "name": "subdir", "is_dir": true},
            ]
        }),
    );

    let encoded = encode_message(&response).unwrap();
    let (_, decoded): (usize, Response) = decode_message(&encoded).unwrap().unwrap();

    let binding = decoded.result.unwrap();
    let items = binding.get("items").unwrap().as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["name"], "a.txt");
    assert_eq!(items[1]["is_dir"], true);
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

#[test]
fn protocol_error_response_decodes() {
    let response = Response::error(42, "item not found: gdrive:nonexistent");

    let encoded = encode_message(&response).unwrap();
    let (_, decoded): (usize, Response) = decode_message(&encoded).unwrap().unwrap();

    assert!(decoded.result.is_none());
    assert_eq!(
        decoded.error,
        Some("item not found: gdrive:nonexistent".to_string())
    );
    assert_eq!(decoded.id, 42);
}

#[test]
fn invalid_json_body_produces_decode_error() {
    let len = 5u32.to_be_bytes();
    let mut buf = len.to_vec();
    buf.extend_from_slice(b"{bad}");

    let result: Result<(usize, Request), _> = decode_message(&buf)
        .ok()
        .flatten()
        .map(Ok)
        .unwrap_or_else(|| Err(anyhow::anyhow!("decode failed")));
    assert!(result.is_err(), "malformed JSON should fail to decode");
}

#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn presenter_operations_fail_outside_macos() {
    use cascade_presenter_fileprovider::FileProviderPresenter;

    let presenter = FileProviderPresenter::new("/tmp/cascade-test.sock");
    let id = ItemId::new("gdrive", "file1");
    let item = VfsItem {
        id: id.clone(),
        parent_id: ItemId::new("gdrive", "root"),
        name: "test.txt".to_string(),
        is_dir: false,
        size: Some(100),
        mod_time: None,
        cache_state: CacheState::Online,
        mime_type: None,
    };

    // All presenter operations should fail outside macOS.
    assert!(presenter.upsert_item(item.clone()).await.is_err());
    assert!(presenter.delete_item(&id).await.is_err());
    assert!(presenter.start(Path::new("/mnt/cascade")).await.is_err());
    assert!(presenter.stop().await.is_err());
}

// ---------------------------------------------------------------------------
// Property tests (proptest)
// ---------------------------------------------------------------------------

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn vfs_item_round_trip_preserves_fields(
            name in "[a-zA-Z0-9_.\\-]{1,30}",
            size in any::<Option<u64>>(),
            is_dir: bool,
            backend_id in "[a-z]{1,10}",
            native_id in "[a-zA-Z0-9]{1,20}",
        ) {
            let item = VfsItem {
                id: ItemId::new(&backend_id, &native_id),
                parent_id: ItemId::new(&backend_id, "root"),
                name: name.clone(),
                is_dir,
                size,
                mod_time: None,
                cache_state: CacheState::Online,
                mime_type: None,
            };

            let fp_item = FileProviderItem::from(item.clone());
            let round_tripped = VfsItem::try_from(fp_item).unwrap();

            prop_assert_eq!(round_tripped.id, item.id);
            prop_assert_eq!(round_tripped.parent_id, item.parent_id);
            prop_assert_eq!(round_tripped.name, item.name);
            prop_assert_eq!(round_tripped.is_dir, item.is_dir);
            prop_assert_eq!(round_tripped.size, item.size);
            prop_assert_eq!(round_tripped.mime_type, item.mime_type);
        }

        #[test]
        fn item_id_bridge_encoding_round_trips(
            backend in "[a-z]{1,10}",
            native in "[a-zA-Z0-9_\\-/]{1,30}",
        ) {
            let id = ItemId::new(&backend, &native);

            // Round-trip through JSON serialisation (same path as the bridge).
            let json = serde_json::to_string(&id).unwrap();
            let decoded: ItemId = serde_json::from_str(&json).unwrap();

            prop_assert_eq!(decoded.backend_id(), backend);
            prop_assert_eq!(decoded.native_id(), native);
        }

        #[test]
        fn request_encoding_round_trips(
            method in "[a-zA-Z]{1,20}",
            id: u32,
        ) {
            let request = Request {
                id,
                method: method.clone(),
                params: json!({"test": true}),
            };

            let encoded = encode_message(&request).unwrap();
            let (consumed, decoded): (usize, Request) = decode_message(&encoded)
                .unwrap()
                .unwrap();

            prop_assert_eq!(consumed, encoded.len());
            prop_assert_eq!(decoded.id, id);
            prop_assert_eq!(decoded.method, method);
        }
    }
}
