//! End-to-end announce directory test.
//!
//! Drives the real [`cascade_p2p::discovery::announce::AnnounceDiscovery`]
//! client against the relay-server's announce endpoint over actual HTTP: the
//! client registers a candidate set, then looks it up by device id and gets
//! the same set back. Compiled only when the `announce` feature is enabled,
//! since the endpoint itself is feature-gated.

#![cfg(feature = "announce")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;

use cascade_p2p::candidate::{Candidate, CandidateKind};
use cascade_p2p::discovery::Discovery;
use cascade_p2p::discovery::announce::AnnounceDiscovery;
use cascade_relay_server::announce::{AnnounceDirectory, serve_announce};

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[tokio::test]
async fn announce_then_lookup_round_trips_over_http() {
    let directory = AnnounceDirectory::new();
    let (bound, _server) = serve_announce(addr(0), directory)
        .await
        .expect("binding announce endpoint");

    let client = AnnounceDiscovery::new(format!("http://{bound}")).expect("announce client");

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
    let (bound, _server) = serve_announce(addr(0), directory)
        .await
        .expect("binding announce endpoint");

    let client = AnnounceDiscovery::new(format!("http://{bound}")).expect("announce client");
    assert!(client.resolve("NEVER-REGISTERED").await.is_empty());
}
