//! Peer-loss and reconnect policy shared by every link end.
//!
//! * A peer that sends nothing for [`PEER_SILENCE_TIMEOUT`] is treated as gone at once, so the
//!   local end tears its link down rather than keying into a dead path.
//! * After a link is lost, the Keyer retries for [`RECONNECT_WINDOW`] and then reports
//!   disconnected. A [`ReconnectWindow`] tracks that budget.

use std::time::{Duration, Instant};

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
}
