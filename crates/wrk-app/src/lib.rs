//! The W1VE Remote Keyer desktop applications (Tauri v2).
//!
//! A focused Server + Client CW keyer pair over the native [`rwk_core`] engine, with **no
//! router** — the Keyer-Router's keying path on its own. It ships as two executables sharing
//! this library and one web bundle:
//!
//! * `w1ve-remote-keyer` — the operating position: paddle/keyer + sidetone, sends CW to the
//!   Server. Entry point [`run_client`].
//! * `w1ve-remote-keyer-station` — the remote radio site: receives keyed edges and keys the
//!   radio. Entry point [`run_server`].
//!
//! Both offer a **WAN/LAN** transport slider and a **pairing token** (see [`pairing`]). The
//! engine, the look-and-feel, and the pairing model match the Keyer-Router app.

mod app_paths;
mod client_controller;
mod pairing;
mod server_controller;

use std::sync::Arc;

use client_controller::{ClientController, ClientEvent};
use server_controller::{ServerController, ServerEvent};
use tauri::{Emitter, Manager, State};

// ═══════════════════════════════════════════════════════════════════════════════
//  Client application (`wrk-client`)
// ═══════════════════════════════════════════════════════════════════════════════

struct ClientState {
    controller: ClientController,
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
            app.manage(ClientState { controller });
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_title("W1VE Remote Keyer — Client");
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_role,
            c_set_speed,
            c_set_weight,
            c_set_mode,
            c_set_paddle_reverse,
            c_set_transport,
            c_set_relay,
            c_set_callsign,
            c_set_keyer_id,
            c_lan_advertise,
            c_set_tone_frequency,
            c_set_tone_volume,
            c_audio_devices,
            c_serial_ports,
            c_select_keying_port,
            c_select_paddle_port,
            c_send_cw,
            c_test_tx,
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
fn c_set_callsign(state: State<'_, ClientState>, callsign: String) {
    state.controller.set_callsign(&callsign);
}
#[tauri::command]
fn c_set_keyer_id(state: State<'_, ClientState>, keyer_id: String) {
    state.controller.set_keyer_id(&keyer_id);
}
#[tauri::command]
fn c_lan_advertise(state: State<'_, ClientState>, on: bool) {
    state.controller.lan_advertise(on);
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
fn c_audio_devices() -> Vec<String> {
    ClientController::audio_devices()
}
#[tauri::command]
fn c_serial_ports() -> Vec<String> {
    ClientController::serial_ports()
}
#[tauri::command]
fn c_select_keying_port(state: State<'_, ClientState>, port: Option<String>) {
    state.controller.select_keying_port(port);
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
            app.manage(ServerState { controller });
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_title("W1VE Remote Keyer — Server");
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            server_app_role,
            s_serial_ports,
            s_select_keying_port,
            s_set_key_line,
            s_set_ptt_line,
            s_set_transport,
            s_set_relay,
            s_regenerate_token,
            s_listen,
            s_lan_browse,
            s_connect_lan,
            s_disconnect,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the W1VE Remote Keyer Server");
}

/// Role probe present only in the Server binary; the web bundle detects the role with it.
#[tauri::command]
fn server_app_role() -> String {
    "server".to_string()
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
fn s_set_key_line(state: State<'_, ServerState>, line: String) {
    state.controller.set_key_line(&line);
}
#[tauri::command]
fn s_set_ptt_line(state: State<'_, ServerState>, line: String) {
    state.controller.set_ptt_line(&line);
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
fn s_regenerate_token(state: State<'_, ServerState>) {
    state.controller.regenerate_token();
}
#[tauri::command]
fn s_listen(state: State<'_, ServerState>) {
    state.controller.listen();
}
#[tauri::command]
fn s_lan_browse(state: State<'_, ServerState>) {
    state.controller.lan_browse();
}
#[tauri::command]
fn s_connect_lan(state: State<'_, ServerState>, key: String, endpoint: String) {
    state.controller.connect_lan(&key, &endpoint);
}
#[tauri::command]
fn s_disconnect(state: State<'_, ServerState>) {
    state.controller.disconnect();
}
