//! The logger-WinKeyer input — the heart of the standalone Keyer.
//!
//! W1VE Keyer *is* a software replacement for a hardware K1EL WinKeyer, so this input is not
//! an afterthought: a logging program (N1MM, etc.) running on the same PC opens a serial port
//! and speaks the WinKeyer2 host protocol to send CW macros. This module decodes that stream
//! into CW that keys the radio through the [`KeyingArbiter`]'s *local* source, alongside the
//! operator's paddle on the same output line.
//!
//! The decode/act/send logic is the proven Station logger host from the Keyer-Router, reused
//! unchanged: the WinKeyer host decoder, the version-reply handshake (so the logger believes
//! it is talking to real WK2 hardware), speed changes, immediate key, and buffered text keyed
//! as CW with [`EdgeScheduleBuilder`]/[`EdgeSchedule::play`]. The local source is held busy
//! for the span of a message so a gap between characters does not hand the line to the paddle
//! mid-transmission.
//!
//! The host owns no radio port itself; it shares the Keyer's single output through the
//! arbiter. Dropping it stops the thread and releases the logger port.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rwk_core::engine::keying::EdgeScheduleBuilder;
use rwk_core::engine::keying_arbiter::LocalKeyHandle;
use rwk_core::protocol::winkeyer_host::{Decoder as WinKeyerHostDecoder, HostEvent};
use rwk_core::timing::{Clock, MonotonicClock};

/// Default WPM until the logger sends a speed command.
const DEFAULT_WPM: u32 = 25;
/// Weight the logger CW is keyed at (loggers do not set weight; use standard 50%).
const WEIGHT: u32 = 50;
/// Serial baud for the logger link. WinKeyer uses 1200 8N2 on real hardware, but a virtual
/// serial pair ignores the rate; 1200 keeps a real cable interoperable.
const LOGGER_BAUD: u32 = 1200;
/// How long the reader blocks for bytes before looping to check the stop flag.
const READ_TIMEOUT: Duration = Duration::from_millis(50);

/// A callback the host calls to report state to the UI/log. Kept framework-free so the
/// controller can bridge it to Tauri events.
pub type HostReporter = Arc<dyn Fn(HostReport) + Send + Sync>;

/// Something the host wants the UI/log to know.
#[derive(Debug, Clone)]
pub enum HostReport {
    /// A human-readable log line.
    Log { level: String, message: String },
    /// The logger is actively sending CW (`true`) or has gone idle (`false`).
    Sending(bool),
}

/// A running logger-WinKeyer host. Dropping it stops the reader thread and closes the port.
pub struct WinKeyerHost {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    port_name: String,
}

impl WinKeyerHost {
    /// Opens `port_name` for the logger and starts decoding WinKeyer into CW on the arbiter's
    /// local source.
    ///
    /// `local` is the arbiter handle the generated CW keys through. `report` surfaces log
    /// lines and the sending indicator.
    ///
    /// # Errors
    ///
    /// Returns an error string if the serial port cannot be opened.
    pub fn open(
        port_name: &str,
        local: LocalKeyHandle,
        report: HostReporter,
    ) -> Result<Self, String> {
        let port = serialport::new(port_name, LOGGER_BAUD)
            .timeout(READ_TIMEOUT)
            .open()
            .map_err(|e| format!("open logger port {port_name}: {e}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(HostShared {
            local,
            report,
            wpm: AtomicU32::new(DEFAULT_WPM),
            queue: Mutex::new(VecDeque::new()),
            stop: Arc::clone(&stop),
        });

        let thread = std::thread::Builder::new()
            .name("wk-winkeyer".into())
            .spawn(move || run(port, shared))
            .map_err(|e| format!("spawn logger thread: {e}"))?;

        Ok(Self { stop, thread: Some(thread), port_name: port_name.to_string() })
    }

    /// The logger port name. Retained for parity with the Keyer-Router host and for logging;
    /// not currently read by the standalone controller.
    #[must_use]
    #[allow(dead_code)]
    pub fn port_name(&self) -> &str {
        &self.port_name
    }

    /// Stops the host and joins its thread. Called by `Drop`, but exposed so the controller
    /// can stop it explicitly (e.g. when the operator clears the logger port).
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for WinKeyerHost {
    fn drop(&mut self) {
        self.stop();
    }
}

/// State shared between the single host thread and its helpers.
struct HostShared {
    local: LocalKeyHandle,
    report: HostReporter,
    wpm: AtomicU32,
    /// Characters received from the logger, waiting to be keyed.
    queue: Mutex<VecDeque<char>>,
    stop: Arc<AtomicBool>,
}

impl HostShared {
    fn log(&self, level: &str, message: impl Into<String>) {
        (self.report)(HostReport::Log { level: level.into(), message: message.into() });
    }
}

/// The host thread: read → decode → act, looping until stopped.
fn run(mut port: Box<dyn serialport::SerialPort>, shared: Arc<HostShared>) {
    let mut decoder = WinKeyerHostDecoder::new();
    let mut events: Vec<HostEvent> = Vec::new();
    let mut read_buf = [0u8; 256];
    let clock = MonotonicClock;

    shared.log("info", format!("Logger input listening at {LOGGER_BAUD} baud."));

    while !shared.stop.load(Ordering::SeqCst) {
        // 1. Read whatever is available; a timeout is normal and just loops.
        match port.read(&mut read_buf) {
            Ok(0) => {}
            Ok(n) => {
                events.clear();
                decoder.feed(&read_buf[..n], &mut events);
                for event in events.drain(..) {
                    act_on(&shared, event);
                }
                // Write back any reply the decoder queued (version byte, echoes).
                let out = decoder.take_output();
                if !out.is_empty() {
                    let _ = port.write_all(&out);
                    let _ = port.flush();
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                shared.log("warn", format!("Logger port read error: {e}"));
                break;
            }
        }

        // 2. Drain any queued text as CW. This blocks the thread for the duration of the
        //    message (on the keyer timeline), which is fine: this thread does nothing else,
        //    and the arbiter serialises its line writes with the paddle source.
        drain_queue(&shared, &clock);
    }

    // Leave the local line released on the way out.
    shared.local.set_busy(false);
    shared.local.set_key(false);
    shared.local.set_ptt(false);
    shared.log("info", "Logger input stopped.");
}

/// Applies one decoded host event.
fn act_on(shared: &Arc<HostShared>, event: HostEvent) {
    match event {
        HostEvent::HostOpened => shared.log("info", "Logger opened host mode (WinKeyer2)."),
        HostEvent::HostClosed => shared.log("info", "Logger closed host mode."),
        HostEvent::SetSpeed(wpm) => {
            shared.wpm.store(wpm, Ordering::SeqCst);
            shared.log("debug", format!("Logger speed {wpm} WPM."));
        }
        HostEvent::KeyImmediate(down) => {
            // Immediate key: the logger is tuning or sending a straight key-down. Take the
            // local line now and hold busy while down so the paddle cannot cut in.
            shared.local.set_busy(down);
            shared.local.set_key(down);
        }
        HostEvent::Text(ch) => {
            shared.queue.lock().unwrap_or_else(|e| e.into_inner()).push_back(ch);
        }
        HostEvent::ClearBuffer => {
            shared.queue.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
        HostEvent::Backspace => {
            shared.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_back();
        }
    }
}

/// Keys any queued characters as CW through the arbiter's local source.
///
/// Takes the whole pending queue, builds one schedule for it at the current speed, and plays
/// it, marking the local source busy for the span so the paddle is masked throughout. New
/// characters that arrive while sending are picked up on the next loop.
fn drain_queue(shared: &Arc<HostShared>, clock: &MonotonicClock) {
    let text: String = {
        let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.is_empty() {
            return;
        }
        q.drain(..).collect()
    };

    let wpm = shared.wpm.load(Ordering::SeqCst);
    let start = clock.now();
    let schedule = match EdgeScheduleBuilder::build(&text, wpm, WEIGHT, start, clock.frequency()) {
        Ok(s) => s,
        Err(e) => {
            shared.log("warn", format!("Logger CW build failed: {e}"));
            return;
        }
    };
    if schedule.edges.is_empty() {
        return;
    }

    // Claim the line for the whole message.
    shared.local.set_busy(true);
    (shared.report)(HostReport::Sending(true));

    let local = shared.local.clone();
    let stop = Arc::clone(&shared.stop);
    schedule.play(
        clock,
        &mut |edge| local.set_key(edge.key_down),
        &|| stop.load(Ordering::SeqCst),
    );

    // Message done (or aborted): release the key and the busy hold.
    shared.local.set_key(false);
    shared.local.set_busy(false);
    (shared.report)(HostReport::Sending(false));
}

#[cfg(test)]
mod tests {
    // The host's serial I/O cannot be exercised without a port, but the decode→act mapping is
    // covered by the decoder's own tests in rwk-core (winkeyer_host). Here we only check the
    // small pure helpers that do not need hardware.
    use super::*;

    #[test]
    fn constants_are_sane() {
        assert!(DEFAULT_WPM >= 5 && DEFAULT_WPM <= 60);
        assert_eq!(WEIGHT, 50);
    }
}
