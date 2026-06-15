#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! Conformance-vector harness for the frozen wire frames.
//!
//! Loads `docs/conformance/frames.v1.json` — the language-neutral, byte-exact
//! frame fixtures every implementation's CI runs — and asserts this codec
//! decodes each `wire_hex` and re-encodes it to exactly the same bytes. Two
//! implementations that both pass these vectors stay wire-compatible; the
//! documentation alone would not guarantee that. The same JSON is consumed by an
//! external peer implementation, so a drift in either codec is caught here.

use cascade_p2p::protocol::{decode_message, encode_message};

/// Decode a lowercase hex string into bytes.
fn from_hex(s: &str) -> Vec<u8> {
    assert!(
        s.len().is_multiple_of(2),
        "hex string must have even length: {s}"
    );
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex byte"))
        .collect()
}

#[test]
fn frame_vectors_round_trip_byte_for_byte() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/conformance/frames.v1.json"
    );
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading conformance vectors at {path}: {e}"));
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("parse frames.v1.json");

    assert_eq!(
        doc["protocol_version"].as_u64(),
        Some(1),
        "the vector file must declare protocol version 1",
    );

    let vectors = doc["vectors"].as_array().expect("vectors array");
    assert!(!vectors.is_empty(), "the vector file must carry vectors");

    for vector in vectors {
        let name = vector["name"].as_str().expect("vector name");
        let wire_hex = vector["wire_hex"].as_str().expect("wire_hex");
        let frame = from_hex(wire_hex);

        // The codec must decode the frozen bytes...
        let decoded = decode_message(&frame)
            .unwrap_or_else(|e| panic!("vector {name}: wire_hex must decode: {e}"));

        // ...and re-encode the decoded message to exactly the same bytes. This is
        // the byte-exact contract that keeps two implementations from drifting.
        let re_encoded = encode_message(&decoded)
            .unwrap_or_else(|e| panic!("vector {name}: decoded message must encode: {e}"));

        assert_eq!(
            re_encoded, frame,
            "vector {name}: re-encoding the decoded message must reproduce wire_hex exactly",
        );
    }
}
