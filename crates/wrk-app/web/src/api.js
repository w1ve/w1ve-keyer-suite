// Tauri command bridge for both roles. The Client binary registers the `c_*` commands; the
// Server binary registers the `s_*` commands. The role is detected once at startup by probing
// `server_app_role` (present only in the Server), then the matching API object is used.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export const clientApi = {
  // Pull the current settings + ports + audio devices (called once the UI has subscribed,
  // since the backend's startup push races the webview and would otherwise be missed).
  requestState: () => invoke("c_request_state"),
  setSpeed: (wpm) => invoke("c_set_speed", { wpm }),
  setWeight: (weight) => invoke("c_set_weight", { weight }),
  setMode: (mode) => invoke("c_set_mode", { mode }),
  setPaddleReverse: (reverse) => invoke("c_set_paddle_reverse", { reverse }),
  setTransport: (transport) => invoke("c_set_transport", { transport }),
  setRelay: (relay) => invoke("c_set_relay", { relay }),
  setToneFrequency: (hz) => invoke("c_set_tone_frequency", { hz }),
  setToneVolume: (percent) => invoke("c_set_tone_volume", { percent }),
  selectAudioDevice: (device) => invoke("c_select_audio_device", { device }),
  audioDevices: () => invoke("c_audio_devices"),
  refreshAudioDevices: () => invoke("c_refresh_audio_devices"),
  serialPorts: () => invoke("c_serial_ports"),
  selectLoggerPort: (port) => invoke("c_select_logger_port", { port }),
  selectPaddlePort: (port) => invoke("c_select_paddle_port", { port }),
  sendCw: (text) => invoke("c_send_cw", { text }),
  testTx: () => invoke("c_test_tx"),
  abortSend: () => invoke("c_abort_send"),
  connect: (token) => invoke("c_connect", { token }),
  disconnect: () => invoke("c_disconnect"),
  // LAN (operating-position advertises itself)
  setCallsign: (callsign) => invoke("c_set_callsign", { callsign }),
  setKeyerId: (keyerId) => invoke("c_set_keyer_id", { keyerId }),
  lanAdvertise: (on) => invoke("c_lan_advertise", { on }),
};

export const serverApi = {
  // Pull the current settings + ports once the UI has subscribed (startup push races it).
  requestState: () => invoke("s_request_state"),
  serialPorts: () => invoke("s_serial_ports"),
  selectKeyingPort: (port) => invoke("s_select_keying_port", { port }),
  selectLoggerPort: (port) => invoke("s_select_logger_port", { port }),
  setKeyLine: (line) => invoke("s_set_key_line", { line }),
  setPttLine: (line) => invoke("s_set_ptt_line", { line }),
  setTransport: (transport) => invoke("s_set_transport", { transport }),
  setRelay: (relay) => invoke("s_set_relay", { relay }),
  regenerateToken: () => invoke("s_regenerate_token"),
  listen: () => invoke("s_listen"),
  disconnect: () => invoke("s_disconnect"),
  // LAN (radio-site browses + picks a keyer)
  lanBrowse: () => invoke("s_lan_browse"),
  connectLan: (key, endpoint) => invoke("s_connect_lan", { key, endpoint }),
};

// Both roles emit on the same `rwk://event` channel.
export function onEvent(handler) {
  return listen("rwk://event", (e) => handler(e.payload));
}

// Resolves to "server" or "client".
export async function detectRole() {
  try {
    const r = await invoke("server_app_role");
    return r === "server" ? "server" : "client";
  } catch {
    return "client";
  }
}
