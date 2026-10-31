//! Peer-loss and reconnect policy shared by every link end.
//!
//! * A peer that sends nothing for [`PEER_SILENCE_TIMEOUT`] is treated as gone at once, so the
//!   local end tears its link down rather than keying into a dead path.
//! * After a link is lost, the Keyer retries for [`RECONNECT_WINDOW`] and then reports
//!   disconnected. A [`ReconnectWindow`] tracks that budget, and [`ReconnectBudget`] drives the
//!   budget over a supervisor loop with a stability threshold so a pair-then-die flap cannot
//!   keep the Keyer reconnecting forever.
//!
//! The async helpers ([`watch_edge_liveness`], [`connect_with_relay_retry`]) are the identical
//! building blocks every networked controller uses, so the Client/Station reconnection and the
//! Router Client/Station reconnection are literally the same code path.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::engine::network::EdgeTransport;
use crate::engine::tunnel::{self, Connected};
use crate::config::Config;

/// How long a freshly paired link must stay continuously up before the reconnect budget is
/// considered "spent well" and reset. Without this, a peer that completes a handshake and then
/// dies instantly reopens a full [`RECONNECT_WINDOW`] every cycle, so the Keyer can flap-reconnect
/// forever instead of giving up. The threshold means a link must prove it is real before it earns
/// a fresh budget.
pub const LINK_STABLE_THRESHOLD: Duration = Duration::from_secs(5);

/// How often a paired link beacons a liveness heartbeat (and a ping) to its peer. It must stay
/// well inside the peer's fail-safe window.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);

/// The RTT readout refreshes once a second: every this many heartbeat ticks.
pub const RTT_EVERY_TICKS: u32 = 4;

/// A one-shot retry delay for the relay rendezvous. The first rendezvous can race the relay or
/// the peer's arrival just after a restart; a single short retry rides that out before the caller
/// reports the relay unreachable.
pub const RELAY_RETRY_DELAY: Duration = Duration::from_millis(750);

/// How long a link end waits without any inbound datagram before declaring the peer dead.
pub const PEER_SILENCE_TIMEOUT: Duration = Duration::from_millis(1500);

/// How long the Keyer keeps trying to re-establish a lost or unreachable link before it gives
/// up and reports disconnected.
pub const RECONNECT_WINDOW: Duration = Duration::from_secs(15);

/// Whether the peer has been silent for at least [`PEER_SILENCE_TIMEOUT`] as of `now`.
pub fn peer_silent(last_heard: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_heard) >= PEER_SILENCE_TIMEOUT
}

/// The reconnect budget: a fixed window opened when a link is lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectWindow {
    started: Instant,
    span: Duration,
}

impl ReconnectWindow {
    /// Opens a [`RECONNECT_WINDOW`] starting at `now`.
    pub fn open(now: Instant) -> Self {
        Self::with_span(now, RECONNECT_WINDOW)
    }

    /// Opens a window of an explicit length starting at `now`.
    pub fn with_span(now: Instant, span: Duration) -> Self {
        Self { started: now, span }
    }

    /// Whether the window has run out as of `now`.
    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= self.span
    }

    /// Time left in the window as of `now`; zero once it has run out.
    pub fn remaining(&self, now: Instant) -> Duration {
        self.span.saturating_sub(now.saturating_duration_since(self.started))
    }
}

/// Drives the reconnect budget across a supervisor loop, with the stability threshold that stops
/// pair-then-die flapping.
///
/// The budget is a single [`ReconnectWindow`] that is opened lazily on the first attempt and
/// re-opened only after a link has stayed up for at least [`LINK_STABLE_THRESHOLD`]. A link that
/// pairs and dies immediately therefore keeps consuming the *same* window instead of resetting it,
/// so the Keyer gives up after [`RECONNECT_WINDOW`] of churn rather than reconnecting forever.
#[derive(Debug, Default)]
pub struct ReconnectBudget {
    window: Option<ReconnectWindow>,
}

impl ReconnectBudget {
    /// A fresh, unopened budget.
    #[must_use]
    pub fn new() -> Self {
        Self { window: None }
    }

    /// Time left in the budget as of `now`, opening the window on first use. Returns
    /// [`Duration::ZERO`] once the window has run out.
    pub fn remaining(&mut self, now: Instant) -> Duration {
        self.window
            .get_or_insert_with(|| ReconnectWindow::open(now))
            .remaining(now)
    }

    /// Whether the budget has run out as of `now` (opening the window on first use).
    pub fn is_expired(&mut self, now: Instant) -> bool {
        self.remaining(now).is_zero()
    }

    /// Records that a link that came up at `connected_at` has now ended at `now`. If the link
    /// stayed up for at least [`LINK_STABLE_THRESHOLD`], the budget is reset (the next attempt
    /// opens a fresh window); otherwise the window is left running so a flap keeps consuming it.
    pub fn record_link_ended(&mut self, connected_at: Instant, now: Instant) {
        if now.saturating_duration_since(connected_at) >= LINK_STABLE_THRESHOLD {
            self.window = None;
        }
    }

    /// Forces the budget back to its fresh, unopened state (e.g. on an operator-driven reconnect).
    pub fn reset(&mut self) {
        self.window = None;
    }
}

/// Why an edge-liveness watch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// No inbound traffic from the peer within [`PEER_SILENCE_TIMEOUT`]; the peer is presumed gone.
    PeerLost,
    /// The edge receive stream ended (the tunnel/netstack closed) — a clean teardown.
    Closed,
}

/// Watches the live `edge` for peer liveness while beaconing this end's own heartbeat.
///
/// Runs two things concurrently against `edge`:
/// * a [`HEARTBEAT_INTERVAL`] sender: a heartbeat (so the peer can tell this end is up even when
///   idle) and a ping the peer answers so the round trip can be measured. Every
///   [`RTT_EVERY_TICKS`] ticks it reports the current RTT through `on_rtt`.
/// * an inbound watch: any datagram refreshes the deadline; [`PEER_SILENCE_TIMEOUT`] of silence
///   is [`Liveness::PeerLost`], and a closed stream is [`Liveness::Closed`].
///
/// Returns when the peer is lost or the stream closes. Keying out is unaffected — this only
/// observes; the caller decides whether to reconnect. This is the single implementation shared by
/// every networked controller.
pub async fn watch_edge_liveness<F>(edge: Arc<EdgeTransport>, mut on_rtt: F) -> Liveness
where
    F: FnMut(Option<u32>) + Send + 'static,
{
    let recv_edge = Arc::clone(&edge);
    let receiver = async move {
        loop {
            match tokio::time::timeout(PEER_SILENCE_TIMEOUT, recv_edge.recv_edge()).await {
                // Any inbound datagram (Some) or a dropped-but-received one (None) = alive.
                Ok(Ok(_)) => continue,
                // The stream ended (netstack stopped): a clean close.
                Ok(Err(_)) => return Liveness::Closed,
                // No datagram at all within the window: the peer is presumed gone.
                Err(_) => return Liveness::PeerLost,
            }
        }
    };

    let hb_edge = Arc::clone(&edge);
    let heartbeat = async move {
        let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
        let mut beats: u32 = 0;
        loop {
            tick.tick().await;
            let _ = hb_edge.send_heartbeat().await;
            let _ = hb_edge.send_ping().await;
            beats = beats.wrapping_add(1);
            if beats % RTT_EVERY_TICKS == 0 {
                let ms = hb_edge
                    .rtt()
                    .map(|d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX));
                on_rtt(ms);
            }
        }
    };

    // The heartbeat future never completes; the receiver decides the outcome.
    tokio::select! {
        outcome = receiver => outcome,
        _ = heartbeat => Liveness::Closed,
    }
}

/// Opens the tunnel for `config`, retrying the rendezvous once after [`RELAY_RETRY_DELAY`].
///
/// The first rendezvous can race the relay or the peer's arrival just after a restart — the
/// classic "stop the Player, immediately reconnect" failure. A single short retry rides that out.
/// Shared by every controller's connect/listen path so the behaviour is identical on both ends.
///
/// # Errors
///
/// Propagates the tunnel connect error from the *second* attempt if both fail.
pub async fn connect_with_relay_retry(config: &Config) -> crate::Result<Connected> {
    match tunnel::connect(config).await {
        Ok(connected) => Ok(connected),
        Err(_) => {
            tokio::time::sleep(RELAY_RETRY_DELAY).await;
            tunnel::connect(config).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_is_silent_only_after_the_timeout() {
        let heard = Instant::now();
        assert!(!peer_silent(heard, heard));
        assert!(!peer_silent(heard, heard + PEER_SILENCE_TIMEOUT - Duration::from_millis(1)));
        assert!(peer_silent(heard, heard + PEER_SILENCE_TIMEOUT));
        assert!(peer_silent(heard, heard + Duration::from_secs(30)));
    }

    #[test]
    fn silence_timeout_is_one_and_a_half_seconds() {
        assert_eq!(PEER_SILENCE_TIMEOUT, Duration::from_millis(1500));
    }

    #[test]
    fn reconnect_window_is_fifteen_seconds() {
        assert_eq!(RECONNECT_WINDOW, Duration::from_secs(15));
        let t0 = Instant::now();
        let w = ReconnectWindow::open(t0);
        assert!(!w.expired(t0));
        assert!(!w.expired(t0 + Duration::from_secs(14)));
        assert!(w.expired(t0 + Duration::from_secs(15)));
    }

    #[test]
    fn remaining_counts_down_and_stops_at_zero() {
        let t0 = Instant::now();
        let w = ReconnectWindow::with_span(t0, Duration::from_secs(15));
        assert_eq!(w.remaining(t0), Duration::from_secs(15));
        assert_eq!(w.remaining(t0 + Duration::from_secs(10)), Duration::from_secs(5));
        assert_eq!(w.remaining(t0 + Duration::from_secs(60)), Duration::ZERO);
    }

    #[test]
    fn a_fresh_window_restarts_the_budget() {
        let t0 = Instant::now();
        let first = ReconnectWindow::open(t0);
        assert!(first.expired(t0 + Duration::from_secs(20)));
        let second = ReconnectWindow::open(t0 + Duration::from_secs(20));
        assert!(!second.expired(t0 + Duration::from_secs(21)));
    }

    #[test]
    fn stable_threshold_is_five_seconds() {
        assert_eq!(LINK_STABLE_THRESHOLD, Duration::from_secs(5));
    }

    #[test]
    fn budget_opens_lazily_and_counts_down() {
        let t0 = Instant::now();
        let mut budget = ReconnectBudget::new();
        assert_eq!(budget.remaining(t0), RECONNECT_WINDOW);
        assert!(!budget.is_expired(t0 + Duration::from_secs(14)));
        assert!(budget.is_expired(t0 + Duration::from_secs(15)));
    }

    #[test]
    fn a_stable_link_resets_the_budget_but_a_flap_does_not() {
        let t0 = Instant::now();
        let mut budget = ReconnectBudget::new();
        // Open the window and burn 10 s of it.
        let _ = budget.remaining(t0);
        let connected_at = t0 + Duration::from_secs(1);

        // A flap: the link came up at +1 s and died at +2 s (only 1 s up, below the threshold).
        // The budget must keep running from t0, so it is still spent by +15 s.
        budget.record_link_ended(connected_at, t0 + Duration::from_secs(2));
        assert!(budget.is_expired(t0 + Duration::from_secs(15)));

        // A stable link: up for 5 s clears the budget, so a fresh window opens afterwards.
        let mut budget = ReconnectBudget::new();
        let _ = budget.remaining(t0);
        budget.record_link_ended(t0, t0 + LINK_STABLE_THRESHOLD);
        // Fresh window opened at +20 s is not expired a moment later.
        assert!(!budget.is_expired(t0 + Duration::from_secs(20)));
    }
}
