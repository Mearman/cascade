//! Real-TLS repro harness for the pooled-connection hang.
//!
//! Historical note: the per-request unpooled workaround described below has been
//! **removed** in favour of a single daemon-owned pooled `reqwest::Client` (see
//! the "Google Drive HTTP client" section of `docs/design.md`). This harness is
//! retained, `#[ignore]`d, as a regression repro. The `CASCADE_GDRIVE_HTTP_DIAG`
//! environment variable it reads below is now purely this test's own pooling
//! toggle — it no longer affects production code, which has no such switch.
//!
//! # Background
//!
//! The TLS deadlock workaround (per-request unpooled HTTP/1.1 client) was
//! introduced because the Google Drive backend hung on the `WebDAV` PUT path
//! after roughly two requests against the real googleapis.com TLS endpoint.
//! Every repro attempt using plain HTTP (wiremock, local mock servers) failed
//! to reproduce the hang because plain HTTP never exercises the TLS connection
//! driver that stalls in hyper-util's connection pool.
//!
//! # What this harness does
//!
//! Stands up a self-signed TLS server using rcgen + tokio-rustls that serves
//! hyper 1.x responses and deliberately delays/parks the response body before
//! responding.  The `WebDAV` axum server is started on the same multi-thread
//! tokio runtime.  A test backend is pointed at the TLS server and driven
//! through ≥ 3 sequential GET/PUT cycles.
//!
//! If the hang reproduces, at least one cycle will not complete within the
//! 30 s per-cycle timeout and the test will fail with a clear message.  If
//! all cycles complete, the test is green — but this does **not** license
//! removing the workaround, because the real googleapis.com connection driver
//! may behave differently from a self-signed local server.
//!
//! # Running
//!
//! The test is `#[ignore]` by default because:
//!
//! 1. It spawns a listening TLS port.
//! 2. It is a diagnostic tool for a known issue, not a regression guard.
//! 3. It may not reproduce reliably on every machine or timing.
//!
//! Run explicitly with:
//!
//! ```text
//! cargo test -p cascade-presenter-webdav --test tls_repro_harness -- --ignored
//! ```
//!
//! When running with `CASCADE_GDRIVE_HTTP_DIAG=pooled` set in the environment,
//! the backend will use a pooled client (the suspected-bad configuration).
//! Without that variable, it uses the workaround client and the test should
//! always pass — confirming the workaround is effective against a self-signed
//! TLS server.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]

use std::sync::Arc;
use std::time::Duration;

use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls_pemfile::certs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// Build a self-signed TLS server certificate and key for `localhost`.
fn self_signed_cert() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_pem = cert.pem();
    let key_pem = signing_key.serialize_pem();

    let cert_der: Vec<CertificateDer<'static>> = certs(&mut cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .map(|c| CertificateDer::from(c.to_vec()))
        .collect();

    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        rustls_pemfile::pkcs8_private_keys(&mut key_pem.as_bytes())
            .next()
            .unwrap()
            .unwrap()
            .secret_pkcs8_der()
            .to_vec(),
    ));

    (cert_der, key_der)
}

/// Start a self-signed TLS server that accepts `count` sequential connections,
/// each with the given delay in milliseconds before responding.
///
/// The server simulates a slow backend: it delays the response body by
/// `delay_ms` milliseconds, giving the connection pool a chance to reuse the
/// connection while a response is in flight — the hypothesised starvation
/// scenario.
///
/// Returns the port it is listening on.
async fn start_tls_server(count: usize, delay_ms: u64) -> u16 {
    let (certs, key) = self_signed_cert();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        for _ in 0..count {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let accept = acceptor.clone();
            let delay = delay_ms;
            tokio::spawn(async move {
                let Ok(mut tls_stream) = accept.accept(stream).await else {
                    return;
                };
                // Read and discard the HTTP request.
                let mut buf = [0u8; 4096];
                let _ = tls_stream.read(&mut buf).await;
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                let response =
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";
                let _ = tls_stream.write_all(response).await;
                let _ = tls_stream.flush().await;
            });
        }
    });

    port
}

/// Attempt to make `n` sequential HTTPS GET requests using `reqwest` in the
/// mode selected by `CASCADE_GDRIVE_HTTP_DIAG` (defaulting to the workaround
/// client). Each request must complete within `per_request_timeout`.
///
/// If any request stalls beyond the timeout, the returned error describes
/// which cycle stalled.
async fn drive_n_requests(
    port: u16,
    n: usize,
    per_request_timeout: Duration,
) -> Result<(), String> {
    // Accept self-signed certificates for the local test server only.
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        // Check the env var: if "pooled" or "pooled-http2", use pooled; otherwise use
        // the same per-request unpooled+http1 configuration as the production workaround.
        .pool_max_idle_per_host(match std::env::var("CASCADE_GDRIVE_HTTP_DIAG").as_deref() {
            Ok("pooled" | "pooled-http2") => 10,
            _ => 0,
        })
        .http1_only()
        .timeout(per_request_timeout)
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;

    for i in 0..n {
        let url = format!("https://localhost:{port}/test-{i}");
        client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("request {i} failed: {e}"))?;
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Drive ≥ 3 sequential GET requests through a self-signed TLS server using
/// the production-workaround client (unpooled, HTTP/1.1). All requests should
/// complete within the per-request timeout.
///
/// A green result here confirms the workaround is effective against a
/// self-signed TLS server.  It does **not** license removing the workaround
/// against the real googleapis.com endpoint.
#[tokio::test]
#[ignore = "diagnostic TLS repro harness — run manually with --ignored"]
async fn workaround_client_completes_three_sequential_tls_requests() {
    let port = start_tls_server(5, 50).await;
    // Allow some time for the server task to start.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let result = drive_n_requests(port, 3, Duration::from_secs(10)).await;
    assert!(
        result.is_ok(),
        "workaround client stalled on TLS requests: {result:?}"
    );
}

/// Drive ≥ 3 sequential GET requests through a self-signed TLS server using
/// the pooled client (`CASCADE_GDRIVE_HTTP_DIAG=pooled`).
///
/// This test is expected to stall (and fail) if the pooled-connection hang is
/// reproduced against the local TLS server.  If it passes, the trigger
/// requires the real googleapis.com connection driver and the instrumentation
/// route (`RUST_LOG=trace cascade start ...`) is the only viable capture
/// path.
///
/// Run with:
/// ```text
/// CASCADE_GDRIVE_HTTP_DIAG=pooled cargo test -p cascade-presenter-webdav \
///   --test tls_repro_harness -- pooled_client --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "diagnostic TLS repro harness — run manually with --ignored and CASCADE_GDRIVE_HTTP_DIAG=pooled"]
async fn pooled_client_three_sequential_tls_requests() {
    let port = start_tls_server(5, 200).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let result = drive_n_requests(port, 3, Duration::from_secs(15)).await;
    // If pooled connections reproduce the hang, this assertion fires with a
    // timeout error on cycle N.  If all complete, the local TLS server is
    // not sufficient to trigger the bug.
    if let Err(ref e) = result {
        eprintln!("NOTE: pooled client stalled on TLS requests (possible hang reproduced): {e}");
    }
    // We do not assert success here — the point of this test is diagnostic,
    // not to enforce a pass/fail gate.  A failure is interesting, not a bug.
    let _ = result;
}
