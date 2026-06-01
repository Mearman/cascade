//! Bidirectional `WebSocket` byte-pipe.
//!
//! Once two peers have authenticated under the same session ID they enter
//! this module. The pipe forwards binary frames from A to B and from B to
//! A until either side closes the connection. The relay never inspects the
//! payload — `Message::Binary` and `Message::Close` are the only variants
//! handled, and the close on either side propagates to the other.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::metrics::Counters;

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

/// Forward binary frames between two `WebSocket` streams until either side
/// closes. Increments the supplied byte counter as frames pass.
pub async fn shuttle(
    a: WebSocketStream<TcpStream>,
    b: WebSocketStream<TcpStream>,
    counters: Arc<Counters>,
) -> (PipeOutcome, PipeOutcome) {
    let (mut a_sink, mut a_stream) = a.split();
    let (mut b_sink, mut b_stream) = b.split();
    let counters_a_to_b = counters.clone();
    let counters_b_to_a = counters;

    let forward_a_to_b = async move {
        loop {
            match a_stream.next().await {
                Some(Ok(Message::Binary(payload))) => {
                    counters_a_to_b
                        .bytes_relayed_total
                        .fetch_add(payload.len() as u64, Ordering::Relaxed);
                    if b_sink.send(Message::Binary(payload)).await.is_err() {
                        return PipeOutcome::TransportError;
                    }
                }
                Some(Ok(Message::Close(_))) | None => {
                    let _ = b_sink.send(Message::Close(None)).await;
                    return PipeOutcome::PeerClosed;
                }
                Some(Ok(Message::Ping(payload))) => {
                    if b_sink.send(Message::Ping(payload)).await.is_err() {
                        return PipeOutcome::TransportError;
                    }
                }
                Some(Ok(Message::Pong(payload))) => {
                    if b_sink.send(Message::Pong(payload)).await.is_err() {
                        return PipeOutcome::TransportError;
                    }
                }
                Some(Ok(Message::Text(_) | Message::Frame(_))) => {
                    let _ = b_sink.send(Message::Close(None)).await;
                    return PipeOutcome::UnexpectedFrame;
                }
                Some(Err(_)) => {
                    let _ = b_sink.send(Message::Close(None)).await;
                    return PipeOutcome::TransportError;
                }
            }
        }
    };

    let forward_b_to_a = async move {
        loop {
            match b_stream.next().await {
                Some(Ok(Message::Binary(payload))) => {
                    counters_b_to_a
                        .bytes_relayed_total
                        .fetch_add(payload.len() as u64, Ordering::Relaxed);
                    if a_sink.send(Message::Binary(payload)).await.is_err() {
                        return PipeOutcome::TransportError;
                    }
                }
                Some(Ok(Message::Close(_))) | None => {
                    let _ = a_sink.send(Message::Close(None)).await;
                    return PipeOutcome::PeerClosed;
                }
                Some(Ok(Message::Ping(payload))) => {
                    if a_sink.send(Message::Ping(payload)).await.is_err() {
                        return PipeOutcome::TransportError;
                    }
                }
                Some(Ok(Message::Pong(payload))) => {
                    if a_sink.send(Message::Pong(payload)).await.is_err() {
                        return PipeOutcome::TransportError;
                    }
                }
                Some(Ok(Message::Text(_) | Message::Frame(_))) => {
                    let _ = a_sink.send(Message::Close(None)).await;
                    return PipeOutcome::UnexpectedFrame;
                }
                Some(Err(_)) => {
                    let _ = a_sink.send(Message::Close(None)).await;
                    return PipeOutcome::TransportError;
                }
            }
        }
    };

    tokio::join!(forward_a_to_b, forward_b_to_a)
}
