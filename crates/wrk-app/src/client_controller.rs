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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use rwk_core::config::{Config, Role};
use rwk_core::engine::audio::SidetoneEngine;
use rwk_core::engine::debounce::ContactDebouncer;
use rwk_core::engine::edge_sender::EdgeSender;
use rwk_core::engine::keying::{EdgeEvent, EdgeSchedule, EdgeScheduleBuilder};
use rwk_core::engine::lan_discovery::{Beacon, LanAdvertiser};
use rwk_core::engine::network::EdgeTransport;
use rwk_core::engine::paddle::PaddleKeyer;
use rwk_core::engine::paddle_input::PaddleInputPort;
use rwk_core::engine::serial::{enumerate_ports, KeyingOutputConfig, SerialKeyingOutput};
use rwk_core::engine::session::Session;
use rwk_core::engine::tunnel;
use rwk_core::platform::{ThreadPriorityGuard, TimerResolutionGuard};
use rwk_core::primitives::{EdgeSource, KeyerMode, KeyingLine};
use rwk_core::timing::{Clock, HybridWaiter, MonotonicClock};

use crate::app_paths;
use crate::pairing::{self, Transport};

/// The inner UDP port the keying edge rides over the tunnel, matching the Server.
const EDGE_PORT: u16 = 5555;

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
        transport: String,
        relay: String,
        callsign: String,
        keyer_id: String,
    },
    /// Whether the LAN discovery beacon is currently being advertised.
    Advertising { active: bool },
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
    keying_port: Option<String>,
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
            keying_port: None,
            paddle_port: None,
            transport: Transport::Wan,
            relay: pairing::DEFAULT_WAN_RELAY.to_string(),
            callsign: String::new(),
            keyer_id: String::new(),
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
            .name("wrk-sidetone".into())
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

struct Shared {
    settings: Mutex<Settings>,
    sidetone: Mutex<Option<Sidetone>>,
    keying: Mutex<Option<SerialKeyingOutput>>,
    transport: Mutex<Option<Arc<EdgeTransport>>>,
    epoch: Mutex<u16>,
    paddle: Mutex<Option<PaddlePollerHandle>>,
    session: tokio::sync::Mutex<Option<Session>>,
    /// The running LAN advertiser task (operating-position side), present while advertising.
    advertise: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
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

    fn emit_settings(&self) {
        let s = self.settings.lock().unwrap();
        self.emit(ClientEvent::Settings {
            wpm: s.wpm,
            weight: s.weight,
            mode: mode_name(s.mode).into(),
            paddle_reverse: s.paddle_reverse,
            tone_hz: s.tone_hz,
            tone_volume: s.tone_volume,
            transport: s.transport.as_str().into(),
            relay: s.relay.clone(),
            callsign: s.callsign.clone(),
            keyer_id: s.keyer_id.clone(),
        });
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
            keying: Mutex::new(None),
            transport: Mutex::new(None),
            epoch: Mutex::new(1),
            paddle: Mutex::new(None),
            session: tokio::sync::Mutex::new(None),
            advertise: tokio::sync::Mutex::new(None),
            config: Mutex::new(config),
            config_path,
            sink,
        });

        let sidetone = Sidetone::spawn(settings.tone_hz, settings.tone_volume, Arc::clone(&shared));
        *shared.sidetone.lock().unwrap() = Some(sidetone);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        let controller = Self { shared, runtime, _clock: Arc::new(MonotonicClock) };
        controller.shared.emit_settings();
        controller
    }

    // ─── Keyer settings ────────────────────────────────────────────────────────

    pub fn set_speed(&self, wpm: u32) {
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

    #[must_use]
    pub fn serial_ports() -> Vec<String> {
        enumerate_ports()
            .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
            .unwrap_or_default()
    }

    pub fn select_keying_port(&self, port: Option<String>) {
        self.shared.settings.lock().unwrap().keying_port = port.clone();
        let mut guard = self.shared.keying.lock().unwrap();
        *guard = None;
        if let Some(name) = port {
            let cfg = KeyingOutputConfig {
                port_name: name.clone(),
                key_line: KeyingLine::Dtr,
                ..Default::default()
            };
            match SerialKeyingOutput::open(cfg) {
                Ok(out) => {
                    *guard = Some(out);
                    self.shared.log("info", format!("Keying output on {name}."));
                }
                Err(e) => self.shared.log("warn", format!("Keying port {name} failed: {e}")),
            }
        }
    }

    pub fn select_paddle_port(&self, port: Option<String>) {
        self.shared.settings.lock().unwrap().paddle_port = port.clone();
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
                    self.shared.log("info", format!("Paddle input on {name}."));
                }
                Err(e) => self.shared.log("warn", format!("Paddle port {name} failed: {e}")),
            },
            None => self.shared.log("info", "Paddle input cleared."),
        }
    }

    // ─── Keying ──────────────────────────────────────────────────────────────

    pub fn send_cw(&self, text: &str) {
        let text = text.to_string();
        let shared = Arc::clone(&self.shared);
        let handle = self.runtime.handle().clone();
        std::thread::spawn(move || play_cw(&shared, &handle, &text));
    }

    pub fn test_tx(&self) {
        self.send_cw("VVV TEST");
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
        let shared = Arc::clone(&self.shared);
        shared.emit(ClientEvent::Connection { state: "connecting".into(), path: "none".into() });

        self.runtime.spawn(async move {
            let base = shared.config.lock().unwrap().clone();
            let config = match pairing::build_config(&base, Role::Client, transport, &token, &relay)
            {
                Ok(c) => c,
                Err(e) => return fail(&shared, e),
            };

            let connected = match tunnel::connect(&config).await {
                Ok(c) => c,
                Err(e) => return fail(&shared, format!("link failed: {e}")),
            };
            bring_up_keying(&shared, connected).await;
        });
    }

    pub fn disconnect(&self) {
        *self.shared.transport.lock().unwrap() = None;
        let shared = Arc::clone(&self.shared);
        self.runtime.block_on(async move {
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

/// The operating-position LAN flow: bind the WireGuard socket, broadcast the beacon with that
/// port, and wait for a radio site to initiate. On success, bring the keying edge up over the
/// resulting tunnel. Runs as one task; aborting it (unpair) stops both the beacon and the wait.
async fn lan_advertise_and_await(shared: Arc<Shared>, callsign: String, keyer_id: String) {
    // 1. Bind the WireGuard socket up front so the beacon can advertise its port.
    let (socket, wg_port) = match tunnel::lan_bind().await {
        Ok(pair) => pair,
        Err(e) => return fail(&shared, format!("LAN bind failed: {e}")),
    };
    let identity = match shared.config.lock().unwrap().identity.to_identity() {
        Ok(id) => id,
        Err(e) => return fail(&shared, format!("identity error: {e}")),
    };

    // 2. Broadcast the beacon until a picker connects. The advertiser runs on its own task so
    //    the await below can proceed concurrently.
    let beacon = Beacon {
        callsign,
        keyer_id,
        public_key: identity.public_bytes(),
        wg_port,
    };
    let advertiser = match LanAdvertiser::new(beacon) {
        Ok(a) => a,
        Err(e) => return fail(&shared, format!("LAN advertise failed: {e}")),
    };
    let beacon_task = tokio::spawn(advertiser.run());

    // 3. Wait for a radio site to pick us and complete the handshake (we are the responder).
    let connected = match tunnel::lan_await_initiator(socket, &identity).await {
        Ok(c) => c,
        Err(e) => {
            beacon_task.abort();
            return fail(&shared, format!("LAN pairing failed: {e}"));
        }
    };
    // Paired: stop advertising so we drop off other radio sites' lists.
    beacon_task.abort();
    shared.emit(ClientEvent::Advertising { active: false });

    bring_up_keying(&shared, connected).await;
}

/// Shared tail of both the WAN and LAN connect paths: start the session, open the keying edge
/// over the tunnel, install it, and report connected.
async fn bring_up_keying(shared: &Arc<Shared>, connected: tunnel::Connected) {
    let path = format!("{:?}", connected.path).to_lowercase();
    // The config only supplies the role here (no forward rules on the Remote Keyer).
    let config = shared.config.lock().unwrap().clone();
    let session = match Session::start(connected, &config) {
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
        std::net::IpAddr::V4(Role::Client.peer_inner_address()),
        EDGE_PORT,
    );
    edge.set_peer(peer).await;

    *shared.transport.lock().unwrap() = Some(edge);
    *shared.session.lock().await = Some(session);
    {
        let mut epoch = shared.epoch.lock().unwrap();
        *epoch = epoch.wrapping_add(1);
    }
    shared.emit(ClientEvent::Connection { state: "connected".into(), path: path.clone() });
    shared.log("info", format!("Paired with the Server over the {path} path."));
}

impl Drop for ClientController {
    fn drop(&mut self) {
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

// ─── Per-edge output fan-out (sidetone + key line + UI + tunnel) ───────────────

fn drive_edge(
    shared: &Arc<Shared>,
    rt: &tokio::runtime::Handle,
    tone: Option<&Sidetone>,
    sender: &mut EdgeSender,
    key_down: bool,
    timestamp_ticks: u64,
) {
    if let Some(tone) = tone {
        tone.key(key_down);
    }
    if let Some(out) = shared.keying.lock().unwrap().as_mut() {
        let _ = out.apply(key_down, false);
    }
    shared.emit(ClientEvent::KeyState { down: key_down });
    let transport = shared.transport.lock().unwrap().clone();
    if let Some(transport) = transport {
        let event = EdgeEvent { timestamp_ticks, key_down, source: EdgeSource::Paddle };
        let frame = sender.frame_for(event);
        let _ = rt.block_on(async { transport.send_frame(&frame).await });
    }
}

fn play_cw(shared: &Arc<Shared>, rt: &tokio::runtime::Handle, text: &str) {
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
    let epoch = *shared.epoch.lock().unwrap();
    let mut sender = EdgeSender::new(epoch, clock.frequency(), start);
    let tone = shared.sidetone.lock().unwrap().clone();
    for edge in &schedule.edges {
        HybridWaiter::wait_until_ticks(&clock, edge.timestamp_ticks, &|| false);
        drive_edge(shared, rt, tone.as_ref(), &mut sender, edge.key_down, edge.timestamp_ticks);
    }
    drive_edge(shared, rt, tone.as_ref(), &mut sender, false, clock.now());
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
    let epoch = *shared.epoch.lock().unwrap();
    let mut sender = EdgeSender::new(epoch, hz, clock.now());
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
        let tone = shared.sidetone.lock().unwrap().clone();
        let played = keyer.run_once(
            &clock,
            &paddle_now,
            &mut |edge| {
                drive_edge(shared, rt, tone.as_ref(), &mut sender, edge.key_down, edge.timestamp_ticks);
            },
            &should_abort,
        );
        if !played {
            let next = clock.now() + poll_step.max(1);
            HybridWaiter::wait_until_ticks(&clock, next, &should_abort);
        }
    }
    let tone = shared.sidetone.lock().unwrap().clone();
    drive_edge(shared, rt, tone.as_ref(), &mut sender, false, clock.now());
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
