//! The W1VE Remote Keyer-Router desktop applications (Tauri v2).
//!
//! This crate is the operator-facing shell over the native [`rwk_core`] engine. It ships as
//! **two separate executables** that share this one library:
//!
//! * `w1ve-rkr` — the **Client** (operating position): the paddle/keyer UI, driven by
//!   [`ClientController`]. Entry point [`run_client`].
//! * `w1ve-rkr-station` — the **Station** (remote radio site): the replay/keying UI, driven
//!   by [`StationController`]. Entry point [`run_station`].
//!
//! They are distinct binaries, not one app in two modes, because the two sides run on
//! different machines and have no overlapping UI: the Client sends keyed edges, the Station
//! receives and replays them onto a radio. Sharing the crate keeps the Tauri bootstrap, the
//! event bridge, and the web asset bundle in one place; each binary selects its controller,
//! its command set, and its window.
//!
//! The split mirrors the C# apps: a headless controller does the work on background threads,
//! and the UI only sends commands (`invoke`) and renders events (`emit` on `rwk://event`).

mod app_paths;
mod controller;
mod device_watch;
mod station_controller;
mod station_winkeyer;

use std::sync::Arc;

use controller::{ClientController, ClientEvent};
use device_watch::DeviceChangeWatcher;
use station_controller::{StationController, StationEvent};
use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::{Emitter, Manager, State};

/// Extracts the native window handle as an `isize` for the serial-PnP device watcher.
///
/// Returns `None` off Windows (the watcher then polls) or if the handle is unavailable.
#[allow(unused_variables)]
fn window_handle_isize(window: &tauri::WebviewWindow) -> Option<isize> {
    #[cfg(windows)]
    {
        return window.hwnd().ok().map(|h| h.0 as isize);
    }
    #[cfg(not(windows))]
    {
        None
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Client application (`w1ve-rkr`)
// ═══════════════════════════════════════════════════════════════════════════════

/// The Tauri-managed state for the Client app: the live Client controller plus the serial
/// PnP watcher (kept here so it lives for the whole app and is not dropped).
struct ClientState {
    controller: ClientController,
    _watcher: DeviceChangeWatcher,
}

/// Launches the Client (operator) application and blocks until the window closes.
pub fn run_client() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Bridge controller events to the webview: every ClientEvent becomes a
            // "rwk://event" the frontend listens for.
            let handle = app.handle().clone();
            let sink: controller::EventSink = Arc::new(move |event: ClientEvent| {
                let _ = handle.emit("rwk://event", &event);
            });
            let controller = ClientController::start(sink);

            // Install the serial PnP watcher. On Windows it subclasses this window for
            // WM_DEVICECHANGE; elsewhere it polls. Either way it fires the controller's port
            // reconciliation, which pushes a fresh Ports event and drops vanished selections.
            let raw_window =
                app.get_webview_window("main").and_then(|w| window_handle_isize(&w));
            let app_handle = app.handle().clone();
            let on_change: device_watch::OnChange = Arc::new(move || {
                if let Some(state) = app_handle.try_state::<ClientState>() {
                    state.controller.reconcile_ports();
                }
            });
            let watcher = DeviceChangeWatcher::start(raw_window, on_change);
            app.manage(ClientState { controller, _watcher: watcher });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ui_ready,
            ping,
            request_state,
            set_speed,
            set_weight,
            set_mode,
            set_paddle_reverse,
            set_transport,
            set_callsign,
            set_keyer_id,
            lan_advertise,
            set_tone_frequency,
            set_tone_volume,
            select_audio_device,
            audio_devices,
            refresh_audio_devices,
            serial_ports,
            select_keying_port,
            select_paddle_port,
            select_winkeyer_port,
            send_cw,
            test_tx,
            abort_send,
            connect,
            disconnect,
            forward_rules_refresh,
            add_forward_rule,
            remove_forward_rule,
            update_forward_rule,
            set_forward_rule_enabled,
            set_flex_discovery,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the RWK Client application");
}

/// Backwards-compatible alias: the original single-binary entry point launched the Client.
pub fn run() {
    run_client();
}

/// Reveals the main window.
///
/// The window is created hidden (`"visible": false` in tauri.conf.json) so the operator never
/// sees a black webview while the front end boots; each UI calls this once it has rendered, and
/// the front end also fires it on a timer as a safety net. Shared by both executables.
#[tauri::command]
fn ui_ready(window: tauri::WebviewWindow) {
    let _ = window.show();
    let _ = window.set_focus();
}

/// A trivial liveness command, used to confirm the UI↔Rust bridge is wired.
#[tauri::command]
fn ping() -> String {
    format!("w1ve-rkr {} ready", env!("CARGO_PKG_VERSION"))
}

/// Re-emits settings, forward rules, flex state, ports, and audio devices. The UI calls this
/// right after subscribing to the event stream to beat the startup race.
#[tauri::command]
fn request_state(state: State<'_, ClientState>) {
    state.controller.request_state();
}

// ─── Client: keyer settings ──────────────────────────────────────────────────

#[tauri::command]
fn set_speed(state: State<'_, ClientState>, wpm: u32) {
    state.controller.set_speed(wpm);
}

#[tauri::command]
fn set_weight(state: State<'_, ClientState>, weight: u32) {
    state.controller.set_weight(weight);
}

#[tauri::command]
fn set_mode(state: State<'_, ClientState>, mode: String) {
    state.controller.set_mode(&mode);
}

#[tauri::command]
fn set_paddle_reverse(state: State<'_, ClientState>, reverse: bool) {
    state.controller.set_paddle_reverse(reverse);
}

#[tauri::command]
fn set_transport(state: State<'_, ClientState>, transport: String) {
    state.controller.set_transport(&transport);
}

#[tauri::command]
fn set_callsign(state: State<'_, ClientState>, callsign: String) {
    state.controller.set_callsign(&callsign);
}

#[tauri::command]
fn set_keyer_id(state: State<'_, ClientState>, keyer_id: String) {
    state.controller.set_keyer_id(&keyer_id);
}

#[tauri::command]
fn lan_advertise(state: State<'_, ClientState>, on: bool) {
    state.controller.lan_advertise(on);
}

// ─── Client: sidetone ─────────────────────────────────────────────────────────

#[tauri::command]
fn set_tone_frequency(state: State<'_, ClientState>, hz: i32) {
    state.controller.set_tone_frequency(hz);
}

#[tauri::command]
fn set_tone_volume(state: State<'_, ClientState>, percent: u32) {
    state.controller.set_tone_volume(percent);
}

#[tauri::command]
fn select_audio_device(state: State<'_, ClientState>, device: Option<String>) {
    state.controller.select_audio_device(device);
}

#[tauri::command]
fn audio_devices() -> Vec<controller::AudioDeviceView> {
    ClientController::audio_devices()
}

#[tauri::command]
fn refresh_audio_devices(state: State<'_, ClientState>) {
    state.controller.refresh_audio_devices();
}

// ─── Client: ports ────────────────────────────────────────────────────────────

#[tauri::command]
fn serial_ports() -> Vec<String> {
    ClientController::serial_ports()
}

#[tauri::command]
fn select_keying_port(state: State<'_, ClientState>, port: Option<String>) {
    state.controller.select_keying_port(port);
}

#[tauri::command]
fn select_paddle_port(state: State<'_, ClientState>, port: Option<String>) {
    state.controller.select_paddle_port(port);
}

#[tauri::command]
fn select_winkeyer_port(state: State<'_, ClientState>, port: Option<String>) {
    state.controller.select_winkeyer_port(port);
}

// ─── Client: keying ───────────────────────────────────────────────────────────

#[tauri::command]
fn send_cw(state: State<'_, ClientState>, text: String) {
    state.controller.send_cw(&text);
}

#[tauri::command]
fn test_tx(state: State<'_, ClientState>) {
    state.controller.test_tx();
}

#[tauri::command]
fn abort_send(state: State<'_, ClientState>) {
    state.controller.abort_send();
}

// ─── Client: pairing ──────────────────────────────────────────────────────────

/// Pairs with a station. Returns immediately; the controller runs the rendezvous and
/// handshake on its own runtime and reports the outcome via `rwk://event` (Connection/Log).
#[tauri::command]
fn connect(state: State<'_, ClientState>, relay: String, code: String) {
    state.controller.connect(&relay, &code);
}

#[tauri::command]
fn disconnect(state: State<'_, ClientState>) {
    state.controller.disconnect();
}

// ─── Client: Ham Router (port forwarding) ──────────────────────────────────────

#[tauri::command]
fn forward_rules_refresh(state: State<'_, ClientState>) {
    state.controller.refresh_forward_rules();
}

#[tauri::command]
fn add_forward_rule(
    state: State<'_, ClientState>,
    name: String,
    protocol: String,
    client_port: u16,
    station_port: u16,
    station_target_address: String,
) {
    state.controller.add_forward_rule(
        &name,
        &protocol,
        client_port,
        station_port,
        &station_target_address,
    );
}

#[tauri::command]
fn update_forward_rule(
    state: State<'_, ClientState>,
    id: String,
    name: String,
    protocol: String,
    client_port: u16,
    station_port: u16,
    station_target_address: String,
) {
    state.controller.update_forward_rule(
        &id,
        &name,
        &protocol,
        client_port,
        station_port,
        &station_target_address,
    );
}

#[tauri::command]
fn remove_forward_rule(state: State<'_, ClientState>, id: String) {
    state.controller.remove_forward_rule(&id);
}

#[tauri::command]
fn set_forward_rule_enabled(state: State<'_, ClientState>, id: String, enabled: bool) {
    state.controller.set_forward_rule_enabled(&id, enabled);
}

#[tauri::command]
fn set_flex_discovery(state: State<'_, ClientState>, enabled: bool) {
    state.controller.set_flex_discovery(enabled);
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Station application (`w1ve-rkr-station`)
// ═══════════════════════════════════════════════════════════════════════════════

/// The Tauri-managed state for the Station app: the live Station controller plus the serial
/// PnP watcher (kept here so it lives for the whole app and is not dropped).
struct StationState {
    controller: StationController,
    _watcher: DeviceChangeWatcher,
}

/// Launches the Station (remote radio site) application and blocks until the window closes.
pub fn run_station() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let sink: station_controller::EventSink = Arc::new(move |event: StationEvent| {
                let _ = handle.emit("rwk://event", &event);
            });
            let controller = StationController::start(sink);

            // Retitle the window for the Station, and capture its handle for the PnP watcher.
            // Both executables share one web bundle and one tauri.conf.json window; the front
            // end probes the `station_app_role` command, which exists only in this binary, and
            // mounts the Station UI when it succeeds (and the Client UI when it does not).
            let raw_window = if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_title(
                    "W1VE Remote Keyer-Router Station — Any Rig, Any Internet, Anytime",
                );
                // A compact site window: about half of a 1024x768 screen. The shared config's
                // 980x640 window (minimum 900x560) is too wide for a Station, so override it here.
                let _ = window.set_min_size(Some(tauri::LogicalSize::new(480.0, 560.0)));
                let _ = window.set_size(tauri::LogicalSize::new(512.0, 700.0));
                window_handle_isize(&window)
            } else {
                None
            };

            // Install the serial PnP watcher (Windows: WM_DEVICECHANGE on this window;
            // elsewhere: polling), which fires the Station's port reconciliation.
            let app_handle = app.handle().clone();
            let on_change: device_watch::OnChange = Arc::new(move || {
                if let Some(state) = app_handle.try_state::<StationState>() {
                    state.controller.reconcile_ports();
                }
            });
            let watcher = DeviceChangeWatcher::start(raw_window, on_change);
            app.manage(StationState { controller, _watcher: watcher });

            // File menu: About... (the UI shows the About dialog) and Exit (stops any active
            // pairing first, then quits).
            let about_item = MenuItemBuilder::with_id("about", "About...").build(app)?;
            let exit_item = MenuItemBuilder::with_id("exit", "Exit").build(app)?;
            let file_menu = SubmenuBuilder::new(app, "File")
                .item(&about_item)
                .separator()
                .item(&exit_item)
                .build()?;
            let menu = MenuBuilder::new(app).item(&file_menu).build()?;
            app.set_menu(menu)?;
            app.on_menu_event(|app, event| match event.id().as_ref() {
                "about" => {
                    let _ = app.emit("rwk://menu", "about");
                }
                "exit" => {
                    if let Some(state) = app.try_state::<StationState>() {
                        state.controller.disconnect();
                    }
                    app.exit(0);
                }
                _ => {}
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ui_ready,
            station_ping,
            station_app_role,
            station_request_state,
            station_show_splash,
            station_hide_splash,
            station_serial_ports,
            station_select_keying_port,
            station_set_key_line,
            station_set_ptt_line,
            station_set_logger_port,
            station_set_logger_enabled,
            station_set_ptt_hang,
            station_regenerate_token,
            station_set_transport,
            station_lan_browse,
            station_connect_lan,
            station_listen,
            station_disconnect,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the RWK Station application");
}

/// Station liveness command.
#[tauri::command]
fn station_ping() -> String {
    format!("w1ve-rkr-station {} ready", env!("CARGO_PKG_VERSION"))
}

/// Which role this executable is. The shared web bundle calls this once at startup and mounts
/// the Station UI when it returns `"station"`.
#[tauri::command]
fn station_app_role() -> String {
    "station".to_string()
}

/// Re-emits the Station settings + ports once the UI has subscribed (beats the startup race).
#[tauri::command]
fn station_request_state(state: State<'_, StationState>) {
    state.controller.request_state();
}

/// Shows the splash as a borderless popup window containing only the artwork.
///
/// The splash is deliberately its own window: an overlay inside the main webview filled the whole
/// window with black around a small graphic. Here the window *is* the graphic — 300x168 (the size
/// of `hero.png`), no decorations, no taskbar entry, not focusable — centred over the main window
/// and kept above it. The UI calls this once its own DOM is fully loaded, then calls
/// [`station_hide_splash`] five seconds later.
#[tauri::command]
async fn station_show_splash(app: tauri::AppHandle) -> Result<(), String> {
    // MUST be async: on Windows, WebviewWindowBuilder deadlocks when it runs inside a synchronous
    // command or event handler (tauri-apps/tauri#13092). The symptom is a window whose webview
    // never initialises — it shows the bare window background and no document at all.
    build_splash_window(&app)
}

/// Builds (or rebuilds) the splash popup. Must run on the main thread.
fn build_splash_window(app: &tauri::AppHandle) -> Result<(), String> {
    // A previous popup (or a reload of the main UI) must not stack up behind the new one.
    if let Some(old) = app.get_webview_window("splash") {
        let _ = old.destroy();
    }

    // Position is computed before the window is built: a freshly built window cannot be measured
    // yet, and asking it for its size there is what left the popup parked at the default cascade
    // position instead of centred.
    const SPLASH_W: f64 = 300.0;
    const SPLASH_H: f64 = 168.0;
    let main = app.get_webview_window("main");
    let (x, y) = main
        .as_ref()
        .and_then(|main| {
            let pos = main.outer_position().ok()?;
            let size = main.outer_size().ok()?;
            let scale = main.scale_factor().unwrap_or(1.0);
            let w = f64::from(size.width) / scale;
            let h = f64::from(size.height) / scale;
            Some((
                f64::from(pos.x) / scale + (w - SPLASH_W) / 2.0,
                f64::from(pos.y) / scale + (h - SPLASH_H) / 2.0,
            ))
        })
        .unwrap_or((100.0, 100.0));

    let splash = tauri::WebviewWindowBuilder::new(
        app,
        "splash",
        // The splash reuses the built entry point; main.jsx tells the two windows apart by label
        // ("splash") and renders the artwork alone in this one.
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("RWK Station")
    .inner_size(SPLASH_W, SPLASH_H)
    .position(x, y)
    .decorations(false)
    .resizable(false)
    // Opaque window whose colour matches the artwork's own background: a transparent window here
    // rendered nothing at all on Windows, leaving the popup see-through.
    .transparent(false)
    .background_color(tauri::window::Color(13, 23, 33, 255))
    .shadow(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .focused(false)
    .build()
    .map_err(|e| e.to_string())?;

    // The File menu belongs to the main window; without this the popup would also carry a menu bar
    // and be taller than the artwork.
    let _ = splash.remove_menu();
    Ok(())
}

/// Fades the splash popup out over ~200 ms, then closes it (called five seconds after it was
/// shown).
///
/// The popup is a window showing a 300x168 panel, so closing it in one call makes the whole panel
/// blink out of existence — the frame after it is simply gone. It is faded instead.
#[tauri::command]
async fn station_hide_splash(app: tauri::AppHandle) {
    if let Some(splash) = app.get_webview_window("splash") {
        fade_splash_out(&splash);
        let _ = splash.close();
    }
}

/// Steps the splash window's alpha down to zero, then returns so the caller can close it.
///
/// Windows composites a top-level window as one surface, so `WS_EX_LAYERED` +
/// `SetLayeredWindowAttributes` fades the artwork *and* the window plate together. Fading inside
/// the webview instead (CSS opacity on the artwork) could only dissolve the artwork into the
/// popup's own background colour, and the plate would still disappear in a single frame.
///
/// Off Windows there is no equivalent one-call window alpha, so the popup just closes.
#[allow(unused_variables)]
fn fade_splash_out(window: &tauri::WebviewWindow) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowLongW, SetLayeredWindowAttributes, SetWindowLongW, GWL_EXSTYLE, LWA_ALPHA,
            WS_EX_LAYERED,
        };

        const FADE_MS: u64 = 200;

        let Ok(hwnd) = window.hwnd() else { return };
        let hwnd = hwnd.0 as *mut core::ffi::c_void;
        unsafe {
            // The window was built opaque; LWA_ALPHA only applies once it is layered.
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
            SetWindowLongW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED as i32);

            // Alpha is driven by elapsed time, not by a step count: `sleep` rounds up to the
            // system timer resolution (~15.6 ms), so sixteen nominal 12 ms steps would stretch
            // the fade to ~250 ms. Here a late step just skips further down the ramp.
            let start = std::time::Instant::now();
            loop {
                let elapsed = start.elapsed().as_millis() as u64;
                let remaining = FADE_MS.saturating_sub(elapsed);
                let alpha = (255 * remaining / FADE_MS) as u8;
                // A failure means the popup is already gone (or refused the layered style): stop
                // fading and let the caller close it.
                if SetLayeredWindowAttributes(hwnd, 0, alpha, LWA_ALPHA) == 0 || remaining == 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(8));
            }
        }
    }
}

#[tauri::command]
fn station_serial_ports() -> Vec<String> {
    StationController::serial_ports()
}

#[tauri::command]
fn station_select_keying_port(state: State<'_, StationState>, port: Option<String>) {
    state.controller.select_keying_port(port);
}

#[tauri::command]
fn station_set_key_line(state: State<'_, StationState>, line: String) {
    state.controller.set_key_line(&line);
}

#[tauri::command]
fn station_set_ptt_line(state: State<'_, StationState>, line: String) {
    state.controller.set_ptt_line(&line);
}

#[tauri::command]
fn station_set_logger_port(state: State<'_, StationState>, port: Option<String>) {
    state.controller.set_logger_port(port);
}

#[tauri::command]
fn station_set_logger_enabled(state: State<'_, StationState>, enabled: bool) {
    state.controller.set_logger_enabled(enabled);
}

#[tauri::command]
fn station_set_ptt_hang(state: State<'_, StationState>, ms: u32) {
    state.controller.set_ptt_hang(ms);
}

#[tauri::command]
fn station_regenerate_token(state: State<'_, StationState>) {
    state.controller.regenerate_token();
}

#[tauri::command]
fn station_set_transport(state: State<'_, StationState>, transport: String) {
    state.controller.set_transport(&transport);
}

#[tauri::command]
fn station_lan_browse(state: State<'_, StationState>) {
    state.controller.lan_browse();
}

#[tauri::command]
fn station_connect_lan(state: State<'_, StationState>, key: String, endpoint: String) {
    state.controller.connect_lan(&key, &endpoint);
}

/// Begins pairing: the Station waits for the Client to connect over the shared relay + code,
/// then replays keyed edges onto the radio. Returns immediately; the outcome arrives via
/// `rwk://event`.
#[tauri::command]
fn station_listen(state: State<'_, StationState>, relay: String, code: String) {
    state.controller.listen(&relay, &code);
}

#[tauri::command]
fn station_disconnect(state: State<'_, StationState>) {
    state.controller.disconnect();
}
