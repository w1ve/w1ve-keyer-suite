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
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
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
