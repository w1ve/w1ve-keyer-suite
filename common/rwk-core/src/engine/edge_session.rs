//! The Client-side edge path shared by every network keyer app.
//!
//! Every edge source on a Client (the paddle poller, the type-ahead send worker, and any
//! future source) must feed **one** session-long [`EdgeSender`], so sequence numbers and
//! timestamps advance together across the whole session. The Station keeps a single
//! `last_sequence` per session, so a second sender restarting at 0 would have every frame
//! discarded as a duplicate. This type owns that sender, restarts it once per pairing, and
//! carries finished frames to the tunnel over one ordered FIFO. Apps call [`EdgeSession::push_edge`]
//! from their keying paths and never build senders or queues themselves.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::engine::edge_sender::EdgeSender;
use crate::engine::keying::EdgeEvent;
use crate::engine::network::EdgeTransport;
use crate::primitives::EdgeSource;
use crate::protocol::edge::RwkPaddleFrame;

/// An already-cancelled edge drain that a caller can await before retiring its carrier.
///
/// Dropping this handle is safe: cancellation has already been requested. Call [`Self::wait`]
/// when a carrier needs to ensure that no in-flight frame callback remains before it sends a
/// terminal protocol message.
pub struct EdgeDrain {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl EdgeDrain {
    /// Waits until the detached drain has stopped. Cancellation join errors are expected when
    /// the active callback was awaiting carrier I/O.
    pub async fn wait(mut self) {
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

struct ActiveEdgeDrain {
    accepting: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl ActiveEdgeDrain {
    fn cancel(self) -> EdgeDrain {
        let Self { accepting, task } = self;
        accepting.store(false, Ordering::Release);
        task.abort();
        EdgeDrain { task: Some(task) }
    }
}

impl std::fmt::Debug for ActiveEdgeDrain {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActiveEdgeDrain")
            .field("accepting", &self.accepting.load(Ordering::Relaxed))
            .finish()
    }
}

/// The session-scoped edge sender plus its ordered outbound queue.
///
/// Lock order is always `sender`, then `latest_frame`, then `queue`, then `drain`, so pairing
/// transitions and [`Self::push_edge`] cannot deadlock. The sender lock is held across publishing
/// the cached frame and queue entry, so the first edge of a new ESP session cannot be erased
/// before its 250 ms safety refresh starts.
#[derive(Debug)]
pub struct EdgeSession {
    sender: Mutex<EdgeSender>,
    queue: Mutex<Option<UnboundedSender<RwkPaddleFrame>>>,
    /// The one cancellable task draining `queue` for the active pairing.
    drain: Mutex<Option<ActiveEdgeDrain>>,
    /// Most recent complete frame, repeated by ESP transport safety refreshes until its terminal
    /// state is observed or the session ends.
    latest_frame: Mutex<Option<RwkPaddleFrame>>,
    /// Authoritative current output state for local/UI reporting.
    key_down: AtomicBool,
}

impl EdgeSession {
    /// Creates an idle session timed on a clock of `frequency` ticks per second, anchored at
    /// `now_ticks`. Nothing is sent until [`Self::begin`] is called for a pairing.
    #[must_use]
    pub fn new(frequency: u64, now_ticks: u64) -> Self {
        Self {
            sender: Mutex::new(EdgeSender::new(1, frequency, now_ticks)),
            queue: Mutex::new(None),
            drain: Mutex::new(None),
            latest_frame: Mutex::new(None),
            key_down: AtomicBool::new(false),
        }
    }

    /// Starts a pairing: restarts the edge stream at sequence 0 on `epoch`, rebases timestamps
    /// at `now_ticks`, and spawns the ordered drain task that sends frames over `transport`.
    ///
    /// Replacing a pairing cancels the previous drain and drops its queue. The Station builds a
    /// fresh replayer per session, so it accepts the restarted sequence.
    /// Must be called from within a tokio runtime.
    pub fn begin(&self, epoch: u16, now_ticks: u64, transport: Arc<EdgeTransport>) {
        self.begin_with_sender(epoch, now_ticks, move |frame| {
            let transport = Arc::clone(&transport);
            async move {
                let _ = transport.send_frame(&frame).await;
            }
        });
    }

    /// Starts a pairing using any ordered frame sender. This is the extension seam for the
    /// trusted-LAN ESP Player transport: edge sequence/timestamp generation stays exactly
    /// the same, while the carrier sends the resulting frame.
    pub fn begin_with_sender<F, Fut>(&self, epoch: u16, now_ticks: u64, sender: F)
    where
        F: Fn(RwkPaddleFrame) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (tx, mut rx) = unbounded_channel::<RwkPaddleFrame>();
        let accepting = Arc::new(AtomicBool::new(true));
        let task_accepting = Arc::clone(&accepting);
        // Spawn before publishing `tx`: the task cannot receive a frame until the queue and its
        // task record have been installed together below, closing the begin/detach race.
        let task = tokio::spawn(async move {
            while task_accepting.load(Ordering::Acquire) {
                let Some(frame) = rx.recv().await else { break };
                // Dropping the sender alone leaves buffered unbounded messages readable. Check
                // cancellation again before each callback so a detached queue cannot drain them.
                if !task_accepting.load(Ordering::Acquire) {
                    break;
                }
                sender(frame).await;
            }
        });
        {
            let mut edge_sender = self.sender.lock().unwrap();
            edge_sender.begin_epoch(epoch, now_ticks);
            self.key_down.store(false, Ordering::Relaxed);
            // Reset the old session's refresh before publishing the new queue. A first edge
            // can then only install a frame after its new sender/queue are fully visible.
            *self.latest_frame.lock().unwrap() = None;
            let mut queue = self.queue.lock().unwrap();
            let mut drain = self.drain.lock().unwrap();
            // `begin` is also a replacement boundary. It must cancel an old callback before
            // publishing this queue, even if an older caller did not await its detach handle.
            if let Some(previous) = drain.replace(ActiveEdgeDrain { accepting, task }) {
                let _ = previous.cancel();
            }
            *queue = Some(tx);
        }
    }

    /// Ends the current pairing and cancels its ordered drain. Later pushes are dropped until
    /// the next [`Self::begin`]. Existing callers that do not need to synchronize a terminal
    /// carrier action can use this fire-and-forget form.
    pub fn detach(&self) {
        let _ = self.detach_with_drain();
    }

    /// Ends the current pairing and returns its already-cancelled drain. The handle owns only
    /// the detached task, so a caller may await [`EdgeDrain::wait`] without touching a queue
    /// installed by a replacement.
    pub fn detach_with_drain(&self) -> EdgeDrain {
        // Follow the same lock order as `begin_with_sender` and `push_edge`, so a final frame
        // cannot repopulate the refresh cache after this session has been detached.
        let _edge_sender = self.sender.lock().unwrap();
        *self.latest_frame.lock().unwrap() = None;
        let mut queue = self.queue.lock().unwrap();
        let drain = self.drain.lock().unwrap().take().map(ActiveEdgeDrain::cancel);
        *queue = None;
        self.key_down.store(false, Ordering::Relaxed);
        drain.unwrap_or(EdgeDrain { task: None })
    }

    /// Returns the newest complete RWK-PADDLE frame. The ESP transport periodically re-sends it
    /// as an edge refresh, so a lost terminal key-up is repaired without a separate raw-state
    /// packet directly driving GPIO.
    #[must_use]
    pub fn latest_frame(&self) -> Option<RwkPaddleFrame> {
        self.latest_frame.lock().unwrap().clone()
    }

    /// Returns the last state supplied to [`Self::push_edge`] for local/UI reporting. ESP
    /// safety refreshes repeat [`Self::latest_frame`] rather than sending this raw state.
    #[must_use]
    pub fn key_down(&self) -> bool {
        self.key_down.load(Ordering::Relaxed)
    }

    /// Records one key transition from any edge source. It gets the next session sequence
    /// and is queued for the tunnel in that same order. This never blocks on the network, so
    /// it is safe on the time-critical keyer thread.
    pub fn push_edge(&self, key_down: bool, timestamp_ticks: u64) {
        self.key_down.store(key_down, Ordering::Relaxed);
        let event = EdgeEvent { timestamp_ticks, key_down, source: EdgeSource::Paddle };
        let mut sender = self.sender.lock().unwrap();
        let frame = sender.frame_for(event);
        // Publish the complete frame and queue it while the session sender remains locked. This
        // makes the cache generation and the FIFO generation indivisible for the first edge.
        let mut latest = self.latest_frame.lock().unwrap();
        let replace = latest
            .as_ref()
            .and_then(|prior| prior.edges().first())
            .map_or(true, |prior| frame.edges()[0].sequence >= prior.sequence);
        if replace {
            *latest = Some(frame.clone());
        }
        if let Some(tx) = self.queue.lock().unwrap().as_ref() {
            let _ = tx.send(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::{oneshot, Notify};

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

    #[test]
    fn latest_complete_frame_retains_terminal_state_for_esp_refresh() {
        let session = EdgeSession::new(TICK_HZ, 0);
        session.push_edge(true, 0);
        let down = session.latest_frame().expect("first complete frame");
        assert_eq!(down.edges()[0].sequence, 0);
        assert!(down.edges()[0].key_down());

        session.push_edge(false, TICK_HZ / 20);
        let up = session.latest_frame().expect("terminal complete frame");
        assert_eq!(up.edges()[0].sequence, 1);
        assert!(!up.edges()[0].key_down());
        assert_eq!(up.edges().len(), 2, "the complete refresh includes redundant history");
    }

    #[tokio::test]
    async fn first_frame_after_begin_remains_available_for_esp_refresh() {
        let (client, station) = loopback_pair().await;
        let session = EdgeSession::new(TICK_HZ, 0);
        // A cached frame from an idle/previous stream must be cleared before the new queue exists.
        session.push_edge(true, 0);
        assert!(session.latest_frame().is_some());

        session.begin(2, TICK_HZ, Arc::clone(&client));
        assert!(session.latest_frame().is_none());
        session.push_edge(true, TICK_HZ);

        let cached = session.latest_frame().expect("new session first frame remains cached");
        let sent = station.recv_frame().await.unwrap().expect("new session first frame sends");
        assert_eq!(cached, sent);
        assert_eq!(cached.edges()[0].sequence, 0);
        assert_eq!(cached.edges()[0].timestamp_ms, 0);
    }

    #[tokio::test]
    async fn detach_cancels_buffered_frame_callbacks() {
        let session = EdgeSession::new(TICK_HZ, 0);
        let (first_callback_started, first_callback_started_rx) = oneshot::channel();
        let first_callback_started = Arc::new(Mutex::new(Some(first_callback_started)));
        let first_callback = Arc::new(AtomicBool::new(true));
        let release_first_callback = Arc::new(Notify::new());
        let (sent, mut received) = unbounded_channel::<u32>();

        session.begin_with_sender(1, 0, {
            let first_callback_started = Arc::clone(&first_callback_started);
            let first_callback = Arc::clone(&first_callback);
            let release_first_callback = Arc::clone(&release_first_callback);
            move |frame| {
                let edge_sequence = frame.edges()[0].sequence;
                let first_callback_started = Arc::clone(&first_callback_started);
                let first_callback = Arc::clone(&first_callback);
                let release_first_callback = Arc::clone(&release_first_callback);
                let sent = sent.clone();
                async move {
                    if first_callback.swap(false, Ordering::AcqRel) {
                        let started = first_callback_started.lock().unwrap().take();
                        let _ = started.expect("first callback start signal").send(());
                        release_first_callback.notified().await;
                    }
                    let _ = sent.send(edge_sequence);
                }
            }
        });

        session.push_edge(true, 0);
        first_callback_started_rx.await.expect("first callback starts");
        // These frames are already buffered behind a callback that cannot yet finish.
        session.push_edge(false, TICK_HZ / 20);
        session.push_edge(true, TICK_HZ / 10);

        let drain = session.detach_with_drain();
        release_first_callback.notify_waiters();
        tokio::time::timeout(Duration::from_millis(100), drain.wait())
            .await
            .expect("cancelling the drain completes");
        // The already-running first callback may have completed at the exact cancellation
        // boundary, but frames queued behind it must never be called after detach.
        while let Ok(sequence) = received.try_recv() {
            assert_eq!(sequence, 0, "only the in-flight first callback may have run");
        }
        assert!(
            matches!(
                tokio::time::timeout(Duration::from_millis(20), received.recv()).await,
                Err(_) | Ok(None)
            ),
            "detach must discard queued frame callbacks"
        );
    }
}
