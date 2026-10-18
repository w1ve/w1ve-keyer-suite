//! The WinKeyer logger input: a serial port a logging program (N1MM+, DXLog, Win-Test) speaks the
//! WinKeyer2 host protocol to, decoded into CW that keys a radio through a [`LocalKey`] output.
//!
//! This is the one implementation shared by the Remote Keyer's Player, the Station and the
//! stand-alone W1VE Keyer. It follows the C# emulator's `KeyerCore`, which N1MM accepts:
//!
//! * Text is batched until the logger goes quiet ([`FLUSH_QUIET`]). The batch is then echoed back,
//!   followed by an idle status byte (`0xC0`), and queued for CW.
//! * Request Status is answered at once. Busy (`0xC5`) is reported while text waits to be flushed,
//!   idle (`0xC0`) otherwise. Status polls are never held behind CW, so a long message cannot hang
//!   the logger.
//! * CW plays on its own thread. The reader thread only decodes, answers the port and batches text.
//! * Clear Buffer and Host Close drop the batch and the queue, and abort CW in progress.
//!
//! The reader and CW threads are separate for exactly that last reason: a status poll during a long
//! message must still get its answer.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::engine::keying::EdgeScheduleBuilder;
use crate::engine::keying_arbiter::LocalKeyHandle;
use crate::protocol::winkeyer_host::{Decoder, HostEvent};
use crate::timing::{Clock, MonotonicClock};

/// Baud for the logger link (WinKeyer2 class). A virtual serial pair ignores the rate.
pub const LOGGER_BAUD: u32 = 1200;
/// WPM until the logger sends a speed command.
const DEFAULT_WPM: u32 = 25;
/// Weight the logger's CW is keyed at (loggers do not set weight; 50% is standard).
const WEIGHT: u32 = 50;
/// How long the reader blocks for bytes before it checks the stop flag and flushes its batch.
const READ_TIMEOUT: Duration = Duration::from_millis(5);
/// Quiet time after the logger's last character before its batch is flushed.
const FLUSH_QUIET: Duration = Duration::from_millis(5);
/// How long the CW thread waits for work before it checks the stop flag.
const QUEUE_WAIT: Duration = Duration::from_millis(50);

/// A callback the host uses to report to its owner: a log line, or the sending indicator.
pub type HostReporter = Arc<dyn Fn(HostReport) + Send + Sync>;

/// Something the host wants its owner to know.
#[derive(Debug, Clone)]
pub enum HostReport {
    /// A human-readable log line.
    Log {
        /// Severity: "info", "warn" or "debug".
        level: String,
        /// The message text.
        message: String,
    },
    /// The host is actively sending CW (`true`) or has gone idle (`false`).
    Sending(bool),
    /// The logger port failed while reading. The reader has stopped; reselecting the port resumes it.
    ReadFailed(String),
}

/// The keying output the logger drives. [`LocalKeyHandle`] implements it; tests use a double.
pub trait LocalKey: Send + Sync + 'static {
    /// Keys the line from the logger.
    fn set_key(&self, key_down: bool);
    /// Holds the logger's priority for the span of a message, so a gap between characters does
    /// not hand the line back to another source.
    fn set_busy(&self, busy: bool);
    /// Asserts or releases PTT from the logger.
    fn set_ptt(&self, asserted: bool);
}

impl LocalKey for LocalKeyHandle {
    fn set_key(&self, key_down: bool) {
        LocalKeyHandle::set_key(self, key_down);
    }
    fn set_busy(&self, busy: bool) {
        LocalKeyHandle::set_busy(self, busy);
    }
    fn set_ptt(&self, asserted: bool) {
        LocalKeyHandle::set_ptt(self, asserted);
    }
}

/// Where a logger's text goes when the host does not key a local output itself. The Remote Keyer
/// Client sends its text over the tunnel this way. The host still answers the logger: it echoes the
/// text, reports status, and batches text exactly as it does for a local key.
pub trait TextSink: Send + Sync + 'static {
    /// The speed flushed text is sent at.
    fn speed(&self) -> u32;
    /// The logger set the speed (WinKeyer Set Speed).
    fn set_speed(&self, wpm: u32);
    /// The logger opened (`true`) or closed (`false`) host mode.
    fn host_mode(&self, open: bool);
    /// Queues flushed text, to be sent at `wpm`.
    fn send_text(&self, text: &str, wpm: u32);
    /// Drops queued text and stops sending (Clear Buffer, Host Close).
    fn abort(&self);
}

/// The local output of a text-sink host: it keys nothing.
struct NoLocalKey;

impl LocalKey for NoLocalKey {
    fn set_key(&self, _key_down: bool) {}
    fn set_busy(&self, _busy: bool) {}
    fn set_ptt(&self, _asserted: bool) {}
}

/// A running logger-WinKeyer host. Dropping it stops both threads and releases the port.
pub struct LoggerHost {
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    port_name: String,
}

impl LoggerHost {
    /// Opens `port_name` for the logger and starts decoding WinKeyer into CW on `local`.
    ///
    /// # Errors
    ///
    /// Returns an error string if the serial port cannot be opened or cloned, or a thread cannot
    /// be spawned.
    pub fn open(port_name: &str, local: impl LocalKey, report: HostReporter) -> Result<Self, String> {
        Self::open_with(port_name, Arc::new(local), None, report)
    }

    /// Opens `port_name` for a logger whose text goes to `sink` instead of a local key. The host
    /// still answers the logger and echoes its text.
    ///
    /// # Errors
    ///
    /// As [`LoggerHost::open`].
    pub fn open_text(
        port_name: &str,
        sink: impl TextSink,
        report: HostReporter,
    ) -> Result<Self, String> {
        Self::open_with(port_name, Arc::new(NoLocalKey), Some(Arc::new(sink)), report)
    }

    fn open_with(
        port_name: &str,
        local: Arc<dyn LocalKey>,
        text: Option<Arc<dyn TextSink>>,
        report: HostReporter,
    ) -> Result<Self, String> {
        let port = serialport::new(port_name, LOGGER_BAUD)
            .timeout(READ_TIMEOUT)
            .open()
            .map_err(|e| format!("open logger port {port_name}: {e}"))?;
        let writer = port
            .try_clone()
            .map_err(|e| format!("clone logger port {port_name}: {e}"))?;
        Self::spawn(port_name, Box::new(port), Box::new(writer), local, text, report)
    }

    /// Starts the reader and CW threads over the given halves. [`LoggerHost::open`] passes the
    /// serial port; tests pass in-memory halves.
    fn spawn(
        port_name: &str,
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        local: Arc<dyn LocalKey>,
        text: Option<Arc<dyn TextSink>>,
        report: HostReporter,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            local,
            text,
            report,
            stop: Arc::clone(&stop),
            wpm: AtomicU32::new(DEFAULT_WPM),
            queue: Mutex::new(VecDeque::new()),
            queue_cv: Condvar::new(),
            abort_gen: AtomicU64::new(0),
            sending: AtomicBool::new(false),
            writer: Mutex::new(writer),
        });

        let reader_shared = Arc::clone(&shared);
        let reader_thread = std::thread::Builder::new()
            .name("winkeyer-logger".into())
            .spawn(move || read_loop(reader, reader_shared))
            .map_err(|e| format!("spawn logger thread: {e}"))?;

        let cw_shared = Arc::clone(&shared);
        let cw_thread = match std::thread::Builder::new()
            .name("winkeyer-logger-cw".into())
            .spawn(move || cw_loop(cw_shared))
        {
            Ok(handle) => handle,
            Err(e) => {
                stop.store(true, Ordering::SeqCst);
                let _ = reader_thread.join();
                return Err(format!("spawn logger CW thread: {e}"));
            }
        };

        Ok(Self {
            stop,
            shared,
            threads: vec![reader_thread, cw_thread],
            port_name: port_name.to_string(),
        })
    }

    /// The logger port name.
    #[must_use]
    pub fn port_name(&self) -> &str {
        &self.port_name
    }

    /// Stops both threads and releases the port. Called by `Drop`.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.shared.queue_cv.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Drop for LoggerHost {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Text the logger has sent that has not been flushed yet. Only the reader thread touches it.
#[derive(Default)]
struct Batch {
    pending: Vec<char>,
    last_text: Option<Instant>,
}

/// State shared between the reader thread and the CW thread.
struct Shared {
    local: Arc<dyn LocalKey>,
    /// Where text goes instead of `local`, when the host runs with a [`TextSink`].
    text: Option<Arc<dyn TextSink>>,
    report: HostReporter,
    stop: Arc<AtomicBool>,
    /// The speed the next flushed batch is keyed at.
    wpm: AtomicU32,
    /// Flushed text waiting for CW, each character with the WPM it was flushed at.
    queue: Mutex<VecDeque<(char, u32)>>,
    queue_cv: Condvar,
    /// Bumped by Clear Buffer and Host Close. CW in progress compares it and stops.
    abort_gen: AtomicU64,
    /// True while CW is playing.
    sending: AtomicBool,
    /// The logger's port, for writes. Both threads write here, one write at a time, so echoes and
    /// status bytes are never interleaved on the wire.
    writer: Mutex<Box<dyn Write + Send>>,
}

impl Shared {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn log(&self, level: &str, message: impl Into<String>) {
        (self.report)(HostReport::Log { level: level.into(), message: message.into() });
    }

    /// Writes bytes to the logger. A failed write is logged, not swallowed.
    fn write(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut writer = lock(&self.writer);
        let result = writer.write_all(bytes).and_then(|()| writer.flush());
        if let Err(e) = result {
            self.log("warn", format!("Logger write failed: {e}"));
        }
    }
}

/// Locks a mutex, recovering the data if another thread panicked while holding it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The reader thread: read, decode, act, reply, and flush the batch once the logger is quiet.
fn read_loop(mut reader: Box<dyn Read + Send>, shared: Arc<Shared>) {
    let mut decoder = Decoder::new();
    let mut events: Vec<HostEvent> = Vec::new();
    let mut batch = Batch::default();
    let mut buf = [0u8; 256];

    shared.log("info", format!("Logger input listening at {LOGGER_BAUD} baud."));

    while !shared.stopped() {
        match reader.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                events.clear();
                decoder.feed(&buf[..n], &mut events);
                for event in events.drain(..) {
                    act_on(&shared, &mut batch, event);
                }
                // Any reply the decoder queued (the Admin Open version byte, Admin Echo).
                shared.write(&decoder.take_output());
            }
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {}
            Err(e) => {
                shared.log(
                    "warn",
                    format!("Logger port read error; logger input stopped (reselect the port to resume): {e}"),
                );
                (shared.report)(HostReport::ReadFailed(e.to_string()));
                break;
            }
        }
        flush_if_quiet(&shared, &mut batch);
    }

    shared.local.set_busy(false);
    shared.local.set_key(false);
    shared.local.set_ptt(false);
    shared.log("info", "Logger input stopped.");
}

/// Applies one decoded WinKeyer event.
fn act_on(shared: &Shared, batch: &mut Batch, event: HostEvent) {
    match event {
        HostEvent::HostOpened => {
            shared.log("info", "Logger opened host mode (WinKeyer2).");
            if let Some(text) = &shared.text {
                text.host_mode(true);
            }
        }
        HostEvent::HostClosed => {
            shared.log("info", "Logger closed host mode.");
            if let Some(text) = &shared.text {
                text.host_mode(false);
            }
            clear_all(shared, batch);
        }
        HostEvent::SetSpeed(wpm) => match &shared.text {
            Some(text) => text.set_speed(wpm),
            None => {
                shared.wpm.store(wpm, Ordering::SeqCst);
                shared.log("debug", format!("Logger speed {wpm} WPM."));
            }
        },
        HostEvent::KeyImmediate(down) => {
            // An immediate key: take the line now, and hold it while down.
            shared.local.set_busy(down);
            shared.local.set_key(down);
        }
        HostEvent::Text(ch) => {
            batch.pending.push(ch);
            batch.last_text = Some(Instant::now());
        }
        HostEvent::ClearBuffer => clear_all(shared, batch),
        HostEvent::Backspace => {
            if batch.pending.pop().is_none() {
                lock(&shared.queue).pop_back();
            }
        }
        HostEvent::RequestStatus => {
            // Busy while the logger's text is still waiting to be flushed, idle otherwise. This is
            // the rule the Client uses, which N1MM is known to accept.
            let waiting = !batch.pending.is_empty();
            shared.write(&[Decoder::status_byte(waiting, waiting)]);
        }
    }
}

/// Drops the batch and the queue, and aborts CW in progress.
fn clear_all(shared: &Shared, batch: &mut Batch) {
    batch.pending.clear();
    batch.last_text = None;
    lock(&shared.queue).clear();
    shared.abort_gen.fetch_add(1, Ordering::SeqCst);
    shared.queue_cv.notify_all();
    if let Some(text) = &shared.text {
        text.abort();
    }
}

/// Once the logger has been quiet for [`FLUSH_QUIET`], echoes the batch, reports idle, and queues
/// the text for CW at the current speed. Echo first, as the C# emulator does at its flush.
fn flush_if_quiet(shared: &Shared, batch: &mut Batch) {
    let quiet = batch.last_text.is_some_and(|t| t.elapsed() >= FLUSH_QUIET);
    if batch.pending.is_empty() || !quiet {
        return;
    }
    let text: String = batch.pending.drain(..).collect();
    batch.last_text = None;

    let mut echo: Vec<u8> = text.bytes().collect();
    echo.push(Decoder::status_byte(false, false));
    shared.write(&echo);

    match &shared.text {
        Some(sink) => sink.send_text(&text, sink.speed()),
        None => {
            let wpm = shared.wpm.load(Ordering::SeqCst);
            lock(&shared.queue).extend(text.chars().map(|c| (c, wpm)));
            shared.queue_cv.notify_all();
        }
    }
}

/// The CW thread: waits for queued text, plays each run of equal speed, and repeats until stopped.
fn cw_loop(shared: Arc<Shared>) {
    let clock = MonotonicClock;
    let hz = clock.frequency();
    loop {
        // Take the next run of characters that share one speed, or wait for some.
        let (text, wpm, generation) = {
            let mut queue = lock(&shared.queue);
            loop {
                if shared.stopped() {
                    return;
                }
                if !queue.is_empty() {
                    break;
                }
                let (guard, _) = shared
                    .queue_cv
                    .wait_timeout(queue, QUEUE_WAIT)
                    .unwrap_or_else(|e| e.into_inner());
                queue = guard;
            }
            let wpm = queue.front().map_or(DEFAULT_WPM, |&(_, w)| w);
            let mut text = String::new();
            while let Some(&(ch, w)) = queue.front() {
                if w != wpm {
                    break;
                }
                text.push(ch);
                queue.pop_front();
            }
            (text, wpm, shared.abort_gen.load(Ordering::SeqCst))
        };
        play(&shared, &clock, hz, &text, wpm, generation);
    }
}

/// Keys one run of text through the local output. It stops early if the host stops or the
/// generation changes (Clear Buffer or Host Close).
fn play(shared: &Shared, clock: &MonotonicClock, hz: u64, text: &str, wpm: u32, generation: u64) {
    let schedule = match EdgeScheduleBuilder::build(text, wpm, WEIGHT, clock.now(), hz) {
        Ok(schedule) => schedule,
        Err(e) => {
            shared.log("warn", format!("Logger CW build failed: {e}"));
            return;
        }
    };
    if schedule.edges.is_empty() {
        return;
    }

    shared.local.set_busy(true);
    shared.sending.store(true, Ordering::SeqCst);
    (shared.report)(HostReport::Sending(true));

    let local = Arc::clone(&shared.local);
    let _completed = schedule.play(
        clock,
        &mut |edge| local.set_key(edge.key_down),
        &|| shared.stopped() || shared.abort_gen.load(Ordering::SeqCst) != generation,
    );

    shared.local.set_key(false);
    shared.local.set_busy(false);
    shared.sending.store(false, Ordering::SeqCst);
    (shared.report)(HostReport::Sending(false));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};

    /// A key output that ignores everything. The tests check the bytes the logger sees.
    struct NullKey;

    impl LocalKey for NullKey {
        fn set_key(&self, _key_down: bool) {}
        fn set_busy(&self, _busy: bool) {}
        fn set_ptt(&self, _asserted: bool) {}
    }

    /// Serves the chunks sent down its channel. Like a serial port, it times out when nothing is
    /// waiting.
    struct FakeReader {
        rx: Receiver<Vec<u8>>,
        pending: VecDeque<u8>,
    }

    impl Read for FakeReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pending.is_empty() {
                match self.rx.recv_timeout(READ_TIMEOUT) {
                    Ok(chunk) => self.pending.extend(chunk),
                    Err(RecvTimeoutError::Timeout) => {
                        return Err(std::io::Error::new(ErrorKind::TimedOut, "no bytes"));
                    }
                    Err(RecvTimeoutError::Disconnected) => return Ok(0),
                }
            }
            let n = buf.len().min(self.pending.len());
            for slot in buf.iter_mut().take(n) {
                *slot = self.pending.pop_front().unwrap_or(0);
            }
            Ok(n)
        }
    }

    /// Records every byte written to the logger.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct Harness {
        input: Sender<Vec<u8>>,
        output: Captured,
        reports: Arc<Mutex<Vec<HostReport>>>,
        _host: LoggerHost,
    }

    fn start() -> Harness {
        let (input, rx) = mpsc::channel();
        let output = Captured::default();
        let reports = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&reports);
        let report: HostReporter = Arc::new(move |r: HostReport| lock(&sink).push(r));
        let host = LoggerHost::spawn(
            "test",
            Box::new(FakeReader { rx, pending: VecDeque::new() }),
            Box::new(output.clone()),
            Arc::new(NullKey),
            None,
            report,
        )
        .expect("spawn logger host");
        Harness { input, output, reports, _host: host }
    }

    fn send(h: &Harness, bytes: &[u8]) {
        h.input.send(bytes.to_vec()).expect("send to logger");
    }

    fn written(h: &Harness) -> Vec<u8> {
        lock(&h.output.0).clone()
    }

    /// Polls the bytes written so far until `done` accepts them, or two seconds pass.
    fn wait_for(h: &Harness, done: impl Fn(&[u8]) -> bool) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let out = written(h);
            if done(&out) || Instant::now() > deadline {
                return out;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn sending_reports(h: &Harness) -> Vec<bool> {
        lock(&h.reports)
            .iter()
            .filter_map(|r| match r {
                HostReport::Sending(active) => Some(*active),
                HostReport::Log { .. } | HostReport::ReadFailed(_) => None,
            })
            .collect()
    }

    #[test]
    fn admin_open_gets_the_version_byte() {
        let h = start();
        send(&h, &[0x00, 0x02]);
        // WinKeyer 2 version byte (23), and nothing else.
        assert_eq!(wait_for(&h, |o| !o.is_empty()), vec![23]);
    }

    #[test]
    fn status_poll_is_idle_when_nothing_waits() {
        let h = start();
        send(&h, &[0x15]);
        assert_eq!(wait_for(&h, |o| !o.is_empty()), vec![0xC0]);
    }

    #[test]
    fn typed_text_is_echoed_then_idle() {
        let h = start();
        send(&h, b"CQ");
        let out = wait_for(&h, |o| o.len() >= 3);
        assert_eq!(out, b"CQ\xC0".to_vec());
    }

    #[test]
    fn status_is_busy_while_text_waits_to_flush() {
        let h = start();
        // Text and a status poll arrive together, before the batch can flush.
        send(&h, &[b'A', 0x15]);
        assert_eq!(wait_for(&h, |o| !o.is_empty()), vec![0xC5]);
    }

    #[test]
    fn n1mm_macro_bytes_echo_every_character() {
        // The exact bytes N1MM sent for "CQ 7Q" in the trace: the first 'C' used to be lost.
        let h = start();
        send(&h, &[0x16, 0x00, 0x16, 0x02, 0x00, 0x1C, 0x18, 0x43, 0x51, 0x20, 0x37, 0x51]);
        let out = wait_for(&h, |o| o.len() >= 6);
        assert_eq!(out, b"CQ 7Q\xC0".to_vec());
    }

    #[test]
    fn clear_buffer_drops_unflushed_text() {
        let h = start();
        send(&h, &[b'C', 0x0A]);
        std::thread::sleep(Duration::from_millis(40));
        assert!(written(&h).is_empty(), "cleared text must not be echoed");
        assert!(sending_reports(&h).is_empty(), "cleared text must not be keyed");
    }

    #[test]
    fn flushed_text_is_keyed_and_reported() {
        let h = start();
        send(&h, b"E");
        wait_for(&h, |o| o.len() >= 2);
        let deadline = Instant::now() + Duration::from_secs(2);
        while sending_reports(&h) != [true, false] && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(sending_reports(&h), vec![true, false]);
    }

    #[test]
    fn clear_buffer_aborts_cw_in_progress() {
        let h = start();
        send(&h, b"PARIS PARIS PARIS");
        // Wait until CW has started.
        let deadline = Instant::now() + Duration::from_secs(2);
        while sending_reports(&h).first() != Some(&true) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(sending_reports(&h).first(), Some(&true), "CW should have started");

        let aborted_at = Instant::now();
        send(&h, &[0x0A]);
        let deadline = Instant::now() + Duration::from_secs(2);
        while sending_reports(&h).last() != Some(&false) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        // The message takes seconds at 25 WPM; an abort must end it far sooner.
        assert_eq!(sending_reports(&h).last(), Some(&false));
        assert!(aborted_at.elapsed() < Duration::from_millis(500));
    }
}
