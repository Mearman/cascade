//! Bidirectional `WebSocket` byte-pipe for the operated relay.
//!
//! The byte-shuttling core lives in [`cascade_p2p::pipe`] and is shared with
//! the in-process peer relay. This module is the thin server-side adapter: it
//! wires the relay server's atomic [`Counters`] into the shared core's
//! [`ByteMeter`] hook and pins the generic shuttle to the server's concrete
//! raw-TCP `WebSocket` stream type.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use cascade_p2p::pipe::{ByteMeter, shuttle};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;

use crate::metrics::Counters;

pub use cascade_p2p::pipe::PipeOutcome;

/// [`ByteMeter`] adapter that folds relayed-byte totals into the relay
/// server's shared [`Counters`].
struct CounterMeter(Arc<Counters>);

impl ByteMeter for CounterMeter {
    fn record(&self, count: u64) {
        self.0
            .bytes_relayed_total
            .fetch_add(count, Ordering::Relaxed);
    }
}

/// Forward binary frames between two raw-TCP `WebSocket` streams until either
/// side closes, accruing the byte total into `counters`.
pub async fn run_pipe(
    a: WebSocketStream<TcpStream>,
    b: WebSocketStream<TcpStream>,
    counters: Arc<Counters>,
) -> (PipeOutcome, PipeOutcome) {
    let meter = CounterMeter(counters);
    shuttle(a, b, &meter).await
}
