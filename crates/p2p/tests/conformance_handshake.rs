#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! Conformance-vector harness for capability negotiation.
//!
//! Loads `docs/conformance/handshake.v1.json` — the language-neutral fixtures
//! every implementation's CI runs — and asserts this node negotiates the same
//! capability set, and refuses the same inbound frame types, that the vectors
//! prescribe for each (local domains, peer domains) pair. Two implementations
//! that both pass these vectors agree on the heterogeneous-peer rules: a domain
//! is usable only when both ends advertise it, and a frame for a domain the peer
//! did not advertise is refused rather than honoured.

use cascade_p2p::protocol::{CapabilityDomain, negotiate_domains};

/// Parse a list of domain wire identifiers, failing loudly on an unknown one so
/// a malformed vector is a test failure rather than a silent drop.
fn parse_domains(value: &serde_json::Value, field: &str, vector: &str) -> Vec<CapabilityDomain> {
    value[field]
        .as_array()
        .unwrap_or_else(|| panic!("vector {vector}: {field} must be an array"))
        .iter()
        .map(|d| {
            let wire = d
                .as_str()
                .unwrap_or_else(|| panic!("vector {vector}: {field} entries must be strings"));
            CapabilityDomain::from_wire(wire)
                .unwrap_or_else(|| panic!("vector {vector}: unknown domain {wire} in {field}"))
        })
        .collect()
}

/// The full frame family this protocol version maps to each domain, named by the
/// frozen wire identifier. Drives the refusal check: a frame whose domain is not
/// in the negotiated set must be refused from this peer.
fn refused_domains(negotiated: &[CapabilityDomain]) -> Vec<&'static str> {
    [
        CapabilityDomain::Content,
        CapabilityDomain::Management,
        CapabilityDomain::Exec,
        CapabilityDomain::Oplog,
    ]
    .into_iter()
    .filter(|domain| !negotiated.contains(domain))
    .map(CapabilityDomain::as_wire)
    .collect()
}

#[test]
fn handshake_vectors_negotiate_and_refuse_as_prescribed() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/conformance/handshake.v1.json"
    );
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading conformance vectors at {path}: {e}"));
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("parse handshake.v1.json");

    assert_eq!(
        doc["protocol_version"].as_u64(),
        Some(1),
        "the vector file must declare protocol version 1",
    );

    let vectors = doc["vectors"].as_array().expect("vectors array");
    assert!(!vectors.is_empty(), "the vector file must carry vectors");

    for vector in vectors {
        let name = vector["name"].as_str().expect("vector name");
        let local = parse_domains(vector, "local_domains", name);
        let peer = parse_domains(vector, "peer_domains", name);
        let expected_negotiated = parse_domains(vector, "expected_negotiated", name);

        let negotiated = negotiate_domains(&local, &peer);
        assert_eq!(
            negotiated, expected_negotiated,
            "vector {name}: negotiated set must match the prescribed expected_negotiated",
        );

        // The refusal set the vector prescribes: domains whose frames the local
        // node must reject from this peer because they were not negotiated.
        let expected_refusals: Vec<&str> = vector["expected_refusals"]
            .as_array()
            .unwrap_or_else(|| panic!("vector {name}: expected_refusals must be an array"))
            .iter()
            .map(|d| {
                d.as_str()
                    .unwrap_or_else(|| panic!("vector {name}: expected_refusals must be strings"))
            })
            .collect();
        let mut computed_refusals = refused_domains(&negotiated);
        computed_refusals.sort_unstable();
        let mut expected_sorted = expected_refusals.clone();
        expected_sorted.sort_unstable();
        assert_eq!(
            computed_refusals, expected_sorted,
            "vector {name}: the domains whose frames are refused must match expected_refusals",
        );
    }
}
