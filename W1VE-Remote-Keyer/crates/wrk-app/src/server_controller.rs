//! The Server controller for W1VE Remote Keyer: receives keyed edges and keys the radio.
//!
//! The Player is the LAN desktop responder: it advertises its persisted station identity as an
//! RWKP v2 CW-capable Player, then waits for an operating Keyer to initiate the WireGuard tunnel.
//! WAN pairing retains its existing shared-token relay path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rwk_core::config::{Config, Role};
use rwk_core::engine::bus::{CoreEvent, EventBus};
use rwk_core::engine::edge_station::{pump_edges, tunnel_replayer};
use rwk_core::engine::keying_arbiter::{KeyingArbiter, LineObserver, LineUpdate};
use rwk_core::engine::lan_discovery::{Beacon, LanAdvertiser, CAPABILITY_CW};
use rwk_core::engine::network::EdgeTransport;
use rwk_core::engine::reconnect::{connect_with_relay_retry, PEER_SILENCE_TIMEOUT};
use rwk_core::engine::replay::{spawn_driver, EdgeJitterProfile, SerialKeyingAdapter};
use rwk_core::engine::serial::{
    enumerate_ports, natural_port_key, KeyingOutputConfig, PttTiming, SerialKeyingOutput,
};
use rwk_core::engine::session::Session;
use rwk_core::engine::tunnel;
use rwk_core::primitives::KeyingLine;
use rwk_core::timing::{Clock, MonotonicClock};

use crate::app_paths;
use crate::logger_host::{HostReport, HostReporter, LoggerHost};
use crate::pairing::{self, Transport};

const EDGE_PORT: u16 = 5555;

/// Control identifiers for inline error messages in the UI.
mod control {
    pub const KEYING: &str = "keying";
    pub const LOGGER: &str = "logger";
}

/// An event the Server controller emits for the UI.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ServerEvent {
    KeyState { down: bool },
    Ptt { asserted: bool },
    Connection { state: String, path: String },
    /// The round-trip time to the Client in whole milliseconds, once a second while paired (`None`
    /// until the first pong arrives).
    Rtt { ms: Option<u32> },
    FailSafe { code: u8, message: String },
    Log { level: String, message: String },
    Settings {
        keying_port: Option<String>,
        key_line: String,
        ptt_line: String,
        transport: String,
        relay: String,
        token: String,
        callsign: String,
        keyer_id: String,
        logger_port: Option<String>,
        ptt_hang_ms: u64,
    },
    /// Whether this desktop Player is currently advertising its LAN identity.
    Advertising { active: bool },
    /// The Player's logger input is sending CW (`true`) or idle (`false`).
    LoggerSending { active: bool },
    /// The current serial-port inventory.
    Ports { available: Vec<String> },
    /// A short, per-control error (empty `message` clears it). Full detail is also logged.
    Error { control: String, message: String },
}

/// A sink the host (Tauri) registers for [`ServerEvent`]s.
pub type EventSink = Arc<dyn Fn(ServerEvent) + Send + Sync>;

/// The PTT hang time (held after key-up) the UI offers: 75 to 600 ms, 200 ms by default.
pub const PTT_HANG_MIN_MS: u32 = 75;
pub const PTT_HANG_MAX_MS: u32 = 600;
const PTT_HANG_DEFAULT_MS: u64 = 200;

/// Clamps a requested PTT hang time to the range the UI offers.
fn clamp_ptt_hang(ms: u32) -> u32 {
    ms.clamp(PTT_HANG_MIN_MS, PTT_HANG_MAX_MS)
}

/// The PTT line to drive, or `None` when it would share the key's line. Key and PTT must be
/// on different control lines, so a duplicate is dropped rather than driven from one pin.
fn ptt_line_for(key: KeyingLine, ptt: Option<KeyingLine>) -> Option<KeyingLine> {
    ptt.filter(|p| *p != key)
}

#[derive(Debug, Clone)]
struct Settings {
    keying_port: Option<String>,
    logger_port: Option<String>,
    key_line: KeyingLine,
    ptt_line: Option<KeyingLine>,
    /// How long PTT is held after key-up, in ms (see [`PTT_HANG_MIN_MS`]).
    ptt_hang_ms: u64,
    transport: Transport,
    relay: String,
    /// The pairing token the Server generated for this run (shown so the Client can enter it).
    token: String,
    /// Display identity advertised by this desktop Player on the LAN.
    callsign: String,
    keyer_id: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            keying_port: None,
            logger_port: None,
            key_line: KeyingLine::Dtr,
            ptt_line: Some(KeyingLine::Rts),
            ptt_hang_ms: PTT_HANG_DEFAULT_MS,
            transport: Transport::Wan,
            relay: pairing::DEFAULT_WAN_RELAY.to_string(),
            token: pairing::generate_token(),
            callsign: String::new(),
            keyer_id: String::new(),
        }
    }
}

struct Shared {
    settings: Mutex<Settings>,
    config: Mutex<Config>,
    config_path: Option<std::path::PathBuf>,
    sink: EventSink,
}

impl Shared {
    fn emit(&self, event: ServerEvent) {
        (self.sink)(event);
    }

    fn log(&self, level: &str, message: impl Into<String>) {
        self.emit(ServerEvent::Log { level: level.into(), message: message.into() });
    }

    /// Logs the full detail and shows a short red message under `control`.
    fn fail(&self, control: &str, short: impl Into<String>, detail: impl Into<String>) {
        self.log("warn", detail);
        self.emit(ServerEvent::Error { control: control.into(), message: short.into() });
    }

    /// Clears any red message under `control`.
    fn clear_error(&self, control: &str) {
        self.emit(ServerEvent::Error { control: control.into(), message: String::new() });
    }

    fn emit_settings(&self) {
        let s = self.settings.lock().unwrap();
        self.emit(ServerEvent::Settings {
            keying_port: s.keying_port.clone(),
            key_line: line_name(s.key_line).into(),
            ptt_line: s.ptt_line.map(|l| line_name(l).to_string()).unwrap_or_else(|| "none".into()),
            transport: s.transport.as_str().into(),
            relay: s.relay.clone(),
            token: s.token.clone(),
            callsign: s.callsign.clone(),
            keyer_id: s.keyer_id.clone(),
            logger_port: s.logger_port.clone(),
            ptt_hang_ms: s.ptt_hang_ms,
        });
    }

    fn persist(&self) {
        let Some(path) = &self.config_path else { return };
        {
            let s = self.settings.lock().unwrap();
            let mut config = self.config.lock().unwrap();
            config.station.keying_port = s.keying_port.clone();
            config.station.key_line = Some(line_name(s.key_line).to_string());
            config.station.ptt_line =
                Some(s.ptt_line.map(|l| line_name(l).to_string()).unwrap_or_else(|| "none".into()));
            config.station.logger_port = s.logger_port.clone();
            config.station.logger_enabled = s.logger_port.is_some();
            config.station.ptt_hang_ms = Some(s.ptt_hang_ms);
            config.callsign = s.callsign.clone();
            config.keyer_id = s.keyer_id.clone();
            config.transport = Some(s.transport.as_str().to_string());
            if let Ok(address) = s.relay.parse() {
                config.relay = Some(rwk_core::config::RelayConfig { address });
            }
            config.pairing_code = Some(s.token.clone());
        }
        let config = self.config.lock().unwrap();
        if let Err(e) = config.save(path) {
            self.log("warn", format!("Could not save config: {e}"));
        }
    }
}

/// Identifies one Server listen/advertise operation. Every newer operation invalidates older
/// asynchronous pairing work before it can install replay resources.
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

/// The resources a running pairing owns, torn down together on disconnect.
///
/// The replay driver runs for the whole session and writes into `arbiter.remote_output()`.
/// When the operator changes the keying port or control lines while paired, the arbiter's
/// serial adapter is swapped in place (`apply_keying_live`) without restarting the replay
/// driver, re-anchoring the jitter buffer, or resetting the logger host.
struct ActiveSession {
    /// Operation that installed this replay stack. Its heartbeat can only tear down this exact
    /// generation, never a replacement session.
    operation: OperationId,
    session: Session,
    pump: tokio::task::JoinHandle<rwk_core::Result<()>>,
    stop_bus: Arc<AtomicBool>,
    /// Set after the session shutdown completes, so LAN re-advertising never overlaps teardown.
    finished: Arc<AtomicBool>,
    /// The logger-WinKeyer host, if running. Declared before `arbiter` so it drops first.
    logger: Option<LoggerHost>,
    /// The keying arbiter the logger and remote replay share. Swapped in place on port/line changes.
    arbiter: Arc<KeyingArbiter>,
    /// Sends a liveness heartbeat to the Client ~once a second so the Client can tell the
    /// Player is still up even when no CW is flowing. Aborted on disconnect.
    heartbeat: tokio::task::JoinHandle<()>,
}

/// Server-owned connection state guarded by the installation gate. Network setup never holds
/// this mutex across an await; it locks only to verify a generation and atomically install or
/// detach task/session resources and their visible connection state.
struct ServerConnection {
    gate: OperationGate,
    /// Set when a WAN listener or LAN responder has announced connecting. It closes the
    /// spawn-to-handle window so Stop still reports disconnected before a task is registered.
    operation_active: bool,
    /// Mirrors the last advertising event for the current operation, including the interval
    /// between spawning a responder and publishing its JoinHandle.
    advertising: bool,
    active: Option<ActiveSession>,
    /// Long-lived desktop LAN responder task, including its advertising/listening loop.
    advertise: Option<(OperationId, tokio::task::JoinHandle<()>)>,
    /// Pending WAN connection task. Tracking it lets Cancel stop a listener before it installs
    /// an `ActiveSession`.
    pending: Option<(OperationId, tokio::task::JoinHandle<()>)>,
}

impl ServerConnection {
    fn permits(&self, operation: OperationId) -> bool {
        self.gate.permits(operation)
    }

    fn permits_replay_install(&self, operation: OperationId) -> bool {
        self.permits(operation) && self.operation_active && self.active.is_none()
    }
}

impl Default for ServerConnection {
    fn default() -> Self {
        Self {
            gate: OperationGate::default(),
            operation_active: false,
            advertising: false,
            active: None,
            advertise: None,
            pending: None,
        }
    }
}

/// Resources detached from the installation gate for cleanup. They are deliberately owned by
/// value, preventing an older operation from touching any replacement stack.
struct RetiredServerConnection {
    active: Option<ActiveSession>,
    advertise: Option<(OperationId, tokio::task::JoinHandle<()>)>,
    pending: Option<(OperationId, tokio::task::JoinHandle<()>)>,
}

/// Aborts a child task if its owning LAN responder is cancelled at any await point. A Tokio
/// `JoinHandle` otherwise detaches on drop, which would let an old beacon continue advertising.
struct AbortOnDrop<T> {
    handle: Option<tokio::task::JoinHandle<T>>,
}

impl<T> AbortOnDrop<T> {
    fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self { handle: Some(handle) }
    }

    fn abort(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.abort();
    }
}

/// How often the Player beacons a liveness heartbeat to the Client. It must stay well inside the
/// F1 window (750 ms), or a key held longer than that with no edges is forced up.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
/// The RTT readout refreshes once a second: every this many heartbeat ticks.
const RTT_EVERY_TICKS: u32 = 4;

/// The Server controller.
pub struct ServerController {
    shared: Arc<Shared>,
    runtime: tokio::runtime::Runtime,
    /// Generation plus task/replay slots. This is the sole installation gate for WAN and LAN.
    connection: Arc<Mutex<ServerConnection>>,
}

impl ServerController {
    pub fn start(sink: EventSink) -> Self {
        let path = app_paths::config_path(Role::Station);
        Self::start_with_config(sink, Some(path))
    }

    pub fn start_with_config(sink: EventSink, config_path: Option<std::path::PathBuf>) -> Self {
        let config = match &config_path {
            Some(p) if p.exists() => {
                Config::load(p).unwrap_or_else(|_| Config::new(Role::Station).expect("identity"))
            }
            _ => {
                let config = Config::new(Role::Station).expect("generate identity");
                if let Some(p) = &config_path {
                    let _ = config.save(p);
                }
                config
            }
        };
        let key_line = config
            .station
            .key_line
            .as_deref()
            .and_then(parse_line)
            .unwrap_or(KeyingLine::Dtr);
        let ptt_line = match config.station.ptt_line.as_deref() {
            Some(s) => ptt_line_for(key_line, parse_line(s)),
            None => ptt_line_for(key_line, Some(KeyingLine::Rts)),
        };
        let settings = Settings {
            keying_port: config.station.keying_port.clone(),
            logger_port: config.station.logger_port.clone(),
            key_line,
            ptt_line,
            ptt_hang_ms: config
                .station
                .ptt_hang_ms
                .unwrap_or(PTT_HANG_DEFAULT_MS)
                .clamp(u64::from(PTT_HANG_MIN_MS), u64::from(PTT_HANG_MAX_MS)),
            callsign: config.callsign.clone(),
            keyer_id: config.keyer_id.clone(),
            transport: config.transport.as_deref().map(Transport::parse).unwrap_or(Transport::Wan),
            relay: config
                .relay
                .as_ref()
                .map(|r| r.address.to_string())
                .unwrap_or_else(|| pairing::DEFAULT_WAN_RELAY.to_string()),
            // Keep the WAN token across launches so the operator's Keyer can reuse it; a fresh
            // one is generated only for a new install or when the operator regenerates it.
            token: config
                .pairing_code
                .clone()
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(pairing::generate_token),
        };
        let shared = Arc::new(Shared {
            settings: Mutex::new(settings),
            config: Mutex::new(config),
            config_path,
            sink,
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let controller = Self {
            shared,
            runtime,
            connection: Arc::new(Mutex::new(ServerConnection::default())),
        };
        // Save once at startup so a token generated for a fresh config is stable from now on.
        controller.shared.persist();
        controller.shared.emit_settings();
        controller.emit_ports();
        controller
    }

    // ─── Config ────────────────────────────────────────────────────────────────

    #[must_use]
    pub fn serial_ports() -> Vec<String> {
        enumerate_ports()
            .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
            .unwrap_or_default()
    }

    /// Re-emits settings and the port list. The UI calls this once it has subscribed to the
    /// event channel, because the startup emit races the webview and is otherwise lost.
    pub fn request_state(&self) {
        self.shared.emit_settings();
        self.emit_ports();
    }

    fn emit_ports(&self) {
        self.shared.emit(ServerEvent::Ports { available: Self::serial_ports() });
    }

    /// Whether the Server currently holds the keying port open (i.e. is paired). The keying
    /// output is opened at pairing in [`bring_up_replay`], so a selection is only "held" while
    /// an active session exists.
    fn is_paired(&self) -> bool {
        self.connection.lock().unwrap().active.is_some()
    }

    /// Re-enumerates serial ports and reconciles the keying selection. While **not** paired, a
    /// vanished keying port drops to `None`. While **paired** the port is held open by the
    /// replay path and must not be disturbed, so it is folded back into the list even if the
    /// OS hides the busy port from enumeration. Always emits the fresh port list.
    pub fn reconcile_ports(&self) {
        let paired = self.is_paired();
        let mut present = Self::serial_ports();
        if paired {
            let held = {
                let s = self.shared.settings.lock().unwrap();
                [s.keying_port.clone(), s.logger_port.clone()]
            };
            for p in held.into_iter().flatten() {
                if !present.contains(&p) {
                    present.push(p);
                }
            }
        }
        present.sort_by(|a, b| natural_port_key(a).cmp(&natural_port_key(b)));
        self.shared.emit(ServerEvent::Ports { available: present.clone() });

        if paired {
            return;
        }
        let (keying_gone, logger_gone) = {
            let s = self.shared.settings.lock().unwrap();
            let gone = |port: &Option<String>| {
                matches!(port, Some(name) if !present.iter().any(|p| p == name))
            };
            (gone(&s.keying_port), gone(&s.logger_port))
        };
        if keying_gone {
            let name = self.shared.settings.lock().unwrap().keying_port.take();
            if let Some(n) = name {
                self.shared.log("warn", format!("Keying port {n} removed; set to (None)."));
            }
            self.shared.clear_error(control::KEYING);
        }
        if logger_gone {
            let name = self.shared.settings.lock().unwrap().logger_port.take();
            if let Some(n) = name {
                self.shared.log("warn", format!("Logger port {n} removed; set to (None)."));
            }
            self.shared.clear_error(control::LOGGER);
        }
        if keying_gone || logger_gone {
            self.shared.persist();
            self.shared.emit_settings();
        }
    }

    /// Reopens the keying output on the running session in place through the [`KeyingArbiter`],
    /// so a changed port or control line takes effect immediately without restarting the replay
    /// driver, re-anchoring the jitter buffer, or disturbing the logger host.
    fn apply_keying_live(&self) {
        let arbiter = match self.connection.lock().unwrap().active.as_ref() {
            Some(active) => Arc::clone(&active.arbiter),
            None => return,
        };
        let (port, key_line, ptt_line) = {
            let s = self.shared.settings.lock().unwrap();
            (s.keying_port.clone(), s.key_line, s.ptt_line)
        };
        let ptt_line = ptt_line_for(key_line, ptt_line);
        let shared = Arc::clone(&self.shared);
        arbiter.replace_adapter_with(move || keying_adapter(&shared, port, key_line, ptt_line));
    }

    /// Restarts the logger input on the running session for the current logger port.
    /// The old host is stopped first, so its port is free before the new one opens.
    fn apply_logger_live(&self) {
        let (operation, arbiter, old) = {
            let mut connection = self.connection.lock().unwrap();
            let Some(active) = connection.active.as_mut() else {
                return;
            };
            (active.operation, Arc::clone(&active.arbiter), active.logger.take())
        };
        if let Some(mut host) = old {
            host.stop();
        }
        let port = self.shared.settings.lock().unwrap().logger_port.clone();
        let mut logger = open_logger(&self.shared, &arbiter, port);
        let mut connection = self.connection.lock().unwrap();
        let current = connection.permits(operation)
            && connection
                .active
                .as_ref()
                .is_some_and(|active| active.operation == operation);
        if current {
            connection.active.as_mut().expect("checked active session").logger = logger.take();
        }
        drop(connection);
        // Stop a logger opened after the active pairing was replaced instead of attaching it to
        // the replacement's replay stack.
        if let Some(mut logger) = logger {
            logger.stop();
        }
    }

    pub fn select_keying_port(&self, port: Option<String>) {
        let logger = self.shared.settings.lock().unwrap().logger_port.clone();
        if let (Some(p), Some(other)) = (&port, &logger) {
            if p == other {
                self.shared.fail(
                    control::KEYING,
                    format!("{p} is the logger port"),
                    format!("{p} is already the logger port; a port can serve one job."),
                );
                return;
            }
        }
        self.shared.settings.lock().unwrap().keying_port = port.clone();
        self.shared.clear_error(control::KEYING);
        match &port {
            Some(name) => self.shared.log("info", format!("Keying output set to {name}.")),
            None => self.shared.log("info", "Keying output cleared."),
        }
        self.shared.persist();
        self.emit_ports();
        self.shared.emit_settings();
        // Apply the change live if we are paired, so fixing a bad port (or clearing it) takes
        // effect immediately without the operator unpairing and re-pairing.
        if self.is_paired() {
            self.apply_keying_live();
        } else if let Some(name) = &port {
            probe_keying_port(&self.shared, name);
        }
    }

    /// Selects the serial port the Player's logger (a WinKeyer host) talks on. Exclusive with the
    /// keying port: one port serves one job. Applied live while paired, like the keying port.
    pub fn select_logger_port(&self, port: Option<String>) {
        let keying = self.shared.settings.lock().unwrap().keying_port.clone();
        if let (Some(p), Some(other)) = (&port, &keying) {
            if p == other {
                self.shared.fail(
                    control::LOGGER,
                    format!("{p} is the keying port"),
                    format!("{p} is already the keying port; a port can serve one job."),
                );
                return;
            }
        }
        self.shared.settings.lock().unwrap().logger_port = port.clone();
        self.shared.clear_error(control::LOGGER);
        match &port {
            Some(name) => self.shared.log("info", format!("Logger input port set to {name}.")),
            None => self.shared.log("info", "Logger input port cleared."),
        }
        self.shared.persist();
        self.emit_ports();
        self.shared.emit_settings();
        if self.is_paired() {
            self.apply_logger_live();
        } else if let Some(name) = &port {
            probe_logger_port(&self.shared, name);
        }
    }

    pub fn set_key_line(&self, line: &str) {
        let key = parse_line(line).unwrap_or(KeyingLine::Dtr);
        let ptt = self.shared.settings.lock().unwrap().ptt_line;
        if ptt == Some(key) {
            self.shared.fail(
                control::KEYING,
                format!("{} is the PTT line", line_name(key).to_uppercase()),
                format!("Key and PTT must be on different lines; {} is already the PTT line.", line_name(key).to_uppercase()),
            );
            return;
        }
        self.shared.settings.lock().unwrap().key_line = key;
        self.shared.clear_error(control::KEYING);
        self.shared.persist();
        self.shared.emit_settings();
        if self.is_paired() {
            self.apply_keying_live();
        }
    }

    pub fn set_ptt_line(&self, line: &str) {
        let ptt = parse_line(line);
        let key = self.shared.settings.lock().unwrap().key_line;
        if ptt == Some(key) {
            self.shared.fail(
                control::KEYING,
                format!("{} is the key line", line_name(key).to_uppercase()),
                format!("Key and PTT must be on different lines; {} is already the key line.", line_name(key).to_uppercase()),
            );
            return;
        }
        self.shared.settings.lock().unwrap().ptt_line = ptt;
        self.shared.clear_error(control::KEYING);
        self.shared.persist();
        self.shared.emit_settings();
        if self.is_paired() {
            self.apply_keying_live();
        }
    }

    /// Sets the PTT hang time, clamped to [`PTT_HANG_MIN_MS`]..=[`PTT_HANG_MAX_MS`]. The value is
    /// read when the replay driver is created, so it takes effect on the next connect. Rebuilding
    /// the live driver on every slider move is what broke keying, so it is not done here.
    pub fn set_ptt_hang(&self, ms: u32) {
        let ms = clamp_ptt_hang(ms);
        self.shared.settings.lock().unwrap().ptt_hang_ms = u64::from(ms);
        self.shared.persist();
        self.shared.emit_settings();
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
        }
        self.shared.emit_settings();
    }

    pub fn set_relay(&self, relay: &str) {
        self.shared.settings.lock().unwrap().relay = relay.to_string();
        self.shared.persist();
        self.shared.emit_settings();
    }

    /// Sets the desktop Player callsign advertised as its LAN display identity.
    pub fn set_callsign(&self, callsign: &str) {
        self.shared.settings.lock().unwrap().callsign = callsign.to_string();
        self.shared.persist();
        self.shared.emit_settings();
    }

    /// Sets the optional desktop Player label (for example, `20m`) advertised on the LAN.
    pub fn set_keyer_id(&self, keyer_id: &str) {
        self.shared.settings.lock().unwrap().keyer_id = keyer_id.to_string();
        self.shared.persist();
        self.shared.emit_settings();
    }

    /// Regenerates the pairing token (a fresh code for a new pairing).
    pub fn regenerate_token(&self) {
        self.shared.settings.lock().unwrap().token = pairing::generate_token();
        self.shared.persist();
        self.shared.emit_settings();
    }

    // ─── Pairing ──────────────────────────────────────────────────────────────

    /// Begins WAN listening: the Server waits for the Client's shared-token relay pairing, then
    /// replays received edges onto the radio.
    pub fn listen(&self) {
        let (transport, relay, token, keying_port, logger_port, key_line, ptt_line) = {
            let s = self.shared.settings.lock().unwrap();
            (
                s.transport,
                s.relay.clone(),
                s.token.clone(),
                s.keying_port.clone(),
                s.logger_port.clone(),
                s.key_line,
                s.ptt_line,
            )
        };
        // A keying port is NOT required to start: with none selected the Server still runs and
        // shows keying on the KEY LED (nothing is wired to the radio). The port is opened
        // best-effort in `bring_up_replay`.
        self.shared.clear_error(control::KEYING);
        let operation = self.replace_operation();

        let shared = Arc::clone(&self.shared);
        if !announce_server_connecting(
            &shared,
            &self.connection,
            operation,
            false,
            "Waiting for the Client to pair…",
        ) {
            return;
        }
        let task_shared = Arc::clone(&shared);
        let connection = Arc::clone(&self.connection);
        let handle = self.runtime.spawn(async move {
            listen_wan(
                task_shared,
                connection,
                operation,
                transport,
                relay,
                token,
                keying_port,
                logger_port,
                key_line,
                ptt_line,
            )
            .await;
        });
        install_server_pending(&self.connection, operation, handle);
    }

    // ─── LAN advertising (radio-site side) ────────────────────────────────────

    /// Starts or stops the desktop Player's relay-free LAN responder. The Player advertises a
    /// CW-capable RWKP v2 beacon and waits for the operating Keyer to initiate the tunnel.
    pub fn lan_advertise(&self, on: bool) {
        if !on {
            self.disconnect();
            return;
        }
        let (transport, callsign, keyer_id) = {
            let settings = self.shared.settings.lock().unwrap();
            (settings.transport, settings.callsign.clone(), settings.keyer_id.clone())
        };
        if transport != Transport::Lan {
            self.shared.log("warn", "Select LAN before advertising this desktop Player.");
            return;
        }
        if callsign.trim().is_empty() && keyer_id.trim().is_empty() {
            self.shared.log("warn", "Enter a Player callsign or Player ID before advertising.");
            return;
        }

        let operation = self.replace_operation();
        let display_name = player_display_name(&callsign, &keyer_id);
        let shared = Arc::clone(&self.shared);
        if !announce_server_connecting(
            &shared,
            &self.connection,
            operation,
            true,
            format!(
                "Advertising desktop Player {display_name} on the LAN; waiting for an operating Keyer…"
            ),
        ) {
            return;
        }
        let task_shared = Arc::clone(&shared);
        let connection = Arc::clone(&self.connection);
        let handle = self.runtime.spawn(async move {
            lan_advertise_and_await(task_shared, connection, operation, callsign, keyer_id).await;
        });
        install_server_advertise(&self.connection, operation, handle);
    }

    pub fn disconnect(&self) {
        let _ = self.replace_operation();
    }

    /// Invalidates all pending setup before any task is aborted or session shutdown is awaited.
    /// A task that wakes after this point can only discard resources it created privately.
    fn replace_operation(&self) -> OperationId {
        let (operation, retired) = replace_server_operation(&self.shared, &self.connection);
        self.runtime.block_on(shutdown_retired_server_connection(retired));
        operation
    }
}

impl Drop for ServerController {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Invalidates the old generation and detaches every externally reachable resource while holding
/// the same mutex used for replay installation. Shutdown and task abort happen only after the
/// state becomes unreachable, so old async work cannot affect a replacement operation.
fn replace_server_operation(
    shared: &Arc<Shared>,
    connection: &Arc<Mutex<ServerConnection>>,
) -> (OperationId, RetiredServerConnection) {
    let mut state = connection.lock().unwrap();
    let operation = state.gate.replace();
    let had_operation = state.operation_active
        || state.active.is_some()
        || state.advertise.is_some()
        || state.pending.is_some();
    let was_advertising = state.advertising;
    state.operation_active = false;
    state.advertising = false;
    let retired = RetiredServerConnection {
        active: state.active.take(),
        advertise: state.advertise.take(),
        pending: state.pending.take(),
    };
    if was_advertising {
        shared.emit(ServerEvent::Advertising { active: false });
    }
    if had_operation {
        // This happens while the installation gate is held. A stale pairing cannot emit a later
        // connected/replay state after Stop has visibly ended this operation.
        emit_server_stopped_state(shared);
    }
    (operation, retired)
}

async fn shutdown_retired_server_connection(mut retired: RetiredServerConnection) {
    if let Some((_operation, task)) = retired.pending.take() {
        task.abort();
    }
    if let Some((_operation, task)) = retired.advertise.take() {
        task.abort();
    }
    if let Some(active) = retired.active.take() {
        shutdown_active_session(active, true).await;
    }
}

/// Stops all replay-side resources owned by a detached active session. `abort_heartbeat` is
/// false only when the heartbeat itself is doing the liveness teardown.
async fn shutdown_active_session(mut active: ActiveSession, abort_heartbeat: bool) {
    active.stop_bus.store(true, Ordering::SeqCst);
    if let Some(mut logger) = active.logger.take() {
        logger.stop();
    }
    if abort_heartbeat {
        active.heartbeat.abort();
    }
    active.pump.abort();
    active.session.shutdown().await;
    active.finished.store(true, Ordering::SeqCst);
}

fn emit_server_stopped_state(shared: &Arc<Shared>) {
    shared.emit(ServerEvent::KeyState { down: false });
    shared.emit(ServerEvent::Ptt { asserted: false });
    shared.emit(ServerEvent::LoggerSending { active: false });
    shared.emit(ServerEvent::Rtt { ms: None });
    shared.emit(ServerEvent::Connection { state: "disconnected".into(), path: "none".into() });
    shared.log("info", "Stopped.");
}

/// Announces a current operation and records its visible state under the installation gate.
/// Recording this before spawning closes Stop's spawn-to-handle registration window.
fn announce_server_connecting(
    shared: &Arc<Shared>,
    connection: &Arc<Mutex<ServerConnection>>,
    operation: OperationId,
    advertising: bool,
    message: impl Into<String>,
) -> bool {
    let mut state = connection.lock().unwrap();
    if !state.permits(operation) {
        return false;
    }
    state.operation_active = true;
    state.advertising = advertising;
    shared.emit(ServerEvent::Connection { state: "connecting".into(), path: "none".into() });
    if advertising {
        shared.emit(ServerEvent::Advertising { active: true });
    }
    shared.log("info", message);
    true
}

fn install_server_pending(
    connection: &Arc<Mutex<ServerConnection>>,
    operation: OperationId,
    handle: tokio::task::JoinHandle<()>,
) {
    let mut state = connection.lock().unwrap();
    if state.permits(operation) && state.operation_active {
        if let Some((_previous_operation, previous)) = state.pending.replace((operation, handle)) {
            previous.abort();
        }
    } else {
        handle.abort();
    }
}

fn install_server_advertise(
    connection: &Arc<Mutex<ServerConnection>>,
    operation: OperationId,
    handle: tokio::task::JoinHandle<()>,
) {
    let mut state = connection.lock().unwrap();
    if state.permits(operation) && state.operation_active {
        if let Some((_previous_operation, previous)) = state.advertise.replace((operation, handle)) {
            previous.abort();
        }
    } else {
        handle.abort();
    }
}

fn server_operation_is_current(
    connection: &Arc<Mutex<ServerConnection>>,
    operation: OperationId,
) -> bool {
    connection.lock().unwrap().permits(operation)
}

fn emit_server_advertising_if_current(
    shared: &Arc<Shared>,
    connection: &Arc<Mutex<ServerConnection>>,
    operation: OperationId,
    active: bool,
) -> bool {
    let mut state = connection.lock().unwrap();
    if !state.permits(operation) {
        return false;
    }
    state.advertising = active;
    shared.emit(ServerEvent::Advertising { active });
    true
}

fn emit_server_rtt_if_current(
    shared: &Arc<Shared>,
    connection: &Arc<Mutex<ServerConnection>>,
    operation: OperationId,
    ms: Option<u32>,
) -> bool {
    let state = connection.lock().unwrap();
    if !state.permits(operation) {
        return false;
    }
    shared.emit(ServerEvent::Rtt { ms });
    true
}

fn log_server_if_current(
    shared: &Arc<Shared>,
    connection: &Arc<Mutex<ServerConnection>>,
    operation: OperationId,
    level: &str,
    message: impl Into<String>,
) -> bool {
    let state = connection.lock().unwrap();
    if !state.permits(operation) {
        return false;
    }
    shared.log(level, message);
    true
}

fn fail_server_operation(
    shared: &Arc<Shared>,
    connection: &Arc<Mutex<ServerConnection>>,
    operation: OperationId,
    message: impl Into<String>,
) {
    let state = connection.lock().unwrap();
    if !state.permits(operation) {
        return;
    }
    shared.log("warn", message);
    shared.emit(ServerEvent::Connection { state: "disconnected".into(), path: "none".into() });
}

/// Runs one WAN listener operation without changing relay or token semantics. The operation
/// gate is checked before and after the relay await, then again by replay installation.
#[allow(clippy::too_many_arguments)]
async fn listen_wan(
    shared: Arc<Shared>,
    connection: Arc<Mutex<ServerConnection>>,
    operation: OperationId,
    transport: Transport,
    relay: String,
    token: String,
    keying_port: Option<String>,
    logger_port: Option<String>,
    key_line: KeyingLine,
    ptt_line: Option<KeyingLine>,
) {
    if !server_operation_is_current(&connection, operation) {
        return;
    }
    let base = shared.config.lock().unwrap().clone();
    let config = match pairing::build_config(&base, Role::Station, transport, &token, &relay) {
        Ok(config) => config,
        Err(error) => {
            fail_server_operation(&shared, &connection, operation, error);
            return;
        }
    };
    if !server_operation_is_current(&connection, operation) {
        return;
    }

    // Retry the relay rendezvous once (750 ms) via the shared helper, so a transient relay race
    // just after a Client restart does not fail the listen immediately — the same resilience the
    // Client's connect path has.
    let connected = connect_with_relay_retry(&config).await;
    if !server_operation_is_current(&connection, operation) {
        // A completed `Connected` is dropped before it is turned into a Session.
        return;
    }
    let connected = match connected {
        Ok(connected) => connected,
        Err(error) => {
            fail_server_operation(&shared, &connection, operation, format!("link failed: {error}"));
            return;
        }
    };
    let _ = bring_up_replay(
        &shared,
        &connection,
        operation,
        connected,
        &config,
        keying_port,
        logger_port,
        key_line,
        ptt_line,
    )
    .await;
}

/// Persistent desktop Player LAN responder. Each cycle binds a fresh WireGuard listener,
/// broadcasts its RWKP v2 CW-capability beacon, waits for the Keyer's initiator handshake, and
/// resumes advertising after the selected Keyer session ends.
async fn lan_advertise_and_await(
    shared: Arc<Shared>,
    connection: Arc<Mutex<ServerConnection>>,
    operation: OperationId,
    callsign: String,
    keyer_id: String,
) {
    if !server_operation_is_current(&connection, operation) {
        return;
    }
    let identity = match shared.config.lock().unwrap().identity.to_identity() {
        Ok(identity) => identity,
        Err(error) => {
            let _ = emit_server_advertising_if_current(&shared, &connection, operation, false);
            fail_server_operation(&shared, &connection, operation, format!("identity error: {error}"));
            return;
        }
    };
    let public_key = identity.public_bytes();
    let display_name = player_display_name(&callsign, &keyer_id);

    loop {
        if !server_operation_is_current(&connection, operation) {
            return;
        }
        let bound = tunnel::lan_bind().await;
        if !server_operation_is_current(&connection, operation) {
            // The newly bound socket is dropped before it can advertise or accept a tunnel.
            return;
        }
        let (socket, wg_port) = match bound {
            Ok(bound) => bound,
            Err(error) => {
                let _ = emit_server_advertising_if_current(&shared, &connection, operation, false);
                fail_server_operation(&shared, &connection, operation, format!("LAN bind failed: {error}"));
                return;
            }
        };
        let beacon = Beacon {
            callsign: callsign.clone(),
            keyer_id: keyer_id.clone(),
            public_key,
            wg_port,
            capabilities: CAPABILITY_CW,
        };
        let advertiser = match LanAdvertiser::new(beacon) {
            Ok(advertiser) => advertiser,
            Err(error) => {
                let _ = emit_server_advertising_if_current(&shared, &connection, operation, false);
                fail_server_operation(&shared, &connection, operation, format!("LAN advertise failed: {error}"));
                return;
            }
        };
        if !server_operation_is_current(&connection, operation) {
            return;
        }
        // The wrapper aborts the child if this responder is cancelled while awaiting the
        // initiator. A parent JoinHandle dropping would otherwise detach the beacon task.
        let mut beacon_task = AbortOnDrop::new(tokio::spawn(advertiser.run()));
        if !emit_server_advertising_if_current(&shared, &connection, operation, true) {
            return;
        }

        // This Player owns the bound socket and stays the WireGuard responder. The operating
        // Keyer resolves the beacon, validates it locally, and calls `lan_connect_to`.
        let connected = tunnel::lan_await_initiator(socket, &identity).await;
        beacon_task.abort();
        if !emit_server_advertising_if_current(&shared, &connection, operation, false) {
            return;
        }

        match connected {
            Ok(connected) => {
                if !server_operation_is_current(&connection, operation) {
                    return;
                }
                let config = shared.config.lock().unwrap().clone();
                let (keying_port, logger_port, key_line, ptt_line) = {
                    let settings = shared.settings.lock().unwrap();
                    (
                        settings.keying_port.clone(),
                        settings.logger_port.clone(),
                        settings.key_line,
                        settings.ptt_line,
                    )
                };
                if !server_operation_is_current(&connection, operation) {
                    return;
                }
                if let Some(finished) = bring_up_replay(
                    &shared,
                    &connection,
                    operation,
                    connected,
                    &config,
                    keying_port,
                    logger_port,
                    key_line,
                    ptt_line,
                )
                .await
                {
                    wait_for_active_session_end(&connection, operation, &finished).await;
                }
                if !announce_server_connecting(
                    &shared,
                    &connection,
                    operation,
                    false,
                    format!(
                        "Operating Keyer disconnected; advertising desktop Player {display_name} again."
                    ),
                ) {
                    return;
                }
            }
            Err(error) => {
                if !log_server_if_current(
                    &shared,
                    &connection,
                    operation,
                    "warn",
                    format!("LAN responder cycle ended: {error}; advertising again."),
                ) {
                    return;
                }
            }
        }
    }
}

/// Waits only for the active session owned by this LAN operation. A replacement generation
/// returns immediately rather than waiting on or observing a newer replay stack.
async fn wait_for_active_session_end(
    connection: &Arc<Mutex<ServerConnection>>,
    operation: OperationId,
    finished: &Arc<AtomicBool>,
) {
    loop {
        let still_waiting = {
            let state = connection.lock().unwrap();
            if !state.permits(operation) {
                return;
            }
            state
                .active
                .as_ref()
                .is_some_and(|active| active.operation == operation)
                || !finished.load(Ordering::SeqCst)
        };
        if !still_waiting {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The Client stopped sending: end this operation's pairing as `disconnect` does, then report
/// disconnected. The operation and active-session identity checks prevent an old heartbeat from
/// taking a newer session after a replacement listener or LAN responder has started.
async fn drop_lost_peer(
    shared: &Arc<Shared>,
    connection: &Arc<Mutex<ServerConnection>>,
    operation: OperationId,
) {
    let active = {
        let mut state = connection.lock().unwrap();
        if !state.permits(operation)
            || !state
                .active
                .as_ref()
                .is_some_and(|active| active.operation == operation)
        {
            return;
        }
        let active = state.active.take().expect("checked active session");
        // Report teardown while holding the same gate as replay installation, so an old
        // heartbeat cannot emit disconnected for a newer operation.
        shared.emit(ServerEvent::KeyState { down: false });
        shared.emit(ServerEvent::Ptt { asserted: false });
        shared.emit(ServerEvent::LoggerSending { active: false });
        shared.emit(ServerEvent::Rtt { ms: None });
        shared.emit(ServerEvent::Connection { state: "disconnected".into(), path: "none".into() });
        shared.log("warn", "The Client stopped responding; disconnected.");
        active
    };
    // The heartbeat task is the caller and returns right after this, so it is not aborted here.
    shutdown_active_session(active, false).await;
}

/// The edge's round-trip time in whole milliseconds, or [`None`] before the first pong.
fn rtt_millis(edge: &EdgeTransport) -> Option<u32> {
    edge.rtt().map(|d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX))
}

/// Starts the Server liveness task for one installed replay stack. Each awaited edge operation
/// is bracketed by a generation check, and `drop_lost_peer` additionally requires the matching
/// active-session operation before it can remove anything.
fn spawn_server_heartbeat(
    shared: Arc<Shared>,
    connection: Arc<Mutex<ServerConnection>>,
    operation: OperationId,
    edge: Arc<EdgeTransport>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
        let mut beats: u32 = 0;
        loop {
            tick.tick().await;
            if !server_operation_is_current(&connection, operation) {
                return;
            }
            // Silence watchdog: the Client's heartbeats and pings arrive on the same edge, so a
            // long gap means it has closed or lost its link. Tear this pairing down now.
            if edge.peer_silent_for() >= PEER_SILENCE_TIMEOUT {
                drop_lost_peer(&shared, &connection, operation).await;
                return;
            }
            if edge.send_heartbeat().await.is_err() {
                // A failed heartbeat is already folded into PathHealth/F9; keep beaconing.
            }
            if !server_operation_is_current(&connection, operation) {
                return;
            }
            let _ = edge.send_ping().await;
            if !server_operation_is_current(&connection, operation) {
                return;
            }
            beats = beats.wrapping_add(1);
            if beats % RTT_EVERY_TICKS == 0
                && !emit_server_rtt_if_current(
                    &shared,
                    &connection,
                    operation,
                    rtt_millis(&edge),
                )
            {
                return;
            }
        }
    })
}

/// Shared post-connect setup for WAN (`listen`) and desktop LAN responder (`lan_advertise`):
/// open the keying edge over the tunnel and atomically install the replay stack while this
/// operation remains current. A stale Session is shut down without ever starting a driver,
/// edge pump, heartbeat, logger, or active-slot publication.
#[allow(clippy::too_many_arguments)]
async fn bring_up_replay(
    shared: &Arc<Shared>,
    connection: &Arc<Mutex<ServerConnection>>,
    operation: OperationId,
    connected: tunnel::Connected,
    config: &Config,
    keying_port: Option<String>,
    logger_port: Option<String>,
    key_line: KeyingLine,
    ptt_line: Option<KeyingLine>,
) -> Option<Arc<AtomicBool>> {
    if !server_operation_is_current(connection, operation) {
        return None;
    }
    let path = format!("{:?}", connected.path).to_lowercase();
    let mut session = match Session::start(connected, config) {
        Ok(session) => session,
        Err(error) => {
            fail_server_operation(shared, connection, operation, format!("session failed: {error}"));
            return None;
        }
    };
    if !server_operation_is_current(connection, operation) {
        session.shutdown().await;
        return None;
    }
    let net = session.handle();
    let conn = match net.open_udp(EDGE_PORT).await {
        Ok(conn) => conn,
        Err(error) => {
            fail_server_operation(shared, connection, operation, format!("edge open failed: {error}"));
            session.shutdown().await;
            return None;
        }
    };
    if !server_operation_is_current(connection, operation) {
        session.shutdown().await;
        return None;
    }
    let edge = Arc::new(EdgeTransport::over_tunnel(conn));
    let peer = std::net::SocketAddr::new(
        std::net::IpAddr::V4(Role::Station.peer_inner_address()),
        EDGE_PORT,
    );
    edge.set_peer(peer).await;
    if !server_operation_is_current(connection, operation) {
        session.shutdown().await;
        return None;
    }

    let message = format!("Paired with the keyer over the {path} path.");
    let mut session_to_install = Some(session);
    let finished = {
        // Resource construction is synchronous, so holding this short gate through driver,
        // pump, heartbeat, and active-slot creation makes Stop/replacement either win before
        // any replay resources exist or take the complete stack immediately afterwards.
        let mut state = connection.lock().unwrap();
        if !state.permits_replay_install(operation) {
            None
        } else {
            let clock: Arc<dyn Clock> = Arc::new(MonotonicClock);
            let (pump, stop_bus, logger, arbiter) = start_replay_driver(
                shared,
                &edge,
                &clock,
                keying_port,
                key_line,
                ptt_line,
                logger_port,
            );
            let finished = Arc::new(AtomicBool::new(false));
            let heartbeat = spawn_server_heartbeat(
                Arc::clone(shared),
                Arc::clone(connection),
                operation,
                Arc::clone(&edge),
            );
            state.active = Some(ActiveSession {
                operation,
                session: session_to_install.take().expect("session not yet installed"),
                pump,
                stop_bus,
                finished: Arc::clone(&finished),
                logger,
                arbiter,
                heartbeat,
            });
            shared.emit(ServerEvent::Connection { state: "connected".into(), path: path.clone() });
            shared.log("info", message);
            Some(finished)
        }
    };
    if finished.is_none() {
        if let Some(mut session) = session_to_install.take() {
            session.shutdown().await;
        }
    }
    finished
}

/// The keying output for `port` with the given lines: a serial adapter when a port is chosen and
/// opens, otherwise LED-only (the KEY/PTT indicators still follow). A port that will not open is
/// reported on the keying control rather than failing the pairing.
fn keying_adapter(
    shared: &Arc<Shared>,
    port: Option<String>,
    key_line: KeyingLine,
    ptt_line: Option<KeyingLine>,
) -> SerialKeyingAdapter {
    let Some(port_name) = port else {
        shared.clear_error(control::KEYING);
        shared.log("info", "No keying port selected — keying the LED only.");
        return SerialKeyingAdapter::led_only();
    };
    let cfg = KeyingOutputConfig {
        port_name: port_name.clone(),
        key_line,
        ptt_line: ptt_line.unwrap_or(KeyingLine::None),
        ..Default::default()
    };
    match SerialKeyingOutput::open(cfg) {
        Ok(out) => {
            shared.clear_error(control::KEYING);
            shared.log("info", format!("Keying output open on {port_name}."));
            SerialKeyingAdapter::new(out)
        }
        Err(e) => {
            shared.fail(
                control::KEYING,
                format!("{port_name} not available"),
                format!("keying port {port_name} failed: {e}. Keying the LED only; pick an available port to drive the radio."),
            );
            SerialKeyingAdapter::led_only()
        }
    }
}

/// Opens `port` once to confirm it exists and is free, then closes it. A failure is reported on
/// the keying control. Pairing opens the port for real, so this only checks the selection early.
fn probe_keying_port(shared: &Arc<Shared>, port: &str) {
    let cfg = KeyingOutputConfig { port_name: port.to_string(), ..Default::default() };
    match SerialKeyingOutput::open(cfg) {
        Ok(_out) => shared.clear_error(control::KEYING),
        Err(e) => shared.fail(
            control::KEYING,
            format!("{port} not available"),
            format!("keying port {port} failed: {e}. Pick an available port to drive the radio."),
        ),
    }
}

/// Opens the logger port once to confirm it exists and is free, then closes it. A failure is
/// reported on the logger control. The logger host opens the port for real at pairing.
fn probe_logger_port(shared: &Arc<Shared>, port: &str) {
    match serialport::new(port, 1200).timeout(std::time::Duration::from_millis(5)).open() {
        Ok(_handle) => shared.clear_error(control::LOGGER),
        Err(e) => shared.fail(
            control::LOGGER,
            format!("{port} not available"),
            format!("logger port {port} failed: {e}. Pick an available port for the logger input."),
        ),
    }
}

/// Opens the logger input on `port` when one is selected, reporting the outcome on the logger control.
fn open_logger(
    shared: &Arc<Shared>,
    arbiter: &Arc<KeyingArbiter>,
    port: Option<String>,
) -> Option<LoggerHost> {
    let Some(lp) = port else {
        shared.clear_error(control::LOGGER);
        return None;
    };
    let reporter: HostReporter = make_host_reporter(Arc::clone(shared));
    match LoggerHost::open(&lp, arbiter.local_handle(), reporter) {
        Ok(host) => {
            shared.clear_error(control::LOGGER);
            shared.log("info", format!("Logger input active on {}.", host.port_name()));
            Some(host)
        }
        Err(e) => {
            shared.fail(
                control::LOGGER,
                format!("{lp} not available"),
                format!("Logger input unavailable: {e}"),
            );
            None
        }
    }
}

/// Opens the keying arbiter, optional logger host, and replay driver + bus forwarder + edge pump
/// for an **already-open** edge transport.
///
/// The arbiter is always created (with an LED-only adapter when no port is selected or available)
/// and passed to the replay driver, so port and line changes during a live session swap the
/// adapter inside the arbiter in place without restarting the replay driver.
fn start_replay_driver(
    shared: &Arc<Shared>,
    edge: &Arc<EdgeTransport>,
    clock: &Arc<dyn Clock>,
    keying_port: Option<String>,
    key_line: KeyingLine,
    ptt_line: Option<KeyingLine>,
    logger_port: Option<String>,
) -> (
    tokio::task::JoinHandle<rwk_core::Result<()>>,
    Arc<AtomicBool>,
    Option<LoggerHost>,
    Arc<KeyingArbiter>,
) {
    // Key and PTT must be on different control lines. A duplicate is ignored here (the setters
    // refuse it too) so the same pin is never driven by both.
    let ptt_line = ptt_line_for(key_line, ptt_line);

    let arbiter = Arc::new(KeyingArbiter::with_observer(
        keying_adapter(shared, keying_port, key_line, ptt_line),
        Some(make_line_observer(Arc::clone(shared))),
    ));

    // PTT sequencing: the replayer asserts PTT just ahead of each keyed edge and holds it for the
    // hang time after key-up. With no PTT line there is nothing to sequence, so pass None.
    let ptt_timing = ptt_line.map(|_| PttTiming {
        tail: std::time::Duration::from_millis(shared.settings.lock().unwrap().ptt_hang_ms),
        ..PttTiming::default()
    });
    let mut replayer = tunnel_replayer(clock.as_ref(), EdgeJitterProfile::PathAdaptive, ptt_timing);
    replayer.begin_session(1);

    let bus = EventBus::new();
    let logger = open_logger(shared, &arbiter, logger_port);

    // F9's event source is the EDGE transport's real path-health flag, not a fresh one: a run
    // of failed sends to a vanished peer raises it, and the watchdog turns that into F9.
    let driver = spawn_driver(
        replayer,
        Box::new(arbiter.remote_output()),
        Arc::clone(clock),
        Some(bus.clone()),
        Some(edge.health()),
    );

    let stop_bus = Arc::new(AtomicBool::new(false));
    spawn_bus_forwarder(bus, Arc::clone(shared), Arc::clone(&stop_bus));

    let pump = tokio::spawn(pump_edges(Arc::clone(edge), driver));
    (pump, stop_bus, logger, arbiter)
}

/// Builds the arbiter's line observer: emits KEY/PTT to the UI only on an actual change, so the
/// LEDs follow both the remote replay and the Player's logger.
fn make_line_observer(shared: Arc<Shared>) -> LineObserver {
    let last_key = Arc::new(Mutex::new(None::<bool>));
    let last_ptt = Arc::new(Mutex::new(None::<bool>));
    Arc::new(move |u: LineUpdate| {
        {
            let mut lk = last_key.lock().unwrap_or_else(|e| e.into_inner());
            if *lk != Some(u.key_down) {
                *lk = Some(u.key_down);
                shared.emit(ServerEvent::KeyState { down: u.key_down });
            }
        }
        {
            let mut lp = last_ptt.lock().unwrap_or_else(|e| e.into_inner());
            if *lp != Some(u.ptt) {
                *lp = Some(u.ptt);
                shared.emit(ServerEvent::Ptt { asserted: u.ptt });
            }
        }
    })
}

/// Builds the logger host's reporter: logs its lines and bridges the sending indicator.
fn make_host_reporter(shared: Arc<Shared>) -> HostReporter {
    Arc::new(move |report: HostReport| match report {
        HostReport::Log { level, message } => shared.log(&level, message),
        HostReport::Sending(active) => shared.emit(ServerEvent::LoggerSending { active }),
        // The reader's failure is also logged as a Log line; nothing more to show.
        HostReport::ReadFailed(_) => {}
    })
}

fn spawn_bus_forwarder(bus: EventBus, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("wrk-server-bus".into())
        .spawn(move || {
            let mut rx = bus.subscribe();
            while !stop.load(Ordering::SeqCst) {
                match rx.try_recv() {
                    Ok(CoreEvent::FailSafe { code, message }) => {
                        shared.emit(ServerEvent::FailSafe { code, message: message.clone() });
                        shared.log("warn", format!("Fail-safe F{code}: {message}"));
                    }
                    Ok(_) => {}
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
                }
            }
        })
        .expect("spawn bus forwarder");
}

fn parse_line(name: &str) -> Option<KeyingLine> {
    match name.to_ascii_lowercase().as_str() {
        "dtr" => Some(KeyingLine::Dtr),
        "rts" => Some(KeyingLine::Rts),
        _ => None,
    }
}

fn line_name(line: KeyingLine) -> &'static str {
    match line {
        KeyingLine::Dtr => "dtr",
        KeyingLine::Rts => "rts",
        KeyingLine::None => "none",
    }
}

/// The human-friendly identity a desktop Player advertises and logs on the LAN.
fn player_display_name(callsign: &str, keyer_id: &str) -> String {
    match (callsign.trim(), keyer_id.trim()) {
        ("", "") => "(unnamed Player)".to_string(),
        (callsign, "") => callsign.to_string(),
        ("", keyer_id) => keyer_id.to_string(),
        (callsign, keyer_id) => format!("{callsign}-{keyer_id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_operation_id_cannot_pass_server_replay_installation_gate() {
        let mut connection = ServerConnection::default();
        let stale = connection.gate.replace();
        let current = connection.gate.replace();
        connection.operation_active = true;

        assert!(!connection.permits_replay_install(stale));
        assert!(connection.permits_replay_install(current));
    }

    fn recording_sink() -> (EventSink, Arc<Mutex<Vec<ServerEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let sink: EventSink = Arc::new(move |e| captured.lock().unwrap().push(e));
        (sink, events)
    }

    fn controller(sink: EventSink) -> ServerController {
        ServerController::start_with_config(sink, None)
    }

    #[test]
    fn player_display_name_keeps_the_callsign_and_optional_label() {
        assert_eq!(player_display_name("W1VE", "20m"), "W1VE-20m");
        assert_eq!(player_display_name(" W1VE ", ""), "W1VE");
        assert_eq!(player_display_name("", "20m"), "20m");
    }

    #[test]
    fn hang_is_clamped_to_the_ui_range() {
        assert_eq!(clamp_ptt_hang(0), PTT_HANG_MIN_MS);
        assert_eq!(clamp_ptt_hang(200), 200);
        assert_eq!(clamp_ptt_hang(10_000), PTT_HANG_MAX_MS);
    }

    #[test]
    fn ptt_never_shares_the_key_line() {
        assert_eq!(ptt_line_for(KeyingLine::Dtr, Some(KeyingLine::Dtr)), None);
        assert_eq!(ptt_line_for(KeyingLine::Dtr, Some(KeyingLine::Rts)), Some(KeyingLine::Rts));
        assert_eq!(ptt_line_for(KeyingLine::Dtr, None), None);
    }

    #[test]
    fn selecting_ports_enforces_exclusivity_and_persists() {
        let dir = std::env::temp_dir().join(format!("wrk-srv-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("server.json");
        let _ = std::fs::remove_file(&path);

        let (sink, _events) = recording_sink();
        let first = ServerController::start_with_config(sink, Some(path.clone()));
        first.select_keying_port(Some("COM3".into()));
        first.select_logger_port(Some("COM4".into()));
        first.set_callsign("W1VE");
        first.set_keyer_id("20m");
        first.set_transport("lan");
        first.set_relay("198.51.100.9:9999");
        let token = first.shared.settings.lock().unwrap().token.clone();
        // Attempting to set keying port to logger port is rejected.
        first.select_keying_port(Some("COM4".into()));
        assert_eq!(first.shared.settings.lock().unwrap().keying_port.as_deref(), Some("COM3"));
        drop(first);

        let (sink2, _events2) = recording_sink();
        let second = ServerController::start_with_config(sink2, Some(path.clone()));
        let s = second.shared.settings.lock().unwrap();
        assert_eq!(s.keying_port.as_deref(), Some("COM3"));
        assert_eq!(s.logger_port.as_deref(), Some("COM4"));
        assert_eq!(s.callsign, "W1VE");
        assert_eq!(s.keyer_id, "20m");
        assert_eq!(s.transport, Transport::Lan);
        assert_eq!(s.relay, "198.51.100.9:9999");
        assert_eq!(s.token, token, "the WAN token survives a restart");
        drop(s);
        drop(second);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn line_setters_reject_sharing_the_same_pin() {
        let (sink, _events) = recording_sink();
        let controller = controller(sink);
        // Default is key=DTR, ptt=RTS. Setting key=RTS while ptt=RTS is rejected.
        controller.set_key_line("rts");
        assert_eq!(controller.shared.settings.lock().unwrap().key_line, KeyingLine::Dtr);
        // Clearing PTT first lets key move to RTS.
        controller.set_ptt_line("none");
        controller.set_key_line("rts");
        assert_eq!(controller.shared.settings.lock().unwrap().key_line, KeyingLine::Rts);
        assert_eq!(controller.shared.settings.lock().unwrap().ptt_line, None);
    }
}
