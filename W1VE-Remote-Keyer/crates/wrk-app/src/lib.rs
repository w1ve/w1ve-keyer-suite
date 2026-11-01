//! The W1VE Remote Keyer desktop applications (Tauri v2).
//!
//! A focused Server + Client CW keyer pair over the native [`rwk_core`] engine, with **no
//! router** — the Keyer-Router's keying path on its own. It ships as two executables sharing
//! this library and one web bundle:
//!
//! * `w1ve-remote-keyer` — the operating position: paddle/keyer + sidetone, sends CW to the
//!   Server. Entry point [`run_client`].
//! * `w1ve-remote-keyer-player` — the Player at the remote radio site: receives keyed edges and
//!   keys the radio. Entry point [`run_server`].
//!
//! Both retain the **WAN** shared-token relay path. On **LAN**, the desktop Player advertises
//! its persisted callsign/label and waits as the tunnel responder; the operating Keyer browses
//! desktop and W1VE ESP32 Players, then initiates the selected session.

mod app_paths;
mod client_controller;
mod device_watch;
mod logger_host;
mod pairing;
mod server_controller;

use std::sync::Arc;

use client_controller::{ClientController, ClientEvent};
use device_watch::DeviceChangeWatcher;
use server_controller::{ServerController, ServerEvent};
use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::{Emitter, Manager, State};

/// Extracts the native window handle as an `isize` for the device watcher.
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

/// Which executable this is, for the parts of the shell that differ by role (the Exit menu item
/// disconnects the matching controller before quitting).
#[derive(Clone, Copy)]
enum Role {
    Client,
    Server,
}

// ─── Shared window reveal + splash + File menu (both binaries) ────────────────

/// The main window is configured visible so WebKitGTK starts navigation. During setup Linux keeps
/// it mapped at zero opacity; other platforms hide it before the event loop presents it.
/// [`ui_ready`] reveals the finished UI.
#[allow(unused_variables)]
fn prepare_hidden_main_window(window: &tauri::WebviewWindow) {
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::WidgetExt;
        if let Ok(native) = window.gtk_window() {
            // The config creates the window visible so WebKitGTK starts navigation; setup runs
            // before the event loop maps it, so zero opacity prevents any startup flash.
            native.set_opacity(0.0);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = window.hide();
    }
}

/// Reveals the main window.
///
/// The configured-visible window is hidden during setup (or mapped at zero opacity on Linux), so
/// the operator never sees an unfinished webview. The UI calls this once rendered; the frontend
/// also fires it on a timer as a safety net. Shared by both executables.
#[tauri::command]
fn ui_ready(window: tauri::WebviewWindow) {
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::{GtkWindowExt, WidgetExt};
        let dispatch_window = window.clone();
        let _ = window.run_on_main_thread(move || {
            if let Ok(native) = dispatch_window.gtk_window() {
                native.set_opacity(1.0);
                native.present();
            }
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Shows the splash as a borderless popup window containing only the artwork.
///
/// The window *is* the graphic — 300x168 (the size of `hero.png`), no decorations, no taskbar
/// entry, not focusable — centred over the main window and kept above it. The UI calls this once
/// its own DOM is fully loaded, then calls [`hide_splash`] five seconds later.
#[tauri::command]
async fn show_splash(app: tauri::AppHandle) -> Result<(), String> {
    // MUST be async: on Windows, WebviewWindowBuilder deadlocks when it runs inside a synchronous
    // command or event handler (tauri-apps/tauri#13092).
    build_splash_window(&app)?;
    // WebKitGTK maps a new top-level webview asynchronously. Hold the IPC response briefly so
    // the frontend's five-second timer begins after the Linux splash is actually visible.
    #[cfg(target_os = "linux")]
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    Ok(())
}

/// Builds (or rebuilds) the splash popup. Must run on the main thread.
fn build_splash_window(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(old) = app.get_webview_window("splash") {
        let _ = old.destroy();
    }

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
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("W1VE Remote Keyer")
    .inner_size(SPLASH_W, SPLASH_H)
    .min_inner_size(SPLASH_W, SPLASH_H)
    .max_inner_size(SPLASH_W, SPLASH_H)
    .position(x, y)
    .decorations(false)
    // GTK chooses a 200 px minimum when the first configure is already non-resizable. Build
    // resizable while hidden, map at 300x168, then lock it after the first configure.
    .resizable(true)
    .transparent(false)
    .background_color(tauri::window::Color(13, 23, 33, 255))
    .shadow(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .focused(false)
    // Build hidden so GTK never maps the inherited application-menu allocation. Remove the menu
    // and apply the exact artwork size below before the popup becomes visible.
    .visible(false)
    .build()
    .map_err(|e| e.to_string())?;

    // GTK may keep the inherited menu row's allocation after removal, so reset the popup to the
    // artwork's exact logical size afterwards.
    let _ = splash.remove_menu();
    let _ = splash.set_size(tauri::LogicalSize::new(SPLASH_W, SPLASH_H));
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::{GtkWindowExt, WidgetExt};
        let dispatch_window = splash.clone();
        let _ = splash.run_on_main_thread(move || {
            if let Ok(native) = dispatch_window.gtk_window() {
                native.resize(SPLASH_W as i32, SPLASH_H as i32);
                // Tauri maps the WebKit child below; the delayed geometry hints correct GTK's
                // initial 200 px floor without bypassing that lifecycle.
                gtk::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(100),
                    move || {
                        let geometry = gtk::gdk::Geometry::new(
                            SPLASH_W as i32,
                            SPLASH_H as i32,
                            SPLASH_W as i32,
                            SPLASH_H as i32,
                            0,
                            0,
                            0,
                            0,
                            0.0,
                            0.0,
                            gtk::gdk::Gravity::Center,
                        );
                        native.set_geometry_hints(
                            None::<&gtk::Window>,
                            Some(&geometry),
                            gtk::gdk::WindowHints::MIN_SIZE | gtk::gdk::WindowHints::MAX_SIZE,
                        );
                        native.resize(SPLASH_W as i32, SPLASH_H as i32);
                    },
                );
            }
        });
        // Map through Tauri so the WebKit child receives its initial navigation.
        let _ = splash.show();
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = splash.set_resizable(false);
        let _ = splash.show();
    }
    Ok(())
}

/// Fades the splash popup out over ~200 ms, then closes it.
#[tauri::command]
async fn hide_splash(app: tauri::AppHandle) {
    if let Some(splash) = app.get_webview_window("splash") {
        fade_splash_out(&splash).await;
        let _ = splash.close();
    }
}

/// Steps the splash window's alpha down to zero, then returns so the caller can close it.
///
/// Windows uses layered-window alpha; Linux uses GTK widget opacity. Both fade the complete
/// top-level window surface, including the artwork and its opaque plate. Other platforms close
/// directly until they gain an equivalent native implementation.
#[allow(unused_variables)]
async fn fade_splash_out(window: &tauri::WebviewWindow) {
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
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
            SetWindowLongW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED as i32);

            let start = std::time::Instant::now();
            loop {
                let elapsed = start.elapsed().as_millis() as u64;
                let remaining = FADE_MS.saturating_sub(elapsed);
                let alpha = (255 * remaining / FADE_MS) as u8;
                if SetLayeredWindowAttributes(hwnd, 0, alpha, LWA_ALPHA) == 0 || remaining == 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(8));
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::WidgetExt;

        const FADE_MS: u64 = 200;
        let start = std::time::Instant::now();
        loop {
            let elapsed = start.elapsed().as_millis() as u64;
            let remaining = FADE_MS.saturating_sub(elapsed);
            let opacity = remaining as f64 / FADE_MS as f64;
            let dispatch_window = window.clone();
            let _ = window.run_on_main_thread(move || {
                if let Ok(native) = dispatch_window.gtk_window() {
                    native.set_opacity(opacity);
                }
            });
            if remaining == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(8)).await;
        }
    }
}

/// Installs the File menu (About…, Exit). About emits `rwk://menu` "about" which the UI uses to
/// open the About dialog; Exit disconnects the matching controller first, then quits.
fn install_file_menu(app: &tauri::App, role: Role) -> tauri::Result<()> {
    let about_item = MenuItemBuilder::with_id("about", "About...").build(app)?;
    let exit_item = MenuItemBuilder::with_id("exit", "Exit").build(app)?;
    let file_menu = SubmenuBuilder::new(app, "File")
        .item(&about_item)
        .separator()
        .item(&exit_item)
        .build()?;
    let menu = MenuBuilder::new(app).item(&file_menu).build()?;
    if let Some(window) = app.get_webview_window("main") {
        window.set_menu(menu)?;
    }
    app.on_menu_event(move |app, event| match event.id().as_ref() {
        "about" => {
            let _ = app.emit("rwk://menu", "about");
        }
        "exit" => {
            match role {
                Role::Client => {
                    if let Some(state) = app.try_state::<ClientState>() {
                        state.controller.disconnect();
                    }
                }
                Role::Server => {
                    if let Some(state) = app.try_state::<ServerState>() {
                        state.controller.disconnect();
                    }
                }
            }
            app.exit(0);
        }
        _ => {}
    });
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Client application (`wrk-client`)
// ═══════════════════════════════════════════════════════════════════════════════

struct ClientState {
    controller: ClientController,
    /// The PnP watcher lives for the whole app; kept here so it is not dropped.
    _watcher: DeviceChangeWatcher,
}

/// Launches the Client (operating-position) application.
pub fn run_client() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let sink: client_controller::EventSink = Arc::new(move |event: ClientEvent| {
                let _ = handle.emit("rwk://event", &event);
            });
            let controller = ClientController::start(sink);

            // Install the serial PnP watcher. On Windows it subclasses this window for
            // WM_DEVICECHANGE; elsewhere it polls. Either way it fires the controller's port
            // reconciliation.
            let raw_window = match app.get_webview_window("main") {
                Some(window) => {
                    prepare_hidden_main_window(&window);
                    let _ = window.set_title("W1VE Remote Keyer");
                    window_handle_isize(&window)
                }
                None => None,
            };
            let app_handle = app.handle().clone();
            let on_change: device_watch::OnChange = Arc::new(move || {
                if let Some(state) = app_handle.try_state::<ClientState>() {
                    state.controller.reconcile_ports();
                }
            });
            let watcher = DeviceChangeWatcher::start(raw_window, on_change);
            app.manage(ClientState { controller, _watcher: watcher });

            install_file_menu(app, Role::Client)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_role,
            ui_ready,
            show_splash,
            hide_splash,
            c_request_state,
            c_set_speed,
            c_set_weight,
            c_set_mode,
            c_set_paddle_reverse,
            c_set_transport,
            c_set_relay,
            c_lan_browse,
            c_connect_lan,
            c_set_tone_frequency,
            c_set_tone_volume,
            c_select_audio_device,
            c_audio_devices,
            c_refresh_audio_devices,
            c_serial_ports,
            c_select_logger_port,
            c_select_paddle_port,
            c_send_cw,
            c_test_tx,
            c_abort_send,
            c_connect,
            c_disconnect,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the W1VE Remote Keyer Client");
}

/// Role probe: the Client binary returns "client".
#[tauri::command]
fn app_role() -> String {
    "client".to_string()
}

#[tauri::command]
fn c_request_state(state: State<'_, ClientState>) {
    state.controller.request_state();
}
#[tauri::command]
fn c_set_speed(state: State<'_, ClientState>, wpm: u32) {
    state.controller.set_speed(wpm);
}
#[tauri::command]
fn c_set_weight(state: State<'_, ClientState>, weight: u32) {
    state.controller.set_weight(weight);
}
#[tauri::command]
fn c_set_mode(state: State<'_, ClientState>, mode: String) {
    state.controller.set_mode(&mode);
}
#[tauri::command]
fn c_set_paddle_reverse(state: State<'_, ClientState>, reverse: bool) {
    state.controller.set_paddle_reverse(reverse);
}
#[tauri::command]
fn c_set_transport(state: State<'_, ClientState>, transport: String) {
    state.controller.set_transport(&transport);
}
#[tauri::command]
fn c_set_relay(state: State<'_, ClientState>, relay: String) {
    state.controller.set_relay(&relay);
}
#[tauri::command]
fn c_lan_browse(state: State<'_, ClientState>) {
    state.controller.lan_browse();
}
#[tauri::command]
fn c_connect_lan(
    state: State<'_, ClientState>,
    selection: client_controller::LanPlayerSelection,
) {
    state.controller.connect_lan(selection);
}
#[tauri::command]
fn c_set_tone_frequency(state: State<'_, ClientState>, hz: i32) {
    state.controller.set_tone_frequency(hz);
}
#[tauri::command]
fn c_set_tone_volume(state: State<'_, ClientState>, percent: u32) {
    state.controller.set_tone_volume(percent);
}
#[tauri::command]
fn c_select_audio_device(state: State<'_, ClientState>, device: Option<String>) {
    state.controller.select_audio_device(device);
}
#[tauri::command]
fn c_audio_devices() -> Vec<client_controller::AudioDeviceView> {
    ClientController::audio_devices()
}
#[tauri::command]
fn c_refresh_audio_devices(state: State<'_, ClientState>) {
    state.controller.refresh_audio_devices();
}
#[tauri::command]
fn c_serial_ports() -> Vec<String> {
    ClientController::serial_ports()
}
#[tauri::command]
fn c_select_logger_port(state: State<'_, ClientState>, port: Option<String>) {
    state.controller.select_logger_port(port);
}
#[tauri::command]
fn c_select_paddle_port(state: State<'_, ClientState>, port: Option<String>) {
    state.controller.select_paddle_port(port);
}
#[tauri::command]
fn c_send_cw(state: State<'_, ClientState>, text: String) {
    state.controller.send_cw(&text);
}
#[tauri::command]
fn c_test_tx(state: State<'_, ClientState>) {
    state.controller.test_tx();
}
#[tauri::command]
fn c_abort_send(state: State<'_, ClientState>) {
    state.controller.abort_send();
}
#[tauri::command]
fn c_connect(state: State<'_, ClientState>, token: String) {
    state.controller.connect(&token);
}
#[tauri::command]
fn c_disconnect(state: State<'_, ClientState>) {
    state.controller.disconnect();
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Server application (`wrk-server`)
// ═══════════════════════════════════════════════════════════════════════════════

struct ServerState {
    controller: ServerController,
    /// The PnP watcher lives for the whole app; kept here so it is not dropped.
    _watcher: DeviceChangeWatcher,
}

/// Launches the Server (remote radio site) application.
pub fn run_server() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let sink: server_controller::EventSink = Arc::new(move |event: ServerEvent| {
                let _ = handle.emit("rwk://event", &event);
            });
            let controller = ServerController::start(sink);

            // Install the serial PnP watcher (WM_DEVICECHANGE on Windows, polling elsewhere),
            // firing the controller's port reconciliation.
            let raw_window = match app.get_webview_window("main") {
                Some(window) => {
                    prepare_hidden_main_window(&window);
                    let _ = window.set_title("W1VE Remote Keyer Player");
                    window_handle_isize(&window)
                }
                None => None,
            };
            let app_handle = app.handle().clone();
            let on_change: device_watch::OnChange = Arc::new(move || {
                if let Some(state) = app_handle.try_state::<ServerState>() {
                    state.controller.reconcile_ports();
                }
            });
            let watcher = DeviceChangeWatcher::start(raw_window, on_change);
            app.manage(ServerState { controller, _watcher: watcher });

            install_file_menu(app, Role::Server)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            server_app_role,
            ui_ready,
            show_splash,
            hide_splash,
            s_request_state,
            s_serial_ports,
            s_select_keying_port,
            s_select_logger_port,
            s_set_key_line,
            s_set_ptt_line,
            s_set_ptt_hang,
            s_set_transport,
            s_set_relay,
            s_set_callsign,
            s_set_keyer_id,
            s_regenerate_token,
            s_listen,
            s_lan_advertise,
            s_disconnect,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the W1VE Remote Keyer Player");
}

/// Role probe present only in the Server binary; the web bundle detects the role with it.
#[tauri::command]
fn server_app_role() -> String {
    "server".to_string()
}

#[tauri::command]
fn s_request_state(state: State<'_, ServerState>) {
    state.controller.request_state();
}
#[tauri::command]
fn s_serial_ports() -> Vec<String> {
    ServerController::serial_ports()
}
#[tauri::command]
fn s_select_keying_port(state: State<'_, ServerState>, port: Option<String>) {
    state.controller.select_keying_port(port);
}
#[tauri::command]
fn s_select_logger_port(state: State<'_, ServerState>, port: Option<String>) {
    state.controller.select_logger_port(port);
}
#[tauri::command]
fn s_set_key_line(state: State<'_, ServerState>, line: String) {
    state.controller.set_key_line(&line);
}
#[tauri::command]
fn s_set_ptt_line(state: State<'_, ServerState>, line: String) {
    state.controller.set_ptt_line(&line);
}
#[tauri::command]
fn s_set_ptt_hang(state: State<'_, ServerState>, ms: u32) {
    state.controller.set_ptt_hang(ms);
}
#[tauri::command]
fn s_set_transport(state: State<'_, ServerState>, transport: String) {
    state.controller.set_transport(&transport);
}
#[tauri::command]
fn s_set_relay(state: State<'_, ServerState>, relay: String) {
    state.controller.set_relay(&relay);
}
#[tauri::command]
fn s_set_callsign(state: State<'_, ServerState>, callsign: String) {
    state.controller.set_callsign(&callsign);
}
#[tauri::command]
fn s_set_keyer_id(state: State<'_, ServerState>, keyer_id: String) {
    state.controller.set_keyer_id(&keyer_id);
}
#[tauri::command]
fn s_regenerate_token(state: State<'_, ServerState>) {
    state.controller.regenerate_token();
}
#[tauri::command]
fn s_listen(state: State<'_, ServerState>) {
    state.controller.listen();
}
#[tauri::command]
fn s_lan_advertise(state: State<'_, ServerState>, on: bool) {
    state.controller.lan_advertise(on);
}
#[tauri::command]
fn s_disconnect(state: State<'_, ServerState>) {
    state.controller.disconnect();
}
