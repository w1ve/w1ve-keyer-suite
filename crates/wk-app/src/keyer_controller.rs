//! The W1VE Keyer controller: a software WinKeyer, all in-process, no networking.
//!
//! It fuses the Remote Keyer's two halves into one local app. Two input sources key one
//! serial output through the shared [`KeyingArbiter`]:
//!
//! * **Paddle + type-ahead / macros** drive the arbiter's *remote* source (so a buffered
//!   logger message takes priority, exactly like a hardware WinKeyer plays a message to
//!   completion before the paddle takes over again).
//! * **The logger (WinKeyer2)** drives the arbiter's *local* source, which has priority.
//!
//! The single [`KeyingArbiter`] owns the one [`SerialKeyingOutput`] (the key-line transistor
//! into the rig). A [`LineObserver`] on the arbiter is the single source of truth for what is
//! actually on the wire — it drives the sidetone and the UI KEY/PTT indicators regardless of
//! which input won arbitration.
//!
//! Serial ports are reconciled on demand (see [`KeyerController::reconcile_ports`]): a plug or
//! unplug — detected instantly on Windows, by polling elsewhere — re-enumerates and, if a
//! selected device vanished, drops that selection to `None` and releases its handle without
//! disturbing the other two.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use rwk_core::engine::audio::SidetoneEngine;
use rwk_core::engine::debounce::ContactDebouncer;
use rwk_core::engine::keying::{EdgeSchedule, EdgeScheduleBuilder};
use rwk_core::engine::keying_arbiter::{
    KeyingArbiter, LineObserver, LineUpdate, RemoteKeyingOutput,
};
use rwk_core::engine::paddle::PaddleKeyer;
use rwk_core::engine::paddle_input::PaddleInputPort;
use rwk_core::engine::replay::{KeyingOutput, SerialKeyingAdapter};
use rwk_core::engine::serial::{enumerate_ports, KeyingOutputConfig, SerialKeyingOutput};
use rwk_core::platform::{ThreadPriorityGuard, TimerResolutionGuard};
use rwk_core::primitives::{KeyerMode, KeyingLine};
use rwk_core::timing::{Clock, HybridWaiter, MonotonicClock};

use crate::app_paths;
use crate::winkeyer_host::{HostReport, HostReporter, WinKeyerHost};

/// An event the controller emits for the UI.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum KeyerEvent {
    /// The effective key state on the radio changed (from any source).
    KeyState { down: bool },
    /// PTT state on the radio changed.
    PttState { on: bool },
    /// The logger is actively sending CW.
    Sending { active: bool },
    /// A human-readable log line.
    Log { level: String, message: String },
    /// The current serial-port inventory (always includes an empty "(None)" via the UI).
    Ports { available: Vec<String> },
    /// A full settings echo so the UI reflects persisted + reconciled state.
    Settings {
        wpm: u32,
        weight: u32,
        mode: String,
        paddle_reverse: bool,
        tone_hz: i32,
        tone_volume: u32,
        key_line: String,
        ptt_line: String,
        paddle_port: Option<String>,
        logger_port: Option<String>,
        logger_enabled: bool,
        keying_port: Option<String>,
    },
}

/// A sink the host (Tauri) registers for [`KeyerEvent`]s.
pub type EventSink = Arc<dyn Fn(KeyerEvent) + Send + Sync>;

/// Persisted + live settings.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Settings {
    wpm: u32,
    weight: u32,
    mode: String,
    paddle_reverse: bool,
    tone_hz: i32,
    tone_volume: u32,
    key_line: String,
    ptt_line: String,
    paddle_port: Option<String>,
    logger_port: Option<String>,
    logger_enabled: bool,
    keying_port: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            wpm: 20,
            weight: 50,
            mode: "iambicB".into(),
            paddle_reverse: false,
            tone_hz: 600,
            tone_volume: 70,
            key_line: "dtr".into(),
            ptt_line: "none".into(),
            paddle_port: None,
            logger_port: None,
            logger_enabled: true,
            keying_port: None,
        }
    }
}

// ─── Sidetone thread (cpal is not Send; owns its own thread) ───────────────────

enum ToneCmd {
    Key(bool),
    Reconfigure { hz: i32, volume: u32 },
}

#[derive(Clone)]
struct Sidetone {
    tx: Sender<ToneCmd>,
}

impl Sidetone {
    fn spawn(hz: i32, volume: u32, shared: Arc<Shared>) -> Self {
        let (tx, rx) = mpsc::channel::<ToneCmd>();
        std::thread::Builder::new()
            .name("wk-sidetone".into())
            .spawn(move || {
                let mut engine = build_engine(hz, volume, &shared);
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        ToneCmd::Key(true) => {
                            if let Some(e) = engine.as_ref() {
                                e.key_down();
                            }
                        }
                        ToneCmd::Key(false) => {
                            if let Some(e) = engine.as_ref() {
                                e.key_up();
                            }
                        }
                        ToneCmd::Reconfigure { hz, volume } => {
                            engine = build_engine(hz, volume, &shared);
                        }
                    }
                }
            })
            .expect("spawn sidetone thread");
        Self { tx }
    }

    fn key(&self, down: bool) {
        let _ = self.tx.send(ToneCmd::Key(down));
    }

    fn reconfigure(&self, hz: i32, volume: u32) {
        let _ = self.tx.send(ToneCmd::Reconfigure { hz, volume });
    }
}

fn build_engine(hz: i32, volume: u32, shared: &Arc<Shared>) -> Option<SidetoneEngine> {
    match SidetoneEngine::new(hz, volume as f64 / 100.0).and_then(|mut e| e.start().map(|()| e)) {
        Ok(engine) => {
            let name = engine.device_name().unwrap_or("output").to_string();
            shared.log("info", format!("Sidetone on {name}."));
            Some(engine)
        }
        Err(e) => {
            shared.log("warn", format!("Sidetone unavailable: {e}"));
            None
        }
    }
}

// ─── Shared state ──────────────────────────────────────────────────────────────

struct Shared {
    settings: Mutex<Settings>,
    sidetone: Mutex<Option<Sidetone>>,
    /// The arbiter owning the keying output, present while a keying port is open.
    arbiter: Mutex<Option<Arc<KeyingArbiter>>>,
    /// The paddle/type-ahead sink (arbiter's remote source), present with the arbiter.
    paddle_sink: Mutex<Option<Arc<Mutex<RemoteKeyingOutput>>>>,
    /// The running paddle poller, present while a paddle port is open.
    paddle: Mutex<Option<PaddlePollerHandle>>,
    /// The running logger host, present while a logger port is open and enabled.
    logger: Mutex<Option<WinKeyerHost>>,
    /// Bumped each time keyer settings change, so running CW uses the latest values.
    config_path: Option<std::path::PathBuf>,
    sink: EventSink,
}

impl Shared {
    fn emit(&self, event: KeyerEvent) {
        (self.sink)(event);
    }

    fn log(&self, level: &str, message: impl Into<String>) {
        self.emit(KeyerEvent::Log { level: level.into(), message: message.into() });
    }

    fn emit_settings(&self) {
        let s = self.settings.lock().unwrap();
        self.emit(KeyerEvent::Settings {
            wpm: s.wpm,
            weight: s.weight,
            mode: s.mode.clone(),
            paddle_reverse: s.paddle_reverse,
            tone_hz: s.tone_hz,
            tone_volume: s.tone_volume,
            key_line: s.key_line.clone(),
            ptt_line: s.ptt_line.clone(),
            paddle_port: s.paddle_port.clone(),
            logger_port: s.logger_port.clone(),
            logger_enabled: s.logger_enabled,
            keying_port: s.keying_port.clone(),
        });
    }

    fn persist(&self) {
        let Some(path) = &self.config_path else { return };
        let s = self.settings.lock().unwrap();
        match serde_json::to_string_pretty(&*s) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    self.log("warn", format!("Could not save settings: {e}"));
                }
            }
            Err(e) => self.log("warn", format!("Could not serialize settings: {e}")),
        }
    }
}

/// The W1VE Keyer controller.
pub struct KeyerController {
    shared: Arc<Shared>,
    runtime: tokio::runtime::Runtime,
}

impl KeyerController {
    /// Starts the controller: loads settings, brings up sidetone, and opens any persisted
    /// ports that are currently present.
    pub fn start(sink: EventSink) -> Self {
        Self::start_with_config(sink, Some(app_paths::settings_path()))
    }

    /// Starts against an explicit settings path (`None` disables persistence, for tests).
    pub fn start_with_config(sink: EventSink, config_path: Option<std::path::PathBuf>) -> Self {
        let settings = load_or_default(&config_path);
        let shared = Arc::new(Shared {
            settings: Mutex::new(settings.clone()),
            sidetone: Mutex::new(None),
            arbiter: Mutex::new(None),
            paddle_sink: Mutex::new(None),
            paddle: Mutex::new(None),
            logger: Mutex::new(None),
            config_path,
            sink,
        });

        let sidetone = Sidetone::spawn(settings.tone_hz, settings.tone_volume, Arc::clone(&shared));
        *shared.sidetone.lock().unwrap() = Some(sidetone);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        let controller = Self { shared, runtime };
        controller.shared.emit_settings();
        controller.emit_ports();
        // Open whatever persisted ports are present right now.
        controller.open_keying_port(settings.keying_port.clone());
        controller.open_paddle_port(settings.paddle_port.clone());
        controller.refresh_logger();
        controller
    }

    // ─── Keyer settings ────────────────────────────────────────────────────────

    pub fn set_speed(&self, wpm: u32) {
        self.shared.settings.lock().unwrap().wpm = wpm.clamp(5, 60);
        self.shared.persist();
    }

    pub fn set_weight(&self, weight: u32) {
        self.shared.settings.lock().unwrap().weight = weight.clamp(25, 75);
        self.shared.persist();
    }

    pub fn set_mode(&self, mode: &str) {
        self.shared.settings.lock().unwrap().mode = normalize_mode(mode).into();
        self.shared.persist();
    }

    pub fn set_paddle_reverse(&self, reverse: bool) {
        self.shared.settings.lock().unwrap().paddle_reverse = reverse;
        self.shared.persist();
    }

    pub fn set_key_line(&self, line: &str) {
        self.shared.settings.lock().unwrap().key_line = normalize_line(line, "dtr").into();
        self.shared.persist();
        // Re-open the keying port so the new line mapping takes effect.
        let port = self.shared.settings.lock().unwrap().keying_port.clone();
        self.open_keying_port(port);
    }

    pub fn set_ptt_line(&self, line: &str) {
        self.shared.settings.lock().unwrap().ptt_line = normalize_line(line, "none").into();
        self.shared.persist();
        let port = self.shared.settings.lock().unwrap().keying_port.clone();
        self.open_keying_port(port);
    }

    // ─── Sidetone ──────────────────────────────────────────────────────────────

    pub fn set_tone_frequency(&self, hz: i32) {
        self.shared.settings.lock().unwrap().tone_hz = hz.clamp(300, 1200);
        self.shared.persist();
        self.restart_sidetone();
    }

    pub fn set_tone_volume(&self, percent: u32) {
        self.shared.settings.lock().unwrap().tone_volume = percent.min(100);
        self.shared.persist();
        self.restart_sidetone();
    }

    fn restart_sidetone(&self) {
        let (hz, vol) = {
            let s = self.shared.settings.lock().unwrap();
            (s.tone_hz, s.tone_volume)
        };
        if let Some(tone) = self.shared.sidetone.lock().unwrap().as_ref() {
            tone.reconfigure(hz, vol);
        }
    }

    #[must_use]
    pub fn audio_devices() -> Vec<String> {
        SidetoneEngine::output_devices()
    }

    // ─── Ports ───────────────────────────────────────────────────────────────

    /// The serial ports currently present on the machine.
    #[must_use]
    pub fn serial_ports() -> Vec<String> {
        enumerate_ports()
            .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
            .unwrap_or_default()
    }

    fn emit_ports(&self) {
        self.shared.emit(KeyerEvent::Ports { available: Self::serial_ports() });
    }

    pub fn select_keying_port(&self, port: Option<String>) {
        self.shared.settings.lock().unwrap().keying_port = port.clone();
        self.shared.persist();
        self.open_keying_port(port);
        self.shared.emit_settings();
    }

    pub fn select_paddle_port(&self, port: Option<String>) {
        self.shared.settings.lock().unwrap().paddle_port = port.clone();
        self.shared.persist();
        self.open_paddle_port(port);
        self.shared.emit_settings();
    }

    pub fn select_logger_port(&self, port: Option<String>) {
        self.shared.settings.lock().unwrap().logger_port = port;
        self.shared.persist();
        self.refresh_logger();
        self.shared.emit_settings();
    }

    pub fn set_logger_enabled(&self, enabled: bool) {
        self.shared.settings.lock().unwrap().logger_enabled = enabled;
        self.shared.persist();
        self.refresh_logger();
        self.shared.emit_settings();
    }

    /// Opens (or re-opens) the keying output and rebuilds the arbiter around it. Clearing the
    /// port tears the arbiter down. The logger is re-attached to the new arbiter afterwards.
    fn open_keying_port(&self, port: Option<String>) {
        // Drop the old arbiter + logger first (releases the port handles).
        *self.shared.logger.lock().unwrap() = None;
        *self.shared.paddle_sink.lock().unwrap() = None;
        *self.shared.arbiter.lock().unwrap() = None;

        let Some(name) = port else {
            self.shared.log("info", "Keying output cleared.");
            return;
        };

        let (key_line, ptt_line) = {
            let s = self.shared.settings.lock().unwrap();
            (parse_line(&s.key_line), parse_line(&s.ptt_line))
        };
        let cfg = KeyingOutputConfig {
            port_name: name.clone(),
            key_line: key_line.unwrap_or(KeyingLine::Dtr),
            ptt_line: ptt_line.unwrap_or(KeyingLine::None),
            ..Default::default()
        };
        let serial = match SerialKeyingOutput::open(cfg) {
            Ok(out) => out,
            Err(e) => {
                self.shared.log("warn", format!("Keying port {name} failed: {e}"));
                return;
            }
        };

        let observer: LineObserver = make_line_observer(Arc::clone(&self.shared));
        let arbiter = Arc::new(KeyingArbiter::with_observer(
            SerialKeyingAdapter::new(serial),
            Some(observer),
        ));
        let sink = Arc::new(Mutex::new(arbiter.remote_output()));
        *self.shared.paddle_sink.lock().unwrap() = Some(sink);
        *self.shared.arbiter.lock().unwrap() = Some(arbiter);
        self.shared.log("info", format!("Keying output on {name}."));

        // Re-attach the logger to the new arbiter if it should be running.
        self.refresh_logger();
    }

    fn open_paddle_port(&self, port: Option<String>) {
        *self.shared.paddle.lock().unwrap() = None;
        let Some(name) = port else {
            self.shared.log("info", "Paddle input cleared.");
            return;
        };
        match PaddleInputPort::open(&name) {
            Ok(input) => {
                let handle = PaddlePollerHandle::spawn(input, Arc::clone(&self.shared));
                *self.shared.paddle.lock().unwrap() = Some(handle);
                self.shared.log("info", format!("Paddle input on {name}."));
            }
            Err(e) => self.shared.log("warn", format!("Paddle port {name} failed: {e}")),
        }
    }

    /// Starts or stops the logger host to match the current port + enabled flag + arbiter.
    fn refresh_logger(&self) {
        *self.shared.logger.lock().unwrap() = None;
        let (enabled, port) = {
            let s = self.shared.settings.lock().unwrap();
            (s.logger_enabled, s.logger_port.clone())
        };
        if !enabled {
            return;
        }
        let Some(name) = port else { return };
        let local = {
            let guard = self.shared.arbiter.lock().unwrap();
            match guard.as_ref() {
                Some(arbiter) => arbiter.local_handle(),
                None => {
                    // No keying output yet; the logger has nothing to key. It will be
                    // attached when a keying port is opened.
                    return;
                }
            }
        };
        let reporter: HostReporter = make_host_reporter(Arc::clone(&self.shared));
        match WinKeyerHost::open(&name, local, reporter) {
            Ok(host) => {
                self.shared.log("info", format!("Logger input active on {name}."));
                *self.shared.logger.lock().unwrap() = Some(host);
            }
            Err(e) => self.shared.log("warn", format!("Logger input unavailable: {e}")),
        }
    }

    /// Re-enumerates serial ports and reconciles the three selections: any selected port no
    /// longer present is dropped to `None` and its handle released, without disturbing the
    /// others. Always emits the fresh port list; emits settings only if something changed.
    /// This is the single callback the [`crate::device_watch::DeviceChangeWatcher`] fires.
    pub fn reconcile_ports(&self) {
        let present = Self::serial_ports();
        self.shared.emit(KeyerEvent::Ports { available: present.clone() });

        let (paddle_gone, logger_gone, keying_gone) = {
            let s = self.shared.settings.lock().unwrap();
            (
                port_missing(&s.paddle_port, &present),
                port_missing(&s.logger_port, &present),
                port_missing(&s.keying_port, &present),
            )
        };
        if !(paddle_gone || logger_gone || keying_gone) {
            return;
        }

        if paddle_gone {
            let name = self.shared.settings.lock().unwrap().paddle_port.take();
            if let Some(n) = name {
                self.shared.log("warn", format!("Paddle port {n} removed; set to (None)."));
            }
            *self.shared.paddle.lock().unwrap() = None;
        }
        if keying_gone {
            // Clearing the keying port tears down the arbiter (and detaches the logger).
            let name = self.shared.settings.lock().unwrap().keying_port.take();
            if let Some(n) = name {
                self.shared.log("warn", format!("Keying port {n} removed; set to (None)."));
            }
            self.open_keying_port(None);
        }
        if logger_gone {
            let name = self.shared.settings.lock().unwrap().logger_port.take();
            if let Some(n) = name {
                self.shared.log("warn", format!("Logger port {n} removed; set to (None)."));
            }
            self.refresh_logger();
        }
        self.shared.persist();
        self.shared.emit_settings();
    }

    // ─── Keying (type-ahead / macros) ──────────────────────────────────────────

    pub fn send_cw(&self, text: &str) {
        let text = text.to_string();
        let shared = Arc::clone(&self.shared);
        std::thread::Builder::new()
            .name("wk-typeahead".into())
            .spawn(move || play_cw(&shared, &text))
            .expect("spawn type-ahead thread");
    }

    pub fn test_tx(&self) {
        self.send_cw("VVV TEST");
    }
}

impl Drop for KeyerController {
    fn drop(&mut self) {
        // Dropping the handles releases all ports and stops the threads.
        *self.shared.logger.lock().unwrap() = None;
        *self.shared.paddle.lock().unwrap() = None;
        *self.shared.paddle_sink.lock().unwrap() = None;
        *self.shared.arbiter.lock().unwrap() = None;
        let _ = &self.runtime;
    }
}

// ─── Observer + reporter bridges (arbiter/logger → UI + sidetone) ──────────────

/// Builds the arbiter line observer: the single source of truth for what is on the wire. It
/// drives the sidetone and the UI KEY/PTT indicators for *any* input source.
fn make_line_observer(shared: Arc<Shared>) -> LineObserver {
    let last_key = Arc::new(AtomicBool::new(false));
    let last_ptt = Arc::new(AtomicBool::new(false));
    Arc::new(move |update: LineUpdate| {
        if last_key.swap(update.key_down, Ordering::Relaxed) != update.key_down {
            if let Some(tone) = shared.sidetone.lock().unwrap().as_ref() {
                tone.key(update.key_down);
            }
            shared.emit(KeyerEvent::KeyState { down: update.key_down });
        }
        if last_ptt.swap(update.ptt, Ordering::Relaxed) != update.ptt {
            shared.emit(KeyerEvent::PttState { on: update.ptt });
        }
    })
}

/// Bridges the logger host's reports to UI events.
fn make_host_reporter(shared: Arc<Shared>) -> HostReporter {
    Arc::new(move |report: HostReport| match report {
        HostReport::Log { level, message } => shared.log(&level, message),
        HostReport::Sending(active) => shared.emit(KeyerEvent::Sending { active }),
    })
}

// ─── Type-ahead CW: build a schedule and key it through the paddle/remote source ──

fn play_cw(shared: &Arc<Shared>, text: &str) {
    let clock = MonotonicClock;
    let (wpm, weight) = {
        let s = shared.settings.lock().unwrap();
        (s.wpm, s.weight)
    };
    let start = clock.now();
    let schedule: EdgeSchedule =
        match EdgeScheduleBuilder::build(text, wpm, weight, start, clock.frequency()) {
            Ok(s) => s,
            Err(e) => {
                shared.log("warn", format!("CW build failed: {e}"));
                return;
            }
        };
    // Key through the paddle/remote source. If no keying port is open, the sink is absent and
    // we simply play the sidetone via the observer-less path (nothing to key).
    let sink = shared.paddle_sink.lock().unwrap().clone();
    for edge in &schedule.edges {
        HybridWaiter::wait_until_ticks(&clock, edge.timestamp_ticks, &|| false);
        if let Some(sink) = &sink {
            sink.lock().unwrap().set_key(edge.key_down);
        }
    }
    if let Some(sink) = &sink {
        sink.lock().unwrap().set_key(false);
    }
}

// ─── Serial paddle poller ───────────────────────────────────────────────────────

const PADDLE_DEBOUNCE_MS: u64 = 5;
const PADDLE_POLL_MS: u64 = 1;

struct PaddlePollerHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PaddlePollerHandle {
    fn spawn(input: PaddleInputPort, shared: Arc<Shared>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let mut input = input;
        let thread = std::thread::Builder::new()
            .name("wk-paddle".into())
            .spawn(move || paddle_loop(&mut input, &shared, &stop_thread))
            .expect("spawn paddle poller");
        Self { stop, thread: Some(thread) }
    }
}

impl Drop for PaddlePollerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn paddle_loop(input: &mut PaddleInputPort, shared: &Arc<Shared>, stop: &Arc<AtomicBool>) {
    let _priority = ThreadPriorityGuard::raise_time_critical();
    let _timer = TimerResolutionGuard::raise(1);
    let clock = MonotonicClock;
    let hz = clock.frequency();
    let mut debouncer = ContactDebouncer::new(PADDLE_DEBOUNCE_MS, hz);
    let (mut wpm, mut weight, mut mode, _r0) = read_keyer_settings(shared);
    let mut keyer = match PaddleKeyer::new(mode, wpm, weight, hz) {
        Ok(k) => k,
        Err(e) => {
            shared.log("warn", format!("Paddle keyer init failed: {e}"));
            return;
        }
    };
    let poll_step = hz.saturating_mul(PADDLE_POLL_MS) / 1000;
    let should_abort = || stop.load(Ordering::SeqCst);

    while !stop.load(Ordering::SeqCst) {
        let raw = match input.read() {
            Ok(s) => s,
            Err(e) => {
                shared.log("warn", format!("Paddle port read error: {e}. Input stopped."));
                break;
            }
        };
        let _ = debouncer.accept(raw, clock.now());
        let accepted = debouncer.accepted();

        let (nw, ng, nm, reverse) = read_keyer_settings(shared);
        if nw != wpm || ng != weight {
            let _ = keyer.retune(nw, ng, hz);
            wpm = nw;
            weight = ng;
        }
        if nm != mode {
            keyer.set_mode(nm);
            mode = nm;
        }

        let paddle_now = move || accepted.to_paddle_state(reverse);
        let sink = shared.paddle_sink.lock().unwrap().clone();
        let played = keyer.run_once(
            &clock,
            &paddle_now,
            &mut |edge| {
                if let Some(sink) = &sink {
                    sink.lock().unwrap().set_key(edge.key_down);
                }
            },
            &should_abort,
        );
        if !played {
            let next = clock.now() + poll_step.max(1);
            HybridWaiter::wait_until_ticks(&clock, next, &should_abort);
        }
    }
    // Release the key on the way out.
    if let Some(sink) = shared.paddle_sink.lock().unwrap().clone() {
        sink.lock().unwrap().set_key(false);
    }
}

fn read_keyer_settings(shared: &Arc<Shared>) -> (u32, u32, KeyerMode, bool) {
    let s = shared.settings.lock().unwrap();
    (s.wpm, s.weight, parse_mode(&s.mode), s.paddle_reverse)
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn load_or_default(path: &Option<std::path::PathBuf>) -> Settings {
    match path {
        Some(p) if p.exists() => std::fs::read_to_string(p)
            .ok()
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default(),
        _ => Settings::default(),
    }
}

/// Whether `selected` names a port that is not in `present` (and is `Some`).
fn port_missing(selected: &Option<String>, present: &[String]) -> bool {
    matches!(selected, Some(name) if !present.iter().any(|p| p == name))
}

fn normalize_mode(name: &str) -> &'static str {
    match name {
        "iambicA" => "iambicA",
        "ultimatic" => "ultimatic",
        "bug" => "bug",
        "straight" => "straight",
        _ => "iambicB",
    }
}

fn parse_mode(name: &str) -> KeyerMode {
    match name {
        "iambicA" => KeyerMode::IambicA,
        "ultimatic" => KeyerMode::Ultimatic,
        "bug" => KeyerMode::Bug,
        "straight" => KeyerMode::Straight,
        _ => KeyerMode::IambicB,
    }
}

fn normalize_line(name: &str, default: &'static str) -> &'static str {
    match name {
        "dtr" => "dtr",
        "rts" => "rts",
        "none" => "none",
        _ => default,
    }
}

fn parse_line(name: &str) -> Option<KeyingLine> {
    match name {
        "dtr" => Some(KeyingLine::Dtr),
        "rts" => Some(KeyingLine::Rts),
        "none" => Some(KeyingLine::None),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_missing_detects_removed_selection() {
        let present = vec!["COM3".to_string(), "COM5".to_string()];
        assert!(port_missing(&Some("COM9".into()), &present));
        assert!(!port_missing(&Some("COM3".into()), &present));
        assert!(!port_missing(&None, &present));
    }

    #[test]
    fn mode_round_trips_through_normalize_and_parse() {
        for m in ["iambicA", "iambicB", "ultimatic", "bug", "straight"] {
            assert_eq!(normalize_mode(m), m);
        }
        assert_eq!(normalize_mode("bogus"), "iambicB");
        assert!(matches!(parse_mode("straight"), KeyerMode::Straight));
    }

    #[test]
    fn settings_default_has_logger_enabled() {
        let s = Settings::default();
        assert!(s.logger_enabled);
        assert_eq!(s.wpm, 20);
    }
}
