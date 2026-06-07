//! Faithful-topology reproduction harness for the `WebDAV` TLS deadlock.
//!
//! The earlier `tls_repro_harness.rs` drives a standalone reqwest client and
//! never reproduces the hang. This harness recreates the *nested* production
//! topology — a reqwest TLS call awaited inside an axum request handler on the
//! daemon's shared multi-thread runtime, with an external `WebDAV` client driving
//! keep-alive requests — and bisects which workaround mitigation is load-bearing
//! by flipping one knob at a time: backend connection pooling, handler runtime
//! isolation, server `Connection: close`, worker-thread count, request
//! concurrency, and reuse of a pooled connection the remote dropped while idle.
//!
//! # Findings (2026-06, this investigation)
//!
//! Across all of the above axes — sequential and concurrent, 1–4 workers,
//! pooled and unpooled, with and without idle-connection drop-and-reuse — the
//! deadlock does **not** reproduce against a synthetic local TLS server. Every
//! configuration below passes. This matches the original investigation's note
//! that "only the real Cascade handler triggered it; a minimal repro could not
//! reproduce." The trigger requires the real googleapis.com endpoint and/or the
//! real macOS `WebDAV` client, whose behaviour (real RTT, cert validation,
//! server-initiated connection close, client connection patterns) a local
//! self-signed server does not exercise.
//!
//! A separate, sharper finding from the same investigation: the workspace
//! `reqwest` has been built with `default-features = false` and no `http2`
//! feature since the first commit, so the Drive client has always been
//! HTTP/1.1-only. The `http1_only()` call in `build_unpooled_http1_client` is
//! therefore redundant (it cannot negotiate HTTP/2 regardless), and the
//! `pooled-http2` diagnostic mode cannot actually exercise HTTP/2. The
//! load-bearing mitigation is `pool_max_idle_per_host(0)` (no idle pooled
//! connection to reuse); the other layers (runtime isolation, `Connection:
//! close`) address a separate or now-moot facet.
//!
//! The only remaining viable root-cause capture is running the daemon with
//! `CASCADE_GDRIVE_HTTP_DIAG=pooled RUST_LOG=trace` against the **real** Drive
//! endpoint and watching for the wedged request span — a human-in-the-loop step
//! requiring real Drive credentials.
//!
//! Run: `cargo test -p cascade-presenter-webdav --test tls_topology_repro -- --ignored --nocapture`

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

/// A self-signed keep-alive TLS backend. If `idle_close_ms > 0` the connection
/// is dropped when no new request arrives within that window — simulating the
/// remote (googleapis) closing idle keep-alive connections, the trigger for the
/// hyper-util reuse-a-dead-pooled-connection hang.
async fn start_tls_backend(delay_ms: u64, idle_close_ms: u64) -> u16 {
    let (certs, key) = self_signed_cert();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let accept = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = accept.accept(stream).await else {
                    return;
                };
                let mut buf = [0u8; 8192];
                // Serve requests on this keep-alive connection until it closes.
                loop {
                    let read = tls.read(&mut buf);
                    let n = if idle_close_ms > 0 {
                        match tokio::time::timeout(Duration::from_millis(idle_close_ms), read).await
                        {
                            // Idle too long, EOF, or read error → drop the
                            // connection (remote idle close).
                            Err(_) | Ok(Ok(0) | Err(_)) => return,
                            Ok(Ok(n)) => n,
                        }
                    } else {
                        match read.await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        }
                    };
                    // Crude: treat any received bytes as one complete request.
                    let _ = n;
                    if delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    }
                    let body = b"hello";
                    let response =
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                    if tls.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                    if tls.write_all(body).await.is_err() {
                        return;
                    }
                    let _ = tls.flush().await;
                }
            });
        }
    });
    port
}

/// Run an async future on a separate OS thread with its own current-thread
/// runtime — the production `run_isolated_blocking` workaround, replicated.
fn run_isolated_blocking<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = tx.send(rt.block_on(future));
    });
    rx.recv().unwrap()
}

#[derive(Clone, Copy)]
struct Knobs {
    backend_pool_idle: usize,
    isolate_handler: bool,
    axum_keepalive: bool,
    workers: usize,
    /// Number of PUTs fired concurrently in each round (1 = sequential).
    concurrency: usize,
    /// If > 0, the backend drops connections idle longer than this (ms),
    /// simulating the remote closing idle keep-alive connections.
    backend_idle_close_ms: u64,
    /// Pause (ms) between rounds, to let pooled connections go idle past the
    /// backend's idle-close window before they are reused.
    idle_pause_ms: u64,
}

/// The axum handler state: a shared backend reqwest client and the backend URL.
#[derive(Clone)]
struct HandlerState {
    backend: reqwest::Client,
    backend_url: String,
    isolate: bool,
    keepalive: bool,
}

async fn put_handler(
    axum::extract::State(st): axum::extract::State<HandlerState>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    // Consume the request body (mirrors the `WebDAV` PUT path).
    let _ = body.len();

    let backend = st.backend.clone();
    let url = st.backend_url.clone();
    // The backend call carries a body, mirroring a Drive multipart upload.
    let call = async move {
        backend
            .post(&url)
            .body(vec![b'y'; 64 * 1024])
            .send()
            .await
            .map(|r| r.status().as_u16())
    };

    // Nested backend call: directly on the axum runtime (buggy) or isolated.
    let status = if st.isolate {
        tokio::task::block_in_place(|| run_isolated_blocking(call))
    } else {
        call.await
    };

    let mut resp = axum::response::Response::new(axum::body::Body::from("ok"));
    if status.is_err() {
        *resp.status_mut() = axum::http::StatusCode::BAD_GATEWAY;
    }
    if !st.keepalive {
        resp.headers_mut().insert(
            axum::http::header::CONNECTION,
            axum::http::HeaderValue::from_static("close"),
        );
    }
    resp
}

/// Start the axum server (the `WebDAV` analogue) on the CURRENT runtime; return its port.
async fn start_axum_server(state: HandlerState) -> u16 {
    let app = axum::Router::new()
        .route("/put", axum::routing::put(put_handler))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

/// The full nested scenario, run on a multi-thread runtime with `k.workers`.
/// The external `WebDAV` client runs on its OWN thread + runtime (mirroring
/// Finder being a separate process), driving `requests` sequential keep-alive
/// PUTs. Returns Err if any PUT stalls past `per_req`.
fn run_scenario(k: Knobs, requests: usize, per_req: Duration) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(k.workers)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let backend_port = start_tls_backend(150, k.backend_idle_close_ms).await;
        let backend = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .pool_max_idle_per_host(k.backend_pool_idle)
            .http1_only()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        let state = HandlerState {
            backend,
            backend_url: format!("https://localhost:{backend_port}/"),
            isolate: k.isolate_handler,
            keepalive: k.axum_keepalive,
        };
        let axum_port = start_axum_server(state).await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        // External client on its own thread + single-thread runtime, reusing one
        // keep-alive connection for all sequential PUTs.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let crt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let res = crt.block_on(async move {
                let client = reqwest::Client::builder()
                    .http1_only()
                    .pool_max_idle_per_host(10)
                    .build()
                    .unwrap();
                let url = format!("http://localhost:{axum_port}/put");
                for round in 0..requests {
                    // Let pooled connections go idle (and be dropped by the
                    // backend) before reusing them — the reuse-dead-connection
                    // window.
                    if round > 0 && k.idle_pause_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(k.idle_pause_ms)).await;
                    }
                    // Fire `concurrency` PUTs at once (Finder uploads in parallel).
                    let mut handles = Vec::new();
                    for _ in 0..k.concurrency {
                        let c = client.clone();
                        let u = url.clone();
                        handles.push(tokio::spawn(async move {
                            c.put(&u).body(vec![b'x'; 4096]).send().await.map(|_| ())
                        }));
                    }
                    for (j, h) in handles.into_iter().enumerate() {
                        match tokio::time::timeout(per_req, h).await {
                            Err(_) => {
                                return Err(format!(
                                    "round {round} PUT {j} STALLED (> {per_req:?})"
                                ));
                            }
                            Ok(Err(e)) => return Err(format!("round {round} PUT {j} join: {e}")),
                            Ok(Ok(Err(e))) => {
                                return Err(format!("round {round} PUT {j} errored: {e}"));
                            }
                            Ok(Ok(Ok(()))) => {}
                        }
                    }
                }
                Ok(())
            });
            let _ = tx.send(res);
        });

        // Overall guard so a hang doesn't wedge the whole test binary.
        let overall = Duration::from_secs((per_req.as_secs() + 2) * requests as u64 + 5);
        tokio::task::spawn_blocking(move || rx.recv_timeout(overall))
            .await
            .unwrap()
            .unwrap_or_else(|_| Err("scenario wedged (overall timeout)".to_string()))
    })
}

#[test]
#[ignore = "diagnostic TLS topology repro — run with --ignored --nocapture"]
fn bisect_tls_deadlock_topology() {
    let requests = 6;
    let per_req = Duration::from_secs(8);

    // Baseline buggy config first, then single-variable flips towards the workaround.
    let base = Knobs {
        backend_pool_idle: 10,
        isolate_handler: false,
        axum_keepalive: true,
        workers: 4,
        concurrency: 1,
        backend_idle_close_ms: 0,
        idle_pause_ms: 0,
    };
    // Idle-reuse variant: backend drops connections idle > 200 ms; client
    // pauses 400 ms between rounds so each round reuses a now-dead pooled
    // connection — the leading hyper-util hypothesis.
    let idle = Knobs {
        backend_idle_close_ms: 200,
        idle_pause_ms: 400,
        ..base
    };
    let configs: &[(&str, Knobs)] = &[
        (
            "NESTED seq: pooled, direct await, keep-alive, 1 worker",
            Knobs { workers: 1, ..base },
        ),
        (
            "NESTED concurrent: pooled, direct, 2 workers, conc=16",
            Knobs {
                workers: 2,
                concurrency: 16,
                ..base
            },
        ),
        (
            "IDLE-REUSE: backend drops idle conns, client reuses pooled",
            idle,
        ),
        (
            "IDLE-REUSE concurrent: 4 workers, conc=8",
            Knobs {
                concurrency: 8,
                ..idle
            },
        ),
        (
            "FLIP unpooled (the load-bearing mitigation)",
            Knobs {
                backend_pool_idle: 0,
                ..idle
            },
        ),
        (
            "FLIP isolate handler runtime",
            Knobs {
                isolate_handler: true,
                ..idle
            },
        ),
        (
            "FLIP Connection: close on server",
            Knobs {
                axum_keepalive: false,
                ..idle
            },
        ),
        (
            "WORKAROUND: unpooled + isolate + close",
            Knobs {
                backend_pool_idle: 0,
                isolate_handler: true,
                axum_keepalive: false,
                ..idle
            },
        ),
    ];

    println!("\n================ TLS DEADLOCK TOPOLOGY BISECTION ================");
    for (label, k) in configs {
        let res = run_scenario(*k, requests, per_req);
        let verdict = match &res {
            Ok(()) => "PASS (all requests completed)".to_string(),
            Err(e) => format!("HANG/FAIL -> {e}"),
        };
        println!(
            "[{:>4}] {label}\n        => {verdict}",
            if res.is_ok() { "ok" } else { "HANG" }
        );
    }
    println!("================================================================\n");
}
