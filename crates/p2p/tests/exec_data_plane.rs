#![cfg(feature = "exec")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::string_slice
)]
//! Exec data-plane integration tests.
//!
//! Spawns a real process on one side via [`cascade_exec::LocalExecProvider`],
//! streams its stdout to a peer over a BEP [`FramedSession`], and asserts the
//! three properties `exec-capability.md` mandates of the data plane: bytes
//! arrive in order and intact, a slow consumer throttles the producer
//! (backpressure), and killing the session tears the stream down cleanly.
//!
//! The peer link is an in-memory [`ChannelTransport`] implementing the public
//! [`Transport`] contract, so the test exercises the real
//! `FramedSession`/`SessionWriter`/`SessionReader` plumbing without a TLS or
//! relay socket — the same wire frames travel, just over channels.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use cascade_exec::{ExecProvider, ExecSessionId, LocalExecProvider, ProcSpec};
use cascade_p2p::exec_stream::{
    DEFAULT_CREDIT_WINDOW, ExecStreamConsumer, ExecStreamCredit, ExecStreamSink,
    pump_session_output, run_exec_stream_loop,
};
use cascade_p2p::framed::{FramedSession, SessionWriter};
use cascade_p2p::protocol::BepMessage;
use cascade_p2p::transport::{Transport, TransportReader, TransportWriter};
use tokio::sync::{Mutex, mpsc};

// ── In-memory channel transport (test double for a real peer link) ──

struct ChannelTransport {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl ChannelTransport {
    fn pair() -> (Self, Self) {
        let (a_tx, b_rx) = mpsc::unbounded_channel();
        let (b_tx, a_rx) = mpsc::unbounded_channel();
        (Self { tx: a_tx, rx: a_rx }, Self { tx: b_tx, rx: b_rx })
    }
}

impl Transport for ChannelTransport {
    type Reader = ChannelReader;
    type Writer = ChannelWriter;

    fn split(self) -> (Self::Reader, Self::Writer) {
        (ChannelReader { rx: self.rx }, ChannelWriter { tx: self.tx })
    }
}

struct ChannelReader {
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
}

#[async_trait]
impl TransportReader for ChannelReader {
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(self.rx.recv().await)
    }
}

struct ChannelWriter {
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

#[async_trait]
impl TransportWriter for ChannelWriter {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        self.tx
            .send(frame.to_vec())
            .map_err(|_| anyhow::anyhow!("channel transport closed"))
    }

    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}

// ── Consumer that records delivered bytes per stream ──

/// Records inbound stdout/stderr bytes in arrival order, and counts acks granted
/// so backpressure assertions can observe the consumer's pace.
#[derive(Clone)]
struct RecordingConsumer {
    stdout: Arc<Mutex<Vec<u8>>>,
    delivered: Arc<AtomicU64>,
}

impl RecordingConsumer {
    fn new() -> Self {
        Self {
            stdout: Arc::new(Mutex::new(Vec::new())),
            delivered: Arc::new(AtomicU64::new(0)),
        }
    }
}

#[async_trait]
impl ExecStreamConsumer for RecordingConsumer {
    async fn on_bytes(&mut self, _session: ExecSessionId, stream: u8, bytes: &[u8]) -> Result<()> {
        // 1 == stdout per the frozen wire discriminants.
        if stream == 1 {
            self.stdout.lock().await.extend_from_slice(bytes);
        }
        self.delivered
            .fetch_add(bytes.len() as u64, Ordering::SeqCst);
        Ok(())
    }
}

/// Spawn a process, pump its stdout to a peer sink over a channel-backed BEP
/// session, and assert the bytes arrive intact and in order.
#[tokio::test]
async fn process_output_streams_to_peer_in_order() {
    let provider = LocalExecProvider::new();

    // A short script emitting deterministic, ordered output.
    let id = provider
        .proc_spawn(ProcSpec {
            argv: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "for i in $(seq 1 200); do printf 'line-%03d\\n' \"$i\"; done".to_owned(),
            ],
            cwd: None,
            env: vec![],
        })
        .await
        .unwrap();
    let events = provider
        .subscribe(id)
        .expect("subscribe yields the receiver");

    let (producer_t, consumer_t) = ChannelTransport::pair();
    let (mut p_reader, p_writer) = FramedSession::new(producer_t).split();
    let (mut c_reader, c_writer) = FramedSession::new(consumer_t).split();

    let producer_writer = Arc::new(Mutex::new(p_writer));
    let consumer_writer: Arc<Mutex<SessionWriter<_>>> = Arc::new(Mutex::new(c_writer));

    // A window deliberately smaller than the total output so the stream cannot
    // complete on the initial credit alone: completion proves ack-driven credit
    // refresh works end to end. 200 lines * 9 bytes = 1800 bytes of output.
    let window: u32 = 256;
    let credit = ExecStreamCredit::new(window);

    // Producer task: drain the session output and frame it.
    let pump = {
        let writer = Arc::clone(&producer_writer);
        let credit = Arc::clone(&credit);
        tokio::spawn(async move { pump_session_output(id, events, &writer, &credit).await })
    };

    // Producer-side ack reader: applies inbound acks to the producer's credit so
    // a consumed window is replenished. Without this the producer stalls after
    // the first `window` bytes.
    let ack_reader = {
        let credit = Arc::clone(&credit);
        tokio::spawn(async move {
            while let Ok(Some(frame)) = p_reader.recv().await {
                if let BepMessage::ExecStreamAck {
                    ack_seq, window, ..
                } = frame
                {
                    credit.apply_ack(ack_seq, window).await;
                }
            }
        })
    };

    // Consumer task: receive frames, deliver bytes, grant credit.
    let consumer = RecordingConsumer::new();
    let collected = Arc::clone(&consumer.stdout);
    let consumer_loop = {
        // The consumer's own producer-credit is unused here (it sends no output),
        // but the loop requires one.
        let unused_credit = ExecStreamCredit::new(window);
        let writer = Arc::clone(&consumer_writer);
        let mut sink = ExecStreamSink::new(id, consumer, window);
        tokio::spawn(async move {
            run_exec_stream_loop(&mut c_reader, &writer, &mut sink, &unused_credit).await
        })
    };

    // The pump returns when the process exits (Exited event).
    let pump_result = tokio::time::timeout(Duration::from_secs(10), pump)
        .await
        .expect("pump should finish")
        .unwrap();
    assert!(
        pump_result.is_ok(),
        "pump returned an error: {pump_result:?}"
    );

    // Drop the producer writer so the consumer loop sees EOF and returns.
    drop(producer_writer);
    let _ = tokio::time::timeout(Duration::from_secs(5), consumer_loop).await;
    ack_reader.abort();

    let bytes = collected.lock().await.clone();
    let text = String::from_utf8(bytes).expect("stdout is valid UTF-8");
    // Every line, in order, intact. Built by appending to a single `String`
    // (rather than `.map(format!).collect()`) to satisfy `clippy::format_collect`.
    let mut expected = String::new();
    for i in 1..=200 {
        use std::fmt::Write as _;
        writeln!(expected, "line-{i:03}").expect("writing to a String cannot fail");
    }
    assert_eq!(
        text, expected,
        "streamed stdout must match the source, in order"
    );
}

/// A consumer that never acks must stall the producer: with the initial window
/// the only credit available, the producer cannot send more bytes than the
/// window once that window is consumed. Asserts the producer parks rather than
/// racing ahead and buffering the whole stream.
#[tokio::test]
async fn slow_consumer_throttles_producer() {
    let provider = LocalExecProvider::new();

    // Emit far more than one window of output so the producer must block.
    let total_lines = 5_000u32;
    let id = provider
        .proc_spawn(ProcSpec {
            argv: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("for i in $(seq 1 {total_lines}); do printf 'XXXXXXXXXXXXXXXX\\n'; done"),
            ],
            cwd: None,
            env: vec![],
        })
        .await
        .unwrap();
    let events = provider.subscribe(id).unwrap();

    let (producer_t, consumer_t) = ChannelTransport::pair();
    let (_p_reader, p_writer) = FramedSession::new(producer_t).split();
    let (mut c_reader, _c_writer) = FramedSession::new(consumer_t).split();

    let producer_writer = Arc::new(Mutex::new(p_writer));
    // Small window so the stall is reached quickly and is easy to bound.
    let small_window: u32 = 4 * 1024;
    let credit = ExecStreamCredit::new(small_window);

    let pump = {
        let writer = Arc::clone(&producer_writer);
        let credit = Arc::clone(&credit);
        tokio::spawn(async move { pump_session_output(id, events, &writer, &credit).await })
    };

    // The consumer never sends an ack: it just counts received bytes after a
    // delay, modelling a slow reader. Because no ack is sent, the producer's
    // window is never refreshed.
    let received = Arc::new(AtomicU64::new(0));
    let drain = {
        let received = Arc::clone(&received);
        tokio::spawn(async move {
            while let Ok(Some(frame)) = c_reader.recv().await {
                if let BepMessage::ExecStream { bytes, .. } = frame {
                    received.fetch_add(bytes.len() as u64, Ordering::SeqCst);
                    // Deliberately slow: never ack, just absorb.
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        })
    };

    // Give the producer time to run as far as the window allows, then stop.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The pump must NOT have completed — it is parked on credit, throttled by
    // the unacking consumer.
    assert!(
        !pump.is_finished(),
        "producer must stall on a non-acking consumer"
    );

    // The bytes the consumer has received are bounded by the window plus at most
    // one frame in flight at the moment of the stall: a slow consumer caps the
    // producer's reach rather than letting it stream the whole output.
    let got = received.load(Ordering::SeqCst);
    let total_bytes = u64::from(total_lines) * 17; // 16 X's + newline
    assert!(
        got < total_bytes,
        "a stalled producer must not deliver the whole stream: got {got} of {total_bytes}"
    );
    assert!(
        got <= u64::from(small_window) + 4 * 1024,
        "in-flight bytes {got} must stay within the window {small_window} (plus one frame)"
    );

    pump.abort();
    drain.abort();
}

/// Killing a running session tears the stream down cleanly: the pump returns
/// `Ok(())` once the `Exited` event arrives, without erroring.
#[tokio::test]
async fn kill_tears_down_stream_cleanly() {
    let provider = LocalExecProvider::new();

    let id = provider
        .proc_spawn(ProcSpec {
            argv: vec!["sleep".to_owned(), "60".to_owned()],
            cwd: None,
            env: vec![],
        })
        .await
        .unwrap();
    let events = provider.subscribe(id).unwrap();

    let (producer_t, _consumer_t) = ChannelTransport::pair();
    let (_p_reader, p_writer) = FramedSession::new(producer_t).split();
    let producer_writer = Arc::new(Mutex::new(p_writer));
    let credit = ExecStreamCredit::new(DEFAULT_CREDIT_WINDOW);

    let pump = {
        let writer = Arc::clone(&producer_writer);
        let credit = Arc::clone(&credit);
        tokio::spawn(async move { pump_session_output(id, events, &writer, &credit).await })
    };

    // Let the session settle, then kill it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    provider.proc_kill(id).await.unwrap();

    let result = tokio::time::timeout(Duration::from_secs(5), pump)
        .await
        .expect("pump should return after kill")
        .unwrap();
    assert!(
        result.is_ok(),
        "pump must return Ok after a clean kill teardown: {result:?}"
    );
}

/// On `ExecEvent::Exited`, the pump sends exactly one `BepMessage::ExecExit`
/// control frame carrying the process's code/signal, after any output frames,
/// then returns `Ok(())`. The exit frame is not credit-gated and carries no
/// sequence number.
#[tokio::test]
async fn pump_emits_exec_exit_on_exited_event() {
    use cascade_exec::ExecEvent;

    let id = ExecSessionId(99);
    let (events_tx, events_rx) = mpsc::channel::<ExecEvent>(8);

    let (producer_t, consumer_t) = ChannelTransport::pair();
    let (_p_reader, p_writer) = FramedSession::new(producer_t).split();
    let (mut c_reader, _c_writer) = FramedSession::new(consumer_t).split();

    let producer_writer = Arc::new(Mutex::new(p_writer));
    let credit = ExecStreamCredit::new(DEFAULT_CREDIT_WINDOW);

    // Send one output chunk, then the exit event. The pump must emit an
    // ExecStream frame followed by an ExecExit frame.
    events_tx
        .send(ExecEvent::Output {
            stream: cascade_exec::ExecStreamKind::Stdout,
            bytes: b"hello".to_vec(),
        })
        .await
        .unwrap();
    events_tx
        .send(ExecEvent::Exited {
            code: Some(42),
            signal: None,
        })
        .await
        .unwrap();

    let pump = {
        let writer = Arc::clone(&producer_writer);
        let credit = Arc::clone(&credit);
        tokio::spawn(async move { pump_session_output(id, events_rx, &writer, &credit).await })
    };

    // First frame is the output chunk.
    let output_frame = tokio::time::timeout(Duration::from_secs(5), c_reader.recv())
        .await
        .expect("output frame should arrive")
        .unwrap()
        .expect("output frame must be Some");
    assert!(matches!(output_frame, BepMessage::ExecStream { bytes, .. } if bytes == b"hello"));

    // Second frame is the exit control frame carrying the code.
    let exit_frame = tokio::time::timeout(Duration::from_secs(5), c_reader.recv())
        .await
        .expect("exit frame should arrive")
        .unwrap()
        .expect("exit frame must be Some");
    assert_eq!(
        exit_frame,
        BepMessage::ExecExit {
            session: 99,
            code: Some(42),
            signal: None,
        }
    );

    let result = tokio::time::timeout(Duration::from_secs(5), pump)
        .await
        .expect("pump should return after exit")
        .unwrap();
    assert!(result.is_ok(), "pump must return Ok after exit: {result:?}");
}

/// A signal-killed process carries `signal` (and no `code`) through the exit
/// frame end to end.
#[tokio::test]
async fn pump_emits_exec_exit_with_signal() {
    use cascade_exec::ExecEvent;

    let id = ExecSessionId(5);
    let (events_tx, events_rx) = mpsc::channel::<ExecEvent>(8);

    let (producer_t, consumer_t) = ChannelTransport::pair();
    let (_p_reader, p_writer) = FramedSession::new(producer_t).split();
    let (mut c_reader, _c_writer) = FramedSession::new(consumer_t).split();

    let producer_writer = Arc::new(Mutex::new(p_writer));
    let credit = ExecStreamCredit::new(DEFAULT_CREDIT_WINDOW);

    events_tx
        .send(ExecEvent::Exited {
            code: None,
            signal: Some(9),
        })
        .await
        .unwrap();

    let pump = {
        let writer = Arc::clone(&producer_writer);
        let credit = Arc::clone(&credit);
        tokio::spawn(async move { pump_session_output(id, events_rx, &writer, &credit).await })
    };

    let exit_frame = tokio::time::timeout(Duration::from_secs(5), c_reader.recv())
        .await
        .expect("exit frame should arrive")
        .unwrap()
        .expect("exit frame must be Some");
    assert_eq!(
        exit_frame,
        BepMessage::ExecExit {
            session: 5,
            code: None,
            signal: Some(9),
        }
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), pump).await;
}
