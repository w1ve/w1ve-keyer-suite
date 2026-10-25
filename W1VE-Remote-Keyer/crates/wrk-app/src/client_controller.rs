//! The Client controller for W1VE Remote Keyer: paddle/keyer at the operating position.
//!
//! This is the Keyer-Router Client's keying path, distilled to the Remote Keyer's job: send
//! CW to a Server over the tunnel, with local sidetone and an optional serial key line. It
//! reuses `rwk-core` wholesale — the WireGuard tunnel, the relay rendezvous, the keyer timing,
//! the sidetone engine, the serial paddle input — and differs from the Keyer-Router Client
//! only in that there is no Ham Router: no forward rules, no control channel, no Flex.
//!
//! The WAN/LAN choice and the pairing token flow through [`crate::pairing::build_config`], so
//! the controller does not care which relay introduced the peer; it just connects, opens the
//! keying edge over the resulting tunnel, and keys.
//!
//! Type-ahead and macros are keyed by a single **send-worker** thread draining a shared
//! [`SendBuffer`] as a continuous CW element stream (one character at a time, managing the
//! inter-character / word gaps itself), ported from the standalone W1VE Keyer. Each element
//! edge fans out through [`drive_edge`] so paired keying, local sidetone, the local key line,
//! and the UI KEY indicator all stay in lock-step.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};

use rwk_core::config::{Config, Role};
use rwk_core::engine::audio::SidetoneEngine;
use rwk_core::engine::debounce::ContactDebouncer;
use rwk_core::engine::edge_session::EdgeSession;
use rwk_core::engine::keying::{KeyerElement, KeyerElementTiming};
use rwk_core::engine::logger_host::{HostReport, HostReporter, LoggerHost, TextSink};
use rwk_core::engine::lan_discovery::{Beacon, LanAdvertiser};
use rwk_core::engine::network::EdgeTransport;
use rwk_core::engine::paddle::PaddleKeyer;
use rwk_core::engine::paddle_input::PaddleInputPort;
use rwk_core::engine::serial::{
    enumerate_ports, natural_port_key,
};
use rwk_core::engine::session::Session;
use rwk_core::engine::tunnel;
use rwk_core::platform::{ThreadPriorityGuard, TimerResolutionGuard};
use rwk_core::primitives::KeyerMode;
use rwk_core::protocol::morse::MorseTable;
use rwk_core::timing::{Clock, HybridWaiter, MonotonicClock};

use crate::app_paths;
use crate::pairing::{self, Transport};

/// The inner UDP port the keying edge rides over the tunnel, matching the Server.
const EDGE_PORT: u16 = 5555;

/// How often the Client beacons a liveness heartbeat to the Player. It must stay well inside the
/// Player's 750 ms F1 window, or a key held longer than that with no edges is forced up.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
/// The RTT readout refreshes once a second: every this many heartbeat ticks.
const RTT_EVERY_TICKS: u32 = 4;

/// How long the Client waits without ANY inbound traffic from the Player (heartbeats or any
/// datagram) before declaring the peer lost and reconnecting. Five seconds rides out a brief
/// relay/roaming hiccup while still catching a Player that was closed.
const PEER_TIMEOUT: std::time::Duration = rwk_core::engine::reconnect::PEER_SILENCE_TIMEOUT;

/// Consecutive failed reconnect attempts after which the Client gives up and stays
/// disconnected (the operator must reconnect manually).
const MAX_RECONNECT_ATTEMPTS: u32 = 5;

/// Budget for re-establishing a lost link (shared policy): after this long the Keyer gives up
/// and reports disconnected.
const RECONNECT_BUDGET: std::time::Duration = rwk_core::engine::reconnect::RECONNECT_WINDOW;

/// Control identifiers for inline error messages in the UI.
mod control {
    pub const LOGGER: &str = "logger";
    pub const PADDLE: &str = "paddle";
    pub const AUDIO: &str = "audio";
}

/// An event the Client controller emits for the UI.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ClientEvent {
    KeyState { down: bool },
    Connection { state: String, path: String },
    Log { level: String, message: String },
    Settings {
        wpm: u32,
        weight: u32,
        mode: String,
        paddle_reverse: bool,
        tone_hz: i32,
        tone_volume: u32,
        audio_device: Option<String>,
        transport: String,
        relay: String,
        callsign: String,
        keyer_id: String,
        paddle_port: Option<String>,
        logger_port: Option<String>,
        /// N1MM has the WinKeyer logger open and owns the speed; the Client's slider is locked.
        speed_locked: bool,
    },
    /// The round-trip time to the Player in whole milliseconds, once a second while paired (`None`
    /// until the first pong arrives).
    Rtt { ms: Option<u32> },
    /// Whether the LAN discovery beacon is currently being advertised.
    Advertising { active: bool },
    /// The type-ahead / macro send buffer is actively keying.
    Sending { active: bool },
    /// The current serial-port inventory.
    Ports { available: Vec<String> },
    /// The current audio output device inventory (for the sidetone device picker).
    AudioDevices { available: Vec<AudioDeviceView> },
    /// A short, per-control error (empty `message` clears it). Full detail is also logged.
    Error { control: String, message: String },
}

/// One audio output device for the UI picker: its name and whether it is the system default.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDeviceView {
    pub name: String,
    pub is_default: bool,
}

/// A sink the host (Tauri) registers for [`ClientEvent`]s.
pub type EventSink = Arc<dyn Fn(ClientEvent) + Send + Sync>;

#[derive(Debug, Clone)]
struct Settings {
    wpm: u32,
    weight: u32,
    mode: KeyerMode,
    paddle_reverse: bool,
    tone_hz: i32,
    tone_volume: u32,
    /// `None` = default output device; `Some("")` = sidetone off; `Some(name)` = that device.
    audio_device: Option<String>,
    logger_port: Option<String>,
    paddle_port: Option<String>,
    transport: Transport,
    relay: String,
    callsign: String,
    keyer_id: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            wpm: 20,
            weight: 50,
            mode: KeyerMode::IambicB,
            paddle_reverse: false,
            tone_hz: 600,
            tone_volume: 70,
            audio_device: None,
            logger_port: None,
            paddle_port: None,
            transport: Transport::Wan,
            relay: pairing::DEFAULT_WAN_RELAY.to_string(),
            callsign: String::new(),
            keyer_id: String::new(),
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
    /// Interprets the `audio_device` setting: `None` = default, `Some("")` = off,
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
            .name("wrk-sidetone".into())
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
    let engine =
        SidetoneEngine::new(hz, volume as f64 / 100.0).and_then(|mut e| e.start_on(named).map(|()| e));
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
    /// Characters waiting to be keyed, in order, each with the WPM its batch was flushed at.
    queue: VecDeque<(char, u32)>,
    /// Set by [`ClientController::abort_send`]; the worker drops the queue and the current
    /// schedule immediately when it sees this, then clears it.
    abort: bool,
    /// Set once at shutdown so the worker thread exits.
    shutdown: bool,
}

/// A thread-safe send buffer: a lock plus a condvar the worker waits on.
#[derive(Default)]
struct SendBuffer {
    state: Mutex<SendState>,
    cv: Condvar,
}

impl SendBuffer {
    /// Appends `text` to the queue at `wpm` and wakes the worker.
    fn push(&self, text: &str, wpm: u32) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.queue.extend(text.chars().map(|c| (c, wpm)));
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

struct Shared {
    settings: Mutex<Settings>,
    sidetone: Mutex<Option<Sidetone>>,
    /// The shared WinKeyer host on the logger port, present while a logger port is selected.
    logger: Mutex<Option<LoggerHost>>,
    /// True while the logger (N1MM) has host mode open. It owns the keying speed then, so the
    /// Client's own speed changes are ignored.
    speed_from_logger: AtomicBool,
    transport: Mutex<Option<Arc<EdgeTransport>>>,
    epoch: Mutex<u16>,
    /// The shared session edge path (`rwk-core`): one sender for every edge source and one
    /// ordered tunnel FIFO. Restarted once per pairing in `bring_up_keying`.
    edge: EdgeSession,
    paddle: Mutex<Option<PaddlePollerHandle>>,
    /// The type-ahead / macro send buffer, drained by the single send-worker thread.
    send: SendBuffer,
    session: tokio::sync::Mutex<Option<Session>>,
    /// The running LAN advertiser task (operating-position side), present while advertising.
    advertise: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The running WAN connect supervisor (pair → key → watch liveness → reconnect), present
    /// between a `connect` and the next `disconnect`/`connect`. Aborted so a new attempt or an
    /// unpair never leaves a second supervisor looping.
    supervisor: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    config: Mutex<Config>,
    config_path: Option<std::path::PathBuf>,
    sink: EventSink,
}

impl Shared {
    fn emit(&self, event: ClientEvent) {
        (self.sink)(event);
    }

    fn log(&self, level: &str, message: impl Into<String>) {
        self.emit(ClientEvent::Log { level: level.into(), message: message.into() });
    }

    /// Logs the full detail and shows a short red message under `control`.
    fn fail(&self, control: &str, short: impl Into<String>, detail: impl Into<String>) {
        self.log("warn", detail);
        self.emit(ClientEvent::Error { control: control.into(), message: short.into() });
    }

    /// Clears any red message under `control`.
    fn clear_error(&self, control: &str) {
        self.emit(ClientEvent::Error { control: control.into(), message: String::new() });
    }

    fn emit_settings(&self) {
        let s = self.settings.lock().unwrap();
        self.emit(ClientEvent::Settings {
            wpm: s.wpm,
            weight: s.weight,
            mode: mode_name(s.mode).into(),
            paddle_reverse: s.paddle_reverse,
            tone_hz: s.tone_hz,
            tone_volume: s.tone_volume,
            audio_device: s.audio_device.clone(),
            transport: s.transport.as_str().into(),
            relay: s.relay.clone(),
            callsign: s.callsign.clone(),
            keyer_id: s.keyer_id.clone(),
            paddle_port: s.paddle_port.clone(),
            logger_port: s.logger_port.clone(),
            speed_locked: self.speed_from_logger.load(Ordering::SeqCst),
        });
    }

    fn tone_device(&self) -> ToneDevice {
        ToneDevice::from_setting(&self.settings.lock().unwrap().audio_device)
    }

    fn persist(&self) {
        let Some(path) = &self.config_path else { return };
        {
            let s = self.settings.lock().unwrap();
            let mut config = self.config.lock().unwrap();
            if let Ok(addr) = s.relay.parse() {
                config.relay = Some(rwk_core::config::RelayConfig { address: addr });
            }
            config.callsign = s.callsign.clone();
            config.keyer_id = s.keyer_id.clone();
        }
        let config = self.config.lock().unwrap();
        if let Err(e) = config.save(path) {
            self.log("warn", format!("Could not save config: {e}"));
        }
    }
}

/// The Client controller.
pub struct ClientController {
    shared: Arc<Shared>,
    runtime: tokio::runtime::Runtime,
    _clock: Arc<dyn Clock>,
}

impl ClientController {
    /// Starts the controller: loads/creates the persisted identity, brings up local sidetone.
    pub fn start(sink: EventSink) -> Self {
        let path = app_paths::config_path(Role::Client);
        Self::start_with_config(sink, Some(path))
    }

    /// Starts against an explicit config path (`None` disables persistence, for tests).
    pub fn start_with_config(sink: EventSink, config_path: Option<std::path::PathBuf>) -> Self {
        let config = load_or_create(&config_path, Role::Client);
        // Seed the UI settings from the persisted config (callsign/keyer_id/relay survive).
        let mut settings = Settings::default();
        settings.callsign = config.callsign.clone();
        settings.keyer_id = config.keyer_id.clone();
        if let Some(r) = &config.relay {
            settings.relay = r.address.to_string();
        }
        let shared = Arc::new(Shared {
            settings: Mutex::new(settings.clone()),
            sidetone: Mutex::new(None),
            logger: Mutex::new(None),
            speed_from_logger: AtomicBool::new(false),
            transport: Mutex::new(None),
            epoch: Mutex::new(1),
            edge: EdgeSession::new(MonotonicClock.frequency(), MonotonicClock.now()),
            paddle: Mutex::new(None),
            send: SendBuffer::default(),
            session: tokio::sync::Mutex::new(None),
            advertise: tokio::sync::Mutex::new(None),
            supervisor: tokio::sync::Mutex::new(None),
            config: Mutex::new(config),
            config_path,
            sink,
        });

        let device = ToneDevice::from_setting(&settings.audio_device);
        let sidetone =
            Sidetone::spawn(settings.tone_hz, settings.tone_volume, device, Arc::clone(&shared));
        *shared.sidetone.lock().unwrap() = Some(sidetone);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        // The single send-worker thread drains the type-ahead / macro buffer with real CW
        // timing. It uses the controller's multi-thread runtime handle so the fire-and-forget
        // tunnel sends in `drive_edge` run on a real worker thread — never blocking the keyer.
        spawn_send_worker(Arc::clone(&shared), runtime.handle().clone());

        let controller = Self { shared, runtime, _clock: Arc::new(MonotonicClock) };
        // Open whatever persisted ports are present right now.
        controller.open_paddle_port(settings.paddle_port.clone());
        controller.shared.emit_settings();
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
        if self.shared.speed_from_logger.load(Ordering::SeqCst) {
            // N1MM owns the speed while it has the logger open: ignore the slider and resync the UI.
            self.shared.emit_settings();
            return;
        }
        self.shared.settings.lock().unwrap().wpm = wpm;
    }

    pub fn set_weight(&self, weight: u32) {
        self.shared.settings.lock().unwrap().weight = weight;
    }

    pub fn set_mode(&self, mode: &str) {
        self.shared.settings.lock().unwrap().mode = parse_mode(mode);
    }

    pub fn set_paddle_reverse(&self, reverse: bool) {
        self.shared.settings.lock().unwrap().paddle_reverse = reverse;
    }

    pub fn set_transport(&self, transport: &str) {
        self.shared.settings.lock().unwrap().transport = Transport::parse(transport);
        self.shared.emit_settings();
    }

    pub fn set_relay(&self, relay: &str) {
        self.shared.settings.lock().unwrap().relay = relay.to_string();
        self.shared.persist();
        self.shared.emit_settings();
    }

    /// Sets the operator callsign used as the LAN beacon name.
    pub fn set_callsign(&self, callsign: &str) {
        self.shared.settings.lock().unwrap().callsign = callsign.to_string();
        self.shared.persist();
        self.shared.emit_settings();
    }

    /// Sets the position label (e.g. `20m`) appended to the callsign in the LAN beacon.
    pub fn set_keyer_id(&self, keyer_id: &str) {
        self.shared.settings.lock().unwrap().keyer_id = keyer_id.to_string();
        self.shared.persist();
        self.shared.emit_settings();
    }

    // ─── LAN advertise (operating-position side) ───────────────────────────────

    /// Starts (or restarts) broadcasting this keyer's LAN beacon so radio-site apps can find
    /// it, and waits for one to pick us — then brings the keying edge up as the responder.
    /// Stops any WAN pairing first. Advertising stops automatically once paired.
    pub fn lan_advertise(&self, on: bool) {
        if !on {
            self.stop_advertising();
            self.disconnect();
            return;
        }
        // Only one pairing attempt at a time.
        self.disconnect();
        let (callsign, keyer_id) = {
            let s = self.shared.settings.lock().unwrap();
            (s.callsign.clone(), s.keyer_id.clone())
        };
        let shared = Arc::clone(&self.shared);
        shared.emit(ClientEvent::Connection { state: "connecting".into(), path: "none".into() });
        shared.emit(ClientEvent::Advertising { active: true });
        shared.log("info", "Advertising on the LAN; waiting for a radio site to connect…");

        let handle = self.runtime.spawn(async move {
            lan_advertise_and_await(shared, callsign, keyer_id).await;
        });
        self.runtime.block_on(async {
            *self.shared.advertise.lock().await = Some(handle);
        });
    }

    fn stop_advertising(&self) {
        let shared = Arc::clone(&self.shared);
        self.runtime.block_on(async move {
            if let Some(task) = shared.advertise.lock().await.take() {
                task.abort();
            }
        });
        self.shared.emit(ClientEvent::Advertising { active: false });
    }

    // ─── Sidetone ──────────────────────────────────────────────────────────────

    pub fn set_tone_frequency(&self, hz: i32) {
        self.shared.settings.lock().unwrap().tone_hz = hz;
        self.restart_sidetone();
    }

    pub fn set_tone_volume(&self, percent: u32) {
        self.shared.settings.lock().unwrap().tone_volume = percent.min(100);
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
        self.shared.emit(ClientEvent::AudioDevices { available: Self::audio_devices() });
    }

    /// Re-enumerates audio devices and pushes the list (for the UI refresh button / PnP).
    pub fn refresh_audio_devices(&self) {
        self.emit_audio_devices();
    }

    // ─── Ports ───────────────────────────────────────────────────────────────

    #[must_use]
    pub fn serial_ports() -> Vec<String> {
        enumerate_ports()
            .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
            .unwrap_or_default()
    }

    fn emit_ports(&self) {
        self.shared.emit(ClientEvent::Ports { available: Self::serial_ports() });
    }

    /// The serial ports we currently hold open (paddle poller running, keying output open).
    /// Used so reconciliation does not drop a working, in-use port the OS hides from
    /// enumeration while it is busy.
    fn held_ports(&self) -> Vec<String> {
        let s = self.shared.settings.lock().unwrap();
        let mut held = Vec::new();
        if self.shared.paddle.lock().unwrap().is_some() {
            if let Some(p) = &s.paddle_port {
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

    pub fn select_logger_port(&self, port: Option<String>) {
        let paddle = self.shared.settings.lock().unwrap().paddle_port.clone();
        if let (Some(p), Some(other)) = (&port, &paddle) {
            if p == other {
                self.shared.fail(
                    control::LOGGER,
                    format!("{p} is the paddle port"),
                    format!("{p} is already the paddle input; a port can serve one job."),
                );
                return;
            }
        }
        self.shared.settings.lock().unwrap().logger_port = port.clone();
        self.open_logger_port(port);
        self.shared.emit_settings();
    }

    pub fn select_paddle_port(&self, port: Option<String>) {
        let logger = self.shared.settings.lock().unwrap().logger_port.clone();
        if let (Some(p), Some(other)) = (&port, &logger) {
            if p == other {
                self.shared.fail(
                    control::PADDLE,
                    format!("{p} is the logger port"),
                    format!("{p} is already the logger port; a port can serve one job."),
                );
                return;
            }
        }
        self.shared.settings.lock().unwrap().paddle_port = port.clone();
        self.open_paddle_port(port);
        self.shared.emit_settings();
    }

    fn open_logger_port(&self, port: Option<String>) {
        // Replacing the handle drops the previous one, which stops its thread and frees the port.
        *self.shared.logger.lock().unwrap() = None;
        self.shared.speed_from_logger.store(false, Ordering::SeqCst);
        let Some(name) = port else {
            self.shared.clear_error(control::LOGGER);
            self.shared.log("info", "Logger port cleared.");
            return;
        };
        let shared = Arc::clone(&self.shared);
        let port = name.clone();
        let report: HostReporter = Arc::new(move |report: HostReport| match report {
            HostReport::Log { level, message } => shared.log(&level, message),
            HostReport::ReadFailed(e) => shared.fail(
                control::LOGGER,
                format!("{port} not available"),
                format!("Logger port {port} failed: {e}"),
            ),
            HostReport::Sending(_) => {}
        });
        let sink = ClientLogger { shared: Arc::clone(&self.shared) };
        match LoggerHost::open_text(&name, sink, report) {
            Ok(host) => {
                *self.shared.logger.lock().unwrap() = Some(host);
                self.shared.clear_error(control::LOGGER);
                self.shared.log("info", format!("Logger port {name} listening for WinKeyer commands."));
            }
            Err(e) => self.shared.fail(control::LOGGER, format!("{name} not available"), e),
        }
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
                let handle = PaddlePollerHandle::spawn(
                    input,
                    Arc::clone(&self.shared),
                    self.runtime.handle().clone(),
                );
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

    /// Re-enumerates serial ports and reconciles the two selections (paddle, logger): any
    /// selected port no longer present is dropped to `None` and its handle released, without
    /// disturbing the other. Always emits the fresh port list; also refreshes audio devices.
    ///
    /// A port we currently hold open is never treated as missing even if absent from the
    /// enumeration: on Windows an exclusively-opened COM port is commonly hidden from
    /// `available_ports()` while in use, and dropping a working, in-use selection would be a
    /// bug (it caused selections to snap back to `(None)`).
    pub fn reconcile_ports(&self) {
        let mut present = Self::serial_ports();
        for held in self.held_ports() {
            if !present.contains(&held) {
                present.push(held);
            }
        }
        present.sort_by(|a, b| natural_port_key(a).cmp(&natural_port_key(b)));
        self.shared.emit(ClientEvent::Ports { available: present.clone() });
        self.emit_audio_devices();

        let (paddle_gone, logger_gone) = {
            let s = self.shared.settings.lock().unwrap();
            (port_missing(&s.paddle_port, &present), port_missing(&s.logger_port, &present))
        };
        if !(paddle_gone || logger_gone) {
            return;
        }

        if paddle_gone {
            let name = self.shared.settings.lock().unwrap().paddle_port.take();
            if let Some(n) = name {
                self.shared.log("warn", format!("Paddle port {n} removed; set to (None)."));
            }
            self.open_paddle_port(None);
        }
        if logger_gone {
            let name = self.shared.settings.lock().unwrap().logger_port.take();
            if let Some(n) = name {
                self.shared.log("warn", format!("Logger port {n} removed; set to (None)."));
            }
            self.open_logger_port(None);
        }
        self.shared.emit_settings();
    }

    // ─── Keying (type-ahead / macros) ──────────────────────────────────────────

    /// Appends text (a typed character or a macro) to the send buffer. It is keyed with proper
    /// CW spacing by the send worker, after whatever is already queued.
    pub fn send_cw(&self, text: &str) {
        let wpm = self.shared.settings.lock().unwrap().wpm;
        self.shared.send.push(text, wpm);
    }

    pub fn test_tx(&self) {
        self.send_cw("VVV TEST");
    }

    /// Immediately stops sending: flushes the buffer and drops the in-progress character
    /// (ESC / WinKeyer "clear buffer"). The key is forced up.
    pub fn abort_send(&self) {
        self.shared.send.abort();
    }

    // ─── Pairing ──────────────────────────────────────────────────────────────

    /// Pairs with the Server using the current transport (WAN/LAN) + token, then brings the
    /// keying edge up over the resulting tunnel.
    pub fn connect(&self, token: &str) {
        let (transport, relay) = {
            let s = self.shared.settings.lock().unwrap();
            (s.transport, s.relay.clone())
        };
        let token = token.to_string();
        // Cancel any prior pairing / supervisor so a re-Connect never leaves two loops running.
        self.disconnect();
        let shared = Arc::clone(&self.shared);
        shared.emit(ClientEvent::Connection { state: "connecting".into(), path: "none".into() });

        let handle = self.runtime.spawn(async move {
            // Supervised WAN connect: pair, key, and watch the Player's liveness. If the Player
            // stops heartbeating (closed, or the link dropped), reconnect automatically with the
            // same token, up to MAX_RECONNECT_ATTEMPTS consecutive failures before giving up.
            let mut attempts: u32 = 0;
            // Opened when a link is lost; the Keyer retries within it, then reports disconnected.
            let mut budget: Option<rwk_core::engine::reconnect::ReconnectWindow> = None;
            loop {
                let window = *budget.get_or_insert_with(|| {
                    rwk_core::engine::reconnect::ReconnectWindow::open(std::time::Instant::now())
                });
                let remaining = window.remaining(std::time::Instant::now());
                if remaining.is_zero() {
                    fail(
                        &shared,
                        format!(
                            "Could not reach the Player within {}s; disconnected. Reconnect to try again.",
                            RECONNECT_BUDGET.as_secs()
                        ),
                    );
                    return;
                }
                let base = shared.config.lock().unwrap().clone();
                let config =
                    match pairing::build_config(&base, Role::Client, transport, &token, &relay) {
                        Ok(c) => c,
                        Err(e) => return fail(&shared, e),
                    };

                // Each attempt is capped by the budget left, so pairing cannot hang past it.
                let attempt = tokio::time::timeout(remaining, tunnel::connect(&config)).await;
                match attempt {
                    Err(_) => shared.log("warn", "link failed: reconnect budget elapsed"),
                    Ok(Ok(connected)) => {
                        if let Some(edge) = bring_up_keying(&shared, connected).await {
                            // Connected: a successful pairing clears the failure budget.
                            attempts = 0;
                            budget = None;
                            let outcome = watch_edge_liveness(edge, Arc::clone(&shared)).await;
                            // Tear the dead session down before retrying.
                            teardown_session(&shared).await;
                            if outcome == Liveness::Closed {
                                // A deliberate local teardown (unpair) ends the supervisor.
                                return;
                            }
                            // Link lost: a fresh reconnect window opens on the next pass.
                            shared.log("warn", "Player stopped responding; reconnecting…");
                            shared.emit(ClientEvent::Connection {
                                state: "connecting".into(),
                                path: "none".into(),
                            });
                            continue;
                        }
                        // bring_up failed after a link came up; count it as an attempt.
                    }
                    Ok(Err(e)) => shared.log("warn", format!("link failed: {e}")),
                }

                attempts += 1;
                if attempts >= MAX_RECONNECT_ATTEMPTS {
                    fail(
                        &shared,
                        format!(
                            "Could not reach the Player after {MAX_RECONNECT_ATTEMPTS} attempts; \
                             giving up. Reconnect to try again."
                        ),
                    );
                    return;
                }
                shared.log(
                    "info",
                    format!("Reconnect attempt {attempts}/{MAX_RECONNECT_ATTEMPTS}…"),
                );
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
        self.runtime.block_on(async {
            *self.shared.supervisor.lock().await = Some(handle);
        });
    }

    pub fn disconnect(&self) {
        *self.shared.transport.lock().unwrap() = None;
        // Ends the pairing's ordered tunnel FIFO; the next pairing starts a fresh one.
        self.shared.edge.detach();
        let shared = Arc::clone(&self.shared);
        self.runtime.block_on(async move {
            if let Some(task) = shared.supervisor.lock().await.take() {
                task.abort();
            }
            if let Some(task) = shared.advertise.lock().await.take() {
                task.abort();
            }
            if let Some(mut session) = shared.session.lock().await.take() {
                session.shutdown().await;
            }
        });
        self.shared.emit(ClientEvent::Advertising { active: false });
        self.shared
            .emit(ClientEvent::Connection { state: "disconnected".into(), path: "none".into() });
        self.shared.log("info", "Unpaired.");
    }
}

/// The operating-position LAN flow: a **persistent responder**. The operator arms it once
/// (Advertise); it then keeps answering radio sites until unpaired. Each cycle binds a fresh
/// WireGuard socket, broadcasts the beacon with that port, waits for a picker to initiate, and
/// brings the keying edge up. When that session is superseded by the next picker (or the link
/// is retried), the loop re-binds and re-advertises — so the keyer reconnects without the
/// operator touching Advertise again. The radio site resolves the keyer's *current* port from
/// the live beacon at connect time, so the re-bound port is never stale.
///
/// Runs as one task; aborting it (unpair) stops the beacon and the wait. The beacon stops only
/// while a handshake is being completed, then resumes for the next picker.
async fn lan_advertise_and_await(shared: Arc<Shared>, callsign: String, keyer_id: String) {
    let identity = match shared.config.lock().unwrap().identity.to_identity() {
        Ok(id) => id,
        Err(e) => return fail(&shared, format!("identity error: {e}")),
    };

    loop {
        // 1. Bind a fresh WireGuard socket for this cycle so the beacon advertises a live port.
        let (socket, wg_port) = match tunnel::lan_bind().await {
            Ok(pair) => pair,
            Err(e) => return fail(&shared, format!("LAN bind failed: {e}")),
        };

        // 2. Broadcast the beacon until a picker connects.
        let beacon = Beacon {
            callsign: callsign.clone(),
            keyer_id: keyer_id.clone(),
            public_key: identity.public_bytes(),
            wg_port,
        };
        let advertiser = match LanAdvertiser::new(beacon) {
            Ok(a) => a,
            Err(e) => return fail(&shared, format!("LAN advertise failed: {e}")),
        };
        let beacon_task = tokio::spawn(advertiser.run());
        shared.emit(ClientEvent::Advertising { active: true });

        // 3. Wait for a radio site to pick us and complete the handshake (we are the responder).
        let connected = match tunnel::lan_await_initiator(socket, &identity).await {
            Ok(c) => c,
            Err(e) => {
                beacon_task.abort();
                // A timeout or socket error on this cycle: log and re-arm rather than giving
                // up, so a transient hiccup does not silently stop the keyer responding.
                shared.log("warn", format!("LAN listen cycle ended: {e}; re-advertising."));
                continue;
            }
        };
        // Handshaking: stop this cycle's beacon so we are not picked twice mid-handshake.
        beacon_task.abort();
        shared.emit(ClientEvent::Advertising { active: false });

        // 4. Bring the keying edge up, then watch the Player's liveness. When the Player stops
        //    responding (closed, or the link dropped), tear the dead session down and loop to
        //    re-advertise, so the radio site can re-establish without the operator re-arming.
        if let Some(edge) = bring_up_keying(&shared, connected).await {
            let _ = watch_edge_liveness(edge, Arc::clone(&shared)).await;
            teardown_session(&shared).await;
            shared.emit(ClientEvent::Connection { state: "connecting".into(), path: "none".into() });
            shared.log("info", "Player stopped responding; re-advertising on the LAN…");
        }
    }
}

/// Tears the current pairing's edge + session down without emitting "disconnected" or touching
/// the LAN advertiser, so the WAN supervisor can reconnect cleanly. The time-critical keyer
/// threads key into `shared.edge`, which is detached here; with no transport the frames are
/// simply dropped until the next `bring_up_keying` reinstalls one.
async fn teardown_session(shared: &Arc<Shared>) {
    *shared.transport.lock().unwrap() = None;
    shared.edge.detach();
    if let Some(mut session) = shared.session.lock().await.take() {
        session.shutdown().await;
    }
}

/// Why an edge-liveness watch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    /// No inbound traffic from the Player within [`PEER_TIMEOUT`]; the peer is presumed gone.
    PeerLost,
    /// The edge receive stream ended (the tunnel/netstack closed) — a clean teardown.
    Closed,
}

/// The edge's round-trip time in whole milliseconds, or [`None`] before the first pong.
fn rtt_millis(edge: &EdgeTransport) -> Option<u32> {
    edge.rtt().map(|d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX))
}

/// Watches the live edge for Player liveness while beaconing our own heartbeat.
///
/// Runs two things concurrently against the current `edge`:
/// * a 1 Hz heartbeat sender, so the Player can tell this keyer is up even when idle;
/// * an inbound watch: every datagram (a Player heartbeat, or any edge frame) refreshes a
///   deadline; if [`PEER_TIMEOUT`] passes with silence, the peer is declared [`Liveness::PeerLost`].
///
/// Returns when the peer is lost or the stream closes. Keying out is unaffected — this only
/// observes; the caller decides whether to reconnect.
async fn watch_edge_liveness(edge: Arc<EdgeTransport>, shared: Arc<Shared>) -> Liveness {
    let recv_edge = Arc::clone(&edge);
    let receiver = async move {
        loop {
            // A received datagram (heartbeat or frame) proves the peer is alive this instant.
            match tokio::time::timeout(PEER_TIMEOUT, recv_edge.recv_edge()).await {
                // Any inbound datagram (Some) or a dropped-but-received one (None) = alive.
                Ok(Ok(_)) => continue,
                // The stream ended (netstack stopped): a clean close.
                Ok(Err(_)) => return Liveness::Closed,
                // No datagram at all within the window: the Player is presumed gone.
                Err(_) => return Liveness::PeerLost,
            }
        }
    };

    // Each tick: a liveness heartbeat, a ping that the Player answers so we can measure the round
    // trip, and the RTT the UI shows.
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
                shared.emit(ClientEvent::Rtt { ms: rtt_millis(&hb_edge) });
            }
        }
    };

    // The heartbeat future never completes; the receiver decides the outcome.
    tokio::select! {
        outcome = receiver => outcome,
        _ = heartbeat => Liveness::Closed,
    }
}

/// Shared tail of both the WAN and LAN connect paths: start the session, open the keying edge
/// over the tunnel, install it, and report connected. Returns the live [`EdgeTransport`] so the
/// caller can watch it for peer liveness, or [`None`] if bring-up failed.
async fn bring_up_keying(shared: &Arc<Shared>, connected: tunnel::Connected) -> Option<Arc<EdgeTransport>> {
    let path = format!("{:?}", connected.path).to_lowercase();
    let config = shared.config.lock().unwrap().clone();
    let session = match Session::start(connected, &config) {
        Ok(s) => s,
        Err(e) => {
            fail(shared, format!("session failed: {e}"));
            return None;
        }
    };
    let net = session.handle();
    let conn = match net.open_udp(EDGE_PORT).await {
        Ok(c) => c,
        Err(e) => {
            fail(shared, format!("edge open failed: {e}"));
            return None;
        }
    };
    let edge = Arc::new(EdgeTransport::over_tunnel(conn));
    let peer = std::net::SocketAddr::new(
        std::net::IpAddr::V4(Role::Client.peer_inner_address()),
        EDGE_PORT,
    );
    edge.set_peer(peer).await;

    // A new pairing restarts the edge stream at sequence 0 and starts its ordered tunnel FIFO.
    // The Station builds a fresh replayer per session, so it accepts the restart.
    shared.edge.begin(*shared.epoch.lock().unwrap(), MonotonicClock.now(), Arc::clone(&edge));
    *shared.transport.lock().unwrap() = Some(Arc::clone(&edge));

    *shared.session.lock().await = Some(session);
    // The epoch is intentionally NOT bumped here. Both edge sources (the paddle poller and
    // the type-ahead send worker) sample `shared.epoch` independently, so a bump would make
    // them disagree and the Server would F4 whichever source sent second. The server adopts
    // the first frame's epoch of each fresh session and re-anchors on reconnect, so a stable
    // epoch is both correct and necessary.
    shared.emit(ClientEvent::Connection { state: "connected".into(), path: path.clone() });
    shared.log("info", format!("Paired with the Player over the {path} path."));
    Some(edge)
}

impl Drop for ClientController {
    fn drop(&mut self) {
        self.shared.send.shutdown();
        self.disconnect();
    }
}

fn fail(shared: &Arc<Shared>, message: String) {
    shared.log("warn", message);
    shared.emit(ClientEvent::Connection { state: "disconnected".into(), path: "none".into() });
}

/// Loads or creates the persisted config for `role`, saving a fresh one so the identity is
/// stable across runs.
fn load_or_create(path: &Option<std::path::PathBuf>, role: Role) -> Config {
    match path {
        Some(p) if p.exists() => {
            Config::load(p).unwrap_or_else(|_| Config::new(role).expect("identity"))
        }
        _ => {
            let config = Config::new(role).expect("generate identity");
            if let Some(p) = path {
                let _ = config.save(p);
            }
            config
        }
    }
}

// ─── Logger port: the shared WinKeyer host, with text to the send buffer ──────

/// The Client's logger output. The shared WinKeyer host (`rwk_core::engine::logger_host`) answers
/// the logger and batches its text; this sink takes the text into the send buffer, which keys it
/// with sidetone, the key line, and the tunnel.
struct ClientLogger {
    shared: Arc<Shared>,
}

impl TextSink for ClientLogger {
    fn speed(&self) -> u32 {
        self.shared.settings.lock().unwrap().wpm
    }

    fn set_speed(&self, wpm: u32) {
        self.shared.settings.lock().unwrap().wpm = wpm;
        self.shared.emit_settings();
    }

    fn host_mode(&self, open: bool) {
        // N1MM owns the speed while host mode is open; the Client's slider is locked.
        self.shared.speed_from_logger.store(open, Ordering::SeqCst);
        self.shared.emit_settings();
    }

    fn send_text(&self, text: &str, wpm: u32) {
        self.shared.send.push(text, wpm);
    }

    fn abort(&self) {
        self.shared.send.abort();
    }
}

// ─── Per-edge output fan-out (sidetone + key line + UI + tunnel) ───────────────

fn drive_edge(
    shared: &Arc<Shared>,
    rt: &tokio::runtime::Handle,
    tone: Option<&Sidetone>,
    key_down: bool,
    timestamp_ticks: u64,
) {
    // Local keying — sidetone and the UI indicator — happens
    // immediately and NEVER waits on the network, so the operator always hears clean CW even
    // if the tunnel stalls.
    if let Some(tone) = tone {
        tone.key(key_down);
    }
    shared.emit(ClientEvent::KeyState { down: key_down });

    // Hand the frame to the ordered outbound queue (non-blocking, FIFO). A single drain task
    // sends them over the tunnel in order, so the keyer thread never waits on the network and
    // datagrams never reorder — the latter is what broke keying above ~39 WPM. The frame
    // carries its own timestamp, so the Station reconstructs spacing regardless of send jitter.
    shared.edge.push_edge(key_down, timestamp_ticks);
    let _ = rt;
}

// ─── Send worker: drains the type-ahead / macro buffer with real CW timing ─────

/// Spawns the single send-worker thread. It waits for buffered characters and keys them as a
/// continuous CW element stream through [`drive_edge`] (sidetone + local key line + UI + the
/// tunnel) with correct element/character/word spacing. An abort (ESC) drops the queue and the
/// in-progress schedule immediately; characters typed during a send chain on seamlessly.
fn spawn_send_worker(shared: Arc<Shared>, rt: tokio::runtime::Handle) {
    std::thread::Builder::new()
        .name("wrk-sender".into())
        .spawn(move || send_worker(&shared, &rt))
        .expect("spawn send worker");
}

/// The send worker keys the buffer as a continuous element stream, one character at a time,
/// managing the inter-character and word gaps itself. Characters typed while sending chain on
/// with correct spacing because the running `position` stays ahead of real time whenever the
/// buffer is non-empty.
fn send_worker(shared: &Arc<Shared>, rt: &tokio::runtime::Handle) {
    // Element timing is sub-element-critical: without a raised priority and a 1 ms timer the
    // OS scheduler can overshoot a ~17 ms inter-character gap by a whole quantum and smear the
    // spacing. Match the paddle loop's scheduling protection.
    let _priority = ThreadPriorityGuard::raise_time_critical();
    let _timer = TimerResolutionGuard::raise(1);
    let clock = MonotonicClock;
    let hz = clock.frequency();

    let mut position = clock.now();
    let mut prev_emitted = false;
    let mut sending = false;
    let aborting = || shared.send.state.lock().unwrap_or_else(|e| e.into_inner()).abort;

    loop {
        // Pop the next character, blocking until one is available.
        let next: Option<(char, u32)> = {
            let mut st = shared.send.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if st.shutdown {
                    return;
                }
                if st.abort {
                    st.abort = false;
                    st.queue.clear();
                    drop(st);
                    position = clock.now();
                    prev_emitted = false;
                    if sending {
                        let tone = shared.sidetone.lock().unwrap().clone();
                        drive_edge(shared, rt, tone.as_ref(), false, clock.now());
                        shared.emit(ClientEvent::Sending { active: false });
                        sending = false;
                    }
                    st = shared.send.state.lock().unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                if let Some(entry) = st.queue.pop_front() {
                    break Some(entry);
                }
                // Nothing queued: drop the SEND indicator and wait for more input.
                if sending {
                    drop(st);
                    shared.emit(ClientEvent::Sending { active: false });
                    sending = false;
                    st = shared.send.state.lock().unwrap_or_else(|e| e.into_inner());
                }
                st = shared.send.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some((ch, wpm)) = next else { continue };

        // WPM travels with the character, so a speed change applies to the next batch and does
        // not retime text already queued.
        let weight = shared.settings.lock().unwrap().weight;
        let timing = match KeyerElementTiming::from_speed(wpm, weight, hz) {
            Ok(t) => t,
            Err(e) => {
                shared.log("warn", format!("CW timing error: {e}"));
                continue;
            }
        };

        // Re-anchor the schedule cursor to "now" whenever it has fallen behind real time
        // (after any idle, and under scheduler slop). We do NOT clear `prev_emitted` here so a
        // character typed a moment after the previous one still gets its inter-character gap.
        let now = clock.now();
        if position < now {
            position = now;
        }

        if !sending {
            shared.emit(ClientEvent::Sending { active: true });
            sending = true;
        }

        // Word separator: advance by a word gap (only between words) and reset.
        if MorseTable::is_word_separator(ch) {
            if prev_emitted {
                position += timing.word_gap_ticks();
            }
            prev_emitted = false;
            continue;
        }

        let Some(pattern) = MorseTable::pattern(ch) else {
            continue; // unkeyable character: skip
        };

        if prev_emitted {
            position += timing.inter_character_gap_ticks();
        }

        let tone = shared.sidetone.lock().unwrap().clone();

        // Key each element of the character with intra-element gaps.
        let mut first_element = true;
        for symbol in pattern.chars() {
            if !first_element {
                position += timing.gap_ticks;
            }
            first_element = false;

            let element = if symbol == '-' { KeyerElement::Dah } else { KeyerElement::Dit };
            let duration = timing.ticks_for(element);

            HybridWaiter::wait_until_ticks(&clock, position, &aborting);
            if aborting() {
                break;
            }
            drive_edge(shared, rt, tone.as_ref(), true, position);

            HybridWaiter::wait_until_ticks(&clock, position + duration, &aborting);
            drive_edge(shared, rt, tone.as_ref(), false, position + duration);
            position += duration;

            if aborting() {
                break;
            }
        }
        prev_emitted = true;
    }
}

// ─── Serial paddle poller (same hardware approach as the Keyer-Router) ──────────

const PADDLE_DEBOUNCE_MS: u64 = 5;
const PADDLE_POLL_MS: u64 = 1;

struct PaddlePollerHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PaddlePollerHandle {
    fn spawn(mut input: PaddleInputPort, shared: Arc<Shared>, rt: tokio::runtime::Handle) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("wrk-paddle".into())
            .spawn(move || paddle_loop(&mut input, &shared, &rt, &stop_thread))
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

fn paddle_loop(
    input: &mut PaddleInputPort,
    shared: &Arc<Shared>,
    rt: &tokio::runtime::Handle,
    stop: &Arc<AtomicBool>,
) {
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
        let tone = shared.sidetone.lock().unwrap().clone();
        let played = keyer.run_once(
            &clock,
            &paddle_now,
            &mut |edge| {
                drive_edge(shared, rt, tone.as_ref(), edge.key_down, edge.timestamp_ticks);
            },
            &should_abort,
        );
        if !played {
            let next = clock.now() + poll_step.max(1);
            HybridWaiter::wait_until_ticks(&clock, next, &should_abort);
        }
    }
    let tone = shared.sidetone.lock().unwrap().clone();
    drive_edge(shared, rt, tone.as_ref(), false, clock.now());
}

fn read_keyer_settings(shared: &Arc<Shared>) -> (u32, u32, KeyerMode, bool) {
    let s = shared.settings.lock().unwrap();
    (s.wpm, s.weight, s.mode, s.paddle_reverse)
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

fn mode_name(mode: KeyerMode) -> &'static str {
    match mode {
        KeyerMode::IambicB => "iambicB",
        KeyerMode::IambicA => "iambicA",
        KeyerMode::Ultimatic => "ultimatic",
        KeyerMode::Bug => "bug",
        KeyerMode::Straight => "straight",
    }
}

/// Whether `selected` names a port that is not in `present` (and is `Some`).
fn port_missing(selected: &Option<String>, present: &[String]) -> bool {
    matches!(selected, Some(name) if !present.iter().any(|p| p == name))
}
