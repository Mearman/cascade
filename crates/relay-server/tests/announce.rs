//! End-to-end announce directory test.
//!
//! Drives the real [`cascade_p2p::discovery::announce::AnnounceDiscovery`]
//! client against the relay-server's announce endpoint over actual HTTP: the
//! client registers a candidate set with its write-auth header, the endpoint
//! verifies the `HMAC` over the wire, then the client looks the set up by
//! device id and gets it back. This is the cross-crate proof that the producer
//! (the client) and the consumer (the endpoint) agree on the write-auth
//! contract — neither a mock that ignores auth nor a hand-built tag. Compiled
//! only when the `announce` feature is enabled, since the endpoint itself is
//! feature-gated.

#![cfg(feature = "announce")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;

use cascade_p2p::candidate::{Candidate, CandidateKind};
use cascade_p2p::discovery::Discovery;
use cascade_p2p::discovery::announce::{AnnounceDiscovery, SHARED_SECRET_LEN};
use cascade_relay_server::announce::{AnnounceDirectory, serve_announce};

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Deterministic 32-byte shared secret the client and the endpoint agree on.
fn secret() -> [u8; SHARED_SECRET_LEN] {
    let mut s = [0u8; SHARED_SECRET_LEN];
    for (idx, byte) in s.iter_mut().enumerate() {
        *byte = u8::try_from(idx).unwrap_or(0);
    }
    s
}

#[tokio::test]
async fn announce_then_lookup_round_trips_over_http() {
    let directory = AnnounceDirectory::new();
    let (bound, _server) = serve_announce(addr(0), directory, secret())
        .await
        .expect("binding announce endpoint");

    let client =
        AnnounceDiscovery::new(format!("http://{bound}"), secret()).expect("announce client");

    let host = Candidate::new(addr(22000), CandidateKind::Host, 65_535);
    let srflx = Candidate::new(addr(33000), CandidateKind::ServerReflexive, 0);
    client.announce("DEVICE-A", &[host, srflx]).await;

    let resolved = client.resolve("DEVICE-A").await;
    assert_eq!(resolved.len(), 2);
    assert!(resolved.contains(&host));
    assert!(resolved.contains(&srflx));
}

#[tokio::test]
async fn lookup_unknown_device_over_http_is_empty() {
    let directory = AnnounceDirectory::new();
    let (bound, _server) = serve_announce(addr(0), directory, secret())
        .await
        .expect("binding announce endpoint");

    let client =
        AnnounceDiscovery::new(format!("http://{bound}"), secret()).expect("announce client");
    assert!(client.resolve("NEVER-REGISTERED").await.is_empty());
}

#[tokio::test]
async fn a_client_with_the_wrong_secret_cannot_register() {
    // The endpoint holds one secret; a client holding a different one produces a
    // write tag that the endpoint rejects (401). The directory stays empty, so a
    // lookup with the right secret resolves nothing. This is exactly the
    // silent-empty-directory failure that a producer/consumer mismatch would
    // cause — here it is the *wrong* secret that fails, not a missing header.
    let directory = AnnounceDirectory::new();
    let (bound, _server) = serve_announce(addr(0), directory, secret())
        .await
        .expect("binding announce endpoint");

    let mut wrong = secret();
    wrong[0] ^= 0xFF;
    let writer = AnnounceDiscovery::new(format!("http://{bound}"), wrong).expect("announce client");
    let host = Candidate::new(addr(22000), CandidateKind::Host, 1);
    writer.announce("DEVICE-A", &[host]).await;

    let reader =
        AnnounceDiscovery::new(format!("http://{bound}"), secret()).expect("announce client");
    assert!(reader.resolve("DEVICE-A").await.is_empty());
}
