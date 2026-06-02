//! Blind bidirectional `WebSocket` byte-pipe — the shared relay session core.
//!
//! Once two peers have authenticated under the same session identifier they
//! enter this module. The pipe forwards binary frames from A to B and from B
//! to A until either side closes the connection. The relay never inspects the
//! payload — `Message::Binary` and `Message::Close` are the only data-bearing
//! variants handled, and the close on either side propagates to the other.
//!
//! The same core drives two callers:
//!
//! - the standalone `cascade-relay-server`, which pairs two raw TCP
//!   `WebSocket` clients and shuttles between them, and
//! - the in-process peer relay (see [`crate::relay`]), where a volunteering
//!   node bridges two already-established relayed `WebSocket` sessions exactly
//!   as the operated relay does.
//!
//! Keeping one implementation means both paths share identical frame-handling
//! semantics and there is a single place where the wire behaviour is defined.

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio_tungstenite::tungstenite::Message;

/// Per-side disposition once the byte-pipe drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeOutcome {
    /// The peer closed the socket gracefully (`Close` frame received).
    PeerClosed,
    /// The transport errored mid-stream.
    TransportError,
    /// The peer sent a non-binary, non-close frame; the pipe rejects this.
    UnexpectedFrame,
}

/// Observer notified of every byte ferried across the pipe.
///
/// The two callers track bytes differently — the standalone server folds
/// them into a `Prometheus` counter, the in-process relay into a
/// per-session bandwidth budget — so the core takes an observer rather than
/// owning a concrete counter. Implementations must be cheap and lock-free;
/// they are invoked on the hot forwarding path for every binary frame.
pub trait ByteMeter: Send + Sync {
    /// Record that `count` payload bytes have been forwarded in one
    /// direction.
    fn record(&self, count: u64);
}

/// A [`ByteMeter`] that discards every measurement. Used where the caller
/// has no interest in byte accounting.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopMeter;

impl ByteMeter for NoopMeter {
    fn record(&self, _count: u64) {}
}

/// Forward binary frames between two `WebSocket` streams until either side
/// closes. Each forwarded binary frame's payload length is reported to
/// `meter`.
///
/// The two halves are generic over the concrete stream type so the same
/// core serves a raw-TCP server socket and a TLS-wrapped relayed peer
/// socket. Both must be a [`Sink`] of [`Message`] and a [`Stream`] yielding
/// `Result<Message, E>`; the error type is opaque — the pipe treats any
/// receive error as a transport fault and tears the session down.
pub async fn shuttle<A, B, E, M>(a: A, b: B, meter: &M) -> (PipeOutcome, PipeOutcome)
where
    A: Sink<Message> + Stream<Item = Result<Message, E>> + Unpin + Send,
    B: Sink<Message> + Stream<Item = Result<Message, E>> + Unpin + Send,
    M: ByteMeter,
{
    let (mut a_sink, mut a_stream) = a.split();
    let (mut b_sink, mut b_stream) = b.split();

    let forward_a_to_b = forward(&mut a_stream, &mut b_sink, meter);
    let forward_b_to_a = forward(&mut b_stream, &mut a_sink, meter);

    tokio::join!(forward_a_to_b, forward_b_to_a)
}

/// Forward one direction of the pipe: read frames from `source` and write
/// them to `dest`, metering binary payloads, until either side ends.
async fn forward<Src, Dst, E, M>(source: &mut Src, dest: &mut Dst, meter: &M) -> PipeOutcome
where
    Src: Stream<Item = Result<Message, E>> + Unpin,
    Dst: Sink<Message> + Unpin,
    M: ByteMeter,
{
    loop {
        match source.next().await {
            Some(Ok(Message::Binary(payload))) => {
                meter.record(payload.len() as u64);
                if dest.send(Message::Binary(payload)).await.is_err() {
                    return PipeOutcome::TransportError;
                }
            }
            Some(Ok(Message::Close(_))) | None => {
                let _ = dest.send(Message::Close(None)).await;
                return PipeOutcome::PeerClosed;
            }
            Some(Ok(Message::Ping(payload))) => {
                if dest.send(Message::Ping(payload)).await.is_err() {
                    return PipeOutcome::TransportError;
                }
            }
            Some(Ok(Message::Pong(payload))) => {
                if dest.send(Message::Pong(payload)).await.is_err() {
                    return PipeOutcome::TransportError;
                }
            }
            Some(Ok(Message::Text(_) | Message::Frame(_))) => {
                let _ = dest.send(Message::Close(None)).await;
                return PipeOutcome::UnexpectedFrame;
            }
            Some(Err(_)) => {
                let _ = dest.send(Message::Close(None)).await;
                return PipeOutcome::TransportError;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll};

    /// Atomic byte meter used to assert the shuttle's accounting.
    #[derive(Default)]
    struct CountingMeter(AtomicU64);

    impl ByteMeter for CountingMeter {
        fn record(&self, count: u64) {
            self.0.fetch_add(count, Ordering::Relaxed);
        }
    }

    /// In-memory mock endpoint with no real socket. Its `Stream` side
    /// replays a finite, pre-loaded script of inbound frames and then ends
    /// (`None`), which the shuttle reads as a peer close. Its `Sink` side
    /// collects everything the shuttle writes into a shared buffer the test
    /// inspects afterwards. Both halves are synchronous and never pend, so
    /// `shuttle` drains both directions and resolves deterministically with
    /// no real I/O or interleaving.
    struct MockEndpoint {
        inbound: VecDeque<Message>,
        outbound: Arc<Mutex<Vec<Message>>>,
    }

    impl MockEndpoint {
        fn new(inbound: Vec<Message>) -> (Self, Arc<Mutex<Vec<Message>>>) {
            let outbound = Arc::new(Mutex::new(Vec::new()));
            let endpoint = Self {
                inbound: inbound.into(),
                outbound: outbound.clone(),
            };
            (endpoint, outbound)
        }
    }

    impl Stream for MockEndpoint {
        type Item = Result<Message, Infallible>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.inbound.pop_front().map(Ok))
        }
    }

    impl Sink<Message> for MockEndpoint {
        type Error = Infallible;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            if let Ok(mut out) = self.outbound.lock() {
                out.push(item);
            }
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Bridge two mock endpoints end to end and prove a binary frame
    /// travels A → B and B → A, the meter counts both directions, and each
    /// direction reports `PeerClosed` once its source script is exhausted.
    #[tokio::test]
    async fn shuttle_bridges_two_mock_endpoints() {
        let frame_a = Message::Binary(b"hello-from-a".to_vec().into());
        let frame_b = Message::Binary(b"reply-from-b".to_vec().into());

        let (endpoint_a, a_out) = MockEndpoint::new(vec![frame_a.clone()]);
        let (endpoint_b, b_out) = MockEndpoint::new(vec![frame_b.clone()]);

        let meter = CountingMeter::default();
        let (outcome_a, outcome_b) = shuttle(endpoint_a, endpoint_b, &meter).await;

        // A's single inbound frame must have been forwarded to B's sink,
        // and B's to A's sink. Each sink also receives a trailing `Close`
        // once the opposite source ends.
        let a_written = a_out.lock().unwrap().clone();
        let b_written = b_out.lock().unwrap().clone();
        assert_eq!(b_written.first(), Some(&frame_a));
        assert_eq!(a_written.first(), Some(&frame_b));
        assert!(matches!(b_written.last(), Some(Message::Close(_))));
        assert!(matches!(a_written.last(), Some(Message::Close(_))));

        // Both directions saw their source end cleanly.
        assert_eq!(outcome_a, PipeOutcome::PeerClosed);
        assert_eq!(outcome_b, PipeOutcome::PeerClosed);

        // The meter accrues one binary payload per direction.
        let total = b"hello-from-a".len() as u64 + b"reply-from-b".len() as u64;
        assert_eq!(meter.0.load(Ordering::Relaxed), total);
    }
}
