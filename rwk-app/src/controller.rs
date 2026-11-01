//! The Client controller: the headless engine owner behind the UI.
//!
//! This mirrors the C# `ClientController`. The UI never touches the engine directly; it
//! sends commands here and renders [`ClientEvent`]s this controller emits. All blocking and
//! async work (CW playback timing, WireGuard links, and direct ESP Player keying) happens on
//! controller-owned threads, so the UI thread only marshals small messages.
//!
//! Current scope includes keyer timing, local sidetone, CW text keying, serial paddle input,
//! an optional local key line, WinKeyer hosting, WAN pairing, and direct LAN Player selection.
//! All blocking and async work stays on controller-owned threads; the web UI sends commands and
//! renders [`ClientEvent`]s only.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};

use rwk_core::config::{base64_decode, base64_encode, Config, ForwardRuleConfig, Role};
use rwk_core::engine::audio::SidetoneEngine;
use rwk_core::engine::control::{is_reserved_port, ControlLink, ControlMessage};
use rwk_core::engine::debounce::ContactDebouncer;
use rwk_core::engine::edge_session::EdgeSession;
use rwk_core::engine::esp_player::{
    EspPlayerAdvertisement, EspPlayerBrowser, EspPlayerInbound, EspPlayerSession,
    ESP_PLAYER_MISSED_PING_LIMIT, ESP_PLAYER_PING_INTERVAL,
};
use rwk_core::engine::flex_discovery::DiscoveryEmitter;
use rwk_core::engine::lan_discovery::LanPeer;
use rwk_core::engine::keying::{KeyerElement, KeyerElementTiming};
use rwk_core::engine::logger_host::{HostReport, HostReporter, LoggerHost, TextSink};
use rwk_core::engine::network::EdgeTransport;
use rwk_core::engine::reconnect::{
    self, connect_with_relay_retry, watch_edge_liveness, Liveness, ReconnectBudget,
};
use rwk_core::engine::paddle::PaddleKeyer;
use rwk_core::engine::paddle_input::PaddleInputPort;
use rwk_core::engine::serial::{
    enumerate_ports, natural_port_key, KeyingOutputConfig, SerialKeyingOutput,
};
use rwk_core::engine::session::Session;
use rwk_core::engine::tunnel;
use rwk_core::protocol::flex_vita;
use rwk_core::protocol::morse::MorseTable;
use rwk_core::platform::{ThreadPriorityGuard, TimerResolutionGuard};
use rwk_core::primitives::{KeyerMode, KeyingLine};
use rwk_core::timing::{Clock, HybridWaiter, MonotonicClock};

/// The inner UDP port the keying edge rides over the tunnel, both ends agreeing.
const EDGE_PORT: u16 = 5555;

/// Consecutive failed *connect* attempts after which the Client gives up and stays disconnected
/// (the operator reconnects manually). The shared 15 s reconnect window bounds the overall churn.
const MAX_RECONNECT_ATTEMPTS: u32 = 5;

/// Control identifiers for inline per-control error messages in the UI.
mod control {
    pub const PADDLE: &str = "paddle";
    pub const WINKEYER: &str = "winkeyer";
    pub const KEYING: &str = "keying";
    pub const AUDIO: &str = "audio";
}

/// A command to the dedicated sidetone thread.
///
/// `cpal::Stream` is not `Send`, so the audio engine can never leave the thread that built
/// it. This channel is the only way to touch it: the controller (and the keying thread) send
/// these messages, and one owner thread services them. The handle the controller holds is a
/// plain `Sender`, which is `Send + Sync`, so the controller itself stays shareable.
enum ToneCmd {
    /// Key the tone down (true) or up (false).
    Key(bool),
    /// Rebuild the stream for a new frequency (Hz), volume (0..=100), and output device.
    Reconfigure { hz: i32, volume: u32, device: ToneDevice },
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

/// A handle to the sidetone thread: cheap to clone, `Send + Sync`.
#[derive(Clone)]
struct Sidetone {
    tx: Sender<ToneCmd>,
}

impl Sidetone {
    /// Spawns the audio-owning thread with an initial tone/volume/device, reporting device
    /// status (or a warning) through `shared`.
    fn spawn(hz: i32, volume: u32, device: ToneDevice, shared: Arc<Shared>) -> Self {
        let (tx, rx) = mpsc::channel::<ToneCmd>();
        std::thread::Builder::new()
            .name("rwk-sidetone".into())
            .spawn(move || {
                // The engine (and its cpal stream) lives entirely on this thread.
                let mut engine = build_engine(hz, volume, &device, &shared);
                // The settings the engine was last built with, so a failed stream can be rebuilt.
                let (mut built_hz, mut built_volume) = (hz, volume);
                let mut built_device = device.clone();
                while let Ok(cmd) = rx.recv() {
                    // A stream that errored (device unplugged, backend reset) renders nothing.
                    // Rebuild it before handling the command, so the tone returns without a setting change.
                    if engine.as_ref().is_some_and(SidetoneEngine::has_failed) {
                        engine = build_engine(built_hz, built_volume, &built_device, &shared);
                    }
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
                            built_hz = hz;
                            built_volume = volume;
                            built_device = device.clone();
                            engine = build_engine(hz, volume, &device, &shared);
                        }
                    }
                }
                // Channel closed: the controller is gone. Dropping the engine stops audio.
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
/// `Off` or the audio stream could not be opened (the error is logged and surfaced under the
/// audio control).
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
    /// Characters waiting to be keyed, in order, each with the WPM its batch was queued at.
    queue: VecDeque<(char, u32)>,
    /// Set by [`ClientController::abort_send`]; the worker drops the queue and the current
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

/// The Router Client's logger output. The shared WinKeyer host (`rwk_core::engine::logger_host`)
/// answers the logger and batches its text; this sink takes the text into the send buffer, which
/// keys it with sidetone, the key line, and the tunnel, as the Remote Keyer's Client does.
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
        // The logger owns the speed while host mode is open; the slider is locked.
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

/// An event the controller emits for the UI to render. Serialized to the webview as-is.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ClientEvent {
    /// The key line changed state (for the KEY indicator and status strip).
    KeyState { down: bool },
    /// The link to the selected Player changed. `path` is "none" | "direct" | "relay" |
    /// "esp".
    Connection { state: String, path: String },
    /// The round-trip time to the selected Player in ms, refreshed about once a second while
    /// paired. `None` until measured (and when unpaired).
    Rtt { ms: Option<u32> },
    /// A human-readable log line for the Log tab.
    Log { level: String, message: String },
    /// A keyer/sidetone setting changed and the UI should reflect it (echo for confirmation).
    ///
    /// NOTE: struct-variant fields keep their snake_case names in the serialized JSON (the
    /// enum-level `rename_all = "camelCase"` renames only the variant *tag*). The web UI
    /// reads `ev.paddle_reverse`, `ev.tone_hz`, `ev.audio_device`, etc. — keep them snake_case.
    Settings {
        wpm: u32,
        weight: u32,
        mode: String,
        paddle_reverse: bool,
        tone_hz: i32,
        tone_volume: u32,
        /// Sidetone output device: `null` = system default, `""` = off, name = that device.
        audio_device: Option<String>,
        keying_port: Option<String>,
        paddle_port: Option<String>,
        winkeyer_port: Option<String>,
        transport: String,
        /// The relay address used for WAN pairing (empty until one is set).
        relay: String,
        /// The remembered WAN pairing code (empty until one has been used).
        pairing_code: String,
        /// The logger has host mode open and owns the speed; the Keyer's slider is locked.
        speed_locked: bool,
    },
    /// The current LAN Player list. Each item is explicitly tagged as desktop or W1VE ESP32.
    LanPlayers { players: Vec<LanPlayerView> },
    /// Whether the active pairing can run Ham Router services. ESP Player pairings are CW-only.
    RouterStatus { available: bool, esp_active: bool },
    /// The current forward-rule table, echoed whenever it changes (for the Ham Router tab).
    ForwardRules { rules: Vec<ForwardRuleView> },
    /// Whether FlexRadio discovery re-emission is enabled (for the Ham Router toggle).
    FlexDiscovery { enabled: bool },
    /// The type-ahead/macro sender is actively keying CW.
    Sending { active: bool },
    /// The current serial-port inventory (numeric-sorted), pushed on change (PnP).
    Ports { available: Vec<String> },
    /// The current audio output device inventory for the sidetone picker.
    AudioDevices { available: Vec<AudioDeviceView> },
    /// A short per-control error to show in red under the offending control (full detail is
    /// logged too). An empty `message` clears the control's error. `control` is one of
    /// "keying" | "paddle" | "winkeyer" | "audio".
    Error { control: String, message: String },
}

/// One audio output device for the UI picker: its name and whether it is the system default.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDeviceView {
    pub name: String,
    pub is_default: bool,
}

/// Which transport the operator chose for pairing: WAN (relay + code) or LAN (relay-free
/// Player discovery). The Keyer-Router's WAN path is the existing relay rendezvous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Over the internet via the relay (the existing `connect(relay, code)` path).
    Wan,
    /// Relay-free on the local network: the operating Keyer browses and initiates a selected
    /// desktop or W1VE ESP32 Player.
    Lan,
}

impl Transport {
    fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("lan") {
            Transport::Lan
        } else {
            Transport::Wan
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Transport::Wan => "wan",
            Transport::Lan => "lan",
        }
    }
}

/// A forward rule as the UI sees it: the persisted [`ForwardRuleConfig`] fields, serialized
/// in camelCase for the web layer.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardRuleView {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub client_port: u16,
    pub station_port: u16,
    pub enabled: bool,
    pub station_target_address: String,
}

impl From<&ForwardRuleConfig> for ForwardRuleView {
    fn from(r: &ForwardRuleConfig) -> Self {
        Self {
            id: r.id.clone(),
            name: r.name.clone(),
            protocol: r.protocol.clone(),
            client_port: r.client_port,
            station_port: r.station_port,
            enabled: r.enabled,
            station_target_address: r.station_target_address.clone(),
        }
    }
}

/// A LAN Player that the operating Keyer may select. The tag intentionally keeps desktop
/// WireGuard Players separate from CW-only W1VE ESP32 Players at the Tauri boundary.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "playerType", rename_all = "camelCase")]
pub enum LanPlayerView {
    /// A desktop Player that advertises a RWKP v2 beacon.
    Desktop {
        /// Base64 WireGuard public key, used only as the stable selection identifier.
        id: String,
        name: String,
        router_capable: bool,
    },
    /// A W1VE ESP32 Player discovered through its LAN advertisement.
    Esp32Player {
        /// Twelve-digit hexadecimal ESP Player identifier.
        id: String,
        name: String,
        busy: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouterAccess {
    /// No live Player session: router configuration remains editable for a future desktop link.
    Inactive,
    /// A desktop Player is paired, with its advertised Router capability recorded.
    Desktop { router_capable: bool },
    /// A W1VE ESP32 Player is paired. This transport is deliberately CW-only.
    Esp32Player,
}

impl RouterAccess {
    fn status(self) -> (bool, bool) {
        match self {
            Self::Inactive | Self::Desktop { router_capable: true } => (true, false),
            Self::Desktop { router_capable: false } => (false, false),
            Self::Esp32Player => (false, true),
        }
    }

    fn block_reason(self) -> Option<&'static str> {
        match self {
            Self::Inactive | Self::Desktop { router_capable: true } => None,
            Self::Desktop { router_capable: false } => Some(
                "The selected desktop Player does not advertise Router capability; forwarding and Flex are unavailable for this pairing.",
            ),
            Self::Esp32Player => Some(
                "W1VE ESP32 Player connections are CW-only; forwarding and Flex are unavailable until it is disconnected.",
            ),
        }
    }
}

/// A selected browser record retained only long enough to validate a Tauri selection.
#[derive(Clone)]
enum DiscoveredLanPlayer {
    Desktop(LanPeer),
    Esp32Player(EspPlayerAdvertisement),
}

#[derive(Default)]
struct LanPlayerRegistry {
    desktop: Vec<LanPeer>,
    esp32: Vec<EspPlayerAdvertisement>,
}

/// Monotonic owner token for all resources created by one Client connection attempt.
///
/// This is intentionally separate from the wire edge epoch: both local edge sources sample the
/// latter independently, while this token only controls installation and teardown ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientOperation(u64);

/// A lifecycle resource together with the operation that is allowed to own it.
struct OperationResource<T> {
    operation: ClientOperation,
    resource: T,
}

/// ESP transport state that must be stopped before the keyed-edge queue is detached.
struct EspActiveSession {
    operation: ClientOperation,
    session: Arc<EspPlayerSession>,
    stop: Arc<AtomicBool>,
}

/// The Client's installation gate. Every connection-facing resource is installed only while this
/// mutex confirms that its owner token is still current. The gate is never held across network
/// awaits; candidates are built locally and either committed here or explicitly shut down.
#[derive(Default)]
struct ClientLifecycle {
    generation: u64,
    transport: Option<OperationResource<Arc<EdgeTransport>>>,
    lan_connect: Option<OperationResource<tokio::task::JoinHandle<()>>>,
    esp_active: Option<EspActiveSession>,
    control_tx: Option<OperationResource<tokio::sync::mpsc::UnboundedSender<ControlMessage>>>,
    control_task: Option<OperationResource<tokio::task::JoinHandle<()>>>,
    supervisor: Option<OperationResource<tokio::task::JoinHandle<()>>>,
    session: Option<OperationResource<Arc<tokio::sync::Mutex<Session>>>>,
}

impl ClientLifecycle {
    fn current(&self) -> ClientOperation {
        ClientOperation(self.generation)
    }

    fn next_operation(&mut self) -> ClientOperation {
        self.generation = self.generation.wrapping_add(1);
        self.current()
    }

    fn is_current(&self, operation: ClientOperation) -> bool {
        self.current() == operation
    }
}

/// Resources retired by Stop or replacement. They are detached while holding the installation
/// gate, then aborted or shut down after releasing it so a newer operation cannot be disturbed.
struct RetiredClientResources {
    supervisor: Option<tokio::task::JoinHandle<()>>,
    lan_connect: Option<tokio::task::JoinHandle<()>>,
    control_task: Option<tokio::task::JoinHandle<()>>,
    esp_active: Option<EspActiveSession>,
    session: Option<Arc<tokio::sync::Mutex<Session>>>,
}

/// A sink the host (Tauri) registers to receive [`ClientEvent`]s. Boxed so the controller
/// does not depend on the UI framework.
pub type EventSink = Arc<dyn Fn(ClientEvent) + Send + Sync>;

/// Live keyer/sidetone settings, shared with the keying thread.
#[derive(Debug, Clone)]
struct Settings {
    wpm: u32,
    weight: u32,
    mode: KeyerMode,
    paddle_reverse: bool,
    tone_hz: i32,
    tone_volume: u32,
    /// Sidetone output device: `None` = system default, `Some("")` = off, `Some(name)` = named.
    audio_device: Option<String>,
    keying_port: Option<String>,
    paddle_port: Option<String>,
    winkeyer_port: Option<String>,
    transport: Transport,
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
            keying_port: None,
            paddle_port: None,
            winkeyer_port: None,
            transport: Transport::Wan,
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
            keying_port: k.keying_port.clone(),
            paddle_port: k.paddle_port.clone(),
            winkeyer_port: k.logger_port.clone(),
            transport: config.transport.as_deref().map(Transport::parse).unwrap_or(defaults.transport),
        }
    }
}

/// Shared state the controller and its background work read and write.
///
/// Everything here is `Send + Sync`: the sidetone is a channel handle (the non-`Send`
/// `cpal::Stream` lives on its own thread), the serial output is `Send`, and the transport is
/// an `Arc`. That is what lets the controller be stored in Tauri's managed state and touched
/// from command handlers on any thread.
struct Shared {
    settings: Mutex<Settings>,
    /// Set once the sidetone thread is spawned (during `start`).
    sidetone: Mutex<Option<Sidetone>>,
    keying: Mutex<Option<SerialKeyingOutput>>,
    /// The Client's installation gate plus every resource that belongs to one connection
    /// operation. Stop and replacement advance the generation under this same mutex before
    /// aborting tasks, so a late task can never publish into a newer lifecycle.
    lifecycle: Mutex<ClientLifecycle>,
    /// Session epoch, incremented on each new pairing so the station re-anchors.
    epoch: Mutex<u16>,
    /// The shared session edge path (`rwk-core`): one sender for every edge source and one
    /// ordered tunnel FIFO. Restarted once per pairing in `bring_up_client`.
    edge: EdgeSession,
    /// The running serial paddle poller, present only while a paddle port is selected.
    paddle: Mutex<Option<PaddlePollerHandle>>,
    /// The type-ahead / macro send buffer, drained by the single send-worker thread.
    send: SendBuffer,
    /// The WinKeyer host on the logger port, present while a logger port is selected.
    logger: Mutex<Option<LoggerHost>>,
    /// True while the logger has host mode open. It owns the speed then, so the slider is locked.
    speed_from_logger: AtomicBool,
    /// The LAN browser tasks (one desktop RWKP v2 browser and one ESP Player browser).
    browse_tasks: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Current browser records, retained only to validate a selected Player at the Tauri boundary.
    lan_players: Mutex<LanPlayerRegistry>,
    /// Runtime Router eligibility for the selected Player.
    router_access: Mutex<RouterAccess>,
    /// The host-LAN FlexRadio discovery emitter, present while re-emission is enabled.
    flex_emitter: Mutex<Option<DiscoveryEmitter>>,
    /// The persisted config, held so pairing reuses a stable WireGuard identity across runs.
    config: Mutex<Config>,
    /// Where the config persists; `None` disables saving (tests).
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
        // Read config fields under one short lock: a temporary guard inside the event literal
        // would live to the end of the statement and deadlock a second `config.lock()`.
        let (relay, pairing_code) = {
            let config = self.config.lock().unwrap();
            (
                config.relay.as_ref().map(|r| r.address.to_string()).unwrap_or_default(),
                config.pairing_code.clone().unwrap_or_default(),
            )
        };
        self.emit(ClientEvent::Settings {
            wpm: s.wpm,
            weight: s.weight,
            mode: mode_name(s.mode).into(),
            paddle_reverse: s.paddle_reverse,
            tone_hz: s.tone_hz,
            tone_volume: s.tone_volume,
            audio_device: s.audio_device.clone(),
            keying_port: s.keying_port.clone(),
            paddle_port: s.paddle_port.clone(),
            winkeyer_port: s.winkeyer_port.clone(),
            transport: s.transport.as_str().into(),
            relay,
            pairing_code,
            speed_locked: self.speed_from_logger.load(Ordering::SeqCst),
        });
    }

    /// Emits the merged, tagged LAN Player list used to validate selections from the webview.
    fn emit_lan_players(&self) {
        let players = {
            let registry = self.lan_players.lock().unwrap();
            let mut players = Vec::with_capacity(registry.desktop.len() + registry.esp32.len());
            players.extend(registry.desktop.iter().map(|player| LanPlayerView::Desktop {
                id: base64_encode(&player.public_key),
                name: player.display_name(),
                router_capable: player.supports_router(),
            }));
            players.extend(registry.esp32.iter().map(|player| LanPlayerView::Esp32Player {
                id: player.id_hex(),
                name: player.name.clone(),
                busy: player.busy(),
            }));
            players
        };
        self.emit(ClientEvent::LanPlayers { players });
    }

    fn set_desktop_lan_players(&self, players: Vec<LanPeer>) {
        self.lan_players.lock().unwrap().desktop = players;
        self.emit_lan_players();
    }

    fn set_esp_lan_players(&self, players: Vec<EspPlayerAdvertisement>) {
        self.lan_players.lock().unwrap().esp32 = players;
        self.emit_lan_players();
    }

    fn clear_lan_players(&self) {
        *self.lan_players.lock().unwrap() = LanPlayerRegistry::default();
        self.emit_lan_players();
    }

    /// Resolves a webview-supplied type/id pair only against the current browser registry.
    fn selected_lan_player(&self, player_type: &str, id: &str) -> Result<DiscoveredLanPlayer, String> {
        match player_type {
            "desktop" => {
                let Some(public_key) = parse_desktop_player_id(id) else {
                    return Err("Invalid desktop Player selection.".to_string());
                };
                self.lan_players
                    .lock()
                    .unwrap()
                    .desktop
                    .iter()
                    .find(|player| player.public_key == public_key)
                    .cloned()
                    .map(DiscoveredLanPlayer::Desktop)
                    .ok_or_else(|| "The selected desktop Player is no longer available.".to_string())
            }
            "esp32Player" => {
                let Some(player_id) = parse_esp_player_id(id) else {
                    return Err("Invalid W1VE ESP32 Player selection.".to_string());
                };
                self.lan_players
                    .lock()
                    .unwrap()
                    .esp32
                    .iter()
                    .find(|player| player.player_id == player_id)
                    .cloned()
                    .map(DiscoveredLanPlayer::Esp32Player)
                    .ok_or_else(|| "The selected W1VE ESP32 Player is no longer available.".to_string())
            }
            _ => Err("Invalid Player type.".to_string()),
        }
    }

    fn emit_router_status(&self) {
        let (available, esp_active) = self.router_access.lock().unwrap().status();
        self.emit(ClientEvent::RouterStatus { available, esp_active });
    }

    fn router_block_reason(&self) -> Option<&'static str> {
        self.router_access.lock().unwrap().block_reason()
    }

    /// Starts a fresh ownership generation after the prior operation has been retired.
    fn begin_operation(&self) -> ClientOperation {
        self.lifecycle.lock().unwrap().next_operation()
    }

    fn current_operation(&self) -> ClientOperation {
        self.lifecycle.lock().unwrap().current()
    }

    fn is_current_operation(&self, operation: ClientOperation) -> bool {
        self.lifecycle.lock().unwrap().is_current(operation)
    }

    /// Emits only while `operation` is still current. Connection-state events are generated
    /// while holding the same gate used by Stop, so an old task cannot report itself live after
    /// a newer Stop or replacement has won the lifecycle.
    fn emit_for_operation(&self, operation: ClientOperation, event: ClientEvent) -> bool {
        let lifecycle = self.lifecycle.lock().unwrap();
        if !lifecycle.is_current(operation) {
            return false;
        }
        self.emit(event);
        true
    }

    fn log_for_operation(&self, operation: ClientOperation, level: &str, message: impl Into<String>) {
        let lifecycle = self.lifecycle.lock().unwrap();
        if lifecycle.is_current(operation) {
            self.log(level, message);
        }
    }

    fn fail_for_operation(&self, operation: ClientOperation, message: impl Into<String>) {
        let message = message.into();
        let lifecycle = self.lifecycle.lock().unwrap();
        if lifecycle.is_current(operation) {
            self.log("warn", message);
            self.emit(ClientEvent::Connection { state: "disconnected".into(), path: "none".into() });
        }
    }

    /// Persists a relay/code pair only while the WAN task still owns the live operation. Without
    /// this gate, a cancelled connect task could overwrite the replacement's shared code.
    fn persist_pairing_for_operation(
        &self,
        operation: ClientOperation,
        relay: std::net::SocketAddr,
        code: &str,
    ) {
        let lifecycle = self.lifecycle.lock().unwrap();
        if lifecycle.is_current(operation) {
            self.persist_pairing(relay, code);
        }
    }

    /// Changes Router eligibility only while the owning pairing remains current. A CW-only
    /// desktop pairing and the ESP pairing both use this gate to prevent a late desktop task
    /// from recreating Router/Flex state after ESP selection.
    fn set_router_access_for_operation(
        &self,
        operation: ClientOperation,
        access: RouterAccess,
        clear_flex: bool,
    ) -> bool {
        let lifecycle = self.lifecycle.lock().unwrap();
        if !lifecycle.is_current(operation) {
            return false;
        }
        if clear_flex {
            *self.flex_emitter.lock().unwrap() = None;
        }
        *self.router_access.lock().unwrap() = access;
        self.emit_router_status();
        if clear_flex {
            self.emit_flex_state();
        }
        true
    }

    /// Atomically enters the strictly CW-only ESP mode. The Flex emitter is removed and Router
    /// access changes under the installation gate before the ESP task can publish an edge sender.
    fn enter_esp_mode_for_operation(&self, operation: ClientOperation) -> bool {
        self.set_router_access_for_operation(operation, RouterAccess::Esp32Player, true)
    }

    /// Installs or removes the local Flex emitter only if the command still belongs to the
    /// current Router-capable lifecycle. An ESP transition cannot interleave after the check.
    fn set_flex_for_operation(
        &self,
        operation: ClientOperation,
        emitter: Option<DiscoveryEmitter>,
    ) -> Result<(), &'static str> {
        let lifecycle = self.lifecycle.lock().unwrap();
        if !lifecycle.is_current(operation) {
            return Err("the pairing was replaced or stopped");
        }
        if let Some(reason) = self.router_access.lock().unwrap().block_reason() {
            return Err(reason);
        }
        *self.flex_emitter.lock().unwrap() = emitter;
        self.emit_flex_state();
        Ok(())
    }

    /// Stores an asynchronous WAN supervisor only if it still owns the current generation.
    /// Replaced and stale handles are explicitly aborted rather than silently dropped.
    fn register_supervisor(&self, operation: ClientOperation, handle: tokio::task::JoinHandle<()>) {
        let discarded = {
            let mut lifecycle = self.lifecycle.lock().unwrap();
            if lifecycle.is_current(operation) {
                lifecycle
                    .supervisor
                    .replace(OperationResource { operation, resource: handle })
                    .map(|owned| owned.resource)
            } else {
                Some(handle)
            }
        };
        if let Some(handle) = discarded {
            handle.abort();
        }
    }

    /// Stores an asynchronous direct-LAN operation only if it is still current. This covers both
    /// desktop WireGuard LAN attempts and CW-only ESP pairing/supervision.
    fn register_lan_connect(&self, operation: ClientOperation, handle: tokio::task::JoinHandle<()>) {
        let discarded = {
            let mut lifecycle = self.lifecycle.lock().unwrap();
            if lifecycle.is_current(operation) {
                lifecycle
                    .lan_connect
                    .replace(OperationResource { operation, resource: handle })
                    .map(|owned| owned.resource)
            } else {
                Some(handle)
            }
        };
        if let Some(handle) = discarded {
            handle.abort();
        }
    }

    /// Publishes a control sender only after the post-connect await has reconfirmed both the
    /// operation and the specific desktop Session that created it. The session identity prevents
    /// a late control dial from a prior WAN reconnect cycle from attaching to its successor.
    fn install_control_sender(
        &self,
        operation: ClientOperation,
        owner_session: &Arc<tokio::sync::Mutex<Session>>,
        sender: tokio::sync::mpsc::UnboundedSender<ControlMessage>,
    ) -> bool {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        if !lifecycle.is_current(operation)
            || !matches!(
                lifecycle.session.as_ref(),
                Some(session)
                    if session.operation == operation
                        && Arc::ptr_eq(&session.resource, owner_session)
            )
        {
            return false;
        }
        lifecycle.control_tx = Some(OperationResource { operation, resource: sender });
        true
    }

    /// Returns whether `session` is still the desktop Session owned by `operation`.
    fn owns_current_session(
        &self,
        operation: ClientOperation,
        session: &Arc<tokio::sync::Mutex<Session>>,
    ) -> bool {
        let lifecycle = self.lifecycle.lock().unwrap();
        lifecycle.is_current(operation)
            && matches!(
                lifecycle.session.as_ref(),
                Some(current)
                    if current.operation == operation && Arc::ptr_eq(&current.resource, session)
            )
    }

    /// Clears only the sender installed by this operation and its specific Session, never a
    /// newer redial or reconnect that happens to share the outer WAN supervisor.
    fn clear_control_sender(
        &self,
        operation: ClientOperation,
        owner_session: &Arc<tokio::sync::Mutex<Session>>,
    ) {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        if lifecycle.is_current(operation)
            && matches!(
                lifecycle.session.as_ref(),
                Some(session)
                    if session.operation == operation
                        && Arc::ptr_eq(&session.resource, owner_session)
            )
            && matches!(lifecycle.control_tx.as_ref(), Some(sender) if sender.operation == operation)
        {
            lifecycle.control_tx = None;
        }
    }

    /// Invalidates all in-flight connection tasks and detaches every installed transport under
    /// one gate. The returned resources are stopped after the gate is released, which prevents a
    /// slow shutdown from taking resources installed by a newer operation.
    fn retire_operation(&self) -> RetiredClientResources {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        lifecycle.next_operation();

        let esp_active = lifecycle.esp_active.take();
        if let Some(active) = &esp_active {
            active.stop.store(true, Ordering::SeqCst);
        }
        self.edge.detach();
        lifecycle.transport = None;
        lifecycle.control_tx = None;

        let supervisor = lifecycle.supervisor.take().map(|owned| owned.resource);
        let lan_connect = lifecycle.lan_connect.take().map(|owned| owned.resource);
        let control_task = lifecycle.control_task.take().map(|owned| owned.resource);
        let session = lifecycle.session.take().map(|owned| owned.resource);

        let clear_flex = matches!(*self.router_access.lock().unwrap(), RouterAccess::Esp32Player);
        *self.router_access.lock().unwrap() = RouterAccess::Inactive;
        if clear_flex {
            *self.flex_emitter.lock().unwrap() = None;
        }
        self.emit_router_status();
        if clear_flex {
            self.emit_flex_state();
        }
        // This is deliberately emitted before the async resource shutdown. It is serialized with
        // all later operation installs, so a late candidate cannot re-publish a connection after
        // Stop has reported disconnected.
        self.emit(ClientEvent::Rtt { ms: None });
        self.emit(ClientEvent::Connection { state: "disconnected".into(), path: "none".into() });
        self.log("info", "Unpaired.");

        RetiredClientResources { supervisor, lan_connect, control_task, esp_active, session }
    }

    /// Interprets the persisted audio-device setting into a [`ToneDevice`].
    fn tone_device(&self) -> ToneDevice {
        ToneDevice::from_setting(&self.settings.lock().unwrap().audio_device)
    }

    /// Remembers the relay + pairing code in the persisted config and writes it to disk, so
    /// the operator does not re-enter them next launch. A write failure is logged, not fatal.
    fn persist_pairing(&self, relay: std::net::SocketAddr, code: &str) {
        let Some(path) = &self.config_path else { return };
        let config = {
            let mut config = self.config.lock().unwrap();
            config.relay = Some(rwk_core::config::RelayConfig { address: relay });
            config.pairing_code = Some(code.to_string());
            config.clone()
        };
        if let Err(e) = config.save(path) {
            self.log("warn", format!("Could not save config: {e}"));
        }
    }

    /// Emits the current forward-rule table to the UI.
    fn emit_forward_rules(&self) {
        let config = self.config.lock().unwrap();
        let rules = config.forwards.iter().map(ForwardRuleView::from).collect();
        self.emit(ClientEvent::ForwardRules { rules });
    }

    /// Emits whether FlexRadio discovery re-emission is enabled.
    fn emit_flex_state(&self) {
        let enabled = self.flex_emitter.lock().unwrap().is_some();
        self.emit(ClientEvent::FlexDiscovery { enabled });
    }

    /// Copies every live keyer/sidetone/port/transport setting into the config and writes it.
    fn persist_settings(&self) {
        {
            let s = self.settings.lock().unwrap();
            let mut config = self.config.lock().unwrap();
            config.transport = Some(s.transport.as_str().to_string());
            config.keyer = rwk_core::config::KeyerSettings {
                wpm: Some(s.wpm),
                weight: Some(s.weight),
                mode: Some(mode_name(s.mode).to_string()),
                paddle_reverse: Some(s.paddle_reverse),
                tone_hz: Some(s.tone_hz),
                tone_volume: Some(s.tone_volume),
                audio_device: s.audio_device.clone(),
                paddle_port: s.paddle_port.clone(),
                logger_port: s.winkeyer_port.clone(),
                keying_port: s.keying_port.clone(),
            };
        }
        self.persist_config();
    }

    /// Writes the current config (including `forwards`) to disk. Logged, not fatal, on error.
    fn persist_config(&self) {
        let Some(path) = &self.config_path else { return };
        let config = self.config.lock().unwrap();
        if let Err(e) = config.save(path) {
            self.log("warn", format!("Could not save config: {e}"));
        }
    }

    /// Returns a clone of the current forward rules.
    fn forward_rules(&self) -> Vec<ForwardRuleConfig> {
        self.config.lock().unwrap().forwards.clone()
    }
}

/// The Client controller. Construct with [`ClientController::start`], then drive it with the
/// command methods; it emits [`ClientEvent`]s to the sink for the UI to render.
pub struct ClientController {
    shared: Arc<Shared>,
    runtime: tokio::runtime::Runtime,
    _clock: Arc<dyn Clock>,
}

impl ClientController {
    /// Starts the controller: loads (or creates) the persisted config so the WireGuard
    /// identity is stable across runs, brings up the local sidetone (best effort), and emits
    /// the initial settings. A missing audio device is a logged warning, not a failure.
    pub fn start(sink: EventSink) -> Self {
        let path = crate::app_paths::config_path(Role::Client);
        Self::start_with_config(sink, Some(path))
    }

    /// Starts the controller against an explicit config path (or `None` to disable
    /// persistence, for tests). Loads the config if present, otherwise generates a fresh
    /// identity and saves it so the next run reuses the same key.
    pub fn start_with_config(sink: EventSink, config_path: Option<std::path::PathBuf>) -> Self {
        let config = match &config_path {
            Some(p) if p.exists() => {
                Config::load(p).unwrap_or_else(|_| Config::new(Role::Client).expect("identity"))
            }
            _ => {
                let config = Config::new(Role::Client).expect("generate client identity");
                if let Some(p) = &config_path {
                    let _ = config.save(p);
                }
                config
            }
        };
        let settings = Settings::from_config(&config);
        let shared = Arc::new(Shared {
            settings: Mutex::new(settings.clone()),
            sidetone: Mutex::new(None),
            keying: Mutex::new(None),
            lifecycle: Mutex::new(ClientLifecycle::default()),
            epoch: Mutex::new(1),
            edge: EdgeSession::new(MonotonicClock.frequency(), MonotonicClock.now()),
            paddle: Mutex::new(None),
            send: SendBuffer::default(),
            logger: Mutex::new(None),
            speed_from_logger: AtomicBool::new(false),
            browse_tasks: tokio::sync::Mutex::new(Vec::new()),
            lan_players: Mutex::new(LanPlayerRegistry::default()),
            router_access: Mutex::new(RouterAccess::Inactive),
            flex_emitter: Mutex::new(None),
            config: Mutex::new(config),
            config_path,
            sink,
        });

        // Start the local sidetone on its own audio-owning thread so the operator can
        // practice before pairing. The thread holds the non-`Send` cpal stream.
        let device = ToneDevice::from_setting(&settings.audio_device);
        let sidetone =
            Sidetone::spawn(settings.tone_hz, settings.tone_volume, device, Arc::clone(&shared));
        *shared.sidetone.lock().unwrap() = Some(sidetone);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        // The single send-worker thread drains the type-ahead / macro buffer with real CW
        // timing, keying every output through `drive_edge`. Spawned once for the controller's
        // life; it needs the runtime handle to pace tunnel frames.
        spawn_send_worker(Arc::clone(&shared), runtime.handle().clone());

        let controller = Self { shared, runtime, _clock: Arc::new(MonotonicClock) };
        // Reopen the persisted local ports. Each opener reports its own "not available" error
        // and keeps the remembered selection if the device is not present right now.
        for (port, open) in [
            (settings.keying_port.clone(), Self::select_keying_port as fn(&Self, Option<String>)),
            (settings.paddle_port.clone(), Self::select_paddle_port),
            (settings.winkeyer_port.clone(), Self::select_winkeyer_port),
        ] {
            if port.is_some() {
                open(&controller, port);
            }
        }
        controller.shared.emit_settings();
        controller.shared.emit_lan_players();
        controller.shared.emit_router_status();
        controller.shared.emit_forward_rules();
        controller.shared.emit_flex_state();
        controller.emit_ports();
        controller.emit_audio_devices();
        controller
    }

    /// Re-emits settings, forward rules, flex state, ports, and audio devices. The UI calls
    /// this once it has subscribed to the event channel, because the startup emit in
    /// [`Self::start`] races the webview and is otherwise lost — the webview does not exist
    /// yet when `start` runs in Tauri's setup.
    pub fn request_state(&self) {
        self.shared.emit_settings();
        self.shared.emit_lan_players();
        self.shared.emit_router_status();
        self.shared.emit_forward_rules();
        self.shared.emit_flex_state();
        self.emit_ports();
        self.emit_audio_devices();
    }

    // ─── Keyer settings ────────────────────────────────────────────────────────

    /// Sets the keyer speed in WPM (clamped 5..=60 downstream).
    pub fn set_speed(&self, wpm: u32) {
        if self.shared.speed_from_logger.load(Ordering::SeqCst) {
            // The logger owns the speed while host mode is open: ignore the slider and resync the UI.
            self.shared.emit_settings();
            return;
        }
        self.shared.settings.lock().unwrap().wpm = wpm;
        self.shared.persist_settings();
        self.shared.log("debug", format!("Speed {wpm} WPM."));
    }

    /// Sets the keyer weight percentage (clamped 25..=75 downstream).
    pub fn set_weight(&self, weight: u32) {
        self.shared.settings.lock().unwrap().weight = weight;
        self.shared.persist_settings();
    }

    /// Sets the keyer mode from a UI name ("iambicB", "iambicA", "ultimatic", "bug", "straight").
    pub fn set_mode(&self, mode: &str) {
        let parsed = parse_mode(mode);
        self.shared.settings.lock().unwrap().mode = parsed;
        self.shared.persist_settings();
        self.shared.log("debug", format!("Mode {}.", mode_name(parsed)));
    }

    /// Sets whether the paddle dit/dah contacts are reversed.
    pub fn set_paddle_reverse(&self, reverse: bool) {
        self.shared.settings.lock().unwrap().paddle_reverse = reverse;
        self.shared.persist_settings();
    }

    // ─── Transport + LAN Player discovery ─────────────────────────────────────

    /// Selects the pairing transport: "wan" (relay + code) or "lan" (direct Player discovery).
    pub fn set_transport(&self, transport: &str) {
        self.shared.settings.lock().unwrap().transport = Transport::parse(transport);
        self.shared.persist_settings();
        self.shared.emit_settings();
    }

    /// Starts fresh desktop and ESP Player LAN browsers. Their live records are merged into one
    /// tagged list, and the Tauri command later accepts only an id from that list.
    pub fn lan_browse_players(&self) {
        let self_key = match self.shared.config.lock().unwrap().identity.to_identity() {
            Ok(identity) => identity.public_bytes(),
            Err(error) => {
                self.shared.log("warn", format!("identity error: {error}"));
                return;
            }
        };
        let esp_keyer_id = esp_keyer_id(self_key);
        self.stop_lan_browse();

        let desktop_shared = Arc::clone(&self.shared);
        let desktop_task = self.runtime.spawn(async move {
            browse_desktop_players(desktop_shared, self_key).await;
        });
        let esp_shared = Arc::clone(&self.shared);
        let esp_task = self.runtime.spawn(async move {
            browse_esp32_players(esp_shared, esp_keyer_id).await;
        });
        self.runtime.block_on(async {
            let mut tasks = self.shared.browse_tasks.lock().await;
            *tasks = vec![desktop_task, esp_task];
        });
    }

    /// Stops and drops both LAN browsers, then removes their stale selection records.
    fn stop_lan_browse(&self) {
        let shared = Arc::clone(&self.shared);
        self.runtime.block_on(async move {
            let mut tasks = shared.browse_tasks.lock().await;
            for task in tasks.drain(..) {
                task.abort();
            }
        });
        self.shared.clear_lan_players();
    }

    /// Validates a selected Player against the current browser list, then starts the correct
    /// direct transport. Desktop Players initiate WireGuard; W1VE ESP32 Players use only the
    /// dedicated CW frame carrier.
    pub fn connect_lan_player(&self, player_type: &str, id: &str) {
        let selected = match self.shared.selected_lan_player(player_type, id) {
            Ok(player) => player,
            Err(message) => {
                self.shared.log("warn", message);
                return;
            }
        };
        if let DiscoveredLanPlayer::Esp32Player(player) = &selected {
            if player.busy() {
                self.shared.log(
                    "warn",
                    format!("W1VE ESP32 Player {} is busy; select an available Player.", player.name),
                );
                return;
            }
        }

        self.disconnect();
        let operation = self.shared.begin_operation();
        // Enter a capability guard before pairing so no concurrent Router command can create a
        // Flex emitter or apply forwarding while a CW-only Player is being negotiated. The guard
        // is owned by this operation, so a replaced attempt cannot change a newer selection.
        match &selected {
            DiscoveredLanPlayer::Esp32Player(_) => {
                if !self.shared.enter_esp_mode_for_operation(operation) {
                    return;
                }
            }
            DiscoveredLanPlayer::Desktop(player) if !player.supports_router() => {
                if !self.shared.set_router_access_for_operation(
                    operation,
                    RouterAccess::Desktop { router_capable: false },
                    true,
                ) {
                    return;
                }
            }
            DiscoveredLanPlayer::Desktop(_) => {}
        }
        let shared = Arc::clone(&self.shared);
        if !shared.emit_for_operation(
            operation,
            ClientEvent::Connection { state: "connecting".into(), path: "none".into() },
        ) {
            return;
        }
        shared.log_for_operation(operation, "info", "Connecting to the selected LAN Player…");

        let handle = self.runtime.spawn(async move {
            if !shared.is_current_operation(operation) {
                return;
            }
            match selected {
                DiscoveredLanPlayer::Desktop(player) => {
                    connect_desktop_lan_player(&shared, operation, player).await;
                }
                DiscoveredLanPlayer::Esp32Player(player) => {
                    let keyer_id = match shared.config.lock().unwrap().identity.to_identity() {
                        Ok(identity) => esp_keyer_id(identity.public_bytes()),
                        Err(error) => {
                            return fail_lan_connection(
                                &shared,
                                operation,
                                format!("identity error: {error}"),
                            )
                        }
                    };
                    connect_esp32_player(&shared, operation, player, keyer_id).await;
                }
            }
        });
        self.shared.register_lan_connect(operation, handle);
    }

    // ─── Sidetone ──────────────────────────────────────────────────────────────

    /// Sets the sidetone frequency in Hz and rebuilds the stream to apply it.
    pub fn set_tone_frequency(&self, hz: i32) {
        self.shared.settings.lock().unwrap().tone_hz = hz;
        self.shared.persist_settings();
        self.restart_sidetone();
    }

    /// Sets the sidetone volume (0..=100) and rebuilds the stream to apply it.
    pub fn set_tone_volume(&self, percent: u32) {
        self.shared.settings.lock().unwrap().tone_volume = percent.min(100);
        self.shared.persist_settings();
        self.restart_sidetone();
    }

    /// Selects the sidetone output device. `None` (JS `null`) = system default; an empty
    /// string = "(None)" = sidetone off; any other string = that device by name.
    pub fn select_audio_device(&self, device: Option<String>) {
        self.shared.settings.lock().unwrap().audio_device = device;
        self.shared.persist_settings();
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

    /// Lists output audio devices (name + default flag) for the sidetone picker.
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

    /// Lists available serial ports for the device pickers.
    #[must_use]
    pub fn serial_ports() -> Vec<String> {
        enumerate_ports()
            .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
            .unwrap_or_default()
    }

    fn emit_ports(&self) {
        self.shared.emit(ClientEvent::Ports { available: Self::serial_ports() });
    }

    /// The serial ports we currently hold open (paddle poller running, keying output open,
    /// WinKeyer port selected). Used so reconciliation does not drop a working, in-use port
    /// that the OS hides from enumeration while it is busy.
    fn held_ports(&self) -> Vec<String> {
        let s = self.shared.settings.lock().unwrap();
        let mut held = Vec::new();
        if self.shared.keying.lock().unwrap().is_some() {
            if let Some(p) = &s.keying_port {
                held.push(p.clone());
            }
        }
        if self.shared.paddle.lock().unwrap().is_some() {
            if let Some(p) = &s.paddle_port {
                held.push(p.clone());
            }
        }
        if let Some(p) = &s.winkeyer_port {
            held.push(p.clone());
        }
        held
    }

    /// Re-enumerates serial ports and reconciles the three selections: any selected port no
    /// longer present (and not held open) is dropped to `None` and its handle released,
    /// without disturbing the others. Always emits the fresh port list; also refreshes audio
    /// devices (a USB sound device may have arrived/left on the same event).
    ///
    /// A port we *currently hold open* is never treated as missing even if it is absent from
    /// the enumeration: on Windows an exclusively-opened COM port is commonly hidden from
    /// `available_ports()` while in use, and dropping a working, in-use selection would snap
    /// it back to `(None)`.
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

        let (keying_gone, paddle_gone, winkeyer_gone) = {
            let s = self.shared.settings.lock().unwrap();
            (
                port_missing(&s.keying_port, &present),
                port_missing(&s.paddle_port, &present),
                port_missing(&s.winkeyer_port, &present),
            )
        };
        if !(keying_gone || paddle_gone || winkeyer_gone) {
            return;
        }

        if keying_gone {
            let name = self.shared.settings.lock().unwrap().keying_port.take();
            if let Some(n) = name {
                self.shared.log("warn", format!("Keying port {n} removed; set to (None)."));
            }
            self.select_keying_port(None);
        }
        if paddle_gone {
            let name = self.shared.settings.lock().unwrap().paddle_port.take();
            if let Some(n) = name {
                self.shared.log("warn", format!("Paddle port {n} removed; set to (None)."));
            }
            self.select_paddle_port(None);
        }
        if winkeyer_gone {
            let name = self.shared.settings.lock().unwrap().winkeyer_port.take();
            if let Some(n) = name {
                self.shared.log("warn", format!("WinKeyer port {n} removed; set to (None)."));
            }
            self.select_winkeyer_port(None);
        }
        self.shared.emit_settings();
    }

    /// Selects the serial port used to key a local rig's control line (DTR by default), or
    /// `None` to release it. This drives the physical key line in lock-step with the keyer,
    /// so a local rig keys alongside the remote one.
    pub fn select_keying_port(&self, port: Option<String>) {
        if let Some(p) = &port {
            let clash = {
                let s = self.shared.settings.lock().unwrap();
                held_by_other(
                    p,
                    &[(&s.paddle_port, "the paddle port"), (&s.winkeyer_port, "the WinKeyer port")],
                )
            };
            if let Some(label) = clash {
                self.shared.fail(
                    control::KEYING,
                    format!("{p} is {label}"),
                    format!("{p} is already {label}; a port can serve one job."),
                );
                return;
            }
        }
        self.shared.settings.lock().unwrap().keying_port = port.clone();
        self.shared.persist_settings();
        {
            let mut guard = self.shared.keying.lock().unwrap();
            *guard = None; // release any previous port first
            match port {
                Some(name) => {
                    let cfg = KeyingOutputConfig {
                        port_name: name.clone(),
                        key_line: KeyingLine::Dtr,
                        ..Default::default()
                    };
                    match SerialKeyingOutput::open(cfg) {
                        Ok(out) => {
                            *guard = Some(out);
                            self.shared.clear_error(control::KEYING);
                            self.shared.log("info", format!("Keying output on {name}."));
                        }
                        Err(e) => self.shared.fail(
                            control::KEYING,
                            format!("{name} not available"),
                            format!("Keying port {name} failed: {e}"),
                        ),
                    }
                }
                None => self.shared.clear_error(control::KEYING),
            }
        }
        self.emit_ports();
        self.shared.emit_settings();
    }

    /// Selects the serial port a paddle/key is wired to, or `None` to release it.
    ///
    /// Opening starts a dedicated high-priority poller thread that reads the port's modem-
    /// status lines (CTS=dit, DSR=dah, DCD=straight), debounces them, runs the live keyer for
    /// the current mode/speed/weight, and keys the local sidetone, the local key line, the UI
    /// indicator, and (when paired) the tunnel — exactly the output path the type-ahead CW
    /// uses. This matches the C# `PaddleInputPoller`: 1 ms poll, raised priority, DTR asserted
    /// as the contact voltage source, so it works on native UARTs and USB-to-serial alike.
    pub fn select_paddle_port(&self, port: Option<String>) {
        if let Some(p) = &port {
            let clash = {
                let s = self.shared.settings.lock().unwrap();
                held_by_other(
                    p,
                    &[(&s.keying_port, "the keying port"), (&s.winkeyer_port, "the WinKeyer port")],
                )
            };
            if let Some(label) = clash {
                self.shared.fail(
                    control::PADDLE,
                    format!("{p} is {label}"),
                    format!("{p} is already {label}; a port can serve one job."),
                );
                return;
            }
        }
        self.shared.settings.lock().unwrap().paddle_port = port.clone();
        self.shared.persist_settings();
        // Stop any running poller first (changing ports is a normal operation).
        *self.shared.paddle.lock().unwrap() = None;

        match port {
            Some(name) => match PaddleInputPort::open(&name) {
                Ok(input) => {
                    let handle = PaddlePollerHandle::spawn(
                        input,
                        Arc::clone(&self.shared),
                        self.runtime.handle().clone(),
                    );
                    *self.shared.paddle.lock().unwrap() = Some(handle);
                    self.shared.clear_error(control::PADDLE);
                    self.shared.log("info", format!("Paddle input on {name} (CTS=dit, DSR=dah, DCD=straight)."));
                }
                Err(e) => self.shared.fail(
                    control::PADDLE,
                    format!("{name} not available"),
                    format!("Paddle port {name} failed: {e}"),
                ),
            },
            None => {
                self.shared.clear_error(control::PADDLE);
                self.shared.log("info", "Paddle input cleared.");
            }
        }
        self.emit_ports();
        self.shared.emit_settings();
    }

    /// Selects the WinKeyer port: a logging program on it is served as a WinKeyer host, and its
    /// text is keyed through the send buffer (sidetone, key line, tunnel).
    pub fn select_winkeyer_port(&self, port: Option<String>) {
        if let Some(p) = &port {
            let clash = {
                let s = self.shared.settings.lock().unwrap();
                held_by_other(
                    p,
                    &[(&s.keying_port, "the keying port"), (&s.paddle_port, "the paddle port")],
                )
            };
            if let Some(label) = clash {
                self.shared.fail(
                    control::WINKEYER,
                    format!("{p} is {label}"),
                    format!("{p} is already {label}; a port can serve one job."),
                );
                return;
            }
        }
        // Replacing the handle drops the previous host, which stops its threads and frees the port.
        *self.shared.logger.lock().unwrap() = None;
        self.shared.speed_from_logger.store(false, Ordering::SeqCst);
        self.shared.settings.lock().unwrap().winkeyer_port = port.clone();
        self.shared.persist_settings();
        match port {
            None => {
                self.shared.clear_error(control::WINKEYER);
                self.shared.log("info", "WinKeyer port cleared.");
            }
            Some(name) => {
                let shared = Arc::clone(&self.shared);
                let port_name = name.clone();
                let report: HostReporter = Arc::new(move |report: HostReport| match report {
                    HostReport::Log { level, message } => shared.log(&level, message),
                    HostReport::ReadFailed(e) => shared.fail(
                        control::WINKEYER,
                        format!("{port_name} not available"),
                        format!("WinKeyer port {port_name} failed: {e}"),
                    ),
                    HostReport::Sending(_) => {}
                });
                let sink = ClientLogger { shared: Arc::clone(&self.shared) };
                match LoggerHost::open_text(&name, sink, report) {
                    Ok(host) => {
                        *self.shared.logger.lock().unwrap() = Some(host);
                        self.shared.clear_error(control::WINKEYER);
                        self.shared.log(
                            "info",
                            format!("WinKeyer port {name} listening for WinKeyer commands."),
                        );
                    }
                    Err(e) => {
                        self.shared.fail(control::WINKEYER, format!("{name} not available"), e);
                    }
                }
            }
        }
        self.emit_ports();
        self.shared.emit_settings();
    }

    // ─── Keying ──────────────────────────────────────────────────────────────

    /// Appends text (a typed character or a macro) to the send buffer. It is keyed with
    /// proper CW spacing by the single persistent send-worker thread, after whatever is
    /// already queued — so characters typed or pasted while a send is in flight chain on
    /// seamlessly. Playback fans to the sidetone, the local key line, the UI indicator, and
    /// (when paired) the tunnel through `drive_edge`.
    pub fn send_cw(&self, text: &str) {
        let wpm = self.shared.settings.lock().unwrap().wpm;
        self.shared.send.push(text, wpm);
    }

    /// Sends the standard test string, matching the C# "Test TX" button.
    pub fn test_tx(&self) {
        self.send_cw("VVV TESTING");
    }

    /// Immediately stops sending: flushes the buffer and drops the in-progress character
    /// (ESC / WinKeyer "clear buffer"). The key is forced up.
    pub fn abort_send(&self) {
        self.shared.send.abort();
    }

    // ─── Ham Router: forward rules ─────────────────────────────────────────────

    /// Adds a forward rule. `protocol` is "tcp" or "udp". Ports are validated: a rule using a
    /// reserved RWK port (7373/41373), or a duplicate client port on the same protocol, is
    /// rejected with a logged warning and no change. New rules default to **disabled** so the
    /// operator enables them deliberately (matching the C#).
    #[allow(clippy::too_many_arguments)]
    pub fn add_forward_rule(
        &self,
        name: &str,
        protocol: &str,
        client_port: u16,
        station_port: u16,
        station_target_address: &str,
    ) {
        if !self.router_command_allowed("Adding a forward rule") {
            return;
        }
        let proto = if protocol.eq_ignore_ascii_case("udp") { "udp" } else { "tcp" };
        if is_reserved_port(client_port) || is_reserved_port(station_port) {
            self.shared.log("warn", "That port is reserved for RWK control traffic.");
            return;
        }
        {
            let mut config = self.shared.config.lock().unwrap();
            if config
                .forwards
                .iter()
                .any(|r| r.protocol == proto && r.client_port == client_port)
            {
                drop(config);
                self.shared.log("warn", format!("A {proto} rule already uses client port {client_port}."));
                return;
            }
            let id = format!("{proto}-{client_port}-{}", config.forwards.len() + 1);
            config.forwards.push(ForwardRuleConfig {
                id,
                name: name.to_string(),
                protocol: proto.to_string(),
                client_port,
                station_port,
                enabled: false,
                station_target_address: station_target_address.to_string(),
            });
        }
        self.shared.persist_config();
        self.shared.emit_forward_rules();
        self.push_and_apply_rules();
    }

    /// Replaces the fields of the rule `id` with the edited values, keeping its id and enabled
    /// state. The table is pushed to the Station and the Client's forwards restart to match, so
    /// the edit takes effect while paired.
    pub fn update_forward_rule(
        &self,
        id: &str,
        name: &str,
        protocol: &str,
        client_port: u16,
        station_port: u16,
        station_target_address: &str,
    ) {
        if !self.router_command_allowed("Updating a forward rule") {
            return;
        }
        let proto = if protocol.eq_ignore_ascii_case("udp") { "udp" } else { "tcp" };
        if is_reserved_port(client_port) || is_reserved_port(station_port) {
            self.shared.log("warn", "That port is reserved for RWK control traffic.");
            return;
        }
        {
            let mut config = self.shared.config.lock().unwrap();
            if !config.forwards.iter().any(|r| r.id == id) {
                drop(config);
                self.shared.log("warn", "That forward rule no longer exists.");
                return;
            }
            if config
                .forwards
                .iter()
                .any(|r| r.id != id && r.protocol == proto && r.client_port == client_port)
            {
                drop(config);
                self.shared.log("warn", format!("A {proto} rule already uses client port {client_port}."));
                return;
            }
            if let Some(rule) = config.forwards.iter_mut().find(|r| r.id == id) {
                rule.name = name.to_string();
                rule.protocol = proto.to_string();
                rule.client_port = client_port;
                rule.station_port = station_port;
                rule.station_target_address = station_target_address.to_string();
            }
        }
        self.shared.persist_config();
        self.shared.emit_forward_rules();
        self.push_and_apply_rules();
    }

    /// Removes the forward rule with `id`.
    pub fn remove_forward_rule(&self, id: &str) {
        if !self.router_command_allowed("Removing a forward rule") {
            return;
        }
        {
            let mut config = self.shared.config.lock().unwrap();
            config.forwards.retain(|r| r.id != id);
        }
        self.shared.persist_config();
        self.shared.emit_forward_rules();
        self.push_and_apply_rules();
    }

    /// Enables or disables the forward rule with `id`.
    pub fn set_forward_rule_enabled(&self, id: &str, enabled: bool) {
        if !self.router_command_allowed("Changing a forward rule") {
            return;
        }
        {
            let mut config = self.shared.config.lock().unwrap();
            if let Some(rule) = config.forwards.iter_mut().find(|r| r.id == id) {
                rule.enabled = enabled;
            }
        }
        self.shared.persist_config();
        self.shared.emit_forward_rules();
        self.push_and_apply_rules();
    }

    /// Re-emits the current rule table to the UI (for an explicit refresh).
    pub fn refresh_forward_rules(&self) {
        if !self.router_command_allowed("Refreshing forward rules") {
            return;
        }
        self.shared.emit_forward_rules();
    }

    /// Pushes the current rules to the Station (if paired) and restarts the Client's own
    /// forwards to match. Runs on the controller runtime; non-blocking for the caller.
    fn push_and_apply_rules(&self) {
        let shared = Arc::clone(&self.shared);
        let operation = shared.current_operation();
        let rules = self.shared.forward_rules();
        self.runtime.spawn(async move {
            push_rules(&shared, operation, &rules).await;
        });
    }

    fn router_command_allowed(&self, action: &str) -> bool {
        let Some(reason) = self.shared.router_block_reason() else {
            return true;
        };
        self.shared.log("warn", format!("{action} was not applied: {reason}"));
        false
    }

    // ─── Ham Router: FlexRadio discovery re-emission ───────────────────────────

    /// Enables or disables FlexRadio discovery re-emission on the Client LAN. Enabling creates
    /// the host-side emitter and auto-adds the Flex forward rules (TCP 4992 command, UDP 4991
    /// stream) if absent; disabling removes the emitter and those rules. The real radio IP is
    /// filled into `station_target_address` when the first discovery announcement arrives.
    pub fn set_flex_discovery(&self, enabled: bool) {
        let operation = self.shared.current_operation();
        if enabled {
            let emitter = match DiscoveryEmitter::new() {
                Ok(emitter) => emitter,
                Err(e) => {
                    self.shared.log_for_operation(
                        operation,
                        "warn",
                        format!("Flex re-emission unavailable: {e}"),
                    );
                    return;
                }
            };
            if let Err(reason) = self.shared.set_flex_for_operation(operation, Some(emitter)) {
                self.shared.log_for_operation(
                    operation,
                    "warn",
                    format!("Changing FlexRadio discovery was not applied: {reason}"),
                );
                return;
            }
            self.ensure_flex_rules();
            self.shared.log_for_operation(operation, "info", "FlexRadio discovery re-emission enabled.");
        } else {
            if let Err(reason) = self.shared.set_flex_for_operation(operation, None) {
                self.shared.log_for_operation(
                    operation,
                    "warn",
                    format!("Changing FlexRadio discovery was not applied: {reason}"),
                );
                return;
            }
            self.remove_flex_rules();
            self.shared.log_for_operation(operation, "info", "FlexRadio discovery re-emission disabled.");
        }
        self.shared.persist_config();
        self.shared.emit_forward_rules();
        self.push_and_apply_rules();
    }

    /// Ensures the two Flex forward rules exist (TCP 4992, UDP 4991), enabled.
    fn ensure_flex_rules(&self) {
        let mut config = self.shared.config.lock().unwrap();
        for (proto, port, label) in [
            ("tcp", 4992u16, "[Flex] SmartSDR Command"),
            ("udp", 4991u16, "[Flex] VITA-49 Stream"),
        ] {
            if !config.forwards.iter().any(|r| r.protocol == proto && r.client_port == port) {
                config.forwards.push(ForwardRuleConfig {
                    id: format!("flex-{proto}-{port}"),
                    name: label.to_string(),
                    protocol: proto.to_string(),
                    client_port: port,
                    station_port: port,
                    enabled: true,
                    station_target_address: String::new(), // filled from the first announce
                });
            }
        }
    }

    /// Removes the auto-managed Flex forward rules.
    fn remove_flex_rules(&self) {
        let mut config = self.shared.config.lock().unwrap();
        config.forwards.retain(|r| !r.id.starts_with("flex-"));
    }

    // ─── Pairing ──────────────────────────────────────────────────────────────

    /// Pairs with the station using a shared pairing code over the configured relay, brings
    /// the keying edge up over the tunnel, and reports the path that won.
    ///
    /// Non-blocking: the (slow) rendezvous and handshake run on the controller's own
    /// runtime, and the outcome is reported through [`ClientEvent::Connection`] /
    /// [`ClientEvent::Log`] rather than a return value, so the UI thread never stalls.
    pub fn connect(&self, relay: &str, code: &str) {
        let relay_addr: std::net::SocketAddr = match relay.parse() {
            Ok(a) => a,
            Err(e) => {
                self.shared.log("warn", format!("Invalid relay address '{relay}': {e}"));
                self.shared.emit(ClientEvent::Connection {
                    state: "disconnected".into(),
                    path: "none".into(),
                });
                return;
            }
        };
        let code = code.to_string();
        // Retire the previous pairing before assigning the operation token to this supervisor.
        // `abort` remains useful, but every post-await install below also verifies this token.
        self.disconnect();
        let operation = self.shared.begin_operation();
        let shared = Arc::clone(&self.shared);

        if !shared.emit_for_operation(
            operation,
            ClientEvent::Connection { state: "connecting".into(), path: "none".into() },
        ) {
            return;
        }

        let handle = self.runtime.spawn(async move {
            if !shared.is_current_operation(operation) {
                return;
            }
            // Remember the relay + code for next launch, but never allow an old task to clobber
            // a replacement's pairing code after it resumes from cancellation.
            shared.persist_pairing_for_operation(operation, relay_addr, &code);
            let mut attempts: u32 = 0;
            let mut budget = ReconnectBudget::new();
            loop {
                if !shared.is_current_operation(operation) {
                    return;
                }
                let remaining = budget.remaining(std::time::Instant::now());
                if remaining.is_zero() {
                    let _ = shared.emit_for_operation(
                        operation,
                        ClientEvent::Error {
                            control: "relay".into(),
                            message: "Relay not reachable".into(),
                        },
                    );
                    shared.fail_for_operation(
                        operation,
                        format!(
                            "Could not reach the Station within {}s; disconnected. Reconnect to try again.",
                            reconnect::RECONNECT_WINDOW.as_secs()
                        ),
                    );
                    return;
                }

                // Clone the PERSISTED config so the WireGuard identity is stable across runs,
                // then layer the relay and pairing code on top. The peer key is discovered by
                // rendezvous, so no key paste is needed. The forward rules travel to the Station
                // over the control channel, so they are not needed in the connect config.
                let config = {
                    let mut config = shared.config.lock().unwrap().clone();
                    config.role = Role::Client;
                    config.relay = Some(rwk_core::config::RelayConfig { address: relay_addr });
                    config.pairing_code = Some(code.clone());
                    config
                };

                // Each attempt is capped by the budget left, so pairing cannot hang past it; the
                // shared helper retries the relay rendezvous once (750 ms) inside that budget.
                let attempt =
                    tokio::time::timeout(remaining, connect_with_relay_retry(&config)).await;
                if !shared.is_current_operation(operation) {
                    // Any connected candidate is dropped here before it can create a Session.
                    return;
                }
                match attempt {
                    Err(_) => shared.log_for_operation(operation, "warn", "link failed: reconnect budget elapsed"),
                    Ok(Ok(connected)) => {
                        if let Some(edge) = bring_up_client(&shared, operation, connected, true).await {
                            if !shared.is_current_operation(operation) {
                                return;
                            }
                            attempts = 0;
                            let connected_at = std::time::Instant::now();
                            let rtt_shared = Arc::clone(&shared);
                            let outcome = watch_edge_liveness(edge, move |ms| {
                                let _ = rtt_shared.emit_for_operation(operation, ClientEvent::Rtt { ms });
                            })
                            .await;
                            budget.record_link_ended(connected_at, std::time::Instant::now());
                            if !teardown_session(&shared, operation).await {
                                return;
                            }
                            if !shared.is_current_operation(operation) {
                                return;
                            }
                            if outcome == Liveness::Closed {
                                // A deliberate local teardown (unpair) ends the supervisor.
                                return;
                            }
                            shared.log_for_operation(
                                operation,
                                "warn",
                                "The Station stopped responding; reconnecting…",
                            );
                            let _ = shared.emit_for_operation(
                                operation,
                                ClientEvent::Connection { state: "connecting".into(), path: "none".into() },
                            );
                            continue;
                        }
                    }
                    Ok(Err(e)) => shared.log_for_operation(operation, "warn", format!("link failed: {e}")),
                }

                if !shared.is_current_operation(operation) {
                    return;
                }
                attempts += 1;
                if attempts >= MAX_RECONNECT_ATTEMPTS {
                    let _ = shared.emit_for_operation(
                        operation,
                        ClientEvent::Error {
                            control: "relay".into(),
                            message: "Relay not reachable".into(),
                        },
                    );
                    shared.fail_for_operation(
                        operation,
                        format!(
                            "Could not reach the Station after {MAX_RECONNECT_ATTEMPTS} attempts; \
                             giving up. Reconnect to try again."
                        ),
                    );
                    return;
                }
                shared.log_for_operation(
                    operation,
                    "info",
                    format!("Reconnect attempt {attempts}/{MAX_RECONNECT_ATTEMPTS}…"),
                );
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
        self.shared.register_supervisor(operation, handle);
    }

    /// Stops the current pairing, LAN browsers, control path, and keyed-edge carrier. A paired
    /// W1VE ESP32 Player receives an explicit terminal unpair before its socket is released.
    pub fn disconnect(&self) {
        self.stop_lan_browse();
        // Retire before requesting task cancellation. `abort` is not a join barrier, so the
        // generation transition and resource detachment are the actual safety boundary.
        let retired = self.shared.retire_operation();
        self.runtime.block_on(dispose_retired_client_resources(retired));
    }
}

/// Stops resources detached by [`Shared::retire_operation`]. This runs after the installation
/// gate has advanced, so it can never take a Session or edge installed by a newer operation.
async fn dispose_retired_client_resources(retired: RetiredClientResources) {
    if let Some(task) = retired.supervisor {
        task.abort();
    }
    if let Some(task) = retired.lan_connect {
        task.abort();
    }
    if let Some(task) = retired.control_task {
        task.abort();
    }
    if let Some(active) = retired.esp_active {
        let _ = active.session.send_unpair().await;
    }
    if let Some(session) = retired.session {
        session.lock().await.shutdown().await;
    }
}

/// Tears this desktop operation's edge, control channel, and WireGuard session down **without**
/// changing its generation, so the WAN supervisor can reconnect cleanly. It does nothing once a
/// Stop or replacement has advanced the installation gate.
async fn teardown_session(shared: &Arc<Shared>, operation: ClientOperation) -> bool {
    let (control_task, session) = {
        let mut lifecycle = shared.lifecycle.lock().unwrap();
        if !lifecycle.is_current(operation) {
            return false;
        }
        let owns_transport = matches!(
            lifecycle.transport.as_ref(),
            Some(transport) if transport.operation == operation
        );
        let owns_session = matches!(
            lifecycle.session.as_ref(),
            Some(session) if session.operation == operation
        );
        if !owns_transport && !owns_session {
            return false;
        }
        if owns_transport {
            lifecycle.transport = None;
        }
        shared.edge.detach();
        let control_task = if matches!(
            lifecycle.control_task.as_ref(),
            Some(task) if task.operation == operation
        ) {
            lifecycle.control_task.take().map(|task| task.resource)
        } else {
            None
        };
        if matches!(
            lifecycle.control_tx.as_ref(),
            Some(sender) if sender.operation == operation
        ) {
            lifecycle.control_tx = None;
        }
        let session = if owns_session {
            lifecycle.session.take().map(|session| session.resource)
        } else {
            None
        };
        // Report the teardown while holding the same gate that accepts a new edge/session.
        shared.emit(ClientEvent::Rtt { ms: None });
        (control_task, session)
    };
    if let Some(task) = control_task {
        task.abort();
    }
    if let Some(session) = session {
        session.lock().await.shutdown().await;
    }
    true
}

/// Shared post-connect wiring for a desktop Player. WAN always permits the Router path; a LAN
/// Player must advertise Router capability before control, forwarding, or Flex starts. Every
/// post-await candidate is checked against `operation`; a rejected Session is shut down locally.
async fn bring_up_client(
    shared: &Arc<Shared>,
    operation: ClientOperation,
    connected: tunnel::Connected,
    router_capable: bool,
) -> Option<Arc<EdgeTransport>> {
    let path = format!("{:?}", connected.path).to_lowercase();
    let config = shared.config.lock().unwrap().clone();
    let mut session = match Session::start(connected, &config) {
        Ok(session) => session,
        Err(error) => {
            shared.fail_for_operation(operation, format!("session failed: {error}"));
            return None;
        }
    };
    if !shared.is_current_operation(operation) {
        session.shutdown().await;
        return None;
    }
    let net = session.handle();

    let conn = match net.open_udp(EDGE_PORT).await {
        Ok(conn) => conn,
        Err(error) => {
            session.shutdown().await;
            shared.fail_for_operation(operation, format!("edge open failed: {error}"));
            return None;
        }
    };
    if !shared.is_current_operation(operation) {
        session.shutdown().await;
        return None;
    }
    let transport = Arc::new(EdgeTransport::over_tunnel(conn));
    let peer = std::net::SocketAddr::new(
        std::net::IpAddr::V4(Role::Client.peer_inner_address()),
        EDGE_PORT,
    );
    transport.set_peer(peer).await;
    if !shared.is_current_operation(operation) {
        session.shutdown().await;
        return None;
    }

    // Wrap the Session only after all connection awaits. The installation block below is the
    // linearization point for edge, session, control, Router, and connected-state publication.
    let session = Arc::new(tokio::sync::Mutex::new(session));
    let committed = {
        let mut lifecycle = shared.lifecycle.lock().unwrap();
        if !lifecycle.is_current(operation) {
            None
        } else {
            let old_control = lifecycle.control_task.take().map(|task| task.resource);
            lifecycle.control_tx = None;
            let old_session = lifecycle.session.take().map(|session| session.resource);

            if router_capable {
                let player_inner = std::net::IpAddr::V4(Role::Client.peer_inner_address());
                let control = spawn_control_link(
                    Arc::clone(shared),
                    operation,
                    Arc::clone(&session),
                    net.clone(),
                    player_inner,
                );
                lifecycle.control_task = Some(OperationResource { operation, resource: control });
            } else {
                // A CW-only desktop Player retains the tunnel edge path but never inherits Flex
                // or Router service from a previous desktop pairing.
                *shared.flex_emitter.lock().unwrap() = None;
                shared.emit_flex_state();
                shared.log(
                    "info",
                    "Selected desktop Player is CW-only; Ham Router control, forwarding, and Flex are disabled.",
                );
            }

            // A new pairing restarts the edge stream at sequence 0 and starts its ordered tunnel
            // FIFO. The wire epoch remains stable because the paddle and type-ahead sources both
            // sample it independently.
            shared.edge.begin(*shared.epoch.lock().unwrap(), MonotonicClock.now(), Arc::clone(&transport));
            lifecycle.transport = Some(OperationResource {
                operation,
                resource: Arc::clone(&transport),
            });
            lifecycle.session = Some(OperationResource {
                operation,
                resource: Arc::clone(&session),
            });
            *shared.router_access.lock().unwrap() = RouterAccess::Desktop { router_capable };
            shared.emit_router_status();
            shared.emit(ClientEvent::Connection { state: "connected".into(), path: path.clone() });
            shared.log("info", format!("Paired over the {path} path."));
            Some((old_control, old_session))
        }
    };

    let Some((old_control, old_session)) = committed else {
        session.lock().await.shutdown().await;
        return None;
    };
    if let Some(task) = old_control {
        task.abort();
    }
    if let Some(session) = old_session {
        session.lock().await.shutdown().await;
    }
    Some(transport)
}

/// The ESP Player safety refresh repeats the newest complete RWK-PADDLE frame often enough to
/// repair a lost terminal edge without creating a second edge sender.
const ESP_FRAME_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
/// Wait briefly for broadcasts before probing the local /24 for ESP Players.
const ESP_DISCOVERY_FALLBACK_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
/// Pace the bounded ESP discovery fallback to at most ten unicast queries per second.
const ESP_DISCOVERY_QUERY_PACE: std::time::Duration = std::time::Duration::from_millis(100);

/// Runs the desktop Player browser until cancelled, updating the shared tagged selection list.
async fn browse_desktop_players(shared: Arc<Shared>, self_key: [u8; 32]) {
    let mut browser = match rwk_core::engine::lan_discovery::LanBrowser::bind(self_key) {
        Ok(browser) => browser,
        Err(error) => {
            shared.log("warn", format!("Desktop Player LAN browse failed: {error}"));
            shared.set_desktop_lan_players(Vec::new());
            return;
        }
    };
    shared.log("info", "Browsing the LAN for desktop Players…");
    shared.set_desktop_lan_players(browser.peers());
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(1), browser.recv_once()).await {
            Ok(Ok(_)) | Err(_) => shared.set_desktop_lan_players(browser.peers()),
            Ok(Err(error)) => {
                shared.log("warn", format!("Desktop Player LAN browse stopped: {error}"));
                break;
            }
        }
    }
    shared.set_desktop_lan_players(Vec::new());
}

/// Runs the ESP Player browser plus its deliberately bounded, paced unicast fallback. The core
/// browser limits a pass to 254 candidates; this loop sends no more than ten probes per second.
async fn browse_esp32_players(shared: Arc<Shared>, keyer_id: [u8; 6]) {
    let mut browser = match EspPlayerBrowser::bind(keyer_id).await {
        Ok(browser) => browser,
        Err(error) => {
            shared.log("warn", format!("W1VE ESP32 Player LAN browse failed: {error}"));
            shared.set_esp_lan_players(Vec::new());
            return;
        }
    };
    shared.log("info", "Browsing the LAN for W1VE ESP32 Players…");
    shared.set_esp_lan_players(browser.players());
    browser.request_discovery();
    let mut query_active = true;
    let mut next_refresh = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut next_query = tokio::time::Instant::now() + ESP_DISCOVERY_FALLBACK_DELAY;

    loop {
        // Keep this receive bounded so the same mutable browser can safely prune records and
        // send at most one fallback query per pacing interval without concurrent borrows.
        match tokio::time::timeout(ESP_DISCOVERY_QUERY_PACE, browser.recv_once()).await {
            Ok(Ok(_)) => shared.set_esp_lan_players(browser.players()),
            Ok(Err(error)) => {
                shared.log("warn", format!("W1VE ESP32 Player LAN browse stopped: {error}"));
                break;
            }
            Err(_) => {}
        }

        let now = tokio::time::Instant::now();
        if now >= next_refresh {
            shared.set_esp_lan_players(browser.players());
            next_refresh = now + std::time::Duration::from_secs(1);
        }
        if query_active && now >= next_query {
            match browser.send_next_discovery_query().await {
                Ok(more) => query_active = more,
                Err(error) => {
                    query_active = false;
                    shared.log("warn", format!("W1VE ESP32 Player discovery fallback stopped: {error}"));
                }
            }
            next_query = tokio::time::Instant::now() + ESP_DISCOVERY_QUERY_PACE;
        }
    }
    shared.set_esp_lan_players(Vec::new());
}

/// Resolves a selected desktop Player from a fresh RWKP v2 beacon so its current endpoint and
/// Router capability, not a stale webview snapshot, decide the direct connection path.
async fn resolve_desktop_player(
    self_key: [u8; 32],
    player_key: [u8; 32],
    timeout: std::time::Duration,
) -> Result<LanPeer, String> {
    let mut browser = rwk_core::engine::lan_discovery::LanBrowser::bind(self_key)
        .map_err(|error| format!("LAN Player resolution failed: {error}"))?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("the selected desktop Player is no longer advertising".to_string());
        }
        match tokio::time::timeout(remaining, browser.recv_once()).await {
            Ok(Ok(_)) => {
                if let Some(player) = browser
                    .peers()
                    .into_iter()
                    .find(|player| player.public_key == player_key)
                {
                    return Ok(player);
                }
            }
            Ok(Err(error)) => return Err(error.to_string()),
            Err(_) => return Err("the selected desktop Player is no longer advertising".to_string()),
        }
    }
}

/// Resolves a selected desktop Player again from a fresh RWKP v2 beacon, then brings the direct
/// WireGuard tunnel up as the initiating Keyer. Only Router-capable Players get the Router path.
async fn connect_desktop_lan_player(
    shared: &Arc<Shared>,
    operation: ClientOperation,
    player: LanPeer,
) {
    if !shared.is_current_operation(operation) {
        return;
    }
    let identity = match shared.config.lock().unwrap().identity.to_identity() {
        Ok(identity) => identity,
        Err(error) => return fail_lan_connection(shared, operation, format!("identity error: {error}")),
    };
    let player = match resolve_desktop_player(
        identity.public_bytes(),
        player.public_key,
        std::time::Duration::from_secs(4),
    )
    .await
    {
        Ok(player) => player,
        Err(error) => {
            return fail_lan_connection(
                shared,
                operation,
                format!("Selected desktop Player is no longer advertising: {error}"),
            )
        }
    };
    if !shared.is_current_operation(operation) {
        return;
    }
    let router_capable = player.supports_router();
    // Publish the fresh RWKP v2 capability only under this operation's gate. A late desktop
    // resolver can therefore never undo the CW-only ESP guard installed by a replacement.
    if !shared.set_router_access_for_operation(
        operation,
        RouterAccess::Desktop { router_capable },
        !router_capable,
    ) {
        return;
    }
    let connected = match tunnel::lan_connect_to(&identity, player.public_key, player.endpoint).await {
        Ok(connected) => connected,
        Err(error) => return fail_lan_connection(shared, operation, format!("LAN link failed: {error}")),
    };
    if !shared.is_current_operation(operation) {
        // `connected` drops before it can be promoted into a Session.
        return;
    }
    let Some(edge) = bring_up_client(shared, operation, connected, router_capable).await else {
        let _ = shared.set_router_access_for_operation(operation, RouterAccess::Inactive, false);
        return;
    };

    let rtt_shared = Arc::clone(shared);
    let outcome = watch_edge_liveness(edge, move |ms| {
        let _ = rtt_shared.emit_for_operation(operation, ClientEvent::Rtt { ms });
    })
    .await;
    if !teardown_session(shared, operation).await {
        return;
    }
    if !shared.set_router_access_for_operation(operation, RouterAccess::Inactive, false) {
        return;
    }
    let _ = shared.emit_for_operation(
        operation,
        ClientEvent::Connection { state: "disconnected".into(), path: "none".into() },
    );
    match outcome {
        Liveness::Closed => {
            shared.log_for_operation(operation, "info", "Desktop Player session closed.");
        }
        _ => {
            shared.log_for_operation(operation, "warn", "Desktop Player stopped responding; disconnected.");
        }
    }
}

/// Pairs with a selected available W1VE ESP32 Player. This path must never construct a
/// WireGuard `Session`, control link, forwarder, Flex emitter, or Router route.
async fn connect_esp32_player(
    shared: &Arc<Shared>,
    operation: ClientOperation,
    player: EspPlayerAdvertisement,
    keyer_id: [u8; 6],
) {
    if player.busy() {
        return fail_lan_connection(
            shared,
            operation,
            format!("W1VE ESP32 Player {} is busy.", player.name),
        );
    }
    if !shared.is_current_operation(operation) {
        return;
    }
    let session = match EspPlayerSession::pair(player, keyer_id).await {
        Ok(session) => Arc::new(session),
        Err(error) => {
            return fail_lan_connection(
                shared,
                operation,
                format!("W1VE ESP32 Player pairing failed: {error}"),
            )
        }
    };
    if !shared.is_current_operation(operation) {
        // Pairing allocates the ESP carrier before its first await returns. Explicitly release it
        // if Stop/replacement won while pairing rather than merely dropping the Arc.
        let _ = session.send_unpair().await;
        return;
    }
    run_esp32_player_session(shared, operation, session).await;
}

/// Drives the CW-only ESP transport. The existing `EdgeSession` remains the single ordered
/// RWK-PADDLE frame source; this function supplies its sender, repeats the newest complete frame
/// every 250 ms, and runs one-second ping/pong supervision with a three-miss limit.
async fn run_esp32_player_session(
    shared: &Arc<Shared>,
    operation: ClientOperation,
    session: Arc<EspPlayerSession>,
) {
    let stop = Arc::new(AtomicBool::new(false));
    let sender_session = Arc::clone(&session);
    let sender_stop = Arc::clone(&stop);
    let sender_shared = Arc::clone(shared);

    // The edge sender, ESP active state, Router isolation, and connected event all commit under
    // the one lifecycle gate. A stale paired ESP socket is explicitly unpaired below.
    let committed = {
        let mut lifecycle = shared.lifecycle.lock().unwrap();
        if !lifecycle.is_current(operation) {
            None
        } else {
            let old_esp = lifecycle.esp_active.take();
            if let Some(active) = &old_esp {
                active.stop.store(true, Ordering::SeqCst);
            }
            let old_control = lifecycle.control_task.take().map(|task| task.resource);
            let old_session = lifecycle.session.take().map(|session| session.resource);
            lifecycle.control_tx = None;
            lifecycle.transport = None;

            shared.edge.begin_with_sender(*shared.epoch.lock().unwrap(), MonotonicClock.now(), move |frame| {
                let session = Arc::clone(&sender_session);
                let stop = Arc::clone(&sender_stop);
                let shared = Arc::clone(&sender_shared);
                async move {
                    if stop.load(Ordering::SeqCst) || !shared.is_current_operation(operation) {
                        return;
                    }
                    if let Err(error) = session.send_frame(&frame).await {
                        if !stop.load(Ordering::SeqCst) && shared.is_current_operation(operation) {
                            shared.log_for_operation(
                                operation,
                                "warn",
                                format!("W1VE ESP32 Player frame send failed: {error}"),
                            );
                        }
                    }
                }
            });
            lifecycle.esp_active = Some(EspActiveSession {
                operation,
                session: Arc::clone(&session),
                stop: Arc::clone(&stop),
            });
            // Entering ESP always removes desktop Router/Flex resources. No desktop control or
            // session is installed by this branch, preserving strict CW-only isolation.
            *shared.flex_emitter.lock().unwrap() = None;
            *shared.router_access.lock().unwrap() = RouterAccess::Esp32Player;
            shared.emit_router_status();
            shared.emit_flex_state();
            shared.emit(ClientEvent::Connection { state: "connected".into(), path: "esp".into() });
            shared.log(
                "info",
                format!(
                    "Paired with W1VE ESP32 Player {} at {} (CW only).",
                    session.player().name,
                    session.player().endpoint
                ),
            );
            Some((old_esp, old_control, old_session))
        }
    };

    let Some((old_esp, old_control, old_session)) = committed else {
        stop.store(true, Ordering::SeqCst);
        let _ = session.send_unpair().await;
        return;
    };
    if let Some(task) = old_control {
        task.abort();
    }
    if let Some(active) = old_esp {
        let _ = active.session.send_unpair().await;
    }
    if let Some(session) = old_session {
        session.lock().await.shutdown().await;
    }

    let mut refresh_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + ESP_FRAME_REFRESH_INTERVAL,
        ESP_FRAME_REFRESH_INTERVAL,
    );
    let mut ping_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + ESP_PLAYER_PING_INTERVAL,
        ESP_PLAYER_PING_INTERVAL,
    );
    let mut pending_ping = None;
    let mut missed_pings = 0u8;
    let mut send_terminal_unpair = true;
    let reason = loop {
        if stop.load(Ordering::SeqCst) || !shared.is_current_operation(operation) {
            break "W1VE ESP32 Player session stopped locally".to_string();
        }
        tokio::select! {
            incoming = session.recv() => match incoming {
                Ok(Some(EspPlayerInbound::Unpaired)) => {
                    send_terminal_unpair = false;
                    break "W1VE ESP32 Player requested unpair".to_string();
                }
                Ok(_) => {}
                Err(error) => break format!("W1VE ESP32 Player receive failed: {error}"),
            },
            _ = refresh_tick.tick() => {
                if !stop.load(Ordering::SeqCst) && shared.is_current_operation(operation) {
                    if let Some(frame) = shared.edge.latest_frame() {
                        if let Err(error) = session.send_frame(&frame).await {
                            if session.sequence_exhausted() {
                                break "W1VE ESP32 Player envelope sequence exhausted".to_string();
                            }
                            if !stop.load(Ordering::SeqCst) && shared.is_current_operation(operation) {
                                shared.log_for_operation(
                                    operation,
                                    "warn",
                                    format!("W1VE ESP32 Player frame refresh failed: {error}"),
                                );
                            }
                        }
                    }
                }
            },
            _ = ping_tick.tick() => {
                if stop.load(Ordering::SeqCst) || !shared.is_current_operation(operation) {
                    break "W1VE ESP32 Player session stopped locally".to_string();
                }
                let prior_missed = if let Some(stamp) = pending_ping.take() {
                    if session.ping_was_acknowledged(stamp) {
                        missed_pings = 0;
                        false
                    } else {
                        missed_pings = missed_pings.saturating_add(1);
                        true
                    }
                } else {
                    false
                };
                if missed_pings >= ESP_PLAYER_MISSED_PING_LIMIT {
                    break "W1VE ESP32 Player missed three ping replies".to_string();
                }
                match session.send_ping().await {
                    Ok(stamp) => pending_ping = Some(stamp),
                    Err(error) => {
                        if session.sequence_exhausted() {
                            break "W1VE ESP32 Player envelope sequence exhausted".to_string();
                        }
                        if !prior_missed {
                            missed_pings = missed_pings.saturating_add(1);
                        }
                        if missed_pings >= ESP_PLAYER_MISSED_PING_LIMIT {
                            break "W1VE ESP32 Player missed three ping replies".to_string();
                        }
                        shared.log_for_operation(
                            operation,
                            "warn",
                            format!("W1VE ESP32 Player ping send failed: {error}"),
                        );
                    }
                }
                let ms = session
                    .rtt()
                    .map(|duration| u32::try_from(duration.as_millis()).unwrap_or(u32::MAX));
                let _ = shared.emit_for_operation(operation, ClientEvent::Rtt { ms });
            }
        }
    };

    // Only the active ESP owner may detach the edge and report terminal state. A Stop or
    // replacement already detached and unpaired its own retired resource, so this stale task
    // simply exits without touching the newer lifecycle.
    let active = {
        let mut lifecycle = shared.lifecycle.lock().unwrap();
        let owns_session = lifecycle.is_current(operation)
            && matches!(
                lifecycle.esp_active.as_ref(),
                Some(active)
                    if active.operation == operation && Arc::ptr_eq(&active.session, &session)
            );
        if !owns_session {
            None
        } else {
            let active = lifecycle.esp_active.take().expect("checked ESP owner");
            active.stop.store(true, Ordering::SeqCst);
            shared.edge.detach();
            *shared.router_access.lock().unwrap() = RouterAccess::Inactive;
            shared.emit_router_status();
            shared.emit(ClientEvent::Rtt { ms: None });
            shared.emit(ClientEvent::Connection { state: "disconnected".into(), path: "none".into() });
            shared.log("warn", format!("{reason}; disconnected."));
            Some(active)
        }
    };
    if let Some(active) = active {
        if send_terminal_unpair {
            let _ = active.session.send_unpair().await;
        }
    }
}

/// Reports a LAN-selection failure and restores normal Router configuration availability only if
/// the failed attempt still owns the lifecycle.
fn fail_lan_connection(shared: &Arc<Shared>, operation: ClientOperation, message: String) {
    let _ = shared.set_router_access_for_operation(operation, RouterAccess::Inactive, false);
    shared.fail_for_operation(operation, message);
}

/// Pushes `rules` to the Station over the control channel (if paired) and restarts the
/// Client's own forward tasks to match. Safe to call when not paired (does nothing).
async fn push_rules(
    shared: &Arc<Shared>,
    operation: ClientOperation,
    rules: &[ForwardRuleConfig],
) {
    let (sender, session, has_transport, block_reason) = {
        let lifecycle = shared.lifecycle.lock().unwrap();
        if !lifecycle.is_current(operation) {
            return;
        }
        let block_reason = shared.router_access.lock().unwrap().block_reason();
        let sender = lifecycle
            .control_tx
            .as_ref()
            .filter(|sender| sender.operation == operation)
            .map(|sender| sender.resource.clone());
        let session = lifecycle
            .session
            .as_ref()
            .filter(|session| session.operation == operation)
            .map(|session| Arc::clone(&session.resource));
        let has_transport = matches!(
            lifecycle.transport.as_ref(),
            Some(transport) if transport.operation == operation
        );
        (sender, session, has_transport, block_reason)
    };
    if let Some(reason) = block_reason {
        shared.log_for_operation(operation, "warn", format!("Forwarding was not applied: {reason}"));
        return;
    }
    if !shared.is_current_operation(operation) {
        return;
    }
    // Queue the push for the Station. While the control channel is down nothing is queued: the
    // next dial sends the rules from the config.
    match sender {
        Some(sender) => {
            let _ = sender.send(ControlMessage::ForwardRules { rules: rules.to_vec() });
        }
        None if has_transport => {
            shared.log_for_operation(
                operation,
                "warn",
                "Control channel is down; rule change not sent to the Station.",
            );
        }
        None => {}
    }
    // Restart the Client's own forwards only on the Session that remains installed for this
    // operation. Stop removes that Session from the gate before it begins shutdown.
    if let Some(session_handle) = session {
        let mut session = session_handle.lock().await;
        if !shared.owns_current_session(operation, &session_handle) {
            return;
        }
        let started = session.apply_forwards(rules).await;
        drop(session);
        shared.log_for_operation(operation, "info", format!("{started} forward rule(s) active locally."));
    }
}

/// How long the Client waits before redialling the control channel. The delay doubles from the
/// minimum up to the maximum while dials keep failing, and resets once a dial succeeds.
const CONTROL_REDIAL_MIN: std::time::Duration = std::time::Duration::from_millis(500);
const CONTROL_REDIAL_MAX: std::time::Duration = std::time::Duration::from_secs(5);

/// Keeps the control channel to the Station up for the whole pairing. The Station listens for it
/// only after its own pairing setup, so an early dial can be refused, and a dropped link is never
/// revived by itself. Each connection queues the current forward rules, carries pushes out and
/// FlexRadio announcements back, and is redialled when it ends.
fn spawn_control_link(
    shared: Arc<Shared>,
    operation: ClientOperation,
    owner_session: Arc<tokio::sync::Mutex<Session>>,
    net: rwk_core::engine::netstack_driver::NetStackHandle,
    station_inner: std::net::IpAddr,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !shared.owns_current_session(operation, &owner_session)
            || shared.router_block_reason().is_some()
        {
            return;
        }
        let mut delay = CONTROL_REDIAL_MIN;
        loop {
            if !shared.owns_current_session(operation, &owner_session)
                || shared.router_block_reason().is_some()
            {
                return;
            }
            match ControlLink::connect(&net, station_inner).await {
                Ok(mut link) => {
                    if !shared.owns_current_session(operation, &owner_session)
                        || shared.router_block_reason().is_some()
                    {
                        return;
                    }
                    delay = CONTROL_REDIAL_MIN;
                    let (tx, mut outgoing) = tokio::sync::mpsc::unbounded_channel();
                    // Queue the current rules first; they go out once the link is up.
                    let rules = shared.config.lock().unwrap().forwards.clone();
                    let _ = tx.send(ControlMessage::ForwardRules { rules });
                    if !shared.install_control_sender(operation, &owner_session, tx) {
                        return;
                    }
                    shared.log_for_operation(operation, "info", "Control channel dialled.");
                    let mut keep_running = true;
                    loop {
                        if !shared.owns_current_session(operation, &owner_session)
                            || shared.router_block_reason().is_some()
                        {
                            keep_running = false;
                            break;
                        }
                        tokio::select! {
                            queued = outgoing.recv() => match queued {
                                Some(message) => {
                                    if let Err(error) = link.send(&message).await {
                                        shared.log_for_operation(
                                            operation,
                                            "warn",
                                            format!("Control channel send failed: {error}"),
                                        );
                                        break;
                                    }
                                }
                                None => break,
                            },
                            incoming = link.recv() => match incoming {
                                Ok(Some(ControlMessage::DiscoveryAnnounce { payload })) => {
                                    handle_discovery_announce(
                                        &shared,
                                        operation,
                                        &owner_session,
                                        &payload,
                                    );
                                }
                                Ok(Some(_)) => {} // the Client ignores other message types
                                Ok(None) => {
                                    shared.log_for_operation(
                                        operation,
                                        "warn",
                                        "Control channel ended by the station.",
                                    );
                                    break;
                                }
                                Err(error) => {
                                    shared.log_for_operation(
                                        operation,
                                        "warn",
                                        format!("Control channel read failed: {error}"),
                                    );
                                    break;
                                }
                            },
                        }
                    }
                    shared.clear_control_sender(operation, &owner_session);
                    if !keep_running || !shared.owns_current_session(operation, &owner_session) {
                        return;
                    }
                    shared.log_for_operation(
                        operation,
                        "warn",
                        "Control channel to the station closed; redialling.",
                    );
                }
                Err(error) => {
                    if !shared.owns_current_session(operation, &owner_session) {
                        return;
                    }
                    shared.log_for_operation(
                        operation,
                        "warn",
                        format!("Control channel unavailable: {error}"),
                    );
                }
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(CONTROL_REDIAL_MAX);
        }
    })
}

/// Rewrites a captured Flex discovery packet to the local forwarded endpoint and re-broadcasts
/// it on the Client LAN. The control task must still own the current desktop Session at the final
/// mutation point, so a stale link cannot revive Flex behavior after ESP selection or reconnect.
fn handle_discovery_announce(
    shared: &Arc<Shared>,
    operation: ClientOperation,
    owner_session: &Arc<tokio::sync::Mutex<Session>>,
    payload_b64: &str,
) {
    let Ok(packet) = base64_decode(payload_b64) else {
        return;
    };
    let radio = flex_vita::parse(&packet).ok();

    let lifecycle = shared.lifecycle.lock().unwrap();
    if !lifecycle.is_current(operation)
        || !matches!(
            lifecycle.session.as_ref(),
            Some(session)
                if session.operation == operation && Arc::ptr_eq(&session.resource, owner_session)
        )
        || shared.router_access.lock().unwrap().block_reason().is_some()
    {
        return;
    }
    // Learn the radio's real address from the packet and set it on the Flex rules. Invalid
    // packets are still re-broadcast below, matching the existing Flex relay behavior.
    if let Some(radio) = radio {
        let mut changed = false;
        {
            let mut config = shared.config.lock().unwrap();
            for rule in config.forwards.iter_mut().filter(|rule| rule.id.starts_with("flex-")) {
                let address = radio.address.to_string();
                if rule.station_target_address != address {
                    rule.station_target_address = address;
                    changed = true;
                }
            }
        }
        if changed {
            shared.persist_config();
            shared.emit_forward_rules();
        }
    }
    // Re-broadcast rewritten to the local Flex command port (4992) on the Client LAN.
    if let Some(emitter) = shared.flex_emitter.lock().unwrap().as_ref() {
        let _ = emitter.rebroadcast(&packet, std::net::Ipv4Addr::LOCALHOST, 4992);
    }
}

impl Drop for ClientController {
    fn drop(&mut self) {
        // Stop the send worker so its thread exits, then tear the link down.
        self.shared.send.shutdown();
        self.disconnect();
    }
}

// ─── Send worker: drains the type-ahead / macro buffer with real CW timing ─────

/// Spawns the single send-worker thread. It waits for buffered characters, keys them through
/// every output (`drive_edge`) with correct element/character/word spacing, and picks up
/// characters typed or pasted during a send on the fly. An abort (ESC) drops the queue and
/// the in-progress character immediately. Spawned once for the controller's life.
fn spawn_send_worker(shared: Arc<Shared>, rt: tokio::runtime::Handle) {
    std::thread::Builder::new()
        .name("rwk-sender".into())
        .spawn(move || send_worker(&shared, &rt))
        .expect("spawn send worker");
}

/// The send worker keys the buffer as a **continuous element stream**, one character at a
/// time, managing the inter-character and word gaps itself. Characters typed *while sending*
/// chain on seamlessly with correct spacing, because the running `position` stays ahead of
/// real time whenever the buffer is non-empty, so the next character is scheduled a proper
/// inter-character gap after the previous one — never jammed up against it.
///
/// Each edge is fanned through [`drive_edge`] (sidetone + local key line + UI indicator +
/// tunnel), exactly the path the paddle poller uses, so a typed macro and a paddle key
/// identically — including paired keying over the tunnel.
fn send_worker(shared: &Arc<Shared>, rt: &tokio::runtime::Handle) {
    // Element timing is sub-element-critical: without a raised priority and a 1 ms timer the
    // OS scheduler can overshoot a ~17 ms inter-character gap by a whole quantum and smear the
    // spacing. Match the paddle loop's scheduling protection.
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
        let next: Option<(char, u32)> = {
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
        // (after any idle, and under scheduler slop). We do NOT clear `prev_emitted` here: a
        // character typed a moment after the previous one must still get its inter-character
        // gap, exactly as a pasted string does, or separate keystrokes would merge.
        // `prev_emitted` is only reset on a real word space, an abort, or startup.
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

            // Key down at `position`, up at `position + duration`.
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

/// Fans a single key transition to every output: local sidetone, the local serial key line,
/// the UI indicator, and (when paired) the tunnel edge transport. This is the one place the
/// Client turns a decided edge into effect, shared by the text/macro path ([`play_cw`]) and
/// the live paddle poller so both key identically.
fn drive_edge(
    shared: &Arc<Shared>,
    rt: &tokio::runtime::Handle,
    tone: Option<&Sidetone>,
    key_down: bool,
    timestamp_ticks: u64,
) {
    // 1. Local sidetone (via its own audio thread) — immediate, never waits on the network.
    if let Some(tone) = tone {
        tone.key(key_down);
    }
    // 2. Local key line, if a port is open.
    if let Some(out) = shared.keying.lock().unwrap().as_mut() {
        let _ = out.apply(key_down, false);
    }
    // 3. UI indicator.
    shared.emit(ClientEvent::KeyState { down: key_down });
    // 4. Selected transport when paired — hand the complete frame to the single ordered FIFO.
    //    The carrier is either the WireGuard edge transport or the CW-only W1VE ESP32 Player
    //    session, but all key sources share this one sender and sequence timeline.
    shared.edge.push_edge(key_down, timestamp_ticks);
    let _ = rt;
}

/// Nominal paddle poll interval, matching the C# `PaddleInputPoller` (1 ms).
const PADDLE_POLL_MS: u64 = 1;
/// Debounce window for the paddle contacts (5 ms), matching the C# default.
const PADDLE_DEBOUNCE_MS: u64 = 5;

/// A handle to the running paddle poller thread. Dropping it stops the thread, joins it, and
/// releases the serial port (DTR dropped). Changing or clearing the paddle port drops the
/// previous handle, so there is never more than one poller.
struct PaddlePollerHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PaddlePollerHandle {
    /// Spawns the poller thread over an open [`PaddleInputPort`].
    fn spawn(
        mut input: PaddleInputPort,
        shared: Arc<Shared>,
        rt: tokio::runtime::Handle,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("rwk-paddle".into())
            .spawn(move || paddle_loop(&mut input, &shared, &rt, &stop_thread))
            .expect("spawn paddle poller thread");
        Self { stop, thread: Some(thread) }
    }
}

impl Drop for PaddlePollerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The paddle poller body. Runs on a dedicated time-critical thread with a 1 ms timer period,
/// polling the serial control lines, debouncing, and driving the live keyer. Each resulting
/// edge is fanned to sidetone / key line / UI / tunnel through [`drive_edge`], the same path
/// the type-ahead CW uses, so a paddle and a typed macro key identically. A port read error
/// (adapter unplugged) ends the loop cleanly after releasing the key.
fn paddle_loop(
    input: &mut PaddleInputPort,
    shared: &Arc<Shared>,
    rt: &tokio::runtime::Handle,
    stop: &Arc<AtomicBool>,
) {
    // Raise priority and timer resolution for the whole session, mirroring the C# poller.
    let _priority = ThreadPriorityGuard::raise_time_critical();
    let _timer = TimerResolutionGuard::raise(1);

    let clock = MonotonicClock;
    let hz = clock.frequency();
    let mut debouncer = ContactDebouncer::new(PADDLE_DEBOUNCE_MS, hz);

    // The live keyer, re-tuned each element from the current UI settings so moving the WPM
    // slider or changing mode takes effect immediately.
    let (mut wpm, mut weight, mut mode, _reverse0) = read_keyer_settings(shared);
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
        // Read + debounce the contacts.
        let raw = match input.read() {
            Ok(sample) => sample,
            Err(e) => {
                shared.log("warn", format!("Paddle port read error: {e}. Input stopped."));
                break;
            }
        };
        let _ = debouncer.accept(raw, clock.now());
        let accepted = debouncer.accepted();

        // Re-read UI settings; re-tune / change mode live if they moved.
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

        // Snapshot of the debounced paddle for the keyer, applying dit/dah reversal.
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
            // Idle: pace the poll at ~1 ms so the thread does not busy-spin.
            let next = clock.now() + poll_step.max(1);
            HybridWaiter::wait_until_ticks(&clock, next, &should_abort);
        }
    }

    // Leave the key up on the way out.
    let tone = shared.sidetone.lock().unwrap().clone();
    drive_edge(shared, rt, tone.as_ref(), false, clock.now());
}

/// Reads the current keyer settings (wpm, weight, mode, paddle-reverse) under the lock.
fn read_keyer_settings(shared: &Arc<Shared>) -> (u32, u32, KeyerMode, bool) {
    let s = shared.settings.lock().unwrap();
    (s.wpm, s.weight, s.mode, s.paddle_reverse)
}

/// Whether `selected` names a port that is not in `present` (and is `Some`).
fn port_missing(selected: &Option<String>, present: &[String]) -> bool {
    matches!(selected, Some(name) if !present.iter().any(|p| p == name))
}

/// The label of the other Client port that already holds `port`, if any. One port serves one
/// job, so a picker refuses a port another selector is using.
fn held_by_other(port: &str, others: &[(&Option<String>, &'static str)]) -> Option<&'static str> {
    others
        .iter()
        .find(|(p, _)| p.as_deref() == Some(port))
        .map(|(_, label)| *label)
}

/// Derives the stable six-byte ESP Keyer identity from the persisted WireGuard public key.
fn esp_keyer_id(public_key: [u8; 32]) -> [u8; 6] {
    let mut keyer_id = [0u8; 6];
    keyer_id.copy_from_slice(&public_key[..6]);
    keyer_id
}

/// Validates the base64 public-key identifier returned by a desktop Player browser.
fn parse_desktop_player_id(id: &str) -> Option<[u8; 32]> {
    base64_decode(id).ok().and_then(|bytes| bytes.try_into().ok())
}

/// Validates the exact six-byte hexadecimal identifier returned by an ESP Player browser.
fn parse_esp_player_id(id: &str) -> Option<[u8; 6]> {
    let bytes = id.as_bytes();
    if bytes.len() != 12 {
        return None;
    }
    let mut player_id = [0u8; 6];
    for (index, slot) in player_id.iter_mut().enumerate() {
        *slot = (hex_nibble(bytes[index * 2])? << 4) | hex_nibble(bytes[index * 2 + 1])?;
    }
    Some(player_id)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Maps a UI mode name to a [`KeyerMode`], defaulting to Iambic B.
fn parse_mode(name: &str) -> KeyerMode {
    match name {
        "iambicA" => KeyerMode::IambicA,
        "ultimatic" => KeyerMode::Ultimatic,
        "bug" => KeyerMode::Bug,
        "straight" => KeyerMode::Straight,
        _ => KeyerMode::IambicB,
    }
}

/// The UI name for a [`KeyerMode`].
fn mode_name(mode: KeyerMode) -> &'static str {
    match mode {
        KeyerMode::IambicB => "iambicB",
        KeyerMode::IambicA => "iambicA",
        KeyerMode::Ultimatic => "ultimatic",
        KeyerMode::Bug => "bug",
        KeyerMode::Straight => "straight",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A sink that records every event for assertions.
    fn recording_sink() -> (EventSink, Arc<Mutex<Vec<ClientEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let sink: EventSink = Arc::new(move |e| captured.lock().unwrap().push(e));
        (sink, events)
    }

    /// Waits until `pred` sees the recorded events satisfied, or the timeout elapses.
    fn wait_for(
        events: &Arc<Mutex<Vec<ClientEvent>>>,
        timeout: Duration,
        pred: impl Fn(&[ClientEvent]) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if pred(&events.lock().unwrap()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        pred(&events.lock().unwrap())
    }

    #[test]
    fn peer_is_lost_only_after_the_full_timeout() {
        // The Client's peer-loss detection uses the shared reconnect policy; assert the boundary
        // against it so this app stays pinned to the same 1.5 s timeout as the Station.
        use rwk_core::engine::reconnect::{peer_silent, PEER_SILENCE_TIMEOUT};
        let heard = Instant::now();
        assert!(!peer_silent(heard, heard));
        assert!(!peer_silent(heard, heard + PEER_SILENCE_TIMEOUT - Duration::from_millis(1)));
        assert!(peer_silent(heard, heard + PEER_SILENCE_TIMEOUT));
        assert!(peer_silent(heard, heard + Duration::from_secs(30)));
    }

    #[test]
    fn start_emits_initial_settings() {
        let (sink, events) = recording_sink();
        let _controller = ClientController::start(sink);
        let ok = wait_for(&events, Duration::from_secs(1), |evs| {
            evs.iter().any(|e| {
                matches!(e, ClientEvent::Settings { wpm, mode, .. } if *wpm == 20 && mode == "iambicB")
            })
        });
        assert!(ok, "start must emit the initial keyer settings");
    }

    #[test]
    fn settings_commands_update_state() {
        let (sink, _events) = recording_sink();
        let controller = ClientController::start(sink);
        controller.set_speed(30);
        controller.set_weight(60);
        controller.set_mode("straight");
        controller.set_paddle_reverse(true);
        let s = controller.shared.settings.lock().unwrap();
        assert_eq!(s.wpm, 30);
        assert_eq!(s.weight, 60);
        assert_eq!(s.mode, KeyerMode::Straight);
        assert!(s.paddle_reverse);
    }

    #[test]
    fn send_cw_emits_key_down_then_up() {
        let (sink, events) = recording_sink();
        let controller = ClientController::start(sink);
        // "E" is a single dit: exactly one key-down then one key-up.
        controller.send_cw("E");

        let ok = wait_for(&events, Duration::from_secs(2), |evs| {
            let states: Vec<bool> = evs
                .iter()
                .filter_map(|e| match e {
                    ClientEvent::KeyState { down } => Some(*down),
                    _ => None,
                })
                .collect();
            // At least a down followed by an up, and the final state is up (safe).
            states.len() >= 2 && states.first() == Some(&true) && states.last() == Some(&false)
        });
        assert!(ok, "send_cw(\"E\") must key down then up, ending key-up");
    }

    #[test]
    fn every_keyer_setting_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("rwk-client-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("client.json");
        let _ = std::fs::remove_file(&path);

        let (sink, _events) = recording_sink();
        let first = ClientController::start_with_config(sink, Some(path.clone()));
        first.set_speed(28);
        first.set_weight(45);
        first.set_mode("iambicA");
        first.set_paddle_reverse(true);
        first.set_tone_frequency(680);
        first.set_tone_volume(25);
        first.select_audio_device(Some(String::new()));
        first.set_transport("lan");
        // Port names that do not exist still record the operator's selection.
        first.select_keying_port(Some("W1VE-TEST-KEY".into()));
        first.select_paddle_port(Some("W1VE-TEST-PADDLE".into()));
        first.select_winkeyer_port(Some("W1VE-TEST-WK".into()));
        drop(first);

        let (sink2, _events2) = recording_sink();
        let second = ClientController::start_with_config(sink2, Some(path.clone()));
        let s = second.shared.settings.lock().unwrap().clone();
        drop(second);
        let _ = std::fs::remove_file(&path);

        assert_eq!(s.wpm, 28);
        assert_eq!(s.weight, 45);
        assert_eq!(mode_name(s.mode), "iambicA");
        assert!(s.paddle_reverse);
        assert_eq!(s.tone_hz, 680);
        assert_eq!(s.tone_volume, 25);
        assert_eq!(s.audio_device.as_deref(), Some(""));
        assert_eq!(s.transport, Transport::Lan);
        assert_eq!(s.keying_port.as_deref(), Some("W1VE-TEST-KEY"));
        assert_eq!(s.paddle_port.as_deref(), Some("W1VE-TEST-PADDLE"));
        assert_eq!(s.winkeyer_port.as_deref(), Some("W1VE-TEST-WK"));
    }

    #[test]
    fn update_forward_rule_edits_in_place_and_keeps_id() {
        let (sink, _events) = recording_sink();
        let controller = ClientController::start_with_config(sink, None);
        controller.add_forward_rule("K4", "tcp", 9100, 9100, "127.0.0.1");
        let id = controller.shared.config.lock().unwrap().forwards[0].id.clone();
        controller.set_forward_rule_enabled(&id, true);
        controller.update_forward_rule(&id, "K4 remote", "udp", 9101, 9102, "10.0.0.5");
        let config = controller.shared.config.lock().unwrap();
        assert_eq!(config.forwards.len(), 1);
        let rule = &config.forwards[0];
        assert_eq!(rule.id, id);
        assert_eq!(rule.name, "K4 remote");
        assert_eq!(rule.protocol, "udp");
        assert_eq!(rule.client_port, 9101);
        assert_eq!(rule.station_port, 9102);
        assert_eq!(rule.station_target_address, "10.0.0.5");
        assert!(rule.enabled, "editing must keep the enabled state");
    }

    #[test]
    fn connect_rejects_a_bad_relay_address_without_blocking() {
        let (sink, events) = recording_sink();
        let controller = ClientController::start(sink);
        // A malformed relay must fail fast and return to disconnected, never hang.
        controller.connect("not-an-address", "code");
        let ok = wait_for(&events, Duration::from_secs(1), |evs| {
            evs.iter().any(|e| {
                matches!(e, ClientEvent::Connection { state, .. } if state == "disconnected")
            }) && evs.iter().any(|e| matches!(e, ClientEvent::Log { .. }))
        });
        assert!(ok, "a bad relay address must report disconnected + a log line");
    }

    #[test]
    fn mode_names_round_trip() {
        for name in ["iambicB", "iambicA", "ultimatic", "bug", "straight"] {
            assert_eq!(mode_name(parse_mode(name)), name);
        }
        // Unknown names fall back to Iambic B.
        assert_eq!(parse_mode("nonsense"), KeyerMode::IambicB);
    }

    #[test]
    fn lan_player_selection_ids_have_strict_boundaries() {
        assert_eq!(parse_esp_player_id("000102aBcDeF"), Some([0, 1, 2, 0xAB, 0xCD, 0xEF]));
        assert_eq!(parse_esp_player_id("000102ABCDEF0"), None);
        assert_eq!(parse_esp_player_id("000102ABCDE?"), None);

        let desktop_key = [9u8; 32];
        let encoded = base64_encode(&desktop_key);
        assert_eq!(parse_desktop_player_id(&encoded), Some(desktop_key));
        assert_eq!(parse_desktop_player_id("not-a-player-key"), None);
    }

    #[test]
    fn esp_router_access_is_cw_only_and_resets_when_inactive() {
        assert_eq!(RouterAccess::Esp32Player.status(), (false, true));
        assert!(RouterAccess::Esp32Player.block_reason().is_some());
        assert_eq!(RouterAccess::Inactive.status(), (true, false));
        assert_eq!(RouterAccess::Inactive.block_reason(), None);
        let mut public_key = [0u8; 32];
        public_key[..6].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(esp_keyer_id(public_key), [1, 2, 3, 4, 5, 6]);
    }
}
