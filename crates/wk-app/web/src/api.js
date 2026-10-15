// Tauri command bridge for the single-binary W1VE Keyer. Each function maps to a
// #[tauri::command] in src/lib.rs.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export const api = {
  // Keyer
  setSpeed: (wpm) => invoke("set_speed", { wpm }),
  setWeight: (weight) => invoke("set_weight", { weight }),
  setMode: (mode) => invoke("set_mode", { mode }),
  setPaddleReverse: (reverse) => invoke("set_paddle_reverse", { reverse }),

  // Sidetone
  setToneFrequency: (hz) => invoke("set_tone_frequency", { hz }),
  setToneVolume: (percent) => invoke("set_tone_volume", { percent }),
  audioDevices: () => invoke("audio_devices"),

  // Ports
  serialPorts: () => invoke("serial_ports"),
  selectKeyingPort: (port) => invoke("select_keying_port", { port }),
  selectPaddlePort: (port) => invoke("select_paddle_port", { port }),
  selectLoggerPort: (port) => invoke("select_logger_port", { port }),
  setKeyLine: (line) => invoke("set_key_line", { line }),
  setPttLine: (line) => invoke("set_ptt_line", { line }),
  setLoggerEnabled: (enabled) => invoke("set_logger_enabled", { enabled }),

  // Keying
  sendCw: (text) => invoke("send_cw", { text }),
  testTx: () => invoke("test_tx"),
};

// Subscribes to the controller event stream on the `wk://event` channel.
export function onEvent(handler) {
  return listen("wk://event", (e) => handler(e.payload));
}
