//! Exec data plane — live stdio over the authenticated peer connection.
//!
//! Control verbs (`pty.spawn`, `proc.spawn`, …) travel as `management` frames;
//! the *live* `stdin` / `stdout` / `stderr` of a running session travel here, as
//! [`BepMessage::ExecStream`] frames over the same BEP session (and the relay
//! for WAN), never through the content-addressed block store. A live stream is
//! ephemeral and mutable; the block store is for immutable, addressable content,
//! so forcing live output through it would be a category error
//! (`docs/exec-capability.md`).
//!
//! # Backpressure
//!
//! The consumer advertises a credit window — the maximum in-flight bytes it will
//! accept past the highest sequence it has acknowledged — in each
//! [`BepMessage::ExecStreamAck`]. The producer must not send beyond it: a slow
//! consumer that delays acking shrinks the effective window to zero and stalls
//! the producer rather than letting the node buffer unboundedly. This is the
//! wire half of the bounded `mpsc` the node-side [`cascade_exec`] provider
//! already applies; together they carry backpressure end to end.
//!
//! # Wiring to [`cascade_exec`]
//!
//! [`pump_session_output`] drains a session's [`ExecEvent`] receiver (obtained
//! from [`cascade_exec::ExecProvider::subscribe`]) and emits stdout/stderr
//! frames. [`ExecStreamSink`] receives inbound frames, forwards `stdin` to
//! [`cascade_exec::ExecProvider::pty_write`], hands stdout/stderr bytes to a
//! consumer, and grants credit. The two are symmetric: the manager side runs a
//! sink for the node's output and a pump for the operator's keystrokes; the node
//! side runs a pump for session output and a sink for inbound stdin.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{Context, Result};
use cascade_exec::{ExecEvent, ExecSessionId, ExecStreamKind};
use tokio::sync::{Mutex, Notify};

use crate::framed::{SessionReader, SessionWriter};
use crate::protocol::BepMessage;
use crate::transport::{TransportReader, TransportWriter};

/// Wire discriminant for the stdin stream in an [`BepMessage::ExecStream`].
///
/// Mirrors the frozen `EXEC_STREAM_STDIN` discriminant in `protocol`; redeclared
/// here (rather than exported) so the data plane reads as self-contained against
/// the frozen wire numbers.
const STREAM_STDIN: u8 = 0;
/// Wire discriminant for the stdout stream in an [`BepMessage::ExecStream`].
const STREAM_STDOUT: u8 = 1;
/// Wire discriminant for the stderr stream in an [`BepMessage::ExecStream`].
const STREAM_STDERR: u8 = 2;

/// Which live stream a decoded [`ExecStreamFrame`] carries, mirroring the wire
/// discriminants without exposing the raw `u8` to manager-side consumers.
///
/// Distinct from [`cascade_exec::ExecStreamKind`] (which names the
/// node-side provider's stream tag) so the wire-layer type stays in the p2p
/// crate and the manager-side consumer does not depend on the exec crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireStreamKind {
    /// Standard input (inbound to the process — only meaningful for writes).
    Stdin,
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl WireStreamKind {
    /// Decode a wire stream discriminant. Returns `None` for an unknown value
    /// so a malformed frame surfaces as a protocol error rather than a silent
    /// misroute.
    #[must_use]
    pub const fn from_wire(raw: u8) -> Option<Self> {
        match raw {
            STREAM_STDIN => Some(Self::Stdin),
            STREAM_STDOUT => Some(Self::Stdout),
            STREAM_STDERR => Some(Self::Stderr),
            _ => None,
        }
    }
}

/// A decoded inbound exec-stream frame delivered to the manager-side consumer.
///
/// The session loop decodes the raw [`BepMessage::ExecStream`] wire frame into
/// this typed value before handing it to the consumer registered via the
/// backend layer's `SyncEngine::subscribe_exec_stream` (in `cascade-backend-p2p`,
/// which depends on this crate; the registration therefore lives there rather
/// than here, where the data-plane type is defined), so the consumer never sees
/// the wire sequence number or raw discriminant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecStreamFrame {
    /// The session the bytes belong to.
    pub session: u64,
    /// Which stream the bytes arrived on.
    pub stream: WireStreamKind,
    /// The raw stream bytes.
    pub bytes: Vec<u8>,
}

/// One item delivered to the manager-side exec-stream consumer over its
/// subscription channel.
///
/// The consumer channel (handed out by `SyncEngine::subscribe_exec_stream`) used
/// to carry only [`ExecStreamFrame`] byte payloads; the terminal
/// [`BepMessage::ExecExit`] control frame is now delivered through the same
/// channel as the [`Self::Exited`] variant, so a one-shot `exec` pump can exit
/// with the remote process's exit code rather than only learning the session
/// ended when the byte stream closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecStreamEvent {
    /// A chunk of stdout/stderr bytes for the session.
    Output(ExecStreamFrame),
    /// The session's process exited. `code` is the normal exit code (if any);
    /// `signal` is the POSIX signal that killed it (if any). Exactly one is
    /// `Some` for a normal Unix exit; both `None` means the exit status was
    /// indeterminate. A consumer that wants a shell-style exit status maps a
    /// signal-killed process to `128 + signal`.
    Exited {
        /// The process exit code, if it exited normally.
        code: Option<i32>,
        /// The POSIX signal number that terminated the process, if killed by a
        /// signal.
        signal: Option<i32>,
    },
}

impl ExecStreamEvent {
    /// The exit status a one-shot `exec` should propagate, following the shell
    /// convention: a normal exit yields `code`, a signal-killed process yields
    /// `128 + signal`, and an indeterminate exit (neither set) yields the
    /// generic failure status `1`.
    #[must_use]
    pub const fn to_exit_code(&self) -> Option<i32> {
        match self {
            Self::Output(_) => None,
            Self::Exited { code, signal } => Some(exit_code_from_code_signal(*code, *signal)),
        }
    }
}

/// Map an optional exit `code` and optional `signal` to a shell-style process
/// exit status. Exposed so the CLI's one-shot `exec` and tests share one
/// definition.
///
/// - `code` present -> `code`.
/// - `code` absent, `signal` present -> `128 + signal`.
/// - both absent -> `1` (generic failure: the process exited without a
///   determinable status).
#[must_use]
pub const fn exit_code_from_code_signal(code: Option<i32>, signal: Option<i32>) -> i32 {
    match (code, signal) {
        (Some(c), _) => c,
        (None, Some(s)) => 128 + s,
        (None, None) => 1,
    }
}

/// Default credit window a consumer advertises, in bytes.
///
/// Sized to the node-side output channel's buffering headroom: the local
/// provider pumps 4 `KiB` reads into a 256-slot bounded channel, so a window an
/// order of magnitude above one read keeps a healthy consumer from stalling on
/// every chunk while still bounding in-flight bytes for a slow one. Exposed so a
/// caller can pin the same value the sink grants.
pub const DEFAULT_CREDIT_WINDOW: u32 = 64 * 1024;

/// Shared backpressure credit for one exec stream's producer.
///
/// The producer records each frame it sends as `(seq, cumulative_bytes)`; the
/// consumer's [`BepMessage::ExecStreamAck`] names the highest sequence it has
/// accepted and the byte window it will accept past it. Applying an ack maps
/// `ack_seq` back to the cumulative byte count for that frame — the authoritative
/// acknowledged-byte position — and refreshes the window. A [`Notify`] wakes a
/// producer parked waiting for credit.
///
/// The byte window is the unit the spec freezes
/// ([`BepMessage::ExecStreamAck::window`] is in bytes), so the credit arithmetic
/// is in bytes throughout; the sequence number is only the key that ties an ack
/// to the byte total the producer had reached when it sent that frame.
#[derive(Debug)]
pub struct ExecStreamCredit {
    /// Guarded credit bookkeeping.
    state: Mutex<CreditState>,
    /// Woken whenever the consumer grants more credit, so a parked producer can
    /// re-check its send condition.
    granted: Notify,
}

/// The mutable half of [`ExecStreamCredit`].
#[derive(Debug)]
struct CreditState {
    /// Total bytes the consumer has acknowledged receiving.
    acked_bytes: u64,
    /// In-flight byte allowance past `acked_bytes`.
    window: u64,
    /// Outstanding `(seq, cumulative_bytes_after_this_frame)` records for frames
    /// the producer has sent but not yet seen acknowledged. Pruned in order as
    /// acks advance, so its length is bounded by the number of in-flight frames
    /// the window permits.
    in_flight: VecDeque<(u64, u64)>,
}

impl ExecStreamCredit {
    /// Create a credit tracker seeded with an initial window.
    ///
    /// The initial window lets the producer start sending before the first ack
    /// arrives; it mirrors the window the consumer's sink advertises.
    #[must_use]
    pub fn new(initial_window: u32) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CreditState {
                acked_bytes: 0,
                window: u64::from(initial_window),
                in_flight: VecDeque::new(),
            }),
            granted: Notify::new(),
        })
    }

    /// Record that the producer has sent the frame numbered `seq`, bringing its
    /// cumulative sent byte total to `cumulative_bytes`.
    async fn record_sent(&self, seq: u64, cumulative_bytes: u64) {
        let mut state = self.state.lock().await;
        state.in_flight.push_back((seq, cumulative_bytes));
    }

    /// Apply an acknowledgement from the consumer.
    ///
    /// `ack_seq` is the highest sequence the consumer has accepted; `window` is
    /// the fresh in-flight byte allowance past it. Maps `ack_seq` to the
    /// cumulative byte total recorded for that frame to advance `acked_bytes`,
    /// then wakes any parked producer.
    pub async fn apply_ack(&self, ack_seq: u64, window: u32) {
        {
            let mut state = self.state.lock().await;
            // Drain every in-flight record up to and including `ack_seq`,
            // advancing the acked byte total to the most recent acknowledged
            // frame's cumulative count. Records arrive and are pruned in send
            // order, so a single forward scan suffices.
            while let Some(&(seq, cumulative)) = state.in_flight.front() {
                if seq <= ack_seq {
                    state.acked_bytes = state.acked_bytes.max(cumulative);
                    state.in_flight.pop_front();
                } else {
                    break;
                }
            }
            state.window = u64::from(window);
        }
        self.granted.notify_waiters();
    }

    /// Wait until the consumer's window has room for another frame, i.e. the
    /// in-flight byte count `sent_bytes - acked_bytes` is below the window.
    ///
    /// Credit is frame-granular: a send is permitted whenever any window remains,
    /// so a frame may carry the in-flight total up to one chunk past the window —
    /// the "window plus one frame" bound the backpressure tests assert. This is
    /// deliberate: a chunk always makes progress when the window is non-zero, even
    /// one larger than the whole window. Requiring the entire chunk to fit
    /// (`in_flight + chunk <= window`) would wedge a stream whose producer emits a
    /// chunk bigger than the window — it could never be sent and the stream would
    /// deadlock. A consumer that stops acking holds the window full (or grants
    /// zero) and parks the producer here until the next [`Self::apply_ack`].
    async fn await_credit(&self, sent_bytes: u64) {
        loop {
            // Register for notification *before* checking the condition: an ack
            // landing between the check and the await must wake this waiter, and
            // `notify_waiters` only wakes already-registered waiters (it stores no
            // permit), so the future is enabled before the window is read.
            let notified = self.granted.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.state.lock().await;
                if sent_bytes.saturating_sub(state.acked_bytes) < state.window {
                    return;
                }
            }
            notified.as_mut().await;
        }
    }
}

/// Drain a session's output receiver and write it to the peer as
/// [`BepMessage::ExecStream`] frames, honouring the consumer's credit window.
///
/// Reads [`ExecEvent`]s from `events` (the receiver handed out by
/// [`cascade_exec::ExecProvider::subscribe`]) and, for each
/// [`ExecEvent::Output`], sends an `ExecStream` frame tagged with a per-session
/// monotonic sequence number and the stream kind. Before each send it waits for
/// credit via `credit`, so a slow consumer throttles this loop rather than
/// letting output pile up.
///
/// Returns when the session's output channel closes (the session ended and all
/// pumps dropped their senders) or an [`ExecEvent::Exited`] arrives, whichever
/// comes first. An `Exited` event ends the stream cleanly: the pump sends a
/// single [`BepMessage::ExecExit`] control frame carrying the process's exit
/// code and signal (NOT credit-gated — it is one terminal frame sent after the
/// last output frame), then returns `Ok(())` without tearing down the shared
/// writer, leaving the caller to send a `Close` or continue using the session
/// for other traffic.
pub async fn pump_session_output<W: TransportWriter>(
    session: ExecSessionId,
    mut events: tokio::sync::mpsc::Receiver<ExecEvent>,
    writer: &Mutex<SessionWriter<W>>,
    credit: &ExecStreamCredit,
) -> Result<()> {
    let mut seq: u64 = 0;
    let mut sent_bytes: u64 = 0;

    while let Some(event) = events.recv().await {
        match event {
            ExecEvent::Output { stream, bytes } => {
                if bytes.is_empty() {
                    continue;
                }
                let chunk = bytes.len() as u64;
                credit.await_credit(sent_bytes).await;

                let wire_stream = match stream {
                    ExecStreamKind::Stdin => STREAM_STDIN,
                    ExecStreamKind::Stdout => STREAM_STDOUT,
                    ExecStreamKind::Stderr => STREAM_STDERR,
                };
                let frame = BepMessage::ExecStream {
                    session: session.0,
                    seq,
                    stream: wire_stream,
                    bytes,
                };
                writer
                    .lock()
                    .await
                    .send(&frame)
                    .await
                    .context("sending exec stream output frame")?;
                sent_bytes = sent_bytes.saturating_add(chunk);
                credit.record_sent(seq, sent_bytes).await;
                seq = seq.wrapping_add(1);
            }
            ExecEvent::Exited { code, signal } => {
                // Terminal control frame: send exactly once, after the last
                // output frame. It is NOT credit-gated — it carries no sequence
                // number and the manager routes it to the consumer without
                // acking, so it must not be throttled by a slow consumer's
                // window (the session is over; the only remaining business is
                // to deliver the exit status).
                let exit_frame = BepMessage::ExecExit {
                    session: session.0,
                    code,
                    signal,
                };
                writer
                    .lock()
                    .await
                    .send(&exit_frame)
                    .await
                    .context("sending exec exit frame")?;
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Where an [`ExecStreamSink`] routes the inbound bytes of one stream.
///
/// `stdin` is forwarded to the node-side session; `stdout`/`stderr` are handed
/// to the manager-side consumer. Implementors are the seam between the data
/// plane and either [`cascade_exec`] (on the node) or a terminal renderer (on
/// the manager).
#[async_trait::async_trait]
pub trait ExecStreamConsumer: Send {
    /// Handle one ordered chunk of stream bytes for `session`.
    ///
    /// `stream` is the frozen wire discriminant (0=stdin, 1=stdout, 2=stderr).
    /// An error tears the sink down — the caller treats it as a fatal session
    /// fault and closes the stream.
    async fn on_bytes(&mut self, session: ExecSessionId, stream: u8, bytes: &[u8]) -> Result<()>;
}

/// Consume inbound [`BepMessage::ExecStream`] frames, enforce ordering, deliver
/// bytes to a [`ExecStreamConsumer`], and grant backpressure credit.
///
/// One sink drives one BEP session's inbound exec traffic. It validates the
/// per-session sequence is contiguous (a gap is a protocol fault, since the
/// underlying transport is reliable and ordered), forwards the bytes to the
/// consumer, advances its acknowledged byte count, and emits an
/// [`BepMessage::ExecStreamAck`] granting a fresh window. The ack is what frees
/// the remote producer's credit, so a consumer that falls behind naturally
/// throttles the producer.
pub struct ExecStreamSink<C: ExecStreamConsumer> {
    /// The session this sink serves.
    session: ExecSessionId,
    /// The downstream consumer of delivered bytes.
    consumer: C,
    /// The credit window this sink advertises in each ack, in bytes.
    window: u32,
    /// The next sequence number expected from the producer.
    expected_seq: u64,
    /// Total bytes accepted so far — the cumulative ack position.
    received_bytes: u64,
}

impl<C: ExecStreamConsumer> std::fmt::Debug for ExecStreamSink<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecStreamSink")
            .field("session", &self.session)
            .field("window", &self.window)
            .field("expected_seq", &self.expected_seq)
            .field("received_bytes", &self.received_bytes)
            .finish_non_exhaustive()
    }
}

impl<C: ExecStreamConsumer> ExecStreamSink<C> {
    /// Create a sink for `session` that grants a `window`-byte credit per ack.
    #[must_use]
    pub const fn new(session: ExecSessionId, consumer: C, window: u32) -> Self {
        Self {
            session,
            consumer,
            window,
            expected_seq: 0,
            received_bytes: 0,
        }
    }

    /// Process one inbound frame that belongs to this sink's session, returning
    /// the [`BepMessage::ExecStreamAck`] to send back (the granted credit), or
    /// `None` for a frame that does not require an ack.
    ///
    /// Rejects a frame for the wrong session or a sequence gap as a protocol
    /// fault. The caller sends the returned ack over the same BEP session; it is
    /// returned rather than sent here so the sink stays free of the writer and a
    /// single writer lock serves both directions.
    pub async fn handle(&mut self, frame: &BepMessage) -> Result<Option<BepMessage>> {
        let BepMessage::ExecStream {
            session,
            seq,
            stream,
            bytes,
        } = frame
        else {
            anyhow::bail!("ExecStreamSink received a non-ExecStream frame");
        };

        if *session != self.session.0 {
            anyhow::bail!(
                "exec stream frame for session {session} routed to sink for session {}",
                self.session.0
            );
        }
        if *seq != self.expected_seq {
            anyhow::bail!(
                "exec stream sequence gap on session {session}: expected {}, got {seq}",
                self.expected_seq
            );
        }

        self.consumer
            .on_bytes(self.session, *stream, bytes)
            .await
            .context("delivering inbound exec stream bytes")?;

        self.expected_seq = self.expected_seq.wrapping_add(1);
        self.received_bytes = self.received_bytes.saturating_add(bytes.len() as u64);

        Ok(Some(BepMessage::ExecStreamAck {
            session: self.session.0,
            ack_seq: *seq,
            window: self.window,
        }))
    }
}

/// Run a full inbound exec-stream receive loop over a BEP session reader.
///
/// Reads frames until clean EOF or a non-exec frame, dispatching each
/// [`BepMessage::ExecStream`] to `sink` and writing the resulting
/// [`BepMessage::ExecStreamAck`] back through `writer`. [`BepMessage::ExecStreamAck`]
/// frames that arrive here are applied to `credit` so a producer sharing the
/// same loop is unblocked. Any other frame ends the loop and is returned to the
/// caller to dispatch — the exec data plane does not own the whole session.
pub async fn run_exec_stream_loop<R, W, C>(
    reader: &mut SessionReader<R>,
    writer: &Mutex<SessionWriter<W>>,
    sink: &mut ExecStreamSink<C>,
    credit: &ExecStreamCredit,
) -> Result<Option<BepMessage>>
where
    R: TransportReader,
    W: TransportWriter,
    C: ExecStreamConsumer,
{
    while let Some(frame) = reader.recv().await.context("receiving exec stream frame")? {
        match &frame {
            BepMessage::ExecStream { .. } => {
                if let Some(ack) = sink.handle(&frame).await? {
                    writer
                        .lock()
                        .await
                        .send(&ack)
                        .await
                        .context("sending exec stream ack")?;
                }
            }
            BepMessage::ExecStreamAck {
                ack_seq, window, ..
            } => {
                // An inbound ack frees a producer sharing this loop's credit: it
                // acknowledges through `ack_seq` inclusive and re-advertises a
                // byte window. The credit maps the sequence back to the
                // cumulative byte total the producer recorded for that frame.
                credit.apply_ack(*ack_seq, *window).await;
            }
            _ => return Ok(Some(frame)),
        }
    }
    Ok(None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_normal_exit_yields_code() {
        assert_eq!(exit_code_from_code_signal(Some(0), None), 0);
        assert_eq!(exit_code_from_code_signal(Some(42), None), 42);
        assert_eq!(exit_code_from_code_signal(Some(-1), None), -1);
    }

    #[test]
    fn exit_code_signal_kill_yields_128_plus_signal() {
        assert_eq!(exit_code_from_code_signal(None, Some(9)), 137);
        assert_eq!(exit_code_from_code_signal(None, Some(15)), 143);
        assert_eq!(exit_code_from_code_signal(None, Some(2)), 130);
    }

    #[test]
    fn exit_code_indeterminate_yields_generic_failure() {
        assert_eq!(exit_code_from_code_signal(None, None), 1);
    }

    #[test]
    fn exit_code_code_takes_precedence_over_signal() {
        // A normal exit code wins even if a signal is also carried (defensive:
        // the node sets exactly one for a real Unix exit, but the mapping must
        // not panic or surprise if both are present).
        assert_eq!(exit_code_from_code_signal(Some(3), Some(9)), 3);
    }

    #[test]
    fn event_to_exit_code_is_none_for_output() {
        let frame = ExecStreamFrame {
            session: 1,
            stream: WireStreamKind::Stdout,
            bytes: Vec::new(),
        };
        assert_eq!(ExecStreamEvent::Output(frame).to_exit_code(), None);
    }

    #[test]
    fn event_to_exit_code_maps_exited() {
        let exited = ExecStreamEvent::Exited {
            code: Some(42),
            signal: None,
        };
        assert_eq!(exited.to_exit_code(), Some(42));

        let killed = ExecStreamEvent::Exited {
            code: None,
            signal: Some(9),
        };
        assert_eq!(killed.to_exit_code(), Some(137));
    }
}
