//! The Station controller: the headless engine owner behind the Station UI.
//!
//! This mirrors [`crate::controller::ClientController`], but for the remote radio site. The
//! Client holds the paddle and *sends* keyed edges; the Station *receives* them over the
//! tunnel, replays them through the jitter buffer and fail-safe engine, and keys the radio's
//! control line in real time. The UI never touches the engine directly — it sends commands
//! here and renders the [`StationEvent`]s this controller emits.
//!
//! All timing-critical work (the replay thread, the watchdog, the WireGuard link) runs on
//! engine-owned threads; the controller holds only `Send + Sync` handles, so it can live in
//! Tauri's managed state and be driven from command handlers on any thread.
//!
//! ## What this slice covers
//!
//! * **Keying output**: pick the serial port + KEY/PTT control lines that key the radio.
//! * **Pairing**: wait for a Client by shared relay + code, or advertise this desktop Player on
//!   the LAN and wait as the direct WireGuard responder, using a **persisted** identity so the
//!   key is stable across restarts.
//! * **Logger input**: an optional second serial port on which a local logging program speaks
//!   WinKeyer2; its CW keys the same radio through a [`KeyingArbiter`] with priority over the
//!   remote replay.
//! * **Persistence**: identity, Player callsign/ID, relay, pairing code and the port selections
//!   are saved to a per-user `config.json` so a restart resumes where the operator left off.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use rwk_core::config::{base64_decode, base64_encode, Config, ForwardRuleConfig, Role};
use rwk_core::engine::bus::{CoreEvent, EventBus};
use rwk_core::engine::control::{ControlLink, ControlMessage};
use rwk_core::engine::edge_station::{pump_edges, tunnel_replayer};
use rwk_core::engine::flex_discovery::DiscoveryListener;
use rwk_core::engine::keying_arbiter::{KeyingArbiter, LineObserver, LineUpdate};
use rwk_core::engine::lan_discovery::{Beacon, LanAdvertiser, CAPABILITY_CW, CAPABILITY_ROUTER};
use rwk_core::engine::network::EdgeTransport;
use rwk_core::engine::replay::{spawn_driver, EdgeJitterProfile, SerialKeyingAdapter};
use rwk_core::engine::reconnect::connect_with_relay_retry;
use rwk_core::engine::serial::{enumerate_ports, KeyingOutputConfig, PttTiming, SerialKeyingOutput};
use rwk_core::engine::session::Session;
use rwk_core::engine::tunnel;
use rwk_core::primitives::KeyingLine;
use rwk_core::timing::{Clock, MonotonicClock};

use rwk_core::engine::serial::natural_port_key;

use crate::app_paths;
use crate::station_winkeyer::{HostReport, HostReporter, StationWinKeyerHost};

/// The inner UDP port the keying edge rides over the tunnel, matching the Client's
/// [`crate::controller`] `EDGE_PORT` so the two ends agree.
const EDGE_PORT: u16 = 5555;

/// PTT hang time bounds and default, in ms (how long PTT is held after key-up). Same range as
/// the Remote Keyer's Player.
pub const PTT_HANG_MIN_MS: u32 = 75;
pub const PTT_HANG_MAX_MS: u32 = 600;
const PTT_HANG_DEFAULT_MS: u64 = 200;

/// Control identifiers for inline per-control error messages in the UI.
mod control {
    pub const KEYING: &str = "keying";
    pub const LOGGER: &str = "logger";
}

/// An event the Station controller emits for the UI to render. Serialized to the webview
/// as-is on the shared `rwk://event` channel.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StationEvent {
    /// The radio key line changed state (for the KEY indicator).
    KeyState { down: bool },
    /// The PTT line changed state (for the PTT indicator).
    Ptt { asserted: bool },
    /// The local logger is actively sending CW (`true`) or idle (`false`).
    LoggerSending { active: bool },
    /// The link to the client changed. `path` is "none" | "direct" | "relay".
    Connection { state: String, path: String },
    /// The round-trip time to the Client in ms, refreshed about once a second while paired.
    /// `None` until measured (and when unpaired).
    Rtt { ms: Option<u32> },
    /// A fail-safe fired (F1..=F10). `code` is the number, `message` the description.
    FailSafe { code: u8, message: String },
    /// A human-readable log line for the Log tab.
    Log { level: String, message: String },
    /// The current serial-port inventory (numeric-sorted), pushed on change (PnP).
    Ports { available: Vec<String> },
    /// A short per-control error to show in red under the offending control (full detail is
    /// logged too). An empty `message` clears the control's error. `control` is one of
    /// "keying" | "logger".
    Error { control: String, message: String },
    /// The current Station settings, echoed so the UI can confirm them.
    Settings {
        keying_port: Option<String>,
        key_line: String,
        ptt_line: String,
        logger_port: Option<String>,
        logger_enabled: bool,
        callsign: String,
        keyer_id: String,
        relay: Option<String>,
        pairing_code: Option<String>,
        transport: String,
        ptt_hang_ms: u64,
    },
    /// Whether this desktop Player is advertising its RWKP v2 LAN availability.
    Advertising { active: bool },
    /// The forward-rule table the Client last pushed (read-only on the Station). Empty when
    /// unpaired. Emitted on every push, so the Station's view follows the operator's rules.
    ForwardRules { rules: Vec<ForwardRuleConfig> },
    /// Whether the Station is capturing FlexRadio discovery and relaying it to the Client. True
    /// while a Flex rule is pushed and the control channel is up.
    FlexDiscovery { enabled: bool },
}

/// A sink the host (Tauri) registers to receive [`StationEvent`]s. Boxed so the controller
/// does not depend on the UI framework.
pub type EventSink = Arc<dyn Fn(StationEvent) + Send + Sync>;

/// Live Station settings, shared with the pairing task and persisted to `config.json`.
#[derive(Debug, Clone)]
struct Settings {
    keying_port: Option<String>,
    key_line: KeyingLine,
    /// `None` means PTT is not driven (QSK / hardware-handled).
    ptt_line: Option<KeyingLine>,
    logger_port: Option<String>,
    logger_enabled: bool,
    relay: Option<String>,
    pairing_code: Option<String>,
    /// Player callsign and optional position label advertised in the RWKP v2 beacon.
    callsign: String,
    keyer_id: String,
    /// Pairing transport: "wan" (relay + code) or "lan" (this Player advertises for a Keyer).
    transport: String,
    /// How long PTT is held after key-up, in ms.
    ptt_hang_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            keying_port: None,
            key_line: KeyingLine::Dtr,
            ptt_line: Some(KeyingLine::Rts),
            logger_port: None,
            logger_enabled: false,
            relay: None,
            pairing_code: None,
            callsign: String::new(),
            keyer_id: String::new(),
            transport: "wan".to_string(),
            ptt_hang_ms: PTT_HANG_DEFAULT_MS,
        }
    }
}

/// Monotonic owner token for one Station pairing or advertising operation.
///
/// This is separate from the keyed-edge epoch. It exists solely to decide which asynchronous
/// work is allowed to install or tear down Station-owned resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StationOperation(u64);

/// A lifecycle resource paired with the operation that is allowed to own it.
struct OperationResource<T> {
    operation: StationOperation,
    resource: T,
}

/// Cooperative cancellation and join ownership for an outer WAN listener or LAN advertiser.
/// Setup tasks are never force-aborted because they may be holding a private `Session` across an
/// edge-setup await; retirement signals this sender and then awaits the task outside the gate.
struct PairTask {
    handle: tokio::task::JoinHandle<()>,
    cancel: tokio::sync::watch::Sender<bool>,
}

/// The Station installation gate. Stop and replacement advance `generation` before cancelling
/// work, so cancellation is an optimization rather than the boundary that prevents a late task
/// from publishing a replay session.
#[derive(Default)]
struct StationLifecycle {
    generation: u64,
    active: Option<OperationResource<ActiveSession>>,
    /// Liveness-owned shutdowns running outside the gate. Stop/replacement detaches and awaits
    /// all of them before beginning another operation that could reuse the same resources.
    cleanup_tasks: Vec<OperationResource<tokio::task::JoinHandle<()>>>,
    pair_task: Option<OperationResource<PairTask>>,
}

impl StationLifecycle {
    fn current(&self) -> StationOperation {
        StationOperation(self.generation)
    }

    fn next_operation(&mut self) -> StationOperation {
        self.generation = self.generation.wrapping_add(1);
        self.current()
    }

    fn is_current(&self, operation: StationOperation) -> bool {
        self.current() == operation
    }

    /// The final replay-session commit uses this predicate while holding the installation gate.
    fn can_install_replay_session(&self, operation: StationOperation) -> bool {
        self.is_current(operation) && self.active.is_none()
    }
}

/// Resources detached by Stop or replacement. They are stopped only after the installation gate
/// has released, so slow cleanup can never touch a newer operation's session.
struct RetiredStationResources {
    pair_task: Option<PairTask>,
    active: Option<ActiveSession>,
    cleanup_tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Signals that one installed replay session has completed all detached cleanup. LAN advertising
/// waits for this before rebinding/reopening its serial resources for the next Keyer.
#[derive(Clone)]
struct StationSessionEnd {
    complete: Arc<AtomicBool>,
}

impl StationSessionEnd {
    fn new() -> Self {
        Self { complete: Arc::new(AtomicBool::new(false)) }
    }

    fn finish(&self) {
        self.complete.store(true, Ordering::SeqCst);
    }

    async fn wait(&self) {
        while !self.complete.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

/// Returns true once retirement has asked an outer setup task to stop cooperatively.
fn operation_cancelled(cancel: &tokio::sync::watch::Receiver<bool>) -> bool {
    *cancel.borrow()
}

/// Awaits one setup operation or a retirement request. The caller still owns any Session it has
/// already constructed, so choosing cancellation always returns to an explicit cleanup branch.
async fn await_or_cancel<T, F>(
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    future: F,
) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    if operation_cancelled(cancel) {
        return None;
    }
    tokio::select! {
        changed = cancel.changed() => {
            let _ = changed;
            None
        }
        value = future => Some(value),
    }
}

/// A session identity returned to the LAN advertiser after its replay resources are installed.
/// The identity distinguishes sequential Keyers accepted by one persistent advertising operation.
#[derive(Clone)]
struct StationSessionHandle {
    session: Arc<tokio::sync::Mutex<Session>>,
    end: StationSessionEnd,
}

/// Shared state the controller and its background work read and write. Everything here is
/// `Send + Sync`, which is what lets the controller be stored in Tauri's managed state.
struct Shared {
    settings: Mutex<Settings>,
    /// True while the desktop Player is broadcasting an RWKP v2 availability beacon.
    advertising: AtomicBool,
    /// The one installation gate for pending pairing work and installed replay resources.
    lifecycle: Mutex<StationLifecycle>,
    /// The persisted config (identity, relay, code, station settings). Held so pairing reuses
    /// the stable identity, and so UI changes can be written back to disk.
    config: Mutex<Config>,
    /// Where the config persists; `None` disables saving (used only in tests).
    config_path: Option<std::path::PathBuf>,
    sink: EventSink,
}

impl Shared {
    fn emit(&self, event: StationEvent) {
        (self.sink)(event);
    }

    fn log(&self, level: &str, message: impl Into<String>) {
        self.emit(StationEvent::Log { level: level.into(), message: message.into() });
    }

    /// Logs the full detail and shows a short red message under `control`.
    fn fail(&self, control: &str, short: impl Into<String>, detail: impl Into<String>) {
        self.log("warn", detail);
        self.emit(StationEvent::Error { control: control.into(), message: short.into() });
    }

    /// Clears any red message under `control`.
    fn clear_error(&self, control: &str) {
        self.emit(StationEvent::Error { control: control.into(), message: String::new() });
    }

    fn emit_settings(&self) {
        let s = self.settings.lock().unwrap();
        self.emit(StationEvent::Settings {
            keying_port: s.keying_port.clone(),
            key_line: line_name(s.key_line).into(),
            ptt_line: s.ptt_line.map(|l| line_name(l).to_string()).unwrap_or_else(|| "none".into()),
            logger_port: s.logger_port.clone(),
            logger_enabled: s.logger_enabled,
            callsign: s.callsign.clone(),
            keyer_id: s.keyer_id.clone(),
            relay: s.relay.clone(),
            pairing_code: s.pairing_code.clone(),
            transport: s.transport.clone(),
            ptt_hang_ms: s.ptt_hang_ms,
        });
    }

    fn emit_advertising(&self) {
        self.emit(StationEvent::Advertising {
            active: self.advertising.load(Ordering::SeqCst),
        });
    }

    /// Updates the advertised-state event only when the state actually changes. Callers hold the
    /// lifecycle gate whenever this state belongs to an asynchronous pairing operation.
    fn set_advertising_while_gated(&self, active: bool) -> bool {
        let changed = self.advertising.swap(active, Ordering::SeqCst) != active;
        if changed {
            self.emit(StationEvent::Advertising { active });
        }
        changed
    }

    /// Starts a fresh owner generation after the prior operation has been retired.
    fn begin_operation(&self) -> StationOperation {
        self.lifecycle.lock().unwrap().next_operation()
    }

    fn is_current_operation(&self, operation: StationOperation) -> bool {
        self.lifecycle.lock().unwrap().is_current(operation)
    }

    /// Emits an event only while `operation` remains current. This shares the installation gate
    /// with Stop, so a late task cannot publish a connected or advertising state after Stop has
    /// won the lifecycle.
    fn emit_for_operation(&self, operation: StationOperation, event: StationEvent) -> bool {
        let lifecycle = self.lifecycle.lock().unwrap();
        if !lifecycle.is_current(operation) {
            return false;
        }
        self.emit(event);
        true
    }

    fn log_for_operation(
        &self,
        operation: StationOperation,
        level: &str,
        message: impl Into<String>,
    ) {
        let lifecycle = self.lifecycle.lock().unwrap();
        if lifecycle.is_current(operation) {
            self.log(level, message);
        }
    }

    /// Emits only while both the operation and the particular replay session remain installed.
    /// LAN advertising deliberately reuses one operation for successive Keyers, so the session
    /// pointer prevents an old watchdog or control task from affecting its successor.
    fn emit_for_session(
        &self,
        operation: StationOperation,
        session: &Arc<tokio::sync::Mutex<Session>>,
        event: StationEvent,
    ) -> bool {
        let lifecycle = self.lifecycle.lock().unwrap();
        if !lifecycle.is_current(operation)
            || !matches!(
                lifecycle.active.as_ref(),
                Some(active)
                    if active.operation == operation
                        && Arc::ptr_eq(&active.resource.session, session)
            )
        {
            return false;
        }
        self.emit(event);
        true
    }

    fn log_for_session(
        &self,
        operation: StationOperation,
        session: &Arc<tokio::sync::Mutex<Session>>,
        level: &str,
        message: impl Into<String>,
    ) {
        let lifecycle = self.lifecycle.lock().unwrap();
        if lifecycle.is_current(operation)
            && matches!(
                lifecycle.active.as_ref(),
                Some(active)
                    if active.operation == operation
                        && Arc::ptr_eq(&active.resource.session, session)
            )
        {
            self.log(level, message);
        }
    }

    fn owns_current_session(
        &self,
        operation: StationOperation,
        session: &Arc<tokio::sync::Mutex<Session>>,
    ) -> bool {
        let lifecycle = self.lifecycle.lock().unwrap();
        lifecycle.is_current(operation)
            && matches!(
                lifecycle.active.as_ref(),
                Some(active)
                    if active.operation == operation
                        && Arc::ptr_eq(&active.resource.session, session)
            )
    }

    fn clear_error_for_operation(&self, operation: StationOperation, control: &str) {
        let lifecycle = self.lifecycle.lock().unwrap();
        if lifecycle.is_current(operation) {
            self.emit(StationEvent::Error { control: control.into(), message: String::new() });
        }
    }

    fn fail_control_for_operation(
        &self,
        operation: StationOperation,
        control: &str,
        short: impl Into<String>,
        detail: impl Into<String>,
    ) {
        let short = short.into();
        let detail = detail.into();
        let lifecycle = self.lifecycle.lock().unwrap();
        if lifecycle.is_current(operation) {
            self.log("warn", detail);
            self.emit(StationEvent::Error { control: control.into(), message: short });
        }
    }

    /// Reports a pairing failure only if its task still owns the active generation.
    fn fail_for_operation(&self, operation: StationOperation, message: impl Into<String>) {
        let message = message.into();
        let lifecycle = self.lifecycle.lock().unwrap();
        if lifecycle.is_current(operation) {
            self.log("warn", message);
            self.emit(StationEvent::Connection { state: "disconnected".into(), path: "none".into() });
        }
    }

    /// Changes the advertising state only while the owning task is current.
    fn set_advertising_for_operation(&self, operation: StationOperation, active: bool) -> bool {
        let lifecycle = self.lifecycle.lock().unwrap();
        if !lifecycle.is_current(operation) {
            return false;
        }
        self.set_advertising_while_gated(active);
        true
    }

    /// Remembers a WAN pairing only when the request still owns the current operation. The actual
    /// disk write occurs after releasing the gate.
    fn remember_pairing_for_operation(
        &self,
        operation: StationOperation,
        relay: &str,
        code: &str,
    ) -> bool {
        let lifecycle = self.lifecycle.lock().unwrap();
        if !lifecycle.is_current(operation) {
            return false;
        }
        let mut settings = self.settings.lock().unwrap();
        settings.relay = Some(relay.to_string());
        settings.pairing_code = Some(code.to_string());
        true
    }

    fn has_active_session(&self) -> bool {
        self.lifecycle.lock().unwrap().active.is_some()
    }

    fn active_session_snapshot(
        &self,
    ) -> Option<(StationOperation, Arc<tokio::sync::Mutex<Session>>, Arc<KeyingArbiter>)> {
        let lifecycle = self.lifecycle.lock().unwrap();
        lifecycle.active.as_ref().map(|active| {
            (
                active.operation,
                Arc::clone(&active.resource.session),
                Arc::clone(&active.resource.arbiter),
            )
        })
    }

    /// Stores an asynchronous waiter/advertiser only while it owns the current generation.
    /// The caller cooperatively stops and joins a displaced task after releasing the gate.
    fn register_pair_task(&self, operation: StationOperation, task: PairTask) -> Option<PairTask> {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        if lifecycle.is_current(operation) {
            lifecycle
                .pair_task
                .replace(OperationResource { operation, resource: task })
                .map(|owned| owned.resource)
        } else {
            Some(task)
        }
    }

    /// Invalidates pending and installed work before requesting cancellation. The resources leave
    /// the gate atomically and are disposed after it is released, so a stale task cannot affect a
    /// newer replay session.
    fn retire_operation(&self) -> RetiredStationResources {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        lifecycle.next_operation();

        let pair_task = lifecycle.pair_task.take().map(|owned| owned.resource);
        if let Some(task) = &pair_task {
            let _ = task.cancel.send(true);
        }
        let active = lifecycle.active.take().map(|owned| owned.resource);
        let cleanup_tasks = lifecycle
            .cleanup_tasks
            .drain(..)
            .map(|owned| owned.resource)
            .collect();
        if let Some(active) = &active {
            active.stop_bus.store(true, Ordering::SeqCst);
        }
        let was_advertising = self.set_advertising_while_gated(false);

        if active.is_some() {
            self.emit_terminal_session_state();
        }
        // A pending WAN listener has no active replay stack, but Stop must still win the UI state
        // before its cancelled task could resume and attempt an install.
        self.emit(StationEvent::Rtt { ms: None });
        self.emit(StationEvent::Connection { state: "disconnected".into(), path: "none".into() });
        if active.is_some() {
            self.log("info", "Unpaired.");
        } else if was_advertising {
            self.log("info", "LAN Player advertising stopped.");
        }

        RetiredStationResources { pair_task, active, cleanup_tasks }
    }

    /// Removes an active session only if this operation and this exact `Session` still own the
    /// slot. This is used by liveness teardown, where one LAN advertising generation can host
    /// several sequential Keyers. The detached cleanup is registered before releasing the gate,
    /// so a concurrent Stop or replacement will join it instead of racing a new session.
    fn retire_active_session_if_owner(
        &self,
        operation: StationOperation,
        session: &Arc<tokio::sync::Mutex<Session>>,
        message: &str,
    ) -> bool {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        let owns_session = lifecycle.is_current(operation)
            && matches!(
                lifecycle.active.as_ref(),
                Some(active)
                    if active.operation == operation
                        && Arc::ptr_eq(&active.resource.session, session)
            );
        if !owns_session {
            return false;
        }
        let active = lifecycle.active.take().expect("checked active session owner").resource;
        active.stop_bus.store(true, Ordering::SeqCst);
        // The cleanup task starts after this gate unlocks. It owns only the retired resources,
        // never a slot that a newer operation can install into.
        let cleanup = tokio::spawn(dispose_active_station_session(active, false));
        lifecycle.cleanup_tasks.push(OperationResource { operation, resource: cleanup });
        self.emit_terminal_session_state();
        self.emit(StationEvent::Rtt { ms: None });
        self.emit(StationEvent::Connection { state: "disconnected".into(), path: "none".into() });
        self.log("warn", message);
        true
    }

    fn emit_terminal_session_state(&self) {
        self.emit(StationEvent::KeyState { down: false });
        self.emit(StationEvent::Ptt { asserted: false });
        self.emit(StationEvent::LoggerSending { active: false });
        self.emit(StationEvent::ForwardRules { rules: Vec::new() });
        self.emit(StationEvent::FlexDiscovery { enabled: false });
    }

    /// Copies the live UI settings into the persisted config and writes it to disk. A write
    /// failure is logged, not fatal — the app still runs, it just will not remember.
    fn persist(&self) {
        let Some(path) = &self.config_path else { return };
        {
            let s = self.settings.lock().unwrap();
            let mut config = self.config.lock().unwrap();
            config.relay = s
                .relay
                .as_deref()
                .and_then(|r| r.parse().ok())
                .map(|address| rwk_core::config::RelayConfig { address });
            config.pairing_code = s.pairing_code.clone();
            config.callsign = s.callsign.clone();
            config.keyer_id = s.keyer_id.clone();
            config.station.keying_port = s.keying_port.clone();
            config.station.key_line = Some(line_name(s.key_line).to_string());
            config.station.ptt_line =
                Some(s.ptt_line.map(|l| line_name(l).to_string()).unwrap_or_else(|| "none".into()));
            config.station.logger_port = s.logger_port.clone();
            config.station.logger_enabled = s.logger_enabled;
            config.station.ptt_hang_ms = Some(s.ptt_hang_ms);
            config.transport = Some(s.transport.clone());
        }
        let config = self.config.lock().unwrap();
        if let Err(e) = config.save(path) {
            self.log("warn", format!("Could not save config: {e}"));
        }
    }
}

/// Serializes synchronous lifecycle commands without keeping a mutex guard across their detached
/// cleanup awaits. A later Start or Stop waits until the current command has either installed its
/// task or completed retirement, so it cannot mint a new operation after that later command won.
struct StationTransitionGuard<'a> {
    transitioning: &'a Mutex<bool>,
    ready: &'a Condvar,
}

impl Drop for StationTransitionGuard<'_> {
    fn drop(&mut self) {
        let mut transitioning = self.transitioning.lock().unwrap_or_else(|error| error.into_inner());
        *transitioning = false;
        self.ready.notify_all();
    }
}

/// The Station controller. Construct with [`StationController::start`], then drive it with the
/// command methods; it emits [`StationEvent`]s to the sink for the UI to render.
pub struct StationController {
    shared: Arc<Shared>,
    runtime: tokio::runtime::Runtime,
    /// Commands claim this gate only while changing lifecycle ownership. It is deliberately not
    /// held while detached session cleanup awaits network work.
    transitioning: Mutex<bool>,
    transition_ready: Condvar,
}

/// Resources built for a replay session before the final installation-gate commit. If Stop or a
/// replacement wins while setup is in progress, this private bundle is torn down without ever
/// entering `StationLifecycle::active`.
struct PendingStationSession {
    session: Arc<tokio::sync::Mutex<Session>>,
    pump: tokio::task::JoinHandle<rwk_core::Result<()>>,
    stop_bus: Arc<AtomicBool>,
    logger: Option<StationWinKeyerHost>,
    arbiter: Arc<KeyingArbiter>,
    end: StationSessionEnd,
}

/// The resources a running pairing owns, torn down together on unpair.
struct ActiveSession {
    /// The live session, shared with the control task so pushed forward rules can be applied
    /// over the same tunnel without tearing the link down.
    session: Arc<tokio::sync::Mutex<Session>>,
    /// Pump task that forwards tunnel datagrams into the replay driver. Aborting it stops
    /// the inflow; the driver's own threads stop when its handle drops.
    pump: tokio::task::JoinHandle<rwk_core::Result<()>>,
    /// The control-channel task (receives forward-rule pushes + sends discovery announces).
    control: tokio::task::JoinHandle<()>,
    /// Wakes the control task out of link/session awaits so it can join its Flex child cleanly.
    control_stop: tokio::sync::watch::Sender<bool>,
    /// The current Flex discovery child, retained so teardown can abort and join it even if the
    /// control task itself is cancelled while blocked in a read.
    flex_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// The liveness task is retained so explicit Stop can cancel it before session shutdown.
    watchdog: tokio::task::JoinHandle<()>,
    /// The bus forwarder is joined before releasing the final Session handle.
    bus_forwarder: std::thread::JoinHandle<()>,
    /// A flag the bus-forwarding and control tasks watch so they exit when the session ends.
    stop_bus: Arc<AtomicBool>,
    /// The logger-WinKeyer host, if the operator enabled it for this session. Dropped (and
    /// its port released) when the session ends.
    logger: Option<StationWinKeyerHost>,
    /// The arbiter that owns the serial output. Swapped in place when the port or its lines change.
    arbiter: Arc<KeyingArbiter>,
    /// Lets the LAN advertiser wait until detached shutdown has released this session's ports.
    end: StationSessionEnd,
}

impl StationController {
    /// Starts the controller: loads (or creates) the persisted config, applies the saved
    /// settings, and emits them. Unlike the Client there is no sidetone to bring up — the
    /// Station keys a radio, it does not generate practice audio.
    pub fn start(sink: EventSink) -> Self {
        let path = app_paths::config_path(Role::Station);
        Self::start_with_config(sink, Some(path))
    }

    /// Starts the controller against an explicit config path (or `None` to disable
    /// persistence, for tests). Loads the config if the file exists, otherwise generates a
    /// fresh identity and saves it so the next run reuses the same key.
    pub fn start_with_config(sink: EventSink, config_path: Option<std::path::PathBuf>) -> Self {
        let (config, settings) = load_or_create(&config_path);

        let shared = Arc::new(Shared {
            settings: Mutex::new(settings),
            advertising: AtomicBool::new(false),
            lifecycle: Mutex::new(StationLifecycle::default()),
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
            transitioning: Mutex::new(false),
            transition_ready: Condvar::new(),
        };
        // Persist once so a freshly generated pairing token survives the next restart.
        controller.shared.persist();
        controller.shared.emit_settings();
        controller.shared.emit_advertising();
        controller.emit_ports();
        controller
    }

    /// Re-emits settings, LAN advertising state, and the serial-port inventory after the webview
    /// subscribes to the event channel.
    pub fn request_state(&self) {
        self.shared.emit_settings();
        self.shared.emit_advertising();
        self.emit_ports();
    }

    /// Selects the pairing transport: "wan" (relay + code) or "lan" (advertise this Player).
    pub fn set_transport(&self, transport: &str) {
        let t = if transport.eq_ignore_ascii_case("lan") { "lan" } else { "wan" };
        self.shared.settings.lock().unwrap().transport = t.to_string();
        self.shared.persist();
        self.shared.emit_settings();
    }

    /// Sets the desktop Player callsign advertised in the bounded RWKP v2 name field.
    pub fn set_callsign(&self, callsign: &str) {
        self.shared.settings.lock().unwrap().callsign = bounded_lan_label(callsign);
        self.shared.persist();
        self.shared.emit_settings();
    }

    /// Sets the optional desktop Player label advertised with its callsign.
    pub fn set_keyer_id(&self, keyer_id: &str) {
        self.shared.settings.lock().unwrap().keyer_id = bounded_lan_label(keyer_id);
        self.shared.persist();
        self.shared.emit_settings();
    }

    // ─── Ports / keying configuration ──────────────────────────────────────────

    /// Lists available serial ports for the keying-output and logger pickers.
    #[must_use]
    pub fn serial_ports() -> Vec<String> {
        enumerate_ports()
            .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
            .unwrap_or_default()
    }

    fn emit_ports(&self) {
        self.shared.emit(StationEvent::Ports { available: Self::serial_ports() });
    }

    /// Claims the command-transition gate. The actual mutex guard is released before callers run
    /// detached cleanup, while this RAII marker keeps concurrent commands from interleaving their
    /// retire/begin phases.
    fn begin_transition(&self) -> StationTransitionGuard<'_> {
        let mut transitioning = self.transitioning.lock().unwrap_or_else(|error| error.into_inner());
        while *transitioning {
            transitioning = self
                .transition_ready
                .wait(transitioning)
                .unwrap_or_else(|error| error.into_inner());
        }
        *transitioning = true;
        drop(transitioning);
        StationTransitionGuard {
            transitioning: &self.transitioning,
            ready: &self.transition_ready,
        }
    }

    /// Whether a pairing session is live (the keying — and maybe logger — port is held open).
    fn is_paired(&self) -> bool {
        self.shared.has_active_session()
    }

    /// Installs an outer setup task before allowing it to run. The start gate closes the tiny
    /// spawn/register race: a task that loses ownership before registration never begins setup.
    fn install_pair_task(
        &self,
        operation: StationOperation,
        handle: tokio::task::JoinHandle<()>,
        cancel: tokio::sync::watch::Sender<bool>,
        start: tokio::sync::oneshot::Sender<()>,
    ) {
        let discarded = self.shared.register_pair_task(operation, PairTask { handle, cancel });
        if let Some(task) = discarded {
            let _ = task.cancel.send(true);
            let _ = self.runtime.spawn(async move {
                let _ = task.handle.await;
            });
        } else {
            let _ = start.send(());
        }
    }

    /// The serial ports held open by the current session, if any. The Station only opens its
    /// keying (and logger) port at pairing time, so held ports are known only while paired.
    fn held_ports(&self) -> Vec<String> {
        if !self.is_paired() {
            return Vec::new();
        }
        let s = self.shared.settings.lock().unwrap();
        let mut held = Vec::new();
        if let Some(p) = &s.keying_port {
            held.push(p.clone());
        }
        if s.logger_enabled {
            if let Some(p) = &s.logger_port {
                held.push(p.clone());
            }
        }
        held
    }

    /// Re-enumerates serial ports and pushes the fresh list. A selected keying/logger port
    /// that vanishes while **not** paired drops to `None`; while paired the selections are
    /// left untouched (the session owns the open ports — dropping them mid-QSO would be a
    /// bug, and a busy COM port is often hidden from enumeration on Windows anyway).
    pub fn reconcile_ports(&self) {
        let mut present = Self::serial_ports();
        for held in self.held_ports() {
            if !present.contains(&held) {
                present.push(held);
            }
        }
        present.sort_by(|a, b| natural_port_key(a).cmp(&natural_port_key(b)));
        self.shared.emit(StationEvent::Ports { available: present.clone() });

        // While paired, do not disturb the selections (the session holds its ports).
        if self.is_paired() {
            return;
        }

        let (keying_gone, logger_gone) = {
            let s = self.shared.settings.lock().unwrap();
            (port_missing(&s.keying_port, &present), port_missing(&s.logger_port, &present))
        };
        if !(keying_gone || logger_gone) {
            return;
        }
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
        self.shared.persist();
        self.shared.emit_settings();
    }

    /// Reopens the keying output on the running session, so a changed port or line takes effect
    /// without dropping the tunnel. The old output is released before the new one opens.
    fn apply_keying_live(&self) {
        let (operation, session, arbiter) = match self.shared.active_session_snapshot() {
            Some(active) => active,
            None => return,
        };
        let (port, key_line, ptt_line) = {
            let s = self.shared.settings.lock().unwrap();
            (s.keying_port.clone(), s.key_line, s.ptt_line)
        };
        let ptt_line = ptt_line_for(key_line, ptt_line);
        let shared = Arc::clone(&self.shared);
        arbiter.replace_adapter_with(move || {
            if !shared.owns_current_session(operation, &session) {
                return SerialKeyingAdapter::led_only();
            }
            keying_adapter_for_operation(&shared, operation, port, key_line, ptt_line)
        });
    }

    /// Restarts the logger input on the running session for the current port and enable state.
    /// The old host is detached under the same gate that owns the session, then stopped before a
    /// replacement opens its port. A late replacement is stopped rather than installed.
    fn apply_logger_live(&self) {
        let (operation, session, arbiter) = match self.shared.active_session_snapshot() {
            Some(active) => active,
            None => return,
        };
        let old = {
            let mut lifecycle = self.shared.lifecycle.lock().unwrap();
            if !lifecycle.is_current(operation) {
                return;
            }
            match lifecycle.active.as_mut() {
                Some(active)
                    if active.operation == operation
                        && Arc::ptr_eq(&active.resource.session, &session) => active.resource.logger.take(),
                _ => return,
            }
        };
        if let Some(mut host) = old {
            host.stop();
        }
        if !self.shared.owns_current_session(operation, &session) {
            return;
        }
        let (port, enabled) = {
            let s = self.shared.settings.lock().unwrap();
            (s.logger_port.clone(), s.logger_enabled)
        };
        let logger = open_logger_for_session(&self.shared, operation, &session, &arbiter, port, enabled);
        let discarded = {
            let mut lifecycle = self.shared.lifecycle.lock().unwrap();
            if !lifecycle.is_current(operation) {
                logger
            } else {
                match lifecycle.active.as_mut() {
                    Some(active)
                        if active.operation == operation
                            && Arc::ptr_eq(&active.resource.session, &session) => {
                        std::mem::replace(&mut active.resource.logger, logger)
                    }
                    _ => logger,
                }
            }
        };
        if let Some(mut host) = discarded {
            host.stop();
        }
    }

    /// Selects the serial port that keys the radio, or `None` to clear it. The port is opened
    /// when pairing starts, so the replay driver owns it for the session; changing it while
    /// paired takes effect on the next pairing.
    pub fn select_keying_port(&self, port: Option<String>) {
        // Exclusive with the logger port: one port serves one job.
        if let Some(p) = &port {
            if self.shared.settings.lock().unwrap().logger_port.as_deref() == Some(p.as_str()) {
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
        if self.is_paired() {
            self.apply_keying_live();
        } else if let Some(name) = &port {
            // Not paired, so nothing is open yet. Check the port now, not at pairing.
            probe_keying_port(&self.shared, name);
        }
    }

    /// Sets which control line keys the radio ("dtr" or "rts").
    pub fn set_key_line(&self, line: &str) {
        let parsed = parse_line(line).unwrap_or(KeyingLine::Dtr);
        let ptt = self.shared.settings.lock().unwrap().ptt_line;
        if ptt == Some(parsed) {
            self.shared.fail(
                control::KEYING,
                format!("{} is the PTT line", line_name(parsed).to_uppercase()),
                format!(
                    "Key and PTT must be on different lines; {} is already the PTT line.",
                    line_name(parsed).to_uppercase()
                ),
            );
            return;
        }
        self.shared.settings.lock().unwrap().key_line = parsed;
        self.shared.clear_error(control::KEYING);
        self.shared.persist();
        self.shared.emit_settings();
        if self.is_paired() {
            self.apply_keying_live();
        }
    }

    /// Sets which control line asserts PTT ("dtr", "rts", or "none").
    pub fn set_ptt_line(&self, line: &str) {
        let ptt = parse_line(line);
        let key = self.shared.settings.lock().unwrap().key_line;
        if ptt == Some(key) {
            self.shared.fail(
                control::KEYING,
                format!("{} is the key line", line_name(key).to_uppercase()),
                format!(
                    "Key and PTT must be on different lines; {} is already the key line.",
                    line_name(key).to_uppercase()
                ),
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

    /// Sets the PTT hang time (how long PTT is held after key-up), clamped to the valid range.
    pub fn set_ptt_hang(&self, ms: u32) {
        let ms = ms.clamp(PTT_HANG_MIN_MS, PTT_HANG_MAX_MS);
        self.shared.settings.lock().unwrap().ptt_hang_ms = u64::from(ms);
        self.shared.persist();
        self.shared.emit_settings();
    }

    /// Generates a new pairing token. The Station shows it and the Client types it.
    pub fn regenerate_token(&self) {
        self.shared.settings.lock().unwrap().pairing_code = Some(generate_token());
        self.shared.persist();
        self.shared.emit_settings();
    }

    /// Selects the serial port the local logger sends WinKeyer protocol on, or `None`. A logger
    /// port turns the logger input on; clearing it turns the input off.
    pub fn set_logger_port(&self, port: Option<String>) {
        // Exclusive with the keying port: one port serves one job.
        if let Some(p) = &port {
            if self.shared.settings.lock().unwrap().keying_port.as_deref() == Some(p.as_str()) {
                self.shared.fail(
                    control::LOGGER,
                    format!("{p} is the keying port"),
                    format!("{p} is already the keying port; a port can serve one job."),
                );
                return;
            }
        }
        {
            let mut s = self.shared.settings.lock().unwrap();
            s.logger_port = port.clone();
            s.logger_enabled = port.is_some();
        }
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

    /// Enables or disables the logger input. Takes effect on the next pairing (the logger
    /// host starts alongside the replay driver so both share the one keying output).
    pub fn set_logger_enabled(&self, enabled: bool) {
        self.shared.settings.lock().unwrap().logger_enabled = enabled;
        self.shared.log("info", if enabled { "Logger input enabled." } else { "Logger input disabled." });
        self.shared.persist();
        self.shared.emit_settings();
        if self.is_paired() {
            self.apply_logger_live();
        }
    }

    // ─── Pairing ──────────────────────────────────────────────────────────────

    /// Pairs with the Client using a shared pairing code over the configured relay, then
    /// starts replaying keyed edges onto the radio. The Station is the responder: it brings
    /// its side of the link up and waits for the Client to initiate the handshake.
    ///
    /// Non-blocking: the rendezvous and handshake run on the controller's runtime, and the
    /// outcome is reported through [`StationEvent::Connection`] / [`StationEvent::Log`].
    pub fn listen(&self, relay: &str, code: &str) {
        // Retire first. The operation generation is the safety boundary; cooperative cancellation
        // and cleanup happen after the lifecycle gate has detached the old resources.
        let _transition = self.begin_transition();
        self.disconnect_inner();
        let relay_addr: std::net::SocketAddr = match relay.parse() {
            Ok(a) => a,
            Err(e) => {
                self.shared.log("warn", format!("Invalid relay address '{relay}': {e}"));
                self.shared.emit(StationEvent::Connection {
                    state: "disconnected".into(),
                    path: "none".into(),
                });
                return;
            }
        };

        let operation = self.shared.begin_operation();
        if !self.shared.remember_pairing_for_operation(operation, relay, code) {
            return;
        }
        // Persist after releasing the lifecycle gate; a later command can safely overwrite the
        // shared settings before this write snapshots them.
        self.shared.persist();
        let (keying_port, logger_port, logger_enabled) = {
            let s = self.shared.settings.lock().unwrap();
            (s.keying_port.clone(), s.logger_port.clone(), s.logger_enabled)
        };

        let code = code.to_string();
        let shared = Arc::clone(&self.shared);
        let (start, start_rx) = tokio::sync::oneshot::channel();
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        if !shared.emit_for_operation(
            operation,
            StationEvent::Connection { state: "connecting".into(), path: "none".into() },
        ) {
            return;
        }
        shared.log_for_operation(operation, "info", "Waiting for the client to pair…");

        let handle = self.runtime.spawn(async move {
            if start_rx.await.is_err()
                || operation_cancelled(&cancelled)
                || !shared.is_current_operation(operation)
            {
                return;
            }
            // Clone the persisted config so the WireGuard identity remains stable, then layer
            // the current relay and code on top for this waiter.
            let mut config = shared.config.lock().unwrap().clone();
            config.role = Role::Station;
            config.relay = Some(rwk_core::config::RelayConfig { address: relay_addr });
            config.pairing_code = Some(code);

            // The first rendezvous can race the relay or the Client's arrival just after a
            // restart; retirement selects the cancellation receiver rather than aborting a task
            // that might later own a Session.
            let connected = match await_or_cancel(&mut cancelled, connect_with_relay_retry(&config)).await {
                Some(Ok(connected)) => connected,
                Some(Err(error)) => {
                    let _ = shared.emit_for_operation(
                        operation,
                        StationEvent::Error {
                            control: "relay".into(),
                            message: "Relay not reachable".into(),
                        },
                    );
                    shared.fail_for_operation(operation, format!("link failed: {error}"));
                    return;
                }
                None => return,
            };
            // A `Connected` candidate is dropped here if Stop/replacement won before it became a
            // Session. Once `bring_up_station` starts a Session, its cancellation branch awaits
            // explicit shutdown before this outer task can finish.
            if operation_cancelled(&cancelled) || !shared.is_current_operation(operation) {
                return;
            }
            let _ = bring_up_station(
                &shared,
                operation,
                &mut cancelled,
                connected,
                keying_port,
                logger_port,
                logger_enabled,
            )
            .await;
        });
        self.install_pair_task(operation, handle, cancel, start);
    }

    // ─── LAN: advertise this desktop Player and await a Keyer ─────────────────

    /// Begins or stops RWKP v2 advertising. The desktop Player owns the callsign/ID beacon,
    /// then waits on that advertised port as the WireGuard responder while a Keyer selects it.
    pub fn lan_advertise(&self, on: bool) {
        // This must precede even validation: every advertising request invalidates a former
        // waiter/advertiser before any new work can await or claim resources.
        let _transition = self.begin_transition();
        self.disconnect_inner();
        if !on {
            return;
        }
        if self.shared.settings.lock().unwrap().callsign.trim().is_empty() {
            self.shared.log("warn", "Enter a Player callsign before advertising on the LAN.");
            return;
        }
        let operation = self.shared.begin_operation();
        let (keying_port, logger_port, logger_enabled) = {
            let settings = self.shared.settings.lock().unwrap();
            (
                settings.keying_port.clone(),
                settings.logger_port.clone(),
                settings.logger_enabled,
            )
        };
        let shared = Arc::clone(&self.shared);
        if !shared.set_advertising_for_operation(operation, true) {
            return;
        }
        if !shared.emit_for_operation(
            operation,
            StationEvent::Connection { state: "connecting".into(), path: "none".into() },
        ) {
            return;
        }
        shared.log_for_operation(
            operation,
            "info",
            "Advertising this desktop Player on the LAN; waiting for a Keyer…",
        );
        let (start, start_rx) = tokio::sync::oneshot::channel();
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let handle = self.runtime.spawn(async move {
            if start_rx.await.is_err()
                || operation_cancelled(&cancelled)
                || !shared.is_current_operation(operation)
            {
                return;
            }
            lan_advertise_and_await(
                shared,
                operation,
                &mut cancelled,
                keying_port,
                logger_port,
                logger_enabled,
            )
            .await;
        });
        self.install_pair_task(operation, handle, cancel, start);
    }

    /// Retires the current operation. Callers hold the command-transition marker, but no mutex
    /// guard remains held while the detached resource cleanup awaits network work.
    fn disconnect_inner(&self) {
        let retired = self.shared.retire_operation();
        self.runtime.block_on(dispose_retired_station_resources(retired));
    }

    /// Tears the session, replay stack, logger host, pairing task, and keying output down,
    /// returning to the unpaired state. This claims the transition marker before retirement so a
    /// concurrently resumed Start cannot create a new generation after Stop.
    pub fn disconnect(&self) {
        let _transition = self.begin_transition();
        self.disconnect_inner();
    }
}

/// Stops resources detached by [`Shared::retire_operation`] after the installation gate has
/// released. The new operation cannot be reached through this bundle.
async fn dispose_retired_station_resources(retired: RetiredStationResources) {
    // Retirement sent the cooperative cancellation signal while holding the gate. Joining the
    // setup task here makes sure a private candidate Session finishes its own cleanup before a
    // replacement is allowed to reopen the same tunnel or serial resources.
    if let Some(task) = retired.pair_task {
        let _ = task.handle.await;
    }
    if let Some(active) = retired.active {
        dispose_active_station_session(active, true).await;
    }
    for cleanup in retired.cleanup_tasks {
        let _ = cleanup.await;
    }
}

/// Stops a candidate that lost the final installation race. It was never published in `active`,
/// but it may already own a tunnel Session, edge pump, logger port, and replay driver.
async fn dispose_pending_station_session(pending: PendingStationSession) {
    let PendingStationSession { session, pump, stop_bus, logger, arbiter, end } = pending;
    stop_bus.store(true, Ordering::SeqCst);
    if let Some(mut logger) = logger {
        logger.stop();
    }
    pump.abort();
    // Dropping a pump is what drops its replay `DriverHandle`; await cancellation so that Drop
    // has joined the replay threads before a LAN cycle may reopen the keying port.
    let _ = pump.await;
    session.lock().await.shutdown().await;
    // The arbiter owns the serial adapter and its observer holds a session Arc. Drop both before
    // waking the LAN loop, so `StationSessionEnd` is a genuine port/resource-release barrier.
    drop(arbiter);
    drop(session);
    end.finish();
}

/// Stops an installed session after it has been detached from the lifecycle gate. A liveness task
/// never aborts its own JoinHandle, while explicit Stop cancels that watcher before shutdown.
async fn dispose_active_station_session(active: ActiveSession, abort_watchdog: bool) {
    let ActiveSession {
        session,
        pump,
        control,
        control_stop,
        flex_task,
        watchdog,
        bus_forwarder,
        stop_bus,
        logger,
        arbiter,
        end,
    } = active;
    stop_bus.store(true, Ordering::SeqCst);
    if let Some(mut logger) = logger {
        logger.stop();
    }
    pump.abort();
    let _ = control_stop.send(true);
    if abort_watchdog {
        watchdog.abort();
    }
    // The pump is force-cancelled because it owns no child tasks. The control task receives a
    // cooperative stop signal, so it can abort and join its Flex listener before returning.
    let _ = pump.await;
    let _ = control.await;
    // The liveness-triggered cleanup is a separate task, so its watcher returns immediately
    // after scheduling this function. Joining it is therefore safe in both teardown paths.
    let _ = watchdog.await;
    let flex_task = flex_task.lock().unwrap().take();
    if let Some(task) = flex_task {
        task.abort();
        let _ = task.await;
    }
    let _ = tokio::task::spawn_blocking(move || {
        let _ = bus_forwarder.join();
    })
    .await;
    session.lock().await.shutdown().await;
    // Release every resource that can retain the serial adapter or Session before signalling
    // the LAN re-arm path. In particular, the arbiter owns the keying port and observer Arc.
    drop(arbiter);
    drop(session);
    end.finish();
}

/// Loads the config at `path` (if present) or creates a fresh one and saves it, then derives
/// the live UI [`Settings`] from it. A fresh config persists its new identity immediately so
/// the next run reuses the same WireGuard key.
fn load_or_create(path: &Option<std::path::PathBuf>) -> (Config, Settings) {
    let config = match path {
        Some(p) if p.exists() => Config::load(p).unwrap_or_else(|_| {
            Config::new(Role::Station).expect("generate station identity")
        }),
        _ => {
            let config = Config::new(Role::Station).expect("generate station identity");
            if let Some(p) = path {
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
        key_line,
        ptt_line,
        logger_port: config.station.logger_port.clone(),
        logger_enabled: config.station.logger_enabled,
        relay: config.relay.as_ref().map(|r| r.address.to_string()),
        pairing_code: Some(config.pairing_code.clone().unwrap_or_else(generate_token)),
        callsign: bounded_lan_label(&config.callsign),
        keyer_id: bounded_lan_label(&config.keyer_id),
        transport: match config.transport.as_deref() {
            Some("lan") => "lan".to_string(),
            _ => "wan".to_string(),
        },
        ptt_hang_ms: config
            .station
            .ptt_hang_ms
            .unwrap_or(PTT_HANG_DEFAULT_MS)
            .clamp(u64::from(PTT_HANG_MIN_MS), u64::from(PTT_HANG_MAX_MS)),
    };
    (config, settings)
}

/// Opens the Station keying output for an operation. Before and after the blocking serial open,
/// the operation token is checked; a handle opened after Stop is dropped with the candidate rather
/// than being allowed to report a stale pairing state.
fn keying_adapter_for_operation(
    shared: &Arc<Shared>,
    operation: StationOperation,
    port: Option<String>,
    key_line: KeyingLine,
    ptt_line: Option<KeyingLine>,
) -> SerialKeyingAdapter {
    if !shared.is_current_operation(operation) {
        return SerialKeyingAdapter::led_only();
    }
    let Some(port_name) = port else {
        shared.clear_error_for_operation(operation, control::KEYING);
        shared.log_for_operation(operation, "info", "No keying port selected — keying the LED only.");
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
            if !shared.is_current_operation(operation) {
                drop(out);
                return SerialKeyingAdapter::led_only();
            }
            shared.clear_error_for_operation(operation, control::KEYING);
            shared.log_for_operation(operation, "info", format!("Keying output open on {port_name}."));
            SerialKeyingAdapter::new(out)
        }
        Err(error) => {
            shared.fail_control_for_operation(
                operation,
                control::KEYING,
                format!("{port_name} not available"),
                format!("keying port {port_name} failed: {error}. Keying the LED only; pick an available port to drive the radio."),
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

/// Opens the logger input on `port` when enabled. A host that opens after its operation became
/// stale is stopped immediately instead of being returned to a candidate or active session.
fn open_logger_for_session(
    shared: &Arc<Shared>,
    operation: StationOperation,
    session: &Arc<tokio::sync::Mutex<Session>>,
    arbiter: &Arc<KeyingArbiter>,
    port: Option<String>,
    enabled: bool,
) -> Option<StationWinKeyerHost> {
    if !enabled || !shared.is_current_operation(operation) {
        return None;
    }
    let Some(lp) = port else {
        shared.log_for_operation(operation, "warn", "Logger input enabled but no port selected.");
        return None;
    };
    let reporter: HostReporter = make_host_reporter(Arc::clone(shared), operation, Arc::clone(session));
    match StationWinKeyerHost::open(&lp, arbiter.local_handle(), reporter) {
        Ok(mut host) => {
            if !shared.is_current_operation(operation) {
                host.stop();
                return None;
            }
            shared.clear_error_for_operation(operation, control::LOGGER);
            shared.log_for_operation(operation, "info", format!("Logger input active on {}.", host.port_name()));
            Some(host)
        }
        Err(error) => {
            shared.fail_control_for_operation(
                operation,
                control::LOGGER,
                format!("{lp} not available"),
                format!("Logger input unavailable: {error}"),
            );
            None
        }
    }
}

/// Builds the arbiter's line observer: emits KEY/PTT to the UI only on an actual change and only
/// while the exact replay session remains installed.
fn make_line_observer(
    shared: Arc<Shared>,
    operation: StationOperation,
    session: Arc<tokio::sync::Mutex<Session>>,
) -> LineObserver {
    let last_key = Arc::new(Mutex::new(None::<bool>));
    let last_ptt = Arc::new(Mutex::new(None::<bool>));
    Arc::new(move |u: LineUpdate| {
        {
            let mut lk = last_key.lock().unwrap_or_else(|e| e.into_inner());
            if *lk != Some(u.key_down) {
                *lk = Some(u.key_down);
                let _ = shared.emit_for_session(operation, &session, StationEvent::KeyState { down: u.key_down });
            }
        }
        {
            let mut lp = last_ptt.lock().unwrap_or_else(|e| e.into_inner());
            if *lp != Some(u.ptt) {
                *lp = Some(u.ptt);
                let _ = shared.emit_for_session(operation, &session, StationEvent::Ptt { asserted: u.ptt });
            }
        }
    })
}

/// Builds the logger host's reporter: logs its lines and bridges the sending indicator only while
/// its owning replay session remains installed.
fn make_host_reporter(
    shared: Arc<Shared>,
    operation: StationOperation,
    session: Arc<tokio::sync::Mutex<Session>>,
) -> HostReporter {
    Arc::new(move |report: HostReport| match report {
        HostReport::Log { level, message } => {
            shared.log_for_session(operation, &session, &level, message);
        }
        HostReport::Sending(active) => {
            let _ = shared.emit_for_session(operation, &session, StationEvent::LoggerSending { active });
        }
        // The reader's failure is also logged as a Log line; nothing more to show.
        HostReport::ReadFailed(_) => {}
    })
}

/// Persistent desktop Player LAN responder. Each cycle binds the exact WireGuard port carried in
/// its RWKP v2 beacon, advertises CW plus Router capability, and waits for a Keyer to initiate.
/// After a dropped session it re-arms automatically until the operator presses Stop.
async fn lan_advertise_and_await(
    shared: Arc<Shared>,
    operation: StationOperation,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    keying_port: Option<String>,
    logger_port: Option<String>,
    logger_enabled: bool,
) {
    if operation_cancelled(cancel) || !shared.is_current_operation(operation) {
        return;
    }
    let identity = match shared.config.lock().unwrap().identity.to_identity() {
        Ok(identity) => identity,
        Err(error) => {
            let _ = shared.set_advertising_for_operation(operation, false);
            shared.fail_for_operation(operation, format!("identity error: {error}"));
            return;
        }
    };

    loop {
        if operation_cancelled(cancel) || !shared.is_current_operation(operation) {
            return;
        }
        let Some(bound) = await_or_cancel(cancel, tunnel::lan_bind()).await else {
            return;
        };
        let (socket, wg_port) = match bound {
            Ok(pair) => pair,
            Err(error) => {
                let _ = shared.set_advertising_for_operation(operation, false);
                shared.fail_for_operation(operation, format!("LAN bind failed: {error}"));
                return;
            }
        };
        if operation_cancelled(cancel) || !shared.is_current_operation(operation) {
            return;
        }
        let (callsign, keyer_id) = {
            let settings = shared.settings.lock().unwrap();
            (settings.callsign.clone(), settings.keyer_id.clone())
        };
        let beacon = Beacon {
            callsign,
            keyer_id,
            public_key: identity.public_bytes(),
            wg_port,
            capabilities: CAPABILITY_CW | CAPABILITY_ROUTER,
        };
        let advertiser = match LanAdvertiser::new(beacon) {
            Ok(advertiser) => advertiser,
            Err(error) => {
                let _ = shared.set_advertising_for_operation(operation, false);
                shared.fail_for_operation(operation, format!("LAN advertisement failed: {error}"));
                return;
            }
        };
        let beacon_task = tokio::spawn(advertiser.run());
        if !shared.set_advertising_for_operation(operation, true) {
            beacon_task.abort();
            let _ = beacon_task.await;
            return;
        }

        let connected = match await_or_cancel(cancel, tunnel::lan_await_initiator(socket, &identity)).await {
            Some(Ok(connected)) => connected,
            Some(Err(error)) => {
                beacon_task.abort();
                let _ = beacon_task.await;
                if operation_cancelled(cancel) || !shared.is_current_operation(operation) {
                    return;
                }
                shared.log_for_operation(
                    operation,
                    "warn",
                    format!("LAN responder cycle ended: {error}; re-advertising."),
                );
                continue;
            }
            None => {
                beacon_task.abort();
                let _ = beacon_task.await;
                return;
            }
        };
        beacon_task.abort();
        let _ = beacon_task.await;
        if operation_cancelled(cancel) || !shared.is_current_operation(operation) {
            return;
        }
        if !shared.set_advertising_for_operation(operation, false) {
            return;
        }

        if let Some(handle) = bring_up_station(
            &shared,
            operation,
            cancel,
            connected,
            keying_port.clone(),
            logger_port.clone(),
            logger_enabled,
        )
        .await
        {
            if !wait_for_station_session_end(&shared, operation, cancel, &handle).await {
                return;
            }
            if !shared.emit_for_operation(
                operation,
                StationEvent::Connection { state: "connecting".into(), path: "none".into() },
            ) {
                return;
            }
            shared.log_for_operation(
                operation,
                "info",
                "Keyer disconnected; re-advertising this desktop Player on the LAN…",
            );
        } else {
            if operation_cancelled(cancel) || !shared.is_current_operation(operation) {
                return;
            }
            // A local bring-up problem should not spin a tight bind/advertise loop.
            if await_or_cancel(cancel, tokio::time::sleep(std::time::Duration::from_secs(1)))
                .await
                .is_none()
            {
                return;
            }
            if operation_cancelled(cancel) || !shared.is_current_operation(operation) {
                return;
            }
        }
    }
}

/// Waits for a particular Player session to finish detached cleanup. Merely observing an empty
/// active slot is insufficient: the old session can still own serial ports while shutdown awaits.
async fn wait_for_station_session_end(
    shared: &Arc<Shared>,
    operation: StationOperation,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    handle: &StationSessionHandle,
) -> bool {
    loop {
        if operation_cancelled(cancel) || !shared.is_current_operation(operation) {
            return false;
        }
        if !shared.owns_current_session(operation, &handle.session) {
            return await_or_cancel(cancel, handle.end.wait()).await.is_some()
                && !operation_cancelled(cancel)
                && shared.is_current_operation(operation);
        }
        if await_or_cancel(cancel, tokio::time::sleep(std::time::Duration::from_millis(25)))
            .await
            .is_none()
        {
            return false;
        }
    }
}

/// Shared post-connect setup for WAN (`listen`) and LAN advertisement (`lan_advertise`): open
/// the keying edge, serial output, arbiter, replay driver, Router control path, and install the
/// running desktop Player session. Every network await reconfirms ownership; the final gate is
/// the only point that publishes a session in `active` or emits its connected state.
async fn bring_up_station(
    shared: &Arc<Shared>,
    operation: StationOperation,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    connected: tunnel::Connected,
    keying_port: Option<String>,
    logger_port: Option<String>,
    logger_enabled: bool,
) -> Option<StationSessionHandle> {
    if operation_cancelled(cancel) || !shared.is_current_operation(operation) {
        return None;
    }
    let path = format!("{:?}", connected.path).to_lowercase();
    let config = shared.config.lock().unwrap().clone();
    let mut session = match Session::start(connected, &config) {
        Ok(session) => session,
        Err(error) => {
            shared.fail_for_operation(operation, format!("session failed: {error}"));
            return None;
        }
    };
    if operation_cancelled(cancel) || !shared.is_current_operation(operation) {
        session.shutdown().await;
        return None;
    }
    let net = session.handle();

    let conn = match await_or_cancel(cancel, net.open_udp(EDGE_PORT)).await {
        Some(Ok(conn)) => conn,
        Some(Err(error)) => {
            session.shutdown().await;
            shared.fail_for_operation(operation, format!("edge open failed: {error}"));
            return None;
        }
        None => {
            session.shutdown().await;
            return None;
        }
    };
    if operation_cancelled(cancel) || !shared.is_current_operation(operation) {
        session.shutdown().await;
        return None;
    }
    let transport = Arc::new(EdgeTransport::over_tunnel(conn));
    let peer = std::net::SocketAddr::new(
        std::net::IpAddr::V4(Role::Station.peer_inner_address()),
        EDGE_PORT,
    );
    if await_or_cancel(cancel, transport.set_peer(peer)).await.is_none()
        || operation_cancelled(cancel)
        || !shared.is_current_operation(operation)
    {
        session.shutdown().await;
        return None;
    }

    // Wrap only after all network setup awaits. The candidate below owns every resource until the
    // final gate accepts it, at which point liveness and control tasks begin under the same gate.
    let session = Arc::new(tokio::sync::Mutex::new(session));
    let (key_line, ptt_line) = {
        let s = shared.settings.lock().unwrap();
        (s.key_line, s.ptt_line)
    };
    // Key and PTT must be on different lines; a duplicate drops PTT rather than fight the key.
    let ptt_line = ptt_line_for(key_line, ptt_line);

    // The serial output is optional: with no keying port (or one that will not open) the Station
    // still runs and the KEY/PTT LEDs follow the Client. Nothing is wired to the radio then.
    let arbiter = Arc::new(KeyingArbiter::with_observer(
        keying_adapter_for_operation(shared, operation, keying_port, key_line, ptt_line),
        Some(make_line_observer(
            Arc::clone(shared),
            operation,
            Arc::clone(&session),
        )),
    ));

    // PTT sequencing: the replayer asserts PTT just ahead of each keyed edge and holds it for the
    // hang time after key-up. With no PTT line there is nothing to sequence.
    let clock: Arc<dyn Clock> = Arc::new(MonotonicClock);
    let ptt_timing = ptt_line.map(|_| PttTiming {
        tail: std::time::Duration::from_millis(shared.settings.lock().unwrap().ptt_hang_ms),
        ..PttTiming::default()
    });
    let mut replayer = tunnel_replayer(clock.as_ref(), EdgeJitterProfile::PathAdaptive, ptt_timing);
    replayer.begin_session(1);

    let bus = EventBus::new();
    let driver = spawn_driver(
        replayer,
        Box::new(arbiter.remote_output()),
        Arc::clone(&clock),
        Some(bus.clone()),
        Some(transport.health()),
    );

    let logger = open_logger_for_session(
        shared,
        operation,
        &session,
        &arbiter,
        logger_port,
        logger_enabled,
    );
    let stop_bus = Arc::new(AtomicBool::new(false));
    // Set when the replay stack closes the session (F2), so the watchdog can unpair.
    let session_closed = Arc::new(AtomicBool::new(false));
    let pump = tokio::spawn(pump_edges(Arc::clone(&transport), driver));
    let end = StationSessionEnd::new();
    let handle = StationSessionHandle { session: Arc::clone(&session), end: end.clone() };
    let mut candidate = Some(PendingStationSession {
        session,
        pump,
        stop_bus,
        logger,
        arbiter,
        end,
    });

    let installed = {
        let mut lifecycle = shared.lifecycle.lock().unwrap();
        // A persistent LAN advertiser can host many sequential Keyers. Completed liveness
        // cleanup handles no longer own resources, so discard them before this next install.
        lifecycle.cleanup_tasks.retain(|task| !task.resource.is_finished());
        if operation_cancelled(cancel) || !lifecycle.can_install_replay_session(operation) {
            None
        } else {
            let candidate = candidate.take().expect("candidate is present before final install");
            let owner_session = Arc::clone(&candidate.session);
            let owner_stop = Arc::clone(&candidate.stop_bus);
            let flex_task = Arc::new(Mutex::new(None));
            let bus_forwarder = spawn_bus_forwarder(
                bus,
                Arc::clone(shared),
                operation,
                Arc::clone(&owner_session),
                Arc::clone(&owner_stop),
                Arc::clone(&session_closed),
            );
            // These tasks start only after the gate is released. Their first ownership checks then
            // see either this fully installed session or a newer generation, never a half install.
            let (control_stop, control_cancel) = tokio::sync::watch::channel(false);
            let control = spawn_control_task(
                Arc::clone(shared),
                operation,
                net.clone(),
                Arc::clone(&owner_session),
                Arc::clone(&flex_task),
                control_cancel,
                Arc::clone(&owner_stop),
            );
            let watchdog = spawn_rtt_probe(
                Arc::clone(shared),
                operation,
                Arc::clone(&transport),
                owner_session,
                owner_stop,
                Arc::clone(&session_closed),
            );
            let displaced = lifecycle
                .active
                .replace(OperationResource {
                    operation,
                    resource: ActiveSession {
                        session: candidate.session,
                        pump: candidate.pump,
                        control,
                        control_stop,
                        flex_task,
                        watchdog,
                        bus_forwarder,
                        stop_bus: candidate.stop_bus,
                        logger: candidate.logger,
                        arbiter: candidate.arbiter,
                        end: candidate.end,
                    },
                })
                .map(|owned| owned.resource);
            if let Some(active) = &displaced {
                active.stop_bus.store(true, Ordering::SeqCst);
            }
            // This is the linearization point: the session, its observers, and its terminal
            // events are now all protected by the same gate as Stop and replacement.
            shared.emit(StationEvent::Connection { state: "connected".into(), path: path.clone() });
            shared.log("info", format!("Paired with the keyer over the {path} path."));
            Some(displaced)
        }
    };

    let Some(displaced) = installed else {
        dispose_pending_station_session(candidate.take().expect("rejected candidate is present")).await;
        return None;
    };
    if let Some(active) = displaced {
        dispose_active_station_session(active, true).await;
    }
    Some(handle)
}

/// Heartbeat and round-trip probe for the Station's link. Each quarter second it sends a heartbeat
/// and a ping (the Client answers, and its pong sets the round trip); every fourth tick it reports
/// the RTT to the UI. It removes a session only after proving that the exact session still owns
/// the active slot, so an old LAN cycle cannot tear down a newer Keyer.
fn spawn_rtt_probe(
    shared: Arc<Shared>,
    operation: StationOperation,
    transport: Arc<EdgeTransport>,
    session: Arc<tokio::sync::Mutex<Session>>,
    stop: Arc<AtomicBool>,
    session_closed: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        let mut beats: u32 = 0;
        loop {
            tick.tick().await;
            if stop.load(Ordering::SeqCst) || !shared.owns_current_session(operation, &session) {
                return;
            }
            // The Client has gone silent, or the replay stack closed the session (F2): tear the
            // pairing down so the UI returns to Start.
            let closed = session_closed.load(Ordering::SeqCst);
            if closed || transport.peer_silent_for() >= rwk_core::engine::reconnect::PEER_SILENCE_TIMEOUT {
                let _ = shared.retire_active_session_if_owner(
                    operation,
                    &session,
                    "The Client stopped responding; unpaired.",
                );
                return;
            }
            let _ = transport.send_heartbeat().await;
            if stop.load(Ordering::SeqCst) || !shared.owns_current_session(operation, &session) {
                return;
            }
            let _ = transport.send_ping().await;
            if stop.load(Ordering::SeqCst) || !shared.owns_current_session(operation, &session) {
                return;
            }
            beats = beats.wrapping_add(1);
            if beats % 4 == 0 {
                let ms = transport.rtt().map(|d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX));
                let _ = shared.emit_for_session(operation, &session, StationEvent::Rtt { ms });
            }
        }
    })
}

/// Spawns the Station's control-channel task: accept the Client's control connection, apply
/// each pushed forward-rule table to the live session, and — when any FlexRadio rule is
/// present — capture the radio's VITA-49 discovery broadcasts and forward them to the Client.
/// Every post-await publication verifies both the operation and the specific session owner.
fn spawn_control_task(
    shared: Arc<Shared>,
    operation: StationOperation,
    net: rwk_core::engine::netstack_driver::NetStackHandle,
    session: Arc<tokio::sync::Mutex<Session>>,
    flex_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    mut control_stop: tokio::sync::watch::Receiver<bool>,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // The Client dials this channel and redials it after any drop, so listen again whenever a
        // connection ends, until the session stops.
        while !stop.load(Ordering::SeqCst) && !operation_cancelled(&control_stop) {
            if !shared.owns_current_session(operation, &session) {
                return;
            }
            let mut link = match await_or_cancel(&mut control_stop, ControlLink::listen(&net)).await {
                Some(Ok(link)) => link,
                Some(Err(error)) => {
                    shared.log_for_session(
                        operation,
                        &session,
                        "warn",
                        format!("Control channel listen failed: {error}"),
                    );
                    return;
                }
                None => return,
            };
            if operation_cancelled(&control_stop) || !shared.owns_current_session(operation, &session) {
                return;
            }
            shared.log_for_session(operation, &session, "info", "Control channel up.");

            // Messages to the Client (FlexRadio discovery) are queued here and sent by the loop
            // below, so the link is never locked across a read.
            let (tx, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<ControlMessage>();

            'link: loop {
                if stop.load(Ordering::SeqCst)
                    || operation_cancelled(&control_stop)
                    || !shared.owns_current_session(operation, &session)
                {
                    break;
                }
                let mut loop_stop = control_stop.clone();
                tokio::select! {
                    changed = loop_stop.changed() => {
                        let _ = changed;
                        break 'link;
                    }
                    queued = outgoing.recv() => match queued {
                        Some(message) => {
                            let mut send_stop = control_stop.clone();
                            match await_or_cancel(&mut send_stop, link.send(&message)).await {
                                Some(Ok(())) => {}
                                Some(Err(error)) => {
                                    shared.log_for_session(
                                        operation,
                                        &session,
                                        "warn",
                                        format!("Control channel send failed: {error}"),
                                    );
                                    break 'link;
                                }
                                None => break 'link,
                            }
                        }
                        None => break 'link,
                    },
                    incoming = link.recv() => match incoming {
                        Ok(Some(ControlMessage::ForwardRules { rules })) => {
                            if !shared.owns_current_session(operation, &session) {
                                break 'link;
                            }
                            shared.log_for_session(
                                operation,
                                &session,
                                "info",
                                "Forward rules received from the client.",
                            );
                            let mut session_stop = control_stop.clone();
                            let Some(mut live_session) =
                                await_or_cancel(&mut session_stop, session.lock()).await
                            else {
                                break 'link;
                            };
                            if operation_cancelled(&control_stop)
                                || !shared.owns_current_session(operation, &session)
                            {
                                drop(live_session);
                                break 'link;
                            }
                            let mut forwards_stop = control_stop.clone();
                            let Some(started) = await_or_cancel(
                                &mut forwards_stop,
                                live_session.apply_forwards(&rules),
                            )
                            .await
                            else {
                                drop(live_session);
                                break 'link;
                            };
                            drop(live_session);
                            if operation_cancelled(&control_stop)
                                || !shared.owns_current_session(operation, &session)
                            {
                                break 'link;
                            }
                            shared.log_for_session(
                                operation,
                                &session,
                                "info",
                                format!("Applied {started} forward rule(s) pushed from the client."),
                            );
                            let _ = shared.emit_for_session(
                                operation,
                                &session,
                                StationEvent::ForwardRules { rules: rules.clone() },
                            );
                            // If the Client pushed any Flex rule, make sure the discovery listener is
                            // running so SmartSDR discovery is relayed back.
                            let has_flex = rules.iter().any(|rule| rule.id.starts_with("flex-"));
                            if has_flex && flex_task.lock().unwrap().is_none() {
                                let task = spawn_flex_listener(
                                    Arc::clone(&shared),
                                    operation,
                                    Arc::clone(&session),
                                    tx.clone(),
                                    Arc::clone(&stop),
                                );
                                *flex_task.lock().unwrap() = Some(task);
                                shared.log_for_session(
                                    operation,
                                    &session,
                                    "info",
                                    "FlexRadio discovery capture started.",
                                );
                                let _ = shared.emit_for_session(
                                    operation,
                                    &session,
                                    StationEvent::FlexDiscovery { enabled: true },
                                );
                            } else if !has_flex {
                                let had_flex = flex_task.lock().unwrap().is_some();
                                if had_flex {
                                    stop_flex_task(&flex_task).await;
                                    let _ = shared.emit_for_session(
                                        operation,
                                        &session,
                                        StationEvent::FlexDiscovery { enabled: false },
                                    );
                                }
                            }
                        }
                        Ok(Some(_)) => {} // the Station ignores other message types
                        Ok(None) => {
                            shared.log_for_session(
                                operation,
                                &session,
                                "warn",
                                "Control channel ended by the client.",
                            );
                            break 'link;
                        }
                        Err(error) => {
                            shared.log_for_session(
                                operation,
                                &session,
                                "warn",
                                format!("Control channel read failed: {error}"),
                            );
                            break 'link;
                        }
                    },
                }
            }
            let had_flex = flex_task.lock().unwrap().is_some();
            if had_flex {
                stop_flex_task(&flex_task).await;
                let _ = shared.emit_for_session(
                    operation,
                    &session,
                    StationEvent::FlexDiscovery { enabled: false },
                );
            }
            if stop.load(Ordering::SeqCst)
                || operation_cancelled(&control_stop)
                || !shared.owns_current_session(operation, &session)
            {
                return;
            }
            shared.log_for_session(
                operation,
                &session,
                "warn",
                "Control channel closed; waiting for the client to redial.",
            );
        }
    })
}

/// Takes, cancels, and joins the currently registered Flex discovery task. The holder is detached
/// before its await, so lifecycle locks never span a network wait.
async fn stop_flex_task(flex_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>) {
    let task = flex_task.lock().unwrap().take();
    if let Some(task) = task {
        task.abort();
        let _ = task.await;
    }
}

/// Spawns a blocking task that captures FlexRadio discovery broadcasts on the Station LAN and
/// forwards each raw packet to the Client over `link` as a base64 `DiscoveryAnnounce`.
fn spawn_flex_listener(
    shared: Arc<Shared>,
    operation: StationOperation,
    session: Arc<tokio::sync::Mutex<Session>>,
    outgoing: tokio::sync::mpsc::UnboundedSender<ControlMessage>,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !shared.owns_current_session(operation, &session) {
            return;
        }
        let listener = match DiscoveryListener::bind() {
            Ok(listener) => listener,
            Err(error) => {
                shared.log_for_session(
                    operation,
                    &session,
                    "warn",
                    format!("Flex discovery listener failed: {error}"),
                );
                return;
            }
        };
        if !shared.owns_current_session(operation, &session) {
            return;
        }
        let _ = listener.set_read_timeout(Some(std::time::Duration::from_millis(300)));
        loop {
            if stop.load(Ordering::SeqCst) || !shared.owns_current_session(operation, &session) {
                return;
            }
            // The capture is blocking; run it off the async worker.
            let captured = tokio::task::block_in_place(|| listener.recv_discovery());
            match captured {
                Ok(Some(packet)) => {
                    if !stop.load(Ordering::SeqCst) && shared.owns_current_session(operation, &session) {
                        let payload = base64_encode(&packet);
                        // A closed queue means the link ended; the control task aborts this listener then.
                        let _ = outgoing.send(ControlMessage::DiscoveryAnnounce { payload });
                    }
                }
                Ok(None) => {} // not a Flex packet, ignore
                Err(_) => {}   // timeout or transient error: loop and re-check the stop flag
            }
        }
    })
}

/// Forwards fail-safe events published on the replay driver's bus to the UI sink until `stop` is
/// set. Runs on a blocking thread because the bus receiver is synchronous. A stale bus thread
/// cannot set liveness state or emit events for a newer session.
fn spawn_bus_forwarder(
    bus: EventBus,
    shared: Arc<Shared>,
    operation: StationOperation,
    session: Arc<tokio::sync::Mutex<Session>>,
    stop: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("rwk-station-bus".into())
        .spawn(move || {
            let mut rx = bus.subscribe();
            while !stop.load(Ordering::SeqCst) {
                if !shared.owns_current_session(operation, &session) {
                    return;
                }
                match rx.try_recv() {
                    Ok(CoreEvent::FailSafe { code, message }) => {
                        if !shared.owns_current_session(operation, &session) {
                            return;
                        }
                        // F2 closes the session; the watchdog unpairs when it sees this flag.
                        if code == 2 {
                            closed.store(true, Ordering::SeqCst);
                        }
                        let _ = shared.emit_for_session(
                            operation,
                            &session,
                            StationEvent::FailSafe { code, message: message.clone() },
                        );
                        shared.log_for_session(
                            operation,
                            &session,
                            "warn",
                            format!("Fail-safe F{code}: {message}"),
                        );
                    }
                    Ok(_) => {}
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
                }
            }
        })
        .expect("spawn station bus forwarder")
}

impl Drop for StationController {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// PTT never shares the key line: if the two match, PTT is dropped (the key line wins).
fn ptt_line_for(key: KeyingLine, ptt: Option<KeyingLine>) -> Option<KeyingLine> {
    match ptt {
        Some(p) if p == key => None,
        other => other,
    }
}

/// Bounds a user-facing RWKP v2 name field without slicing through a UTF-8 code point. The core
/// beacon codec permits at most 32 bytes per field, so this keeps Player configuration safe.
fn bounded_lan_label(value: &str) -> String {
    let mut bounded = String::new();
    for character in value.trim().chars() {
        if bounded.len() + character.len_utf8() > 32 {
            break;
        }
        bounded.push(character);
    }
    bounded
}

/// A short, human-sharable pairing token: twelve hex characters from a fresh CSPRNG identity.
/// The Station shows it and the Client types it, so it only needs to be unguessable for the
/// short pairing window.
fn generate_token() -> String {
    let bytes: Vec<u8> = Config::new(Role::Station)
        .ok()
        .and_then(|c| c.identity.public_key_base64().ok())
        .and_then(|b64| base64_decode(&b64).ok())
        .filter(|b| b.len() >= 6)
        .map(|b| b[..6].to_vec())
        .unwrap_or_else(|| {
            // Extremely unlikely fallback: time-seeded so the UI still gets a token.
            let n = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            n.to_be_bytes()[2..8].to_vec()
        });
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `selected` names a port that is not in `present` (and is `Some`).
fn port_missing(selected: &Option<String>, present: &[String]) -> bool {
    matches!(selected, Some(name) if !present.iter().any(|p| p == name))
}

/// Parses a UI line name into a [`KeyingLine`]; "none"/empty maps to [`None`].
fn parse_line(name: &str) -> Option<KeyingLine> {
    match name.to_ascii_lowercase().as_str() {
        "dtr" => Some(KeyingLine::Dtr),
        "rts" => Some(KeyingLine::Rts),
        _ => None,
    }
}

/// The UI name for a [`KeyingLine`].
fn line_name(line: KeyingLine) -> &'static str {
    match line {
        KeyingLine::Dtr => "dtr",
        KeyingLine::Rts => "rts",
        KeyingLine::None => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn recording_sink() -> (EventSink, Arc<Mutex<Vec<StationEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let sink: EventSink = Arc::new(move |e| captured.lock().unwrap().push(e));
        (sink, events)
    }

    /// A controller with persistence disabled (no file writes in tests).
    fn controller(sink: EventSink) -> StationController {
        StationController::start_with_config(sink, None)
    }

    fn wait_for(
        events: &Arc<Mutex<Vec<StationEvent>>>,
        timeout: Duration,
        pred: impl Fn(&[StationEvent]) -> bool,
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
    fn stale_operation_is_not_eligible_to_install_a_replay_session() {
        // `bring_up_station` uses this same predicate at its final installation gate. A newer
        // operation makes an earlier listener/advertiser unable to publish its replay stack.
        let mut lifecycle = StationLifecycle::default();
        let stale = lifecycle.next_operation();
        let current = lifecycle.next_operation();

        assert!(!lifecycle.can_install_replay_session(stale));
        assert!(lifecycle.can_install_replay_session(current));
    }

    #[test]
    fn start_emits_initial_settings() {
        let (sink, events) = recording_sink();
        let _controller = controller(sink);
        let ok = wait_for(&events, Duration::from_secs(1), |evs| {
            evs.iter().any(|e| matches!(e, StationEvent::Settings { key_line, .. } if key_line == "dtr"))
        });
        assert!(ok, "start must emit the initial station settings");
    }

    #[test]
    fn selecting_a_keying_port_updates_and_echoes_settings() {
        let (sink, events) = recording_sink();
        let controller = controller(sink);
        controller.select_keying_port(Some("COM7".into()));
        assert_eq!(controller.shared.settings.lock().unwrap().keying_port.as_deref(), Some("COM7"));
        let ok = wait_for(&events, Duration::from_secs(1), |evs| {
            evs.iter().any(|e| {
                matches!(e, StationEvent::Settings { keying_port, .. } if keying_port.as_deref() == Some("COM7"))
            })
        });
        assert!(ok, "selecting a port must echo updated settings");
    }

    #[test]
    fn line_setters_parse_and_store() {
        let (sink, _events) = recording_sink();
        let controller = controller(sink);
        controller.set_ptt_line("none");
        controller.set_key_line("rts");
        let s = controller.shared.settings.lock().unwrap();
        assert_eq!(s.key_line, KeyingLine::Rts);
        assert_eq!(s.ptt_line, None);
    }

    #[test]
    fn logger_settings_store() {
        let (sink, _events) = recording_sink();
        let controller = controller(sink);
        controller.set_logger_port(Some("COM9".into()));
        controller.set_logger_enabled(true);
        let s = controller.shared.settings.lock().unwrap();
        assert_eq!(s.logger_port.as_deref(), Some("COM9"));
        assert!(s.logger_enabled);
    }

    #[test]
    fn listen_without_a_keying_port_still_starts_pairing() {
        // A keying port is optional: pairing starts with none selected (the LEDs follow the
        // Client), so the controller reports connecting rather than refusing.
        let (sink, events) = recording_sink();
        let controller = controller(sink);
        controller.listen("127.0.0.1:9999", "code");
        let ok = wait_for(&events, Duration::from_secs(1), |evs| {
            evs.iter().any(|e| matches!(e, StationEvent::Connection { state, .. } if state == "connecting"))
        });
        assert!(ok, "pairing without a keying port must start (connecting)");
    }

    #[test]
    fn listen_rejects_a_bad_relay_address_without_blocking() {
        let (sink, events) = recording_sink();
        let controller = controller(sink);
        controller.select_keying_port(Some("COM1".into()));
        controller.listen("not-an-address", "code");
        let ok = wait_for(&events, Duration::from_secs(1), |evs| {
            evs.iter().any(|e| matches!(e, StationEvent::Connection { state, .. } if state == "disconnected"))
        });
        assert!(ok, "a bad relay address must report disconnected, never hang");
    }

    #[test]
    fn persistence_round_trips_identity_and_settings() {
        // Write a config via one controller, then load it in a second and confirm the
        // identity and settings survived.
        let dir = std::env::temp_dir().join(format!("rwk-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("station.json");
        let _ = std::fs::remove_file(&path);

        let (sink, _events) = recording_sink();
        let first = StationController::start_with_config(sink, Some(path.clone()));
        first.select_keying_port(Some("COM3".into()));
        first.set_logger_port(Some("COM4".into()));
        first.set_logger_enabled(true);
        let first_pub = first.shared.config.lock().unwrap().identity.public_key_base64().unwrap();
        drop(first);

        let (sink2, _e2) = recording_sink();
        let second = StationController::start_with_config(sink2, Some(path.clone()));
        let second_pub = second.shared.config.lock().unwrap().identity.public_key_base64().unwrap();
        assert_eq!(first_pub, second_pub, "identity must persist across restarts");
        let s = second.shared.settings.lock().unwrap();
        assert_eq!(s.keying_port.as_deref(), Some("COM3"));
        assert_eq!(s.logger_port.as_deref(), Some("COM4"));
        assert!(s.logger_enabled);
        drop(s);
        drop(second);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn line_names_round_trip() {
        assert_eq!(line_name(parse_line("dtr").unwrap()), "dtr");
        assert_eq!(line_name(parse_line("rts").unwrap()), "rts");
        assert_eq!(parse_line("none"), None);
        assert_eq!(parse_line("nonsense"), None);
    }

    #[test]
    fn player_lan_labels_are_utf8_safe_and_bounded() {
        assert_eq!(bounded_lan_label("  W1VE  "), "W1VE");
        assert_eq!(bounded_lan_label(&"A".repeat(33)), "A".repeat(32));
        assert_eq!(bounded_lan_label(&"é".repeat(17)), "é".repeat(16));
    }
}
