//! The standalone **W1VE Keyer** desktop app (Tauri v2).
//!
//! A software replacement for a hardware K1EL WinKeyer: paddle input and a logger (WinKeyer2)
//! input both key one serial output, with local sidetone, all in one process — no networking.
//! It ships as a single executable (`w1ve-keyer`) over the shared [`rwk_core`] engine.
//!
//! Typical station: a paddle on one COM port, a logger (N1MM) sending CW macros on a second
//! COM port, and a serial key-line transistor switch into the rig on a third.

mod app_paths;
mod device_watch;
mod keyer_controller;
mod winkeyer_host;

use std::sync::Arc;

use keyer_controller::{KeyerController, KeyerEvent};
use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::{Emitter, Manager, State};

use device_watch::DeviceChangeWatcher;

struct AppState {
    controller: KeyerController,
    /// The PnP watcher lives for the whole app; kept here so it is not dropped.
    _watcher: DeviceChangeWatcher,
}

/// Launches the W1VE Keyer application.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let sink: keyer_controller::EventSink = Arc::new(move |event: KeyerEvent| {
                let _ = handle.emit("wk://event", &event);
            });
            let controller = KeyerController::start(sink);

            if let Some(window) = app.get_webview_window("main") {
                prepare_hidden_main_window(&window);
                let _ = window.set_title("W1VE Keyer");

                // Install the serial PnP watcher. On Windows it subclasses this window for
                // WM_DEVICECHANGE; elsewhere it polls. Either way it fires the controller's
                // port reconciliation.
                let raw_window = window_handle_isize(&window);
                let app_handle = app.handle().clone();
                let on_change: device_watch::OnChange = Arc::new(move || {
                    if let Some(state) = app_handle.try_state::<AppState>() {
                        state.controller.reconcile_ports();
                    }
                });
                let watcher = DeviceChangeWatcher::start(raw_window, on_change);
                app.manage(AppState { controller, _watcher: watcher });
            } else {
                // No window (shouldn't happen); still manage the controller with a poll-only
                // watcher so the app is usable.
                let app_handle = app.handle().clone();
                let on_change: device_watch::OnChange = Arc::new(move || {
                    if let Some(state) = app_handle.try_state::<AppState>() {
                        state.controller.reconcile_ports();
                    }
                });
                let watcher = DeviceChangeWatcher::start(None, on_change);
                app.manage(AppState { controller, _watcher: watcher });
            }

            // File menu: About... (the UI shows the About dialog) and Exit (quits).
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
            app.on_menu_event(|app, event| match event.id().as_ref() {
                "about" => {
                    let _ = app.emit("wk://menu", "about");
                }
                "exit" => app.exit(0),
                _ => {}
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ui_ready,
            show_splash,
            hide_splash,
            request_state,
            set_speed,
            set_weight,
            set_mode,
            set_paddle_reverse,
            set_key_line,
            set_ptt_line,
            set_tone_frequency,
            set_tone_volume,
            select_audio_device,
            audio_devices,
            refresh_audio_devices,
            serial_ports,
            select_keying_port,
            select_paddle_port,
            select_logger_port,
            send_cw,
            test_tx,
            abort_send,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the W1VE Keyer");
}

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

// ─── Window reveal + splash ──────────────────────────────────────────────────

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
/// also fires it on a timer as a safety net.
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

    // Position is computed before the window is built: a freshly built window cannot be measured
    // yet, and asking it for its size there parks the popup at the default cascade position.
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
        // The splash reuses the built entry point; main.jsx tells the windows apart by label.
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("W1VE Keyer")
    .inner_size(SPLASH_W, SPLASH_H)
    .min_inner_size(SPLASH_W, SPLASH_H)
    .max_inner_size(SPLASH_W, SPLASH_H)
    .position(x, y)
    .decorations(false)
    // GTK chooses a 200 px minimum when the first configure is already non-resizable. Build
    // resizable while hidden, map at 300x168, then lock it after the first configure.
    .resizable(true)
    // Opaque window whose colour matches the artwork's background: a transparent window here
    // rendered nothing at all on Windows.
    .transparent(false)
    .background_color(tauri::window::Color(13, 23, 33, 255))
    .shadow(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .focused(false)
    // Build hidden so GTK never maps the inherited application-menu allocation. The menu is
    // removed and the exact artwork size applied below before the popup becomes visible.
    .visible(false)
    .build()
    .map_err(|e| e.to_string())?;

    // The File menu belongs to the main window. GTK may keep the menu row's allocation after
    // removal, so reset the popup to the artwork's exact logical size afterwards.
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
                // initial 200 px floor without bypassing the webview lifecycle.
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

            // Alpha is driven by elapsed time, not a step count: `sleep` rounds up to the system
            // timer resolution (~15.6 ms), so a late step just skips further down the ramp.
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

// ─── Tauri commands ─────────────────────────────────────────────────────────────

#[tauri::command]
fn request_state(state: State<'_, AppState>) {
    state.controller.request_state();
}
#[tauri::command]
fn set_speed(state: State<'_, AppState>, wpm: u32) {
    state.controller.set_speed(wpm);
}
#[tauri::command]
fn set_weight(state: State<'_, AppState>, weight: u32) {
    state.controller.set_weight(weight);
}
#[tauri::command]
fn set_mode(state: State<'_, AppState>, mode: String) {
    state.controller.set_mode(&mode);
}
#[tauri::command]
fn set_paddle_reverse(state: State<'_, AppState>, reverse: bool) {
    state.controller.set_paddle_reverse(reverse);
}
#[tauri::command]
fn set_key_line(state: State<'_, AppState>, line: String) {
    state.controller.set_key_line(&line);
}
#[tauri::command]
fn set_ptt_line(state: State<'_, AppState>, line: String) {
    state.controller.set_ptt_line(&line);
}
#[tauri::command]
fn set_tone_frequency(state: State<'_, AppState>, hz: i32) {
    state.controller.set_tone_frequency(hz);
}
#[tauri::command]
fn set_tone_volume(state: State<'_, AppState>, percent: u32) {
    state.controller.set_tone_volume(percent);
}
#[tauri::command]
fn select_audio_device(state: State<'_, AppState>, device: Option<String>) {
    state.controller.select_audio_device(device);
}
#[tauri::command]
fn audio_devices() -> Vec<keyer_controller::AudioDeviceView> {
    KeyerController::audio_devices()
}
#[tauri::command]
fn refresh_audio_devices(state: State<'_, AppState>) {
    state.controller.refresh_audio_devices();
}
#[tauri::command]
fn serial_ports() -> Vec<String> {
    KeyerController::serial_ports()
}
#[tauri::command]
fn select_keying_port(state: State<'_, AppState>, port: Option<String>) {
    state.controller.select_keying_port(port);
}
#[tauri::command]
fn select_paddle_port(state: State<'_, AppState>, port: Option<String>) {
    state.controller.select_paddle_port(port);
}
#[tauri::command]
fn select_logger_port(state: State<'_, AppState>, port: Option<String>) {
    state.controller.select_logger_port(port);
}
#[tauri::command]
fn send_cw(state: State<'_, AppState>, text: String) {
    state.controller.send_cw(&text);
}
#[tauri::command]
fn test_tx(state: State<'_, AppState>) {
    state.controller.test_tx();
}
#[tauri::command]
fn abort_send(state: State<'_, AppState>) {
    state.controller.abort_send();
}
