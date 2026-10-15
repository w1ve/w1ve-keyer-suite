//! The Server controller for W1VE Remote Keyer: receives keyed edges and keys the radio.
//!
//! This is the Keyer-Router Station's replay path, distilled to the Remote Keyer's job. It
//! reuses `rwk-core`'s replay engine (jitter buffer, fail-safes, the time-critical replay
//! thread) and serial keying output, and the same WAN/LAN pairing as the Client. There is no
//! router, no logger input, and no keying arbiter — the Server has a single keying source (the
//! remote Client), so edges flow straight from the tunnel into the replay driver and out to
//! the radio's control line.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rwk_core::config::{Config, Role};
use rwk_core::engine::bus::{CoreEvent, EventBus};
use rwk_core::engine::edge_station::{pump_edges, tunnel_replayer};
use rwk_core::engine::network::EdgeTransport;
use rwk_core::engine::replay::{spawn_driver, EdgeJitterProfile, KeyingOutput, SerialKeyingAdapter};
use rwk_core::engine::serial::{
    enumerate_ports, natural_port_key, KeyingOutputConfig, SerialKeyingOutput,
};
use rwk_core::engine::session::Session;
use rwk_core::engine::tunnel;
use rwk_core::primitives::KeyingLine;
use rwk_core::timing::{Clock, MonotonicClock};

use crate::app_paths;
use crate::pairing::{self, Transport};

const EDGE_PORT: u16 = 5555;

/// Control identifiers for inline error messages in the UI.
mod control {
    pub const KEYING: &str = "keying";
}

/// An event the Server controller emits for the UI.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ServerEvent {
    KeyState { down: bool },
    Ptt { asserted: bool },
    Connection { state: String, path: String },
    FailSafe { code: u8, message: String },
    Log { level: String, message: String },
    Settings {
        keying_port: Option<String>,
        key_line: String,
        ptt_line: String,
        transport: String,
        relay: String,
        token: String,
    },
    /// The live list of LAN keyers available to connect to (for the dropdown).
    LanPeers { peers: Vec<LanPeerView> },
    /// The current serial-port inventory.
    Ports { available: Vec<String> },
    /// A short, per-control error (empty `message` clears it). Full detail is also logged.
    Error { control: String, message: String },
}

/// A LAN keyer as the Server UI shows it in the dropdown.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LanPeerView {
    /// Base64 public key — the stable id passed back to `connect_lan`.
    pub key: String,
    /// `callsign-keyerId` display name.
    pub name: String,
    /// The keyer's endpoint (`ip:port`), passed back to `connect_lan`.
    pub endpoint: String,
}

/// A sink the host (Tauri) registers for [`ServerEvent`]s.
pub type EventSink = Arc<dyn Fn(ServerEvent) + Send + Sync>;

#[derive(Debug, Clone)]
struct Settings {
    keying_port: Option<String>,
    key_line: KeyingLine,
    ptt_line: Option<KeyingLine>,
    transport: Transport,
    relay: String,
    /// The pairing token the Server generated for this run (shown so the Client can enter it).
    token: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            keying_port: None,
            key_line: KeyingLine::Dtr,
            ptt_line: Some(KeyingLine::Rts),
            transport: Transport::Wan,
            relay: pairing::DEFAULT_WAN_RELAY.to_string(),
            token: pairing::generate_token(),
        }
    }
}

struct Shared {
    settings: Mutex<Settings>,
    config: Mutex<Config>,
    config_path: Option<std::path::PathBuf>,
    sink: EventSink,
}

/// Emits the browser's current peer list as a [`ServerEvent::LanPeers`].
fn emit_peers(shared: &Arc<Shared>, browser: &mut rwk_core::engine::lan_discovery::LanBrowser) {
    let peers = browser
        .peers()
        .into_iter()
        .map(|p| LanPeerView {
            key: rwk_core::config::base64_encode(&p.public_key),
            name: p.display_name(),
            endpoint: p.endpoint.to_string(),
        })
        .collect();
    shared.emit(ServerEvent::LanPeers { peers });
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
        });
    }

    fn persist(&self) {
        let Some(path) = &self.config_path else { return };
        let config = self.config.lock().unwrap();
        if let Err(e) = config.save(path) {
            self.log("warn", format!("Could not save config: {e}"));
        }
    }
}

/// The resources a running pairing owns.
struct ActiveSession {
    session: Session,
    pump: tokio::task::JoinHandle<rwk_core::Result<()>>,
    stop_bus: Arc<AtomicBool>,
}

/// The Server controller.
pub struct ServerController {
    shared: Arc<Shared>,
    runtime: tokio::runtime::Runtime,
    active: Arc<Mutex<Option<ActiveSession>>>,
    /// The running LAN browse task (radio-site side), present while browsing.
    browse: Mutex<Option<tokio::task::JoinHandle<()>>>,
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
        let shared = Arc::new(Shared {
            settings: Mutex::new(Settings::default()),
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
            active: Arc::new(Mutex::new(None)),
            browse: Mutex::new(None),
        };
        controller.shared.emit_settings();
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
        self.active.lock().unwrap().is_some()
    }

    /// Re-enumerates serial ports and reconciles the keying selection. While **not** paired, a
    /// vanished keying port drops to `None`. While **paired** the port is held open by the
    /// replay path and must not be disturbed, so it is folded back into the list even if the
    /// OS hides the busy port from enumeration. Always emits the fresh port list.
    pub fn reconcile_ports(&self) {
        let paired = self.is_paired();
        let mut present = Self::serial_ports();
        if paired {
            if let Some(p) = self.shared.settings.lock().unwrap().keying_port.clone() {
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
        let keying_gone = {
            let s = self.shared.settings.lock().unwrap();
            matches!(&s.keying_port, Some(name) if !present.iter().any(|p| p == name))
        };
        if keying_gone {
            let name = self.shared.settings.lock().unwrap().keying_port.take();
            if let Some(n) = name {
                self.shared.log("warn", format!("Keying port {n} removed; set to (None)."));
            }
            self.shared.clear_error(control::KEYING);
            self.shared.emit_settings();
        }
    }

    pub fn select_keying_port(&self, port: Option<String>) {
        self.shared.settings.lock().unwrap().keying_port = port;
        self.shared.clear_error(control::KEYING);
        self.shared.emit_settings();
    }

    pub fn set_key_line(&self, line: &str) {
        self.shared.settings.lock().unwrap().key_line = parse_line(line).unwrap_or(KeyingLine::Dtr);
        self.shared.emit_settings();
    }

    pub fn set_ptt_line(&self, line: &str) {
        self.shared.settings.lock().unwrap().ptt_line = parse_line(line);
        self.shared.emit_settings();
    }

    pub fn set_transport(&self, transport: &str) {
        self.shared.settings.lock().unwrap().transport = Transport::parse(transport);
        self.shared.emit_settings();
    }

    pub fn set_relay(&self, relay: &str) {
        self.shared.settings.lock().unwrap().relay = relay.to_string();
        self.shared.emit_settings();
    }

    /// Regenerates the pairing token (a fresh code for a new pairing).
    pub fn regenerate_token(&self) {
        self.shared.settings.lock().unwrap().token = pairing::generate_token();
        self.shared.emit_settings();
    }

    // ─── Pairing ──────────────────────────────────────────────────────────────

    /// Begins listening: the Server waits for the Client to pair over the current transport +
    /// token, then replays received edges onto the radio.
    pub fn listen(&self) {
        let (transport, relay, token, keying_port, key_line, ptt_line) = {
            let s = self.shared.settings.lock().unwrap();
            (s.transport, s.relay.clone(), s.token.clone(), s.keying_port.clone(), s.key_line, s.ptt_line)
        };
        if keying_port.is_none() {
            self.shared.fail(
                control::KEYING,
                "select a port",
                "Select a keying output port before starting.",
            );
            self.shared
                .emit(ServerEvent::Connection { state: "disconnected".into(), path: "none".into() });
            return;
        }
        self.shared.clear_error(control::KEYING);
        self.disconnect();

        let shared = Arc::clone(&self.shared);
        let active_slot = Arc::clone(&self.active);
        shared.emit(ServerEvent::Connection { state: "connecting".into(), path: "none".into() });
        shared.log("info", "Waiting for the Client to pair…");

        self.runtime.spawn(async move {
            let base = shared.config.lock().unwrap().clone();
            let config =
                match pairing::build_config(&base, Role::Station, transport, &token, &relay) {
                    Ok(c) => c,
                    Err(e) => return fail(&shared, e),
                };

            let connected = match tunnel::connect(&config).await {
                Ok(c) => c,
                Err(e) => return fail(&shared, format!("link failed: {e}")),
            };
            let config = config;
            bring_up_replay(&shared, &active_slot, connected, &config, keying_port, key_line, ptt_line)
                .await;
        });
    }

    // ─── LAN: browse available keyers and pick one ─────────────────────────────

    /// Starts browsing the LAN for advertised keyers, emitting a [`ServerEvent::LanPeers`]
    /// list the UI renders as a dropdown. Runs until the controller stops browsing (a new
    /// browse call, a `listen`/`connect_lan`, or disconnect).
    pub fn lan_browse(&self) {
        // Stop any prior browse.
        self.stop_browse();
        let self_key = match self.shared.config.lock().unwrap().identity.to_identity() {
            Ok(id) => id.public_bytes(),
            Err(e) => {
                self.shared.log("warn", format!("identity error: {e}"));
                return;
            }
        };
        let shared = Arc::clone(&self.shared);
        let handle = self.runtime.spawn(async move {
            let mut browser = match rwk_core::engine::lan_discovery::LanBrowser::bind(self_key) {
                Ok(b) => b,
                Err(e) => {
                    shared.log("warn", format!("LAN browse failed: {e}"));
                    return;
                }
            };
            shared.log("info", "Browsing the LAN for keyers…");
            // Emit the (possibly empty) list promptly, then on every change and on a timer so
            // stale entries prune out of the dropdown.
            emit_peers(&shared, &mut browser);
            loop {
                match tokio::time::timeout(std::time::Duration::from_secs(1), browser.recv_once())
                    .await
                {
                    Ok(Ok(true)) => emit_peers(&shared, &mut browser),
                    Ok(Ok(false)) => {}
                    Ok(Err(_)) => break,
                    Err(_) => emit_peers(&shared, &mut browser), // timeout: re-emit (prunes)
                }
            }
        });
        *self.browse.lock().unwrap() = Some(handle);
    }

    fn stop_browse(&self) {
        if let Some(task) = self.browse.lock().unwrap().take() {
            task.abort();
        }
    }

    /// Picks a browsed keyer by its base64 public key and brings the link up as the initiator,
    /// then starts replaying its edges onto the radio.
    pub fn connect_lan(&self, peer_key_b64: &str, peer_endpoint: &str) {
        let (keying_port, key_line, ptt_line) = {
            let s = self.shared.settings.lock().unwrap();
            (s.keying_port.clone(), s.key_line, s.ptt_line)
        };
        if keying_port.is_none() {
            self.shared.fail(
                control::KEYING,
                "select a port",
                "Select a keying output port before connecting.",
            );
            return;
        }
        self.shared.clear_error(control::KEYING);
        let peer_key = match rwk_core::config::base64_decode(peer_key_b64)
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
        {
            Some(k) => k,
            None => {
                self.shared.log("warn", "Invalid keyer selection.");
                return;
            }
        };
        let endpoint: std::net::SocketAddr = match peer_endpoint.parse() {
            Ok(a) => a,
            Err(e) => {
                self.shared.log("warn", format!("Invalid keyer address: {e}"));
                return;
            }
        };
        self.stop_browse();
        self.disconnect();

        let shared = Arc::clone(&self.shared);
        let active_slot = Arc::clone(&self.active);
        shared.emit(ServerEvent::Connection { state: "connecting".into(), path: "none".into() });
        shared.log("info", "Connecting to the selected keyer…");

        self.runtime.spawn(async move {
            let config = shared.config.lock().unwrap().clone();
            let identity = match config.identity.to_identity() {
                Ok(id) => id,
                Err(e) => return fail(&shared, format!("identity error: {e}")),
            };
            let connected = match tunnel::lan_connect_to(&identity, peer_key, endpoint).await {
                Ok(c) => c,
                Err(e) => return fail(&shared, format!("LAN link failed: {e}")),
            };
            bring_up_replay(&shared, &active_slot, connected, &config, keying_port, key_line, ptt_line)
                .await;
        });
    }

    pub fn disconnect(&self) {
        self.stop_browse();
        let active = self.active.lock().unwrap().take();
        if let Some(mut active) = active {
            active.stop_bus.store(true, Ordering::SeqCst);
            active.pump.abort();
            let mut session = active.session;
            self.runtime.block_on(async { session.shutdown().await });
            self.shared.emit(ServerEvent::KeyState { down: false });
            self.shared.emit(ServerEvent::Ptt { asserted: false });
            self.shared
                .emit(ServerEvent::Connection { state: "disconnected".into(), path: "none".into() });
            self.shared.log("info", "Stopped.");
        }
    }
}

impl Drop for ServerController {
    fn drop(&mut self) {
        self.disconnect();
    }
}

fn fail(shared: &Arc<Shared>, message: String) {
    shared.log("warn", message);
    shared.emit(ServerEvent::Connection { state: "disconnected".into(), path: "none".into() });
}

/// Shared post-connect setup for both the WAN (`listen`) and LAN (`connect_lan`) paths: open
/// the keying edge over the tunnel, open the serial keying output, spawn the replay driver,
/// and install the running session. Reports connected (or fails) through the sink.
#[allow(clippy::too_many_arguments)]
async fn bring_up_replay(
    shared: &Arc<Shared>,
    active_slot: &Arc<Mutex<Option<ActiveSession>>>,
    connected: tunnel::Connected,
    config: &Config,
    keying_port: Option<String>,
    key_line: KeyingLine,
    ptt_line: Option<KeyingLine>,
) {
    let path = format!("{:?}", connected.path).to_lowercase();
    let session = match Session::start(connected, config) {
        Ok(s) => s,
        Err(e) => return fail(shared, format!("session failed: {e}")),
    };
    let net = session.handle();
    let conn = match net.open_udp(EDGE_PORT).await {
        Ok(c) => c,
        Err(e) => return fail(shared, format!("edge open failed: {e}")),
    };
    let edge = Arc::new(EdgeTransport::over_tunnel(conn));
    let peer = std::net::SocketAddr::new(
        std::net::IpAddr::V4(Role::Station.peer_inner_address()),
        EDGE_PORT,
    );
    edge.set_peer(peer).await;

    let port_name = keying_port.expect("checked present before connect");
    let cfg = KeyingOutputConfig {
        port_name: port_name.clone(),
        key_line,
        ptt_line: ptt_line.unwrap_or(KeyingLine::None),
        ..Default::default()
    };
    let serial = match SerialKeyingOutput::open(cfg) {
        Ok(out) => out,
        Err(e) => {
            shared.fail(
                control::KEYING,
                format!("{port_name} not available"),
                format!("keying port {port_name} failed: {e}"),
            );
            shared.emit(ServerEvent::Connection { state: "disconnected".into(), path: "none".into() });
            return;
        }
    };
    shared.clear_error(control::KEYING);
    shared.log("info", format!("Keying output open on {port_name}."));

    let clock: Arc<dyn Clock> = Arc::new(MonotonicClock);
    let mut replayer = tunnel_replayer(clock.as_ref(), EdgeJitterProfile::PathAdaptive, None);
    replayer.begin_session(1);

    let bus = EventBus::new();
    let output = UiKeyingOutput::new(SerialKeyingAdapter::new(serial), Arc::clone(shared));
    let path_health = rwk_core::engine::network::PathHealth::new();
    let driver = spawn_driver(
        replayer,
        Box::new(output),
        Arc::clone(&clock),
        Some(bus.clone()),
        Some(path_health),
    );

    let stop_bus = Arc::new(AtomicBool::new(false));
    spawn_bus_forwarder(bus, Arc::clone(shared), Arc::clone(&stop_bus));

    let pump = tokio::spawn(pump_edges(Arc::clone(&edge), driver));

    *active_slot.lock().unwrap() = Some(ActiveSession { session, pump, stop_bus });
    shared.emit(ServerEvent::Connection { state: "connected".into(), path: path.clone() });
    shared.log("info", format!("Paired with the keyer over the {path} path."));
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

/// A keying output that drives the serial line and reports KEY/PTT to the UI on each change.
struct UiKeyingOutput {
    inner: SerialKeyingAdapter,
    shared: Arc<Shared>,
    last_key: Option<bool>,
    last_ptt: Option<bool>,
}

impl UiKeyingOutput {
    fn new(inner: SerialKeyingAdapter, shared: Arc<Shared>) -> Self {
        Self { inner, shared, last_key: None, last_ptt: None }
    }
}

impl KeyingOutput for UiKeyingOutput {
    fn set_key(&mut self, key_down: bool) {
        self.inner.set_key(key_down);
        if self.last_key != Some(key_down) {
            self.last_key = Some(key_down);
            self.shared.emit(ServerEvent::KeyState { down: key_down });
        }
    }

    fn set_ptt(&mut self, asserted: bool) {
        self.inner.set_ptt(asserted);
        if self.last_ptt != Some(asserted) {
            self.last_ptt = Some(asserted);
            self.shared.emit(ServerEvent::Ptt { asserted });
        }
    }

    fn take_fault(&mut self) -> Option<String> {
        self.inner.take_fault()
    }
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
