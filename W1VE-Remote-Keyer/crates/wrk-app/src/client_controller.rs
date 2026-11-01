//! The Client controller for W1VE Remote Keyer: paddle/keyer at the operating position.
//!
//! This is the Keyer-Router Client's keying path, distilled to the Remote Keyer's job: send
//! CW to a Server over the tunnel, with local sidetone and an optional serial key line. It
//! reuses `rwk-core` wholesale — the WireGuard tunnel, the relay rendezvous, the keyer timing,
//! the sidetone engine, the serial paddle input — and differs from the Keyer-Router Client
//! only in that there is no Ham Router: no forward rules, no control channel, no Flex.
//!
//! WAN pairing uses [`crate::pairing::build_config`] and the shared relay token. LAN pairing is
//! relay-free: the Client browses advertising desktop and W1VE ESP32 Players, validates a
//! selected discovery record, and initiates the matching session.
//!
//! Type-ahead and macros are keyed by a single **send-worker** thread draining a shared
//! [`SendBuffer`] as a continuous CW element stream (one character at a time, managing the
//! inter-character / word gaps itself), ported from the standalone W1VE Keyer. Each element
//! edge fans out through [`drive_edge`] so paired keying, local sidetone, the local key line,
//! and the UI KEY indicator all stay in lock-step.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};

use rwk_core::config::{Config, Role};
use rwk_core::engine::audio::SidetoneEngine;
use rwk_core::engine::debounce::ContactDebouncer;
use rwk_core::engine::edge_session::{EdgeDrain, EdgeSession};
use rwk_core::engine::esp_player::{
    EspPlayerAdvertisement, EspPlayerBrowser, EspPlayerInbound, EspPlayerSession,
    ESP_PLAYER_MISSED_PING_LIMIT, ESP_PLAYER_PING_INTERVAL,
};
use rwk_core::engine::keying::{KeyerElement, KeyerElementTiming};
use rwk_core::engine::logger_host::{HostReport, HostReporter, LoggerHost, TextSink};
use rwk_core::engine::lan_discovery::LanBrowser;
use rwk_core::engine::network::EdgeTransport;
use rwk_core::engine::reconnect::{
    self, connect_with_relay_retry, watch_edge_liveness, Liveness, ReconnectBudget,
};
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

/// Consecutive failed *connect* attempts after which the Client gives up and stays disconnected
/// (the operator must reconnect manually). The 15 s [`reconnect::RECONNECT_WINDOW`] budget bounds
/// the overall churn; this caps repeated hard failures inside it.
const MAX_RECONNECT_ATTEMPTS: u32 = 5;

/// Controlled cadence for the bounded ESP32 Player unicast-discovery pass.
const ESP_DISCOVERY_QUERY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);
/// Keeps the UI list fresh and lets both core browsers prune expired advertisements promptly.
const LAN_PLAYER_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

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
        /// The remembered WAN pairing token (empty until one has been used).
        token: String,
        paddle_port: Option<String>,
        logger_port: Option<String>,
        /// N1MM has the WinKeyer logger open and owns the speed; the Client's slider is locked.
        speed_locked: bool,
    },
    /// The round-trip time to the Player in whole milliseconds, once a second while paired (`None`
    /// until the first pong arrives).
    Rtt { ms: Option<u32> },
    /// Desktop and W1VE ESP32 Players currently available on the LAN.
    LanPlayers { players: Vec<LanPlayerView> },
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

/// A discovered LAN Player exposed to the web UI. The `kind` tag keeps desktop tunnel and
/// W1VE ESP32 Player selections distinct even when their display names are identical.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum LanPlayerView {
    Desktop { id: String, name: String, endpoint: String },
    Esp { id: String, name: String, endpoint: String, busy: bool },
}

/// A tagged Player selection returned by the web UI. The controller parses and matches every
/// identifier and endpoint against its current discovery catalogue before opening a session.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum LanPlayerSelection {
    Desktop { id: String, endpoint: String },
    Esp { id: String, endpoint: String },
}

#[derive(Clone)]
struct DesktopPlayer {
    public_key: [u8; 32],
    name: String,
    endpoint: SocketAddr,
}

#[derive(Default)]
struct LanPlayerCatalog {
    /// Zero means there is no live browse task. A task may update only its own generation.
    generation: u64,
    desktop: Vec<DesktopPlayer>,
    esp: Vec<EspPlayerAdvertisement>,
}

enum ValidatedLanSelection {
    Desktop { public_key: [u8; 32], endpoint: SocketAddr },
    Esp(EspPlayerAdvertisement),
}

/// Identifies one user-visible connection operation. A newer operation invalidates every older
/// asynchronous setup flow before it is allowed to install connection resources.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OperationId(u64);

#[derive(Default)]
struct OperationGate {
    generation: u64,
}

impl OperationGate {
    fn replace(&mut self) -> OperationId {
        self.generation = self.generation.wrapping_add(1);
        OperationId(self.generation)
    }

    fn permits(&self, operation: OperationId) -> bool {
        self.generation == operation.0
    }
}

/// One-way stop token shared by the ESP edge callback and its connection owner. A replacement
/// permanently revokes the old token before it detaches that operation's edge drain.
#[derive(Clone)]
struct EspSenderStop {
    active: Arc<AtomicBool>,
}

impl EspSenderStop {
    fn new() -> Self {
        Self { active: Arc::new(AtomicBool::new(true)) }
    }

    fn stop(&self) {
        self.active.store(false, Ordering::Release);
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

/// The ESP session and the callback capability that belongs only to its installation operation.
struct ActiveEspSession {
    operation: OperationId,
    session: Arc<EspPlayerSession>,
    sender_stop: EspSenderStop,
}

/// All resources tied to a Client pairing live behind one short-held installation gate. No
/// asynchronous setup code holds this lock while awaiting network I/O; it only acquires it to
/// verify its generation and atomically publish or retire resources and connection events.
struct ClientConnection {
    gate: OperationGate,
    transport: Option<Arc<EdgeTransport>>,
    session: Option<Session>,
    esp_session: Option<ActiveEspSession>,
    supervisor: Option<(OperationId, tokio::task::JoinHandle<()>)>,
}

impl ClientConnection {
    fn permits(&self, operation: OperationId) -> bool {
        self.gate.permits(operation)
    }

    fn permits_resource_install(&self, operation: OperationId) -> bool {
        self.permits(operation)
            && self.transport.is_none()
            && self.session.is_none()
            && self.esp_session.is_none()
    }
}

impl Default for ClientConnection {
    fn default() -> Self {
        Self {
            gate: OperationGate::default(),
            transport: None,
            session: None,
            esp_session: None,
            supervisor: None,
        }
    }
}

/// Resources detached from the installation gate for asynchronous cleanup. They are never
/// returned to the shared state, so an older operation can only tear down what it created.
struct RetiredClientConnection {
    transport: Option<Arc<EdgeTransport>>,
    session: Option<Session>,
    esp_session: Option<ActiveEspSession>,
    /// The exact old edge task detached while the operation gate was held.
    edge_drain: EdgeDrain,
    supervisor: Option<(OperationId, tokio::task::JoinHandle<()>)>,
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
    /// The last WAN pairing token entered, remembered so the operator need not retype it.
    token: String,
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
            token: String::new(),
        }
    }
}

impl Settings {
    /// Restores every persisted UI setting, keeping the default for anything never saved.
    fn from_config(config: &Config) -> Self {
        let defaults = Self::default();
        let k = &config.keyer;
        Self {
            wpm: k.wpm.unwrap_or(defaults.wpm).clamp(5, 60),
            weight: k.weight.unwrap_or(defaults.weight).clamp(25, 75),
            mode: k.mode.as_deref().map(parse_mode).unwrap_or(defaults.mode),
            paddle_reverse: k.paddle_reverse.unwrap_or(defaults.paddle_reverse),
            tone_hz: k.tone_hz.unwrap_or(defaults.tone_hz),
            tone_volume: k.tone_volume.unwrap_or(defaults.tone_volume).min(100),
            audio_device: k.audio_device.clone(),
            logger_port: k.logger_port.clone(),
            paddle_port: k.paddle_port.clone(),
            transport: config.transport.as_deref().map(Transport::parse).unwrap_or(defaults.transport),
            relay: config.relay.as_ref().map(|r| r.address.to_string()).unwrap_or(defaults.relay),
            token: config.pairing_code.clone().unwrap_or_default(),
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
    /// Serializes operation replacement with resource installation. The generation stored here
    /// is the authority for WAN, desktop-LAN, and ESP pairing work.
    connection: Mutex<ClientConnection>,
    epoch: Mutex<u16>,
    /// The shared session edge path (`rwk-core`): one sender for every edge source and one
    /// ordered outbound carrier. Restarted for each tunnel or W1VE ESP32 Player pairing.
    edge: EdgeSession,
    paddle: Mutex<Option<PaddlePollerHandle>>,
    /// The type-ahead / macro send buffer, drained by the single send-worker thread.
    send: SendBuffer,
    /// The live desktop/W1VE ESP32 Player browse task. It is aborted before a selection connects.
    browse: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Monotonic generation assigned to each browse task, preventing a cancelled task from
    /// repopulating the selection catalogue.
    browse_generation: AtomicU64,
    /// Discovery-validated candidates used as the only source for LAN selection endpoints.
    lan_players: Mutex<LanPlayerCatalog>,
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
            token: s.token.clone(),
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
            config.transport = Some(s.transport.as_str().to_string());
            config.pairing_code = (!s.token.is_empty()).then(|| s.token.clone());
            config.keyer = rwk_core::config::KeyerSettings {
                wpm: Some(s.wpm),
                weight: Some(s.weight),
                mode: Some(mode_name(s.mode).to_string()),
                paddle_reverse: Some(s.paddle_reverse),
                tone_hz: Some(s.tone_hz),
                tone_volume: Some(s.tone_volume),
                audio_device: s.audio_device.clone(),
                paddle_port: s.paddle_port.clone(),
                logger_port: s.logger_port.clone(),
                keying_port: None,
            };
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
        // The Client restores every keyer, sidetone, port, transport, relay and token setting;
        // Player display identity belongs to the advertising Server configuration.
        let settings = Settings::from_config(&config);
        let shared = Arc::new(Shared {
            settings: Mutex::new(settings.clone()),
            sidetone: Mutex::new(None),
            logger: Mutex::new(None),
            speed_from_logger: AtomicBool::new(false),
            connection: Mutex::new(ClientConnection::default()),
            epoch: Mutex::new(1),
            edge: EdgeSession::new(MonotonicClock.frequency(), MonotonicClock.now()),
            paddle: Mutex::new(None),
            send: SendBuffer::default(),
            browse: Mutex::new(None),
            browse_generation: AtomicU64::new(0),
            lan_players: Mutex::new(LanPlayerCatalog::default()),
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
        controller.open_logger_port(settings.logger_port.clone());
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
        self.shared.persist();
    }

    pub fn set_weight(&self, weight: u32) {
        self.shared.settings.lock().unwrap().weight = weight;
        self.shared.persist();
    }

    pub fn set_mode(&self, mode: &str) {
        self.shared.settings.lock().unwrap().mode = parse_mode(mode);
        self.shared.persist();
    }

    pub fn set_paddle_reverse(&self, reverse: bool) {
        self.shared.settings.lock().unwrap().paddle_reverse = reverse;
        self.shared.persist();
    }

    pub fn set_transport(&self, transport: &str) {
        let transport = Transport::parse(transport);
        let leaving_lan = {
            let mut settings = self.shared.settings.lock().unwrap();
            let leaving_lan = settings.transport == Transport::Lan && transport != Transport::Lan;
            settings.transport = transport;
            leaving_lan
        };
        self.shared.persist();
        if leaving_lan {
            self.disconnect();
        } else if transport != Transport::Lan {
            self.stop_browsing();
        }
        self.shared.emit_settings();
    }

    pub fn set_relay(&self, relay: &str) {
        self.shared.settings.lock().unwrap().relay = relay.to_string();
        self.shared.persist();
        self.shared.emit_settings();
    }

    // ─── LAN browse and selection (operating-position side) ───────────────────

    /// Starts a fresh LAN browse for advertising desktop and W1VE ESP32 Players. The ESP32
    /// browser begins one bounded unicast query pass in addition to accepting advertisements.
    pub fn lan_browse(&self) {
        // A browse is a new pairing operation, so it cannot coexist with a live WAN/LAN session.
        let operation = self.replace_operation();
        let identity = match self.shared.config.lock().unwrap().identity.to_identity() {
            Ok(identity) => identity,
            Err(error) => {
                log_client_if_current(&self.shared, operation, "warn", format!("identity error: {error}"));
                return;
            }
        };
        let self_key = identity.public_bytes();
        let keyer_id = keyer_id_from_public_key(self_key);
        let Some(generation) = activate_lan_browse(&self.shared, operation) else {
            return;
        };

        let shared = Arc::clone(&self.shared);
        let handle = self.runtime.spawn(async move {
            browse_lan_players(shared, generation, self_key, keyer_id).await;
        });
        install_browse_task(&self.shared, operation, generation, handle);
    }

    /// Validates a tagged UI selection against a current discovery record, then begins exactly
    /// the selected desktop tunnel or W1VE ESP32 Player session as the operating Keyer.
    pub fn connect_lan(&self, selection: LanPlayerSelection) {
        let selected = {
            let players = self.shared.lan_players.lock().unwrap();
            validate_lan_selection(&players, &selection)
        };
        let selected = match selected {
            Ok(selected) => selected,
            Err(message) => {
                self.shared.log("warn", message);
                return;
            }
        };

        // Clear the browse task only after the selected record has been matched. No ESP session
        // is constructed from a UI-supplied id/address; `selected` contains browser-owned data.
        let operation = self.replace_operation();
        let shared = Arc::clone(&self.shared);
        match selected {
            ValidatedLanSelection::Desktop { public_key, endpoint } => {
                if !announce_client_connecting(
                    &shared,
                    operation,
                    "Connecting to the selected desktop Player…",
                ) {
                    return;
                }
                let task_shared = Arc::clone(&shared);
                let handle = self.runtime.spawn(async move {
                    connect_desktop_player(&task_shared, operation, public_key, endpoint).await;
                });
                install_client_supervisor(&shared, operation, handle);
            }
            ValidatedLanSelection::Esp(player) => {
                if !announce_client_connecting(
                    &shared,
                    operation,
                    "Pairing with the selected W1VE ESP32 Player…",
                ) {
                    return;
                }
                let task_shared = Arc::clone(&shared);
                let handle = self.runtime.spawn(async move {
                    connect_esp_player(&task_shared, operation, player).await;
                });
                install_client_supervisor(&shared, operation, handle);
            }
        }
    }

    fn stop_browsing(&self) {
        // Use the same gate as operation replacement so a browse spawned immediately before
        // Stop cannot register its handle or repopulate the catalogue afterwards.
        let _connection = self.shared.connection.lock().unwrap();
        stop_browsing_locked(&self.shared);
    }

    // ─── Sidetone ──────────────────────────────────────────────────────────────

    pub fn set_tone_frequency(&self, hz: i32) {
        self.shared.settings.lock().unwrap().tone_hz = hz;
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
        self.shared.persist();
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
        self.shared.persist();
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

    /// Pairs with the Server over WAN using the current relay and shared token, then brings the
    /// keying edge up over the resulting tunnel.
    pub fn connect(&self, token: &str) {
        let (transport, relay) = {
            let s = self.shared.settings.lock().unwrap();
            (s.transport, s.relay.clone())
        };
        let token = token.trim().to_string();
        // Remember the token so the next launch pre-fills it.
        self.shared.settings.lock().unwrap().token = token.clone();
        self.shared.persist();
        // Replacement invalidates older tasks before they can install a tunnel, edge sender, or
        // session. It also tears down resources already installed by the prior operation.
        let operation = self.replace_operation();
        let shared = Arc::clone(&self.shared);
        if !announce_client_connecting(&shared, operation, "Connecting to the Player…") {
            return;
        }

        let task_shared = Arc::clone(&shared);
        let handle = self.runtime.spawn(async move {
            connect_wan(task_shared, operation, transport, relay, token).await;
        });
        install_client_supervisor(&shared, operation, handle);
    }

    pub fn disconnect(&self) {
        let _ = self.replace_operation();
    }

    /// Invalidates every existing asynchronous connection flow before cleanup starts. The new
    /// identifier is handed only to the replacement flow, so stale work cannot publish after a
    /// Stop or a subsequent Connect/LAN selection.
    fn replace_operation(&self) -> OperationId {
        let (operation, retired) = replace_client_operation(&self.shared);
        self.runtime.block_on(shutdown_retired_client_connection(retired));
        operation
    }
}

/// Replaces the current connection operation while holding the same gate used for installation.
/// The returned resources have already been made unreachable from shared state; they must be
/// shut down outside the lock so their old async work cannot affect a replacement operation.
fn replace_client_operation(shared: &Arc<Shared>) -> (OperationId, RetiredClientConnection) {
    let mut connection = shared.connection.lock().unwrap();
    let operation = connection.gate.replace();
    // Revoke the ESP callback before it loses its queue. The callback also checks this token, so
    // a buffered frame cannot begin a new ESP send after this retirement becomes visible.
    if let Some(esp) = connection.esp_session.as_ref() {
        esp.sender_stop.stop();
    }
    stop_browsing_locked(shared);
    // Ends the pairing's ordered edge FIFO before any old session is shut down. `edge_drain`
    // belongs only to this retired queue, so a later operation cannot be awaited by mistake.
    let edge_drain = shared.edge.detach_with_drain();
    let retired = RetiredClientConnection {
        transport: connection.transport.take(),
        session: connection.session.take(),
        esp_session: connection.esp_session.take(),
        edge_drain,
        supervisor: connection.supervisor.take(),
    };
    // Report teardown before awaiting a session shutdown, so the UI cannot stay live behind a
    // stuck network close. This is inside the installation gate, so a stale connect cannot emit
    // a later connected state after this Stop is visible.
    shared.emit(ClientEvent::Rtt { ms: None });
    shared.emit(ClientEvent::Connection { state: "disconnected".into(), path: "none".into() });
    shared.log("info", "Unpaired.");
    (operation, retired)
}

async fn shutdown_retired_client_connection(retired: RetiredClientConnection) {
    let RetiredClientConnection { transport, session, esp_session, edge_drain, supervisor } = retired;
    if let Some((_operation, task)) = supervisor {
        task.abort();
    }
    if let Some(esp) = esp_session {
        // The stop token was revoked before detaching. Awaiting this specific cancelled drain
        // makes the ESP terminal unpair the first carrier action after all old frame callbacks.
        edge_drain.wait().await;
        let _ = esp.session.send_unpair().await;
    }
    if let Some(mut session) = session {
        session.shutdown().await;
    }
    // Dropping the tunnel transport releases its UDP path after the global edge FIFO has been
    // detached. It is deliberately retained until here so teardown ownership is explicit.
    drop(transport);
}

/// Stops the LAN browse task and invalidates its catalogue generation. The caller must hold the
/// Client connection gate whenever this races a new operation or a browse-handle installation.
fn stop_browsing_locked(shared: &Arc<Shared>) {
    if let Some(task) = shared.browse.lock().unwrap().take() {
        task.abort();
    }
    shared.browse_generation.fetch_add(1, Ordering::SeqCst);
    let mut players = shared.lan_players.lock().unwrap();
    players.generation = 0;
    players.desktop.clear();
    players.esp.clear();
    shared.emit(ClientEvent::LanPlayers { players: Vec::new() });
}

/// Opens a fresh browse generation only if its parent operation is still current. Holding the
/// connection gate through catalogue publication closes the Stop/spawn window.
fn activate_lan_browse(shared: &Arc<Shared>, operation: OperationId) -> Option<u64> {
    let connection = shared.connection.lock().unwrap();
    if !connection.permits(operation) {
        return None;
    }
    let generation = shared.browse_generation.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    let mut players = shared.lan_players.lock().unwrap();
    players.generation = generation;
    players.desktop.clear();
    players.esp.clear();
    shared.emit(ClientEvent::LanPlayers { players: Vec::new() });
    Some(generation)
}

/// Stores a browse handle only if both its pairing operation and discovery generation remain
/// current. A stale spawned task is aborted instead of becoming an unreachable background task.
fn install_browse_task(
    shared: &Arc<Shared>,
    operation: OperationId,
    generation: u64,
    handle: tokio::task::JoinHandle<()>,
) {
    let connection = shared.connection.lock().unwrap();
    let mut browse = shared.browse.lock().unwrap();
    if connection.permits(operation)
        && shared.browse_generation.load(Ordering::SeqCst) == generation
    {
        if let Some(previous) = browse.replace(handle) {
            previous.abort();
        }
    } else {
        handle.abort();
    }
}

/// Stores the WAN/LAN supervisor only while its operation is current. This closes the race where
/// Stop runs after `spawn` but before the task handle reaches shared state.
fn install_client_supervisor(
    shared: &Arc<Shared>,
    operation: OperationId,
    handle: tokio::task::JoinHandle<()>,
) {
    let mut connection = shared.connection.lock().unwrap();
    if connection.permits(operation) {
        if let Some((_previous_operation, previous)) = connection.supervisor.replace((operation, handle)) {
            previous.abort();
        }
    } else {
        handle.abort();
    }
}

fn client_operation_is_current(shared: &Arc<Shared>, operation: OperationId) -> bool {
    shared.connection.lock().unwrap().permits(operation)
}

/// Pure ownership check used by the ESP frame sender. Both the operation generation and the
/// one-way sender token must still authorize a callback before it starts carrier I/O.
fn esp_sender_is_permitted(
    connection: &ClientConnection,
    operation: OperationId,
    sender_stop: &EspSenderStop,
) -> bool {
    connection.permits(operation) && sender_stop.is_active()
}

fn esp_sender_is_current(
    shared: &Arc<Shared>,
    operation: OperationId,
    sender_stop: &EspSenderStop,
) -> bool {
    let connection = shared.connection.lock().unwrap();
    esp_sender_is_permitted(&connection, operation, sender_stop)
}

/// Emits a connecting state while holding the operation gate, so it cannot appear after a
/// competing Stop or replacement has reported disconnected.
fn announce_client_connecting(shared: &Arc<Shared>, operation: OperationId, message: &str) -> bool {
    let connection = shared.connection.lock().unwrap();
    if !connection.permits(operation) {
        return false;
    }
    shared.emit(ClientEvent::Connection { state: "connecting".into(), path: "none".into() });
    shared.log("info", message);
    true
}

fn log_client_if_current(
    shared: &Arc<Shared>,
    operation: OperationId,
    level: &str,
    message: impl Into<String>,
) -> bool {
    let connection = shared.connection.lock().unwrap();
    if !connection.permits(operation) {
        return false;
    }
    shared.log(level, message);
    true
}

fn fail_client_operation(shared: &Arc<Shared>, operation: OperationId, message: impl Into<String>) {
    let connection = shared.connection.lock().unwrap();
    if !connection.permits(operation) {
        return;
    }
    shared.log("warn", message);
    shared.emit(ClientEvent::Connection { state: "disconnected".into(), path: "none".into() });
}

fn emit_client_rtt_if_current(shared: &Arc<Shared>, operation: OperationId, ms: Option<u32>) -> bool {
    let connection = shared.connection.lock().unwrap();
    if !connection.permits(operation) {
        return false;
    }
    shared.emit(ClientEvent::Rtt { ms });
    true
}

/// Browses both Player families in one cancellable task. The ESP32 query pass is deliberately
/// finite and paced; receiving an advertisement never creates a session by itself.
async fn browse_lan_players(
    shared: Arc<Shared>,
    generation: u64,
    self_key: [u8; 32],
    keyer_id: [u8; 6],
) {
    let mut desktop = match LanBrowser::bind(self_key) {
        Ok(browser) => browser,
        Err(error) => {
            shared.log("warn", format!("Desktop Player browse failed: {error}"));
            clear_lan_players(&shared, generation);
            return;
        }
    };
    // The desktop browser is already bound to the local LAN address selected for this Keyer.
    // Reuse that address for ESP type-10 traffic instead of independently guessing from an
    // Internet-route probe, which can select a VPN or virtual adapter on a multi-homed host.
    let esp_bind_address = match desktop.local_addr() {
        Ok(SocketAddr::V4(address)) if !address.ip().is_unspecified() => Some(*address.ip()),
        Ok(_) | Err(_) => None,
    };
    let mut esp = match esp_bind_address {
        Some(address) => EspPlayerBrowser::bind_on(keyer_id, address).await,
        None => EspPlayerBrowser::bind(keyer_id).await,
    }
    .map(Some)
    .unwrap_or_else(|error| {
        shared.log("warn", format!("W1VE ESP32 Player browse unavailable: {error}"));
        None
    });
    let mut query_active = false;
    if let Some(browser) = esp.as_mut() {
        browser.request_discovery();
        query_active = true;
    }

    shared.log("info", "Browsing the LAN for desktop and W1VE ESP32 Players…");
    if !publish_lan_players(&shared, generation, &mut desktop, esp.as_mut()) {
        return;
    }

    enum BrowseEvent {
        DesktopUpdated,
        DesktopStopped(String),
        EspUpdated,
        EspStopped(String),
        Deadline,
    }

    let mut next_query = tokio::time::Instant::now();
    let mut next_refresh = tokio::time::Instant::now() + LAN_PLAYER_REFRESH_INTERVAL;
    loop {
        // Send at most one unicast query per controlled interval. The core browser itself caps
        // this pass to its local subnet, and `request_discovery` was called exactly once above.
        let now = tokio::time::Instant::now();
        if query_active && now >= next_query {
            let result = match esp.as_mut() {
                Some(browser) => browser.send_next_discovery_query().await,
                None => Ok(false),
            };
            match result {
                Ok(has_more) => query_active = has_more,
                Err(error) => {
                    shared.log("warn", format!("W1VE ESP32 Player discovery query stopped: {error}"));
                    query_active = false;
                }
            }
            next_query = tokio::time::Instant::now() + ESP_DISCOVERY_QUERY_INTERVAL;
        }
        if tokio::time::Instant::now() >= next_refresh {
            if !publish_lan_players(&shared, generation, &mut desktop, esp.as_mut()) {
                return;
            }
            next_refresh = tokio::time::Instant::now() + LAN_PLAYER_REFRESH_INTERVAL;
        }

        let next_deadline = if query_active { next_query.min(next_refresh) } else { next_refresh };
        let wait = next_deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::select! {
            result = desktop.recv_once() => match result {
                Ok(_) => BrowseEvent::DesktopUpdated,
                Err(error) => BrowseEvent::DesktopStopped(error.to_string()),
            },
            result = async {
                match esp.as_mut() {
                    Some(browser) => browser.recv_once().await,
                    None => std::future::pending::<rwk_core::Result<bool>>().await,
                }
            } => match result {
                Ok(_) => BrowseEvent::EspUpdated,
                Err(error) => BrowseEvent::EspStopped(error.to_string()),
            },
            _ = tokio::time::sleep(wait) => BrowseEvent::Deadline,
        };
        match event {
            BrowseEvent::DesktopUpdated | BrowseEvent::EspUpdated => {
                if !publish_lan_players(&shared, generation, &mut desktop, esp.as_mut()) {
                    return;
                }
            }
            BrowseEvent::DesktopStopped(error) => {
                shared.log("warn", format!("Desktop Player browse stopped: {error}"));
                clear_lan_players(&shared, generation);
                return;
            }
            BrowseEvent::EspStopped(error) => {
                shared.log("warn", format!("W1VE ESP32 Player browse stopped: {error}"));
                esp = None;
                query_active = false;
                if !publish_lan_players(&shared, generation, &mut desktop, None) {
                    return;
                }
            }
            BrowseEvent::Deadline => {}
        }
    }
}

/// Publishes browser-owned records and replaces the selection catalogue only when this is the
/// current browse generation. Holding the catalogue lock through the event emission makes a
/// stop's empty snapshot win over any racing cancelled task.
fn publish_lan_players(
    shared: &Arc<Shared>,
    generation: u64,
    desktop: &mut LanBrowser,
    esp: Option<&mut EspPlayerBrowser>,
) -> bool {
    let desktop_players: Vec<DesktopPlayer> = desktop
        .peers()
        .into_iter()
        .map(|player| DesktopPlayer {
            public_key: player.public_key,
            name: player.display_name(),
            endpoint: player.endpoint,
        })
        .collect();
    let esp_players = match esp {
        Some(browser) => browser.players(),
        None => Vec::new(),
    };
    let mut views: Vec<LanPlayerView> = desktop_players
        .iter()
        .map(|player| LanPlayerView::Desktop {
            id: rwk_core::config::base64_encode(&player.public_key),
            name: player.name.clone(),
            endpoint: player.endpoint.to_string(),
        })
        .collect();
    views.extend(esp_players.iter().map(|player| LanPlayerView::Esp {
        id: player.id_hex(),
        name: player.name.clone(),
        endpoint: player.endpoint.to_string(),
        busy: player.busy(),
    }));

    let mut catalogue = shared.lan_players.lock().unwrap();
    if catalogue.generation != generation {
        return false;
    }
    catalogue.desktop = desktop_players;
    catalogue.esp = esp_players;
    shared.emit(ClientEvent::LanPlayers { players: views });
    true
}

fn clear_lan_players(shared: &Arc<Shared>, generation: u64) {
    let mut catalogue = shared.lan_players.lock().unwrap();
    if catalogue.generation != generation {
        return;
    }
    catalogue.desktop.clear();
    catalogue.esp.clear();
    shared.emit(ClientEvent::LanPlayers { players: Vec::new() });
}

/// Runs the WAN reconnect supervisor for one operation. The relay address and shared token are
/// deliberately retained exactly as supplied by `connect`; generation checks only decide whether
/// this operation may still publish a resulting session.
async fn connect_wan(
    shared: Arc<Shared>,
    operation: OperationId,
    transport: Transport,
    relay: String,
    token: String,
) {
    // Supervised WAN connect: pair, key, and watch the Player's liveness. If the Player stops
    // heartbeating (closed, or the link dropped), reconnect automatically with the same token,
    // up to MAX_RECONNECT_ATTEMPTS consecutive failures before giving up.
    let mut attempts: u32 = 0;
    // The shared reconnect budget: a 15 s window that only resets after a link has stayed up for
    // the stability threshold, so a pair-then-die flap cannot reconnect forever.
    let mut budget = ReconnectBudget::new();
    loop {
        if !client_operation_is_current(&shared, operation) {
            return;
        }
        let remaining = budget.remaining(std::time::Instant::now());
        if remaining.is_zero() {
            fail_client_operation(
                &shared,
                operation,
                format!(
                    "Could not reach the Player within {}s; disconnected. Reconnect to try again.",
                    reconnect::RECONNECT_WINDOW.as_secs()
                ),
            );
            return;
        }
        let base = shared.config.lock().unwrap().clone();
        let config = match pairing::build_config(&base, Role::Client, transport, &token, &relay) {
            Ok(config) => config,
            Err(error) => {
                fail_client_operation(&shared, operation, error);
                return;
            }
        };
        if !client_operation_is_current(&shared, operation) {
            return;
        }

        // Each attempt is capped by the budget left, so pairing cannot hang past it. The shared
        // helper retries the relay rendezvous once (750 ms) inside that budget.
        let attempt = tokio::time::timeout(remaining, connect_with_relay_retry(&config)).await;
        if !client_operation_is_current(&shared, operation) {
            // A completed `Connected` is dropped here before a Session has been started.
            return;
        }
        match attempt {
            Err(_) => {
                if !log_client_if_current(&shared, operation, "warn", "link failed: reconnect budget elapsed") {
                    return;
                }
            }
            Ok(Ok(connected)) => {
                if let Some(edge) = bring_up_keying(&shared, operation, connected).await {
                    if !client_operation_is_current(&shared, operation) {
                        return;
                    }
                    // A link came up; it must prove it is stable before the budget resets.
                    attempts = 0;
                    let connected_at = std::time::Instant::now();
                    let rtt_shared = Arc::clone(&shared);
                    let outcome = watch_edge_liveness(edge, move |ms| {
                        let _ = emit_client_rtt_if_current(&rtt_shared, operation, ms);
                    })
                    .await;
                    if !client_operation_is_current(&shared, operation) {
                        return;
                    }
                    // Only a link that stayed up past the stability threshold earns a fresh
                    // budget; a flap keeps consuming the current window.
                    budget.record_link_ended(connected_at, std::time::Instant::now());
                    // Tear the dead session down before retrying. This only takes resources that
                    // still belong to this operation.
                    if !teardown_session(&shared, operation).await {
                        return;
                    }
                    if outcome == Liveness::Closed {
                        // A deliberate local teardown (unpair) ends the supervisor.
                        return;
                    }
                    // Link lost: retry within whatever budget remains.
                    if !log_client_if_current(
                        &shared,
                        operation,
                        "warn",
                        "Player stopped responding; reconnecting…",
                    ) || !announce_client_connecting(&shared, operation, "Reconnecting to the Player…")
                    {
                        return;
                    }
                    continue;
                }
                if !client_operation_is_current(&shared, operation) {
                    return;
                }
                // bring_up failed after a link came up; count it as an attempt.
            }
            Ok(Err(error)) => {
                if !log_client_if_current(&shared, operation, "warn", format!("link failed: {error}")) {
                    return;
                }
            }
        }

        attempts += 1;
        if attempts >= MAX_RECONNECT_ATTEMPTS {
            fail_client_operation(
                &shared,
                operation,
                format!(
                    "Could not reach the Player after {MAX_RECONNECT_ATTEMPTS} attempts; \
                     giving up. Reconnect to try again."
                ),
            );
            return;
        }
        if !log_client_if_current(
            &shared,
            operation,
            "info",
            format!("Reconnect attempt {attempts}/{MAX_RECONNECT_ATTEMPTS}…"),
        ) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Resolves the selected desktop Player's current advertised endpoint, then connects as the
/// WireGuard initiator. The browser-provided endpoint remains an identity-validated fallback if
/// the Player re-advertisement is missed during the short resolution window.
async fn connect_desktop_player(
    shared: &Arc<Shared>,
    operation: OperationId,
    public_key: [u8; 32],
    endpoint_hint: SocketAddr,
) {
    if !client_operation_is_current(shared, operation) {
        return;
    }
    let config = shared.config.lock().unwrap().clone();
    let identity = match config.identity.to_identity() {
        Ok(identity) => identity,
        Err(error) => {
            fail_client_operation(shared, operation, format!("identity error: {error}"));
            return;
        }
    };
    let resolved = LanBrowser::resolve(
        identity.public_bytes(),
        public_key,
        std::time::Duration::from_secs(4),
    )
    .await;
    if !client_operation_is_current(shared, operation) {
        return;
    }
    let endpoint = match resolved {
        Ok(endpoint) => endpoint,
        Err(error) => {
            if !log_client_if_current(
                shared,
                operation,
                "warn",
                format!(
                    "Desktop Player did not refresh during selection: {error}; using its last advertised address."
                ),
            ) {
                return;
            }
            endpoint_hint
        }
    };
    let connected = tunnel::lan_connect_to(&identity, public_key, endpoint).await;
    if !client_operation_is_current(shared, operation) {
        // The tunnel has not become a Session yet, so dropping `Connected` tears down its
        // uninstalled setup state.
        return;
    }
    let connected = match connected {
        Ok(connected) => connected,
        Err(error) => {
            fail_client_operation(shared, operation, format!("LAN link failed: {error}"));
            return;
        }
    };
    if let Some(edge) = bring_up_keying(shared, operation, connected).await {
        if !client_operation_is_current(shared, operation) {
            return;
        }
        let rtt_shared = Arc::clone(shared);
        let _ = watch_edge_liveness(edge, move |ms| {
            let _ = emit_client_rtt_if_current(&rtt_shared, operation, ms);
        })
        .await;
        if !teardown_session(shared, operation).await {
            return;
        }
        let connection = shared.connection.lock().unwrap();
        if !connection.permits(operation) {
            return;
        }
        shared.emit(ClientEvent::Rtt { ms: None });
        shared.emit(ClientEvent::Connection { state: "disconnected".into(), path: "none".into() });
        shared.log("warn", "Desktop Player stopped responding; disconnected.");
    }
}

/// Pairs only with a discovery-validated W1VE ESP32 Player record. The compact Keyer id is
/// always derived from this Client's persisted WireGuard public key, never UI input.
async fn connect_esp_player(
    shared: &Arc<Shared>,
    operation: OperationId,
    player: EspPlayerAdvertisement,
) {
    if !client_operation_is_current(shared, operation) {
        return;
    }
    let identity = match shared.config.lock().unwrap().identity.to_identity() {
        Ok(identity) => identity,
        Err(error) => {
            fail_client_operation(shared, operation, format!("identity error: {error}"));
            return;
        }
    };
    let keyer_id = keyer_id_from_public_key(identity.public_bytes());
    let session = EspPlayerSession::pair(player, keyer_id).await;
    if !client_operation_is_current(shared, operation) {
        // Pairing succeeded after Stop/replacement. It was never published, so explicitly
        // release the Player's temporary pairing state instead of relying on task abort alone.
        if let Ok(session) = session {
            let _ = session.send_unpair().await;
        }
        return;
    }
    let session = match session {
        Ok(session) => Arc::new(session),
        Err(error) => {
            fail_client_operation(
                shared,
                operation,
                format!("W1VE ESP32 Player pairing failed: {error}"),
            );
            return;
        }
    };
    run_esp_player_session(shared, operation, session).await;
}

/// Installs the ESP edge sender and session only while this operation still owns the single
/// connection gate. The callback captures this operation and a one-way stop token, so it cannot
/// send an old queued frame after the session has been retired.
fn install_esp_session(
    shared: &Arc<Shared>,
    operation: OperationId,
    session: &Arc<EspPlayerSession>,
) -> Option<EspSenderStop> {
    let message = format!(
        "Paired with W1VE ESP32 Player {} at {}.",
        session.player().id_hex(),
        session.player().endpoint
    );
    let mut connection = shared.connection.lock().unwrap();
    if !connection.permits_resource_install(operation) {
        return None;
    }
    let sender_stop = EspSenderStop::new();
    let sender_session = Arc::clone(session);
    let sender_shared = Arc::clone(shared);
    let callback_stop = sender_stop.clone();
    shared.edge.begin_with_sender(*shared.epoch.lock().unwrap(), MonotonicClock.now(), move |frame| {
        let session = Arc::clone(&sender_session);
        let shared = Arc::clone(&sender_shared);
        let sender_stop = callback_stop.clone();
        async move {
            if !esp_sender_is_current(&shared, operation, &sender_stop) {
                return;
            }
            let _ = session.send_frame(&frame).await;
        }
    });
    connection.esp_session = Some(ActiveEspSession {
        operation,
        session: Arc::clone(session),
        sender_stop: sender_stop.clone(),
    });
    shared.emit(ClientEvent::Connection { state: "connected".into(), path: "esp".into() });
    shared.log("info", message);
    Some(sender_stop)
}

/// Runs one selected W1VE ESP32 Player session. It keeps the edge generator's ordered FIFO and
/// resends its newest complete edge frame every 250 ms so the Player's key-state lease never
/// depends on a separate state packet.
async fn run_esp_player_session(
    shared: &Arc<Shared>,
    operation: OperationId,
    session: Arc<EspPlayerSession>,
) {
    let Some(sender_stop) = install_esp_session(shared, operation, &session) else {
        // This includes a pairing that completed after Stop. It cannot install the edge sender
        // or ESP session, and it actively unpairs the resource it just created.
        let _ = session.send_unpair().await;
        return;
    };

    let mut refresh_ticker = tokio::time::interval(LAN_PLAYER_REFRESH_INTERVAL);
    let mut ping_ticker = tokio::time::interval(ESP_PLAYER_PING_INTERVAL);
    let mut pending_ping = None;
    let mut missed_pings = 0u8;
    let reason = loop {
        if !esp_sender_is_current(shared, operation, &sender_stop) {
            return;
        }
        tokio::select! {
            incoming = session.recv() => match incoming {
                Ok(Some(EspPlayerInbound::Unpaired)) => break "W1VE ESP32 Player requested unpair",
                Ok(_) => {}
                Err(_) => break "W1VE ESP32 Player stopped responding",
            },
            _ = refresh_ticker.tick() => {
                if let Some(frame) = shared.edge.latest_frame() {
                    if !esp_sender_is_current(shared, operation, &sender_stop) {
                        return;
                    }
                    if let Err(error) = session.send_frame(&frame).await {
                        if session.sequence_exhausted() {
                            break "W1VE ESP32 Player envelope sequence exhausted";
                        }
                        if !log_client_if_current(
                            shared,
                            operation,
                            "warn",
                            format!("W1VE ESP32 Player frame refresh failed: {error}"),
                        ) {
                            return;
                        }
                    }
                }
                if !esp_sender_is_current(shared, operation, &sender_stop) {
                    return;
                }
            }
            _ = ping_ticker.tick() => {
                let mut counted_miss = false;
                if let Some(stamp) = pending_ping.take() {
                    if session.ping_was_acknowledged(stamp) {
                        missed_pings = 0;
                    } else {
                        missed_pings = missed_pings.saturating_add(1);
                        counted_miss = true;
                        if missed_pings >= ESP_PLAYER_MISSED_PING_LIMIT {
                            break "W1VE ESP32 Player missed three matching PONG replies";
                        }
                    }
                }

                match session.send_ping().await {
                    Ok(stamp) => pending_ping = Some(stamp),
                    Err(error) => {
                        if session.sequence_exhausted() {
                            break "W1VE ESP32 Player envelope sequence exhausted";
                        }
                        // A failed send accounts for one interval only if the preceding missed
                        // PONG was not already counted on this tick.
                        if !counted_miss {
                            missed_pings = missed_pings.saturating_add(1);
                        }
                        if missed_pings >= ESP_PLAYER_MISSED_PING_LIMIT {
                            break "W1VE ESP32 Player missed three matching PONG replies";
                        }
                        if !log_client_if_current(
                            shared,
                            operation,
                            "warn",
                            format!("W1VE ESP32 Player ping send failed: {error}"),
                        ) {
                            return;
                        }
                    }
                }
                let ms = session
                    .rtt()
                    .map(|duration| u32::try_from(duration.as_millis()).unwrap_or(u32::MAX));
                if !emit_client_rtt_if_current(shared, operation, ms) {
                    return;
                }
            }
        }
    };

    let edge_drain = {
        let mut connection = shared.connection.lock().unwrap();
        if !connection.permits(operation) {
            None
        } else {
            let owns_session = match connection.esp_session.as_ref() {
                Some(current) => {
                    current.operation == operation && Arc::ptr_eq(&current.session, &session)
                }
                None => false,
            };
            if !owns_session {
                None
            } else {
                let current = connection.esp_session.take().expect("owned ESP session");
                // Revoke this session's callback before detaching its queue. The returned handle
                // cannot refer to a replacement queue because both actions happen under the gate.
                current.sender_stop.stop();
                let edge_drain = shared.edge.detach_with_drain();
                shared.emit(ClientEvent::Rtt { ms: None });
                shared.emit(ClientEvent::Connection { state: "disconnected".into(), path: "none".into() });
                shared.log("warn", format!("{reason}; disconnected."));
                Some(edge_drain)
            }
        }
    };
    if let Some(edge_drain) = edge_drain {
        // A terminal authenticated unpair is safe even if the Player already closed its side.
        // It follows the stopped callback's completed drain, never a replacement operation.
        edge_drain.wait().await;
        let _ = session.send_unpair().await;
    }
}

/// Tears this operation's tunnel pairing down without emitting "disconnected" or touching the
/// selected-LAN browse task, so the WAN supervisor can reconnect cleanly. A stale supervisor
/// cannot detach a newer operation's edge or take its session.
async fn teardown_session(shared: &Arc<Shared>, operation: OperationId) -> bool {
    let session = {
        let mut connection = shared.connection.lock().unwrap();
        if !connection.permits(operation) {
            return false;
        }
        connection.transport = None;
        // Tunnel callers do not send an ESP terminal message, so cancellation is sufficient.
        let _ = shared.edge.detach();
        connection.session.take()
    };
    if let Some(mut session) = session {
        session.shutdown().await;
    }
    true
}

/// Shared tail of both the WAN and LAN connect paths: start the session, open the keying edge
/// over the tunnel, and atomically install it while this operation is still current. A Session
/// created by stale work is always shut down before returning, never published to shared state.
async fn bring_up_keying(
    shared: &Arc<Shared>,
    operation: OperationId,
    connected: tunnel::Connected,
) -> Option<Arc<EdgeTransport>> {
    if !client_operation_is_current(shared, operation) {
        return None;
    }
    let path = format!("{:?}", connected.path).to_lowercase();
    let config = shared.config.lock().unwrap().clone();
    let mut session = match Session::start(connected, &config) {
        Ok(session) => session,
        Err(error) => {
            fail_client_operation(shared, operation, format!("session failed: {error}"));
            return None;
        }
    };
    if !client_operation_is_current(shared, operation) {
        session.shutdown().await;
        return None;
    }
    let net = session.handle();
    let conn = match net.open_udp(EDGE_PORT).await {
        Ok(conn) => conn,
        Err(error) => {
            fail_client_operation(shared, operation, format!("edge open failed: {error}"));
            session.shutdown().await;
            return None;
        }
    };
    if !client_operation_is_current(shared, operation) {
        session.shutdown().await;
        return None;
    }
    let edge = Arc::new(EdgeTransport::over_tunnel(conn));
    let peer = std::net::SocketAddr::new(
        std::net::IpAddr::V4(Role::Client.peer_inner_address()),
        EDGE_PORT,
    );
    edge.set_peer(peer).await;
    if !client_operation_is_current(shared, operation) {
        session.shutdown().await;
        return None;
    }

    let message = format!("Paired with the Player over the {path} path.");
    let mut session_to_install = Some(session);
    let installed = {
        let mut connection = shared.connection.lock().unwrap();
        if !connection.permits_resource_install(operation) {
            false
        } else {
            // A new pairing restarts the edge stream at sequence 0 and starts its ordered tunnel
            // FIFO. The Station builds a fresh replayer per session, so it accepts the restart.
            shared.edge.begin(*shared.epoch.lock().unwrap(), MonotonicClock.now(), Arc::clone(&edge));
            connection.transport = Some(Arc::clone(&edge));
            connection.session = session_to_install.take();
            // The epoch is intentionally NOT bumped here. Both edge sources (the paddle poller
            // and the type-ahead send worker) sample `shared.epoch` independently, so a bump
            // would make them disagree and the Server would F4 whichever source sent second.
            shared.emit(ClientEvent::Connection { state: "connected".into(), path: path.clone() });
            shared.log("info", message);
            true
        }
    };
    if !installed {
        if let Some(mut session) = session_to_install.take() {
            session.shutdown().await;
        }
        return None;
    }
    Some(edge)
}

impl Drop for ClientController {
    fn drop(&mut self) {
        self.shared.send.shutdown();
        self.disconnect();
    }
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

/// Derives the compact W1VE ESP32 Player protocol id from the stable persisted identity key.
fn keyer_id_from_public_key(public_key: [u8; 32]) -> [u8; 6] {
    let mut keyer_id = [0u8; 6];
    keyer_id.copy_from_slice(&public_key[..6]);
    keyer_id
}

/// Parses the fixed-width hexadecimal ESP32 Player identity used by the tagged UI selection.
fn parse_esp_player_id(value: &str) -> Option<[u8; 6]> {
    if value.len() != 12 || !value.is_ascii() {
        return None;
    }
    let mut id = [0u8; 6];
    for (slot, pair) in id.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        *slot = hex_nibble(pair[0])? << 4 | hex_nibble(pair[1])?;
    }
    Some(id)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

/// Turns a UI selection into only browser-owned data. A UI caller cannot substitute an endpoint,
/// pick an expired/unknown Player, or pair with a Player that reports itself busy.
fn validate_lan_selection(
    players: &LanPlayerCatalog,
    selection: &LanPlayerSelection,
) -> Result<ValidatedLanSelection, String> {
    if players.generation == 0 {
        return Err("LAN browsing is not active. Refresh the Player list and try again.".into());
    }
    match selection {
        LanPlayerSelection::Desktop { id, endpoint } => {
            let public_key = rwk_core::config::base64_decode(id)
                .ok()
                .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
                .ok_or_else(|| "Invalid desktop Player selection.".to_string())?;
            let endpoint = endpoint
                .parse::<SocketAddr>()
                .map_err(|_| "Invalid desktop Player address.".to_string())?;
            if players
                .desktop
                .iter()
                .any(|player| player.public_key == public_key && player.endpoint == endpoint)
            {
                Ok(ValidatedLanSelection::Desktop { public_key, endpoint })
            } else {
                Err("The selected desktop Player is no longer in the current LAN list.".into())
            }
        }
        LanPlayerSelection::Esp { id, endpoint } => {
            let player_id = parse_esp_player_id(id)
                .ok_or_else(|| "Invalid W1VE ESP32 Player selection.".to_string())?;
            let endpoint = endpoint
                .parse::<SocketAddr>()
                .map_err(|_| "Invalid W1VE ESP32 Player address.".to_string())?;
            let player = players
                .esp
                .iter()
                .find(|player| player.player_id == player_id && player.endpoint == endpoint)
                .cloned()
                .ok_or_else(|| "The selected W1VE ESP32 Player is no longer in the current LAN list.".to_string())?;
            if player.busy() {
                return Err("The selected W1VE ESP32 Player is busy. Choose another Player or refresh the list.".into());
            }
            Ok(ValidatedLanSelection::Esp(player))
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_client_setting_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("wrk-client-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("client.json");
        let _ = std::fs::remove_file(&path);
        let sink: EventSink = Arc::new(|_| {});

        let first = ClientController::start_with_config(Arc::clone(&sink), Some(path.clone()));
        first.set_speed(33);
        first.set_weight(60);
        first.set_mode("ultimatic");
        first.set_paddle_reverse(true);
        first.set_tone_frequency(750);
        first.set_tone_volume(35);
        first.select_audio_device(Some(String::new()));
        first.set_relay("198.51.100.9:9999");
        first.set_transport("lan");
        // Port names that do not exist still record the operator's selection.
        first.select_paddle_port(Some("W1VE-TEST-PADDLE".into()));
        first.select_logger_port(Some("W1VE-TEST-LOGGER".into()));
        first.shared.settings.lock().unwrap().token = "ab12cd".into();
        first.shared.persist();
        drop(first);

        let second = ClientController::start_with_config(sink, Some(path.clone()));
        let s = second.shared.settings.lock().unwrap().clone();
        drop(second);
        let _ = std::fs::remove_file(&path);

        assert_eq!(s.wpm, 33);
        assert_eq!(s.weight, 60);
        assert_eq!(mode_name(s.mode), "ultimatic");
        assert!(s.paddle_reverse);
        assert_eq!(s.tone_hz, 750);
        assert_eq!(s.tone_volume, 35);
        assert_eq!(s.audio_device.as_deref(), Some(""));
        assert_eq!(s.relay, "198.51.100.9:9999");
        assert_eq!(s.transport, Transport::Lan);
        assert_eq!(s.paddle_port.as_deref(), Some("W1VE-TEST-PADDLE"));
        assert_eq!(s.logger_port.as_deref(), Some("W1VE-TEST-LOGGER"));
        assert_eq!(s.token, "ab12cd");
    }

    #[test]
    fn stale_operation_id_cannot_pass_client_installation_gate() {
        let mut connection = ClientConnection::default();
        let stale = connection.gate.replace();
        let current = connection.gate.replace();

        assert!(!connection.permits_resource_install(stale));
        assert!(connection.permits_resource_install(current));
    }

    #[test]
    fn persisted_identity_key_derives_the_first_six_keyer_id_bytes() {
        let mut public_key = [0u8; 32];
        public_key[..6].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(keyer_id_from_public_key(public_key), [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn esp_player_ids_must_be_exact_hex_six_byte_values() {
        assert_eq!(parse_esp_player_id("0A1B2C3D4E5F"), Some([10, 27, 44, 61, 78, 95]));
        assert_eq!(parse_esp_player_id("0a1b2c3d4e5f"), Some([10, 27, 44, 61, 78, 95]));
        assert_eq!(parse_esp_player_id("0A1B2C3D4E5"), None);
        assert_eq!(parse_esp_player_id("0A1B2C3D4E5G"), None);
    }

    #[test]
    fn lan_selection_must_match_a_current_discovery_record() {
        let endpoint: SocketAddr = "192.168.1.25:4995".parse().unwrap();
        let player = EspPlayerAdvertisement::from_parts(
            [1, 2, 3, 4, 5, 6],
            "Radio room".into(),
            endpoint,
            0,
        );
        let players = LanPlayerCatalog {
            generation: 7,
            desktop: vec![DesktopPlayer {
                public_key: [9u8; 32],
                name: "Desktop".into(),
                endpoint: "192.168.1.20:51820".parse().unwrap(),
            }],
            esp: vec![player],
        };
        let desktop_id = rwk_core::config::base64_encode(&[9u8; 32]);

        assert!(matches!(
            validate_lan_selection(
                &players,
                &LanPlayerSelection::Desktop {
                    id: desktop_id,
                    endpoint: "192.168.1.99:51820".into(),
                },
            ),
            Err(_)
        ));
        assert!(matches!(
            validate_lan_selection(
                &players,
                &LanPlayerSelection::Esp {
                    id: "010203040506".into(),
                    endpoint: endpoint.to_string(),
                },
            ),
            Ok(ValidatedLanSelection::Esp(_))
        ));
    }

    #[test]
    fn busy_esp_player_selection_is_rejected_before_pairing() {
        let endpoint: SocketAddr = "192.168.1.25:4995".parse().unwrap();
        let players = LanPlayerCatalog {
            generation: 1,
            desktop: Vec::new(),
            esp: vec![EspPlayerAdvertisement::from_parts(
                [1, 2, 3, 4, 5, 6],
                "Busy radio room".into(),
                endpoint,
                1,
            )],
        };

        assert!(matches!(
            validate_lan_selection(
                &players,
                &LanPlayerSelection::Esp {
                    id: "010203040506".into(),
                    endpoint: endpoint.to_string(),
                },
            ),
            Err(message) if message.contains("busy")
        ));
    }
}
