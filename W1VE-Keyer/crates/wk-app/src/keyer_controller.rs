//! The W1VE Keyer controller: a software WinKeyer, all in-process, no networking.
//!
//! Two input sources key one serial output through the shared [`KeyingArbiter`]:
//!
//! * **Paddle + type-ahead / macros** drive the arbiter's *remote* source.
//! * **The logger (WinKeyer2)** drives the arbiter's *local* source, which has priority (so a
//!   buffered logger message plays to completion, like a hardware WinKeyer).
//!
//! The arbiter owns the one [`SerialKeyingOutput`] and its [`LineObserver`] drives the UI
//! KEY/PTT indicators from the real line state. **Sidetone is independent of the output**: it
//! is keyed directly by each edge source so the operator always hears their sending, even with
//! no keying port selected (there is no radio to key, but there is a tone to hear).
//!
//! Serial ports are reconciled on demand (see [`KeyerController::reconcile_ports`]): a plug or
//! unplug drops any vanished selection to `None` and releases its handle without disturbing
//! the others.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};

use rwk_core::engine::audio::SidetoneEngine;
use rwk_core::engine::debounce::ContactDebouncer;
use rwk_core::engine::keying::{KeyerElement, KeyerElementTiming};
use rwk_core::engine::keying_arbiter::{
    KeyingArbiter, LineObserver, LineUpdate, RemoteKeyingOutput,
};
use rwk_core::engine::paddle::PaddleKeyer;
use rwk_core::engine::paddle_input::PaddleInputPort;
use rwk_core::engine::replay::{KeyingOutput, SerialKeyingAdapter};
use rwk_core::engine::serial::{
    enumerate_ports, natural_port_key, KeyingOutputConfig, SerialKeyingOutput,
};
use rwk_core::platform::{ThreadPriorityGuard, TimerResolutionGuard};
use rwk_core::primitives::{KeyerMode, KeyingLine};
use rwk_core::timing::{Clock, HybridWaiter, MonotonicClock};

use crate::app_paths;
use crate::winkeyer_host::{HostReport, HostReporter, WinKeyerHost};

/// Control identifiers for inline error messages in the UI.
mod control {
    pub const PADDLE: &str = "paddle";
    pub const LOGGER: &str = "logger";
    pub const KEYING: &str = "keying";
    pub const AUDIO: &str = "audio";
}

/// An event the controller emits for the UI.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum KeyerEvent {
    /// The effective key state (drives sidetone + the KEY indicator).
    KeyState { down: bool },
    /// PTT state on the radio changed.
    PttState { on: bool },
    /// The logger is actively sending CW.
    Sending { active: bool },
    /// A human-readable log line.
    Log { level: String, message: String },
    /// A short, per-control error to show in red under the offending control (full detail is
    /// also logged). An empty `message` clears the control's error.
    Error { control: String, message: String },
    /// The current serial-port inventory.
    Ports { available: Vec<String> },
    /// The current audio output device inventory (for the sidetone device picker): each entry
    /// is the device name and whether it is the system default output.
    AudioDevices { available: Vec<AudioDeviceView> },
    /// A full settings echo so the UI reflects persisted + reconciled state. The settings are
    /// flattened into the event object, so the UI reads `ev.wpm`, `ev.paddlePort`, etc.
    Settings {
        #[serde(flatten)]
        settings: Settings,
    },
}

/// One audio output device for the UI picker: its name and whether it is the system default.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDeviceView {
    pub name: String,
    pub is_default: bool,
}

/// A sink the host (Tauri) registers for [`KeyerEvent`]s.
pub type EventSink = Arc<dyn Fn(KeyerEvent) + Send + Sync>;

/// Persisted + live settings. Serializes camelCase so the web UI reads `paddlePort`, `toneHz`,
/// etc. directly (and the persisted JSON matches).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Settings {
    wpm: u32,
    weight: u32,
    mode: String,
    paddle_reverse: bool,
    tone_hz: i32,
    tone_volume: u32,
    /// `None` = default output device. Empty-string selection in the UI means "(None)" =
    /// sidetone off, stored here as `Some("")` and treated as off.
    audio_device: Option<String>,
    key_line: String,
    ptt_line: String,
    paddle_port: Option<String>,
    logger_port: Option<String>,
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
            audio_device: None,
            key_line: "dtr".into(),
            ptt_line: "none".into(),
            paddle_port: None,
            logger_port: None,
            keying_port: None,
        }
    }
}

/// How the sidetone device is configured: off, the system default, or a named device.
#[derive(Debug, Clone)]
enum ToneDevice {
    Off,
    Default,
    Named(String),
}

impl ToneDevice {
    /// Interprets the persisted `audio_device` field: `None` = default, `Some("")` = off,
    /// `Some(name)` = that device.
    fn from_setting(setting: &Option<String>) -> Self {
        match setting {
            None => Self::Default,
            Some(s) if s.is_empty() => Self::Off,
            Some(name) => Self::Named(name.clone()),
        }
    }
}

// ─── Sidetone thread (cpal is not Send; owns its own thread) ───────────────────

enum ToneCmd {
    Key(bool),
    Reconfigure { hz: i32, volume: u32, device: ToneDevice },
}

#[derive(Clone)]
struct Sidetone {
    tx: Sender<ToneCmd>,
}

impl Sidetone {
    fn spawn(hz: i32, volume: u32, device: ToneDevice, shared: Arc<Shared>) -> Self {
        let (tx, rx) = mpsc::channel::<ToneCmd>();
        std::thread::Builder::new()
            .name("wk-sidetone".into())
            .spawn(move || {
                let mut engine = build_engine(hz, volume, &device, &shared);
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
                        ToneCmd::Reconfigure { hz, volume, device } => {
                            engine = build_engine(hz, volume, &device, &shared);
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

    fn reconfigure(&self, hz: i32, volume: u32, device: ToneDevice) {
        let _ = self.tx.send(ToneCmd::Reconfigure { hz, volume, device });
    }
}

/// Builds a started sidetone engine for the requested device, or `None` when the device is
/// `Off` or the audio stream could not be opened (the error is logged and surfaced).
fn build_engine(
    hz: i32,
    volume: u32,
    device: &ToneDevice,
    shared: &Arc<Shared>,
) -> Option<SidetoneEngine> {
    if matches!(device, ToneDevice::Off) {
        shared.clear_error(control::AUDIO);
        shared.log("info", "Sidetone off (device set to None).");
        return None;
    }
    let named = match device {
        ToneDevice::Named(n) => Some(n.as_str()),
        _ => None,
    };
    let engine = SidetoneEngine::new(hz, volume as f64 / 100.0)
        .and_then(|mut e| e.start_on(named).map(|()| e));
    match engine {
        Ok(engine) => {
            let name = engine.device_name().unwrap_or("output").to_string();
            shared.clear_error(control::AUDIO);
            shared.log("info", format!("Sidetone on {name}."));
            Some(engine)
        }
        Err(e) => {
            let short = match device {
                ToneDevice::Named(n) => format!("{n} unavailable"),
                _ => "no audio output".to_string(),
            };
            shared.fail(control::AUDIO, short, format!("Sidetone unavailable: {e}"));
            None
        }
    }
}

// ─── Send buffer (type-ahead + macros) ─────────────────────────────────────────

/// The character queue + control flags shared with the send-worker thread.
#[derive(Default)]
struct SendState {
    /// Characters waiting to be keyed, in order.
    queue: VecDeque<char>,
    /// Set by [`KeyerController::abort_send`]; the worker drops the queue and the current
    /// schedule immediately when it sees this, then clears it.
    abort: bool,
    /// Set once at shutdown so the worker thread exits.
    shutdown: bool,
}

/// A thread-safe send buffer: a lock plus a condvar the worker waits on. Appending text or
/// requesting an abort notifies the worker.
#[derive(Default)]
struct SendBuffer {
    state: Mutex<SendState>,
    cv: Condvar,
}

impl SendBuffer {
    /// Appends `text` to the queue and wakes the worker.
    fn push(&self, text: &str) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.queue.extend(text.chars());
        self.cv.notify_all();
    }

    /// Flushes the queue and signals the worker to drop the in-progress schedule now.
    fn abort(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.queue.clear();
        st.abort = true;
        self.cv.notify_all();
    }

    /// Signals the worker to exit.
    fn shutdown(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.shutdown = true;
        self.cv.notify_all();
    }
}

// ─── Shared state ──────────────────────────────────────────────────────────────

struct Shared {
    settings: Mutex<Settings>,
    sidetone: Mutex<Option<Sidetone>>,
    /// The arbiter owning the keying output (LED-only when no keying port is open).
    arbiter: Mutex<Option<Arc<KeyingArbiter>>>,
    /// True while `arbiter` currently holds an open hardware serial port.
    keying_open: AtomicBool,
    /// The paddle/type-ahead sink (arbiter's remote source), present with the arbiter.
    paddle_sink: Mutex<Option<Arc<Mutex<RemoteKeyingOutput>>>>,
    /// The running paddle poller, present while a paddle port is open.
    paddle: Mutex<Option<PaddlePollerHandle>>,
    /// The running logger host, present while a logger port is open.
    logger: Mutex<Option<WinKeyerHost>>,
    /// The type-ahead / macro send buffer, drained by the send-worker thread.
    send: SendBuffer,
    /// De-duplicates the KEY indicator/sidetone so repeated same-state edges are cheap.
    last_key: AtomicBool,
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

    /// Logs the full detail and shows a short red message under `control`.
    fn fail(&self, control: &str, short: impl Into<String>, detail: impl Into<String>) {
        self.log("warn", detail);
        self.emit(KeyerEvent::Error { control: control.into(), message: short.into() });
    }

    /// Clears any red message under `control`.
    fn clear_error(&self, control: &str) {
        self.emit(KeyerEvent::Error { control: control.into(), message: String::new() });
    }

    /// Keys the sidetone and the UI KEY indicator from any edge source, independent of whether
    /// a radio keying port is open.
    fn drive_key(&self, down: bool) {
        if self.last_key.swap(down, Ordering::Relaxed) == down {
            return;
        }
        if let Some(tone) = self.sidetone.lock().unwrap().as_ref() {
            tone.key(down);
        }
        self.emit(KeyerEvent::KeyState { down });
    }

    fn emit_settings(&self) {
        let settings = self.settings.lock().unwrap().clone();
        self.emit(KeyerEvent::Settings { settings });
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

    fn tone_device(&self) -> ToneDevice {
        ToneDevice::from_setting(&self.settings.lock().unwrap().audio_device)
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
            keying_open: AtomicBool::new(false),
            paddle_sink: Mutex::new(None),
            paddle: Mutex::new(None),
            logger: Mutex::new(None),
            send: SendBuffer::default(),
            last_key: AtomicBool::new(false),
            config_path,
            sink,
        });

        let observer: LineObserver = make_line_observer(Arc::clone(&shared));
        let arbiter = Arc::new(KeyingArbiter::with_observer(
            SerialKeyingAdapter::led_only(),
            Some(observer),
        ));
        *shared.paddle_sink.lock().unwrap() = Some(Arc::new(Mutex::new(arbiter.remote_output())));
        *shared.arbiter.lock().unwrap() = Some(arbiter);

        let device = ToneDevice::from_setting(&settings.audio_device);
        let sidetone =
            Sidetone::spawn(settings.tone_hz, settings.tone_volume, device, Arc::clone(&shared));
        *shared.sidetone.lock().unwrap() = Some(sidetone);

        // The single send-worker thread drains the type-ahead / macro buffer with real CW
        // timing. Spawned once for the controller's life.
        spawn_send_worker(Arc::clone(&shared));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        let controller = Self { shared, runtime };
        // Open whatever persisted ports are present right now.
        controller.open_keying_port(settings.keying_port.clone());
        controller.open_paddle_port(settings.paddle_port.clone());
        controller.refresh_logger();
        controller
    }

    /// Re-emits settings, ports, and audio devices. The UI calls this once it has subscribed
    /// to the event channel, because the startup emit in [`Self::start`] races the webview and
    /// is otherwise lost — the webview does not exist yet when `start` runs in Tauri's setup.
    pub fn request_state(&self) {
        self.shared.emit_settings();
        self.emit_ports();
        self.emit_audio_devices();
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

    /// Selects the sidetone output device. `None` (JS `null`) = system default; an empty
    /// string = "(None)" = sidetone off; any other string = that device by name.
    pub fn select_audio_device(&self, device: Option<String>) {
        self.shared.settings.lock().unwrap().audio_device = device;
        self.shared.persist();
        self.restart_sidetone();
        self.shared.emit_settings();
    }

    fn restart_sidetone(&self) {
        let (hz, vol) = {
            let s = self.shared.settings.lock().unwrap();
            (s.tone_hz, s.tone_volume)
        };
        let device = self.shared.tone_device();
        if let Some(tone) = self.shared.sidetone.lock().unwrap().as_ref() {
            tone.reconfigure(hz, vol, device);
        }
    }

    #[must_use]
    pub fn audio_devices() -> Vec<AudioDeviceView> {
        SidetoneEngine::output_devices_marked()
            .into_iter()
            .map(|(name, is_default)| AudioDeviceView { name, is_default })
            .collect()
    }

    fn emit_audio_devices(&self) {
        self.shared.emit(KeyerEvent::AudioDevices { available: Self::audio_devices() });
    }

    /// Re-enumerates audio devices and pushes the list (for the UI refresh button / PnP).
    pub fn refresh_audio_devices(&self) {
        self.emit_audio_devices();
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

    /// The serial ports we currently hold open (paddle poller running, arbiter/keying open,
    /// logger host running). Used so reconciliation does not drop a working, in-use port that
    /// the OS hides from enumeration while it is busy.
    fn held_ports(&self) -> Vec<String> {
        let s = self.shared.settings.lock().unwrap();
        let mut held = Vec::new();
        if self.shared.paddle.lock().unwrap().is_some() {
            if let Some(p) = &s.paddle_port {
                held.push(p.clone());
            }
        }
        if self.shared.keying_open.load(Ordering::Relaxed) {
            if let Some(p) = &s.keying_port {
                held.push(p.clone());
            }
        }
        if self.shared.logger.lock().unwrap().is_some() {
            if let Some(p) = &s.logger_port {
                held.push(p.clone());
            }
        }
        held
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

    /// Swaps the keying output adapter inside the persistent [`KeyingArbiter`] in place.
    /// Clearing the port installs an LED-only adapter so the logger host and paddle sink keep
    /// running uninterrupted.
    fn open_keying_port(&self, port: Option<String>) {
        let arbiter = match self.shared.arbiter.lock().unwrap().as_ref() {
            Some(a) => Arc::clone(a),
            None => return,
        };
        let (key_line, ptt_line) = {
            let s = self.shared.settings.lock().unwrap();
            (parse_line(&s.key_line), parse_line(&s.ptt_line))
        };
        let shared = Arc::clone(&self.shared);
        arbiter.replace_adapter_with(move || {
            let Some(name) = port else {
                shared.keying_open.store(false, Ordering::Relaxed);
                shared.clear_error(control::KEYING);
                shared.log("info", "Keying output cleared.");
                return SerialKeyingAdapter::led_only();
            };
            let cfg = KeyingOutputConfig {
                port_name: name.clone(),
                key_line: key_line.unwrap_or(KeyingLine::Dtr),
                ptt_line: ptt_line.unwrap_or(KeyingLine::None),
                ..Default::default()
            };
            match SerialKeyingOutput::open(cfg) {
                Ok(out) => {
                    shared.keying_open.store(true, Ordering::Relaxed);
                    shared.clear_error(control::KEYING);
                    shared.log("info", format!("Keying output on {name}."));
                    SerialKeyingAdapter::new(out)
                }
                Err(e) => {
                    shared.keying_open.store(false, Ordering::Relaxed);
                    shared.fail(
                        control::KEYING,
                        format!("{name} not available"),
                        format!("Keying port {name} failed: {e}"),
                    );
                    SerialKeyingAdapter::led_only()
                }
            }
        });
    }

    fn open_paddle_port(&self, port: Option<String>) {
        *self.shared.paddle.lock().unwrap() = None;
        let Some(name) = port else {
            self.shared.clear_error(control::PADDLE);
            self.shared.log("info", "Paddle input cleared.");
            return;
        };
        match PaddleInputPort::open(&name) {
            Ok(input) => {
                let handle = PaddlePollerHandle::spawn(input, Arc::clone(&self.shared));
                *self.shared.paddle.lock().unwrap() = Some(handle);
                self.shared.clear_error(control::PADDLE);
                self.shared.log("info", format!("Paddle input on {name}."));
            }
            Err(e) => {
                self.shared.fail(
                    control::PADDLE,
                    format!("{name} not available"),
                    format!("Paddle port {name} failed: {e}"),
                );
            }
        }
    }

    /// Starts or stops the logger host to match the current port + arbiter. A `None` logger
    /// port means the logger is off.
    fn refresh_logger(&self) {
        *self.shared.logger.lock().unwrap() = None;
        let port = self.shared.settings.lock().unwrap().logger_port.clone();
        let Some(name) = port else {
            self.shared.clear_error(control::LOGGER);
            return;
        };
        let local = {
            let guard = self.shared.arbiter.lock().unwrap();
            match guard.as_ref() {
                Some(arbiter) => arbiter.local_handle(),
                None => {
                    // No keying output yet; the logger will be attached when one opens. The
                    // logger still accepts a port here so the selection sticks.
                    self.shared.clear_error(control::LOGGER);
                    self.shared.log(
                        "info",
                        "Logger port selected; it will key once a keying output is set.",
                    );
                    return;
                }
            }
        };
        let reporter: HostReporter = make_host_reporter(Arc::clone(&self.shared));
        match WinKeyerHost::open(&name, local, reporter) {
            Ok(host) => {
                self.shared.clear_error(control::LOGGER);
                self.shared.log("info", format!("Logger input active on {name}."));
                *self.shared.logger.lock().unwrap() = Some(host);
            }
            Err(e) => self.shared.fail(
                control::LOGGER,
                format!("{name} not available"),
                format!("Logger input unavailable: {e}"),
            ),
        }
    }

    /// Re-enumerates serial ports and reconciles the three selections: any selected port no
    /// longer present is dropped to `None` and its handle released, without disturbing the
    /// others. Always emits the fresh port list; also refreshes audio devices (a USB sound
    /// device may have arrived/left on the same event).
    ///
    /// A port we *currently hold open* is never treated as missing even if it is absent from
    /// the enumeration: on Windows an exclusively-opened COM port is commonly hidden from
    /// `available_ports()` while in use, and dropping a working, in-use selection would be a
    /// bug (it caused selections to snap back to `(None)`). A selection is only reset when it
    /// is both absent from the enumeration *and* we hold no open handle for it.
    pub fn reconcile_ports(&self) {
        let mut present = Self::serial_ports();
        // Fold in the ports we are actively holding, so a busy-hidden port still shows in the
        // list and is not reconciled away.
        for held in self.held_ports() {
            if !present.contains(&held) {
                present.push(held);
            }
        }
        present.sort_by(|a, b| natural_port_key(a).cmp(&natural_port_key(b)));
        self.shared.emit(KeyerEvent::Ports { available: present.clone() });
        self.emit_audio_devices();

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
            self.shared.clear_error(control::PADDLE);
        }
        if keying_gone {
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

    /// Appends text (a typed character or a macro) to the send buffer. It is keyed with proper
    /// CW spacing by the send worker, after whatever is already queued.
    pub fn send_cw(&self, text: &str) {
        self.shared.send.push(text);
    }

    pub fn test_tx(&self) {
        self.send_cw("VVV TEST");
    }

    /// Immediately stops sending: flushes the buffer and drops the in-progress character/macro
    /// (ESC / WinKeyer "clear buffer"). The key is forced up.
    pub fn abort_send(&self) {
        self.shared.send.abort();
    }
}

impl Drop for KeyerController {
    fn drop(&mut self) {
        // Stop the send worker, then release all port handles.
        self.shared.send.shutdown();
        *self.shared.logger.lock().unwrap() = None;
        *self.shared.paddle.lock().unwrap() = None;
        *self.shared.paddle_sink.lock().unwrap() = None;
        *self.shared.arbiter.lock().unwrap() = None;
        let _ = &self.runtime;
    }
}

// ─── Observer + reporter bridges ───────────────────────────────────────────────

/// The arbiter line observer drives the **PTT** indicator (and could show which source owns
/// the line). The KEY indicator + sidetone are driven directly by each edge source via
/// [`Shared::drive_key`], so they work with no keying port open.
fn make_line_observer(shared: Arc<Shared>) -> LineObserver {
    let last_ptt = Arc::new(AtomicBool::new(false));
    Arc::new(move |update: LineUpdate| {
        if last_ptt.swap(update.ptt, Ordering::Relaxed) != update.ptt {
            shared.emit(KeyerEvent::PttState { on: update.ptt });
        }
    })
}

/// Bridges the logger host's reports to UI events, keying the sidetone while it sends.
fn make_host_reporter(shared: Arc<Shared>) -> HostReporter {
    Arc::new(move |report: HostReport| match report {
        HostReport::Log { level, message } => shared.log(&level, message),
        // The reader's failure is also logged as a Log line; nothing more to show.
        HostReport::ReadFailed(_) => {}
        HostReport::Sending(active) => {
            shared.emit(KeyerEvent::Sending { active });
            // The logger host keys the arbiter's local handle for the radio; mirror its send
            // span onto the sidetone so the operator hears logger CW too. (Element-level tone
            // for the logger would require per-edge taps; the sending span is a faithful
            // audible cue that a macro is going out.)
            shared.drive_key(active);
        }
    })
}

// ─── Send worker: drains the type-ahead / macro buffer with real CW timing ─────

/// Spawns the single send-worker thread. It waits for buffered characters, drains them into
/// one CW schedule at the current speed, and keys them through the arbiter's remote source +
/// the sidetone with correct element/character/word spacing. An abort (ESC) drops the queue
/// and the in-progress schedule immediately; new characters typed during a send are picked up
/// on the next drain.
fn spawn_send_worker(shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("wk-sender".into())
        .spawn(move || send_worker(&shared))
        .expect("spawn send worker");
}

/// The send worker keys the buffer as a **continuous element stream**, one character at a
/// time, managing the inter-character and word gaps itself. This is the key difference from a
/// build-the-whole-string approach: characters typed *while sending* chain on seamlessly with
/// correct spacing, because the running `position` stays ahead of real time whenever the
/// buffer is non-empty, so the next character is scheduled a proper inter-character gap after
/// the previous one — never jammed up against it.
fn send_worker(shared: &Arc<Shared>) {
    // Element timing is sub-element-critical: without a raised priority and a 1 ms timer the
    // OS scheduler can overshoot a ~17 ms inter-character gap by a whole quantum and smear the
    // spacing (characters slur together). Match the paddle loop's scheduling protection.
    let _priority = ThreadPriorityGuard::raise_time_critical();
    let _timer = TimerResolutionGuard::raise(1);
    let clock = MonotonicClock;
    let hz = clock.frequency();

    // Running schedule cursor and whether a character has been keyed since the last gap, so
    // the next character knows to insert an inter-character gap.
    let mut position = clock.now();
    let mut prev_emitted = false;
    let mut sending = false;

    let aborting = || shared.send.state.lock().unwrap_or_else(|e| e.into_inner()).abort;

    loop {
        // Pop the next character, blocking until one is available.
        let next: Option<char> = {
            let mut st = shared.send.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if st.shutdown {
                    return;
                }
                if st.abort {
                    st.abort = false;
                    st.queue.clear();
                    // Reset the stream so the next character starts fresh.
                    drop(st);
                    position = clock.now();
                    prev_emitted = false;
                    if sending {
                        shared.drive_key(false);
                        set_sink(shared, false);
                        shared.emit(KeyerEvent::Sending { active: false });
                        sending = false;
                    }
                    st = shared.send.state.lock().unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                if let Some(ch) = st.queue.pop_front() {
                    break Some(ch);
                }
                // Nothing queued: drop the SEND indicator and wait for more input.
                if sending {
                    drop(st);
                    shared.emit(KeyerEvent::Sending { active: false });
                    sending = false;
                    st = shared.send.state.lock().unwrap_or_else(|e| e.into_inner());
                }
                st = shared.send.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(ch) = next else { continue };

        let (wpm, weight) = {
            let s = shared.settings.lock().unwrap();
            (s.wpm, s.weight)
        };
        let timing = match KeyerElementTiming::from_speed(wpm, weight, hz) {
            Ok(t) => t,
            Err(e) => {
                shared.log("warn", format!("CW timing error: {e}"));
                continue;
            }
        };

        // Re-anchor the schedule cursor to "now" whenever it has fallen behind real time
        // (which happens after any idle, and under scheduler slop). Crucially we do NOT clear
        // `prev_emitted` here: a character typed a moment after the previous one — the normal
        // human-typing case — must still get its inter-character gap, exactly as a pasted
        // string does. Otherwise "s" then "t" typed as separate keystrokes would merge into a
        // single "v". `prev_emitted` is only reset on a real word space, an abort, or startup.
        let now = clock.now();
        if position < now {
            position = now;
        }

        if !sending {
            shared.emit(KeyerEvent::Sending { active: true });
            sending = true;
        }

        // Word separator: advance by a word gap (only between words) and reset.
        if rwk_core::protocol::morse::MorseTable::is_word_separator(ch) {
            if prev_emitted {
                position += timing.word_gap_ticks();
            }
            prev_emitted = false;
            continue;
        }

        let Some(pattern) = rwk_core::protocol::morse::MorseTable::pattern(ch) else {
            continue; // unkeyable character: skip
        };

        if prev_emitted {
            position += timing.inter_character_gap_ticks();
        }

        // Key each element of the character with intra-element gaps.
        let mut first_element = true;
        for symbol in pattern.chars() {
            if !first_element {
                position += timing.gap_ticks;
            }
            first_element = false;

            let element = if symbol == '-' { KeyerElement::Dah } else { KeyerElement::Dit };
            let duration = timing.ticks_for(element);

            // Key down at `position`, up at `position + duration`.
            HybridWaiter::wait_until_ticks(&clock, position, &aborting);
            if aborting() {
                break;
            }
            shared.drive_key(true);
            set_sink(shared, true);

            HybridWaiter::wait_until_ticks(&clock, position + duration, &aborting);
            shared.drive_key(false);
            set_sink(shared, false);
            position += duration;

            if aborting() {
                break;
            }
        }
        prev_emitted = true;
    }
}

/// Keys the arbiter's remote (paddle/type-ahead) sink if a keying port is open.
fn set_sink(shared: &Arc<Shared>, down: bool) {
    if let Some(sink) = shared.paddle_sink.lock().unwrap().clone() {
        sink.lock().unwrap_or_else(|e| e.into_inner()).set_key(down);
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
                shared.fail(
                    control::PADDLE,
                    "paddle read error",
                    format!("Paddle port read error: {e}. Input stopped."),
                );
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
                shared.drive_key(edge.key_down);
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
    shared.drive_key(false);
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
    fn tone_device_interprets_the_setting() {
        assert!(matches!(ToneDevice::from_setting(&None), ToneDevice::Default));
        assert!(matches!(ToneDevice::from_setting(&Some(String::new())), ToneDevice::Off));
        assert!(matches!(
            ToneDevice::from_setting(&Some("Speakers".into())),
            ToneDevice::Named(_)
        ));
    }
}
