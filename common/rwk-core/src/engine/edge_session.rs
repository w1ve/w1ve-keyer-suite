//! The Client-side edge path shared by every network keyer app.
//!
//! Every edge source on a Client (the paddle poller, the type-ahead send worker, and any
//! future source) must feed **one** session-long [`EdgeSender`], so sequence numbers and
//! timestamps advance together across the whole session. The Station keeps a single
//! `last_sequence` per session, so a second sender restarting at 0 would have every frame
//! discarded as a duplicate. This type owns that sender, restarts it once per pairing, and
//! carries finished frames to the tunnel over one ordered FIFO. Apps call [`EdgeSession::push_edge`]
//! from their keying paths and never build senders or queues themselves.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::engine::edge_sender::EdgeSender;
use crate::engine::keying::EdgeEvent;
use crate::engine::network::EdgeTransport;
use crate::primitives::EdgeSource;
use crate::protocol::edge::RwkPaddleFrame;

/// The session-scoped edge sender plus its ordered outbound queue.
///
/// Lock order is always `sender` then `queue`, so [`Self::push_edge`] and [`Self::begin`]
/// cannot deadlock. The sender lock is held across the queue push, so the sequence a frame is
/// given always matches the order it leaves in.
#[derive(Debug)]
pub struct EdgeSession {
    sender: Mutex<EdgeSender>,
    queue: Mutex<Option<UnboundedSender<RwkPaddleFrame>>>,
}

impl EdgeSession {
    /// Creates an idle session timed on a clock of `frequency` ticks per second, anchored at
    /// `now_ticks`. Nothing is sent until [`Self::begin`] is called for a pairing.
    #[must_use]
    pub fn new(frequency: u64, now_ticks: u64) -> Self {
        Self {
            sender: Mutex::new(EdgeSender::new(1, frequency, now_ticks)),
            queue: Mutex::new(None),
        }
    }

    /// Starts a pairing: restarts the edge stream at sequence 0 on `epoch`, rebases timestamps
    /// at `now_ticks`, and spawns the ordered drain task that sends frames over `transport`.
    ///
    /// Replacing the queue drops the previous pairing's sender, so its drain task exits. The
    /// Station builds a fresh replayer per session, so it accepts the restarted sequence.
    /// Must be called from within a tokio runtime.
    pub fn begin(&self, epoch: u16, now_ticks: u64, transport: Arc<EdgeTransport>) {
        let (tx, mut rx) = unbounded_channel::<RwkPaddleFrame>();
        {
            let mut sender = self.sender.lock().unwrap();
            sender.begin_epoch(epoch, now_ticks);
            *self.queue.lock().unwrap() = Some(tx);
        }
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                let _ = transport.send_frame(&frame).await;
            }
        });
    }

    /// Ends the current pairing: drops the queue so its drain task exits. Later pushes are
    /// dropped until the next [`Self::begin`].
    pub fn detach(&self) {
        *self.queue.lock().unwrap() = None;
    }

    /// Records one key transition from any edge source. It gets the next session sequence
    /// and is queued for the tunnel in that same order. This never blocks on the network, so
    /// it is safe on the time-critical keyer thread.
    pub fn push_edge(&self, key_down: bool, timestamp_ticks: u64) {
        let event = EdgeEvent { timestamp_ticks, key_down, source: EdgeSource::Paddle };
        let mut sender = self.sender.lock().unwrap();
        let frame = sender.frame_for(event);
        if let Some(tx) = self.queue.lock().unwrap().as_ref() {
            let _ = tx.send(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK_HZ: u64 = 1_000_000_000;

    /// Two loopback transports stand in for the Client and Station ends of the tunnel.
    async fn loopback_pair() -> (Arc<EdgeTransport>, Arc<EdgeTransport>) {
        let client = EdgeTransport::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let station = EdgeTransport::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        client.set_peer(station.local_addr().unwrap()).await;
        station.set_peer(client.local_addr().unwrap()).await;
        (Arc::new(client), Arc::new(station))
    }

    #[tokio::test]
    async fn every_source_shares_one_sequence_in_send_order() {
        let (client, station) = loopback_pair().await;
        let session = EdgeSession::new(TICK_HZ, 0);
        session.begin(1, 0, Arc::clone(&client));

        // A send span and a paddle both push through the one session.
        session.push_edge(true, 0);
        session.push_edge(false, TICK_HZ / 20);
        session.push_edge(true, TICK_HZ / 10);

        let mut seen = Vec::new();
        for _ in 0..3 {
            let frame = station.recv_frame().await.unwrap().expect("a frame");
            seen.push(frame.edges()[0].sequence);
        }
        assert_eq!(seen, vec![0, 1, 2], "frames must leave in the order they were keyed");
    }

    #[tokio::test]
    async fn begin_restarts_the_sequence_for_a_new_pairing() {
        let (client, station) = loopback_pair().await;
        let session = EdgeSession::new(TICK_HZ, 0);
        session.begin(1, 0, Arc::clone(&client));
        session.push_edge(true, 0);
        let _ = station.recv_frame().await.unwrap();

        session.begin(1, TICK_HZ, Arc::clone(&client));
        session.push_edge(true, TICK_HZ);
        let frame = station.recv_frame().await.unwrap().expect("a frame");
        assert_eq!(frame.edges()[0].sequence, 0, "a new pairing restarts at 0");
        assert_eq!(frame.edges()[0].timestamp_ms, 0, "and rebases timing on its start");
    }

    #[test]
    fn pushes_before_begin_are_dropped_not_panicking() {
        let session = EdgeSession::new(TICK_HZ, 0);
        session.push_edge(true, 0);
        session.detach();
        session.push_edge(false, TICK_HZ / 20);
    }
}
