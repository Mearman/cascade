#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! Conformance-vector harness for capability-token verification.
//!
//! Loads `docs/conformance/tokens.v1.json` — the language-neutral token fixtures
//! every implementation's CI runs — and asserts Cascade's verifier reaches the
//! prescribed decision for each token under the prescribed inputs (verifying
//! node, connected device, wall clock, revocation set). Each vector carries a
//! fully materialised token: the issuer certificate and signature travel inside
//! the token JSON, so verification is deterministic and reproducible across
//! languages without sharing any private key. A second implementation runs the
//! same JSON through its own verifier; a divergence in either verifier's verdict
//! is caught here, which is what keeps the two from drifting on the security-
//! critical authorisation path.
//!
//! The vectors are generated once by `generate_token_vectors` (an ignored test
//! in `crates/engine/src/manage/token.rs`) and committed. This harness only
//! reads them; it never regenerates, because regenerating would mint fresh keys
//! and mask a real drift behind a moving target.

#![cfg(feature = "p2p")]

use cascade_engine::manage::{CapabilityToken, DeviceId, TokenVerifyError};
use chrono::DateTime;

/// Map a vector's `expected` field to the discriminant name this harness
/// compares against, so a verdict mismatch names the divergence precisely.
const fn error_discriminant(err: &TokenVerifyError) -> &'static str {
    match err {
        TokenVerifyError::BadSignature { .. } => "bad_signature",
        TokenVerifyError::WrongIssuer { .. } => "wrong_issuer",
        TokenVerifyError::BearerMismatch { .. } => "bearer_mismatch",
        TokenVerifyError::Expired { .. } => "expired",
        TokenVerifyError::Revoked { .. } => "revoked",
        TokenVerifyError::DelegationExceedsParent { .. } => "delegation_exceeds_parent",
        TokenVerifyError::ParentInvalid { .. } => "parent_invalid",
        TokenVerifyError::ChainTooDeep { .. } => "chain_too_deep",
    }
}

#[test]
fn token_vectors_verify_to_the_prescribed_decision() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/conformance/tokens.v1.json"
    );
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading conformance vectors at {path}: {e}"));
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("parse tokens.v1.json");

    assert_eq!(
        doc["protocol_version"].as_u64(),
        Some(1),
        "the vector file must declare protocol version 1",
    );

    let vectors = doc["vectors"].as_array().expect("vectors array");
    assert!(!vectors.is_empty(), "the vector file must carry vectors");

    for vector in vectors {
        let name = vector["name"].as_str().expect("vector name");

        let token: CapabilityToken = serde_json::from_value(vector["token_json"].clone())
            .unwrap_or_else(|e| panic!("vector {name}: token_json must deserialise: {e}"));

        let verifying_node = DeviceId::new(
            vector["verifying_node"]
                .as_str()
                .unwrap_or_else(|| panic!("vector {name}: verifying_node must be a string")),
        );
        let connected_device = DeviceId::new(
            vector["connected_device"]
                .as_str()
                .unwrap_or_else(|| panic!("vector {name}: connected_device must be a string")),
        );
        let now_unix_ms = vector["now_unix_ms"]
            .as_i64()
            .unwrap_or_else(|| panic!("vector {name}: now_unix_ms must be an integer"));
        let now = DateTime::from_timestamp_millis(now_unix_ms)
            .unwrap_or_else(|| panic!("vector {name}: now_unix_ms out of range"));

        let revoked: Vec<String> = vector["revoked_ids"]
            .as_array()
            .unwrap_or_else(|| panic!("vector {name}: revoked_ids must be an array"))
            .iter()
            .map(|v| {
                v.as_str()
                    .unwrap_or_else(|| panic!("vector {name}: revoked_ids must be strings"))
                    .to_owned()
            })
            .collect();
        let is_revoked = |id: &str| revoked.iter().any(|r| r == id);

        let result = token.verify(&verifying_node, &connected_device, now, &is_revoked);

        match &vector["expected"] {
            serde_json::Value::String(s) if s == "ok" => {
                let claims = result.unwrap_or_else(|e| {
                    panic!("vector {name}: expected ok but verification failed: {e}")
                });
                // The verified leaf claims must be the ones the token names; a
                // verifier that accepted a different grant would be a silent
                // authority bug.
                assert_eq!(
                    claims.bearer, connected_device,
                    "vector {name}: verified bearer must equal the connected device",
                );
            }
            serde_json::Value::Object(obj) => {
                let expected_err = obj["err"]
                    .as_str()
                    .unwrap_or_else(|| panic!("vector {name}: expected.err must be a string"));
                let err = result.err().unwrap_or_else(|| {
                    panic!(
                        "vector {name}: expected error {expected_err} but verification succeeded"
                    )
                });
                assert_eq!(
                    error_discriminant(&err),
                    expected_err,
                    "vector {name}: verification failed with the wrong error kind",
                );
            }
            other => panic!("vector {name}: expected must be \"ok\" or {{err}}, got {other}"),
        }
    }
}
