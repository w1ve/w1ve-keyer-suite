import { useEffect, useRef, useState } from "react";
import { api, onRwkEvent } from "./api.js";
import KeyerTab from "./KeyerTab.jsx";
import RouterTab from "./RouterTab.jsx";
import RelayDialog, { DEFAULT_WAN_RELAY } from "./RelayDialog.jsx";
import { LogEntries } from "./LogPanel.jsx";

const DEFAULT_SETTINGS = {
  wpm: 20,
  speedLocked: false,
  weight: 50,
  mode: "iambicB",
  paddleReverse: false,
  toneHz: 600,
  toneVolume: 70,
  audioDevice: null,
  keyingPort: null,
  paddlePort: null,
  winkeyerPort: null,
  transport: "wan",
  relay: "",
  callsign: "",
  keyerId: "",
};

// The Client application shell: the Remote Keyer's status bar (RTT, then Disconnected / Pairing /
// Direct / DERP at the top right), the Keyer and Ham Router tabs, and a log drawer. The bottom-left
// shows whether FlexRadio discovery is enabled.
export default function App() {
  const [tab, setTab] = useState("keyer");
  const [settings, setSettings] = useState(DEFAULT_SETTINGS);
  const [connection, setConnection] = useState({ state: "disconnected", path: "none" });
  const [rtt, setRtt] = useState(null);
  const [keyDown, setKeyDown] = useState(false);
  const [sending, setSending] = useState(false);
  const [advertising, setAdvertising] = useState(false);
  const [ports, setPorts] = useState([]);
  const [audioDevices, setAudioDevices] = useState([]);
  const [errors, setErrors] = useState({}); // control -> short red message
  const [forwardRules, setForwardRules] = useState([]);
  const [flexEnabled, setFlexEnabled] = useState(false);
  const [log, setLog] = useState([]);
  const [logOpen, setLogOpen] = useState(false);
  const [relayPrompt, setRelayPrompt] = useState(false);
  const pendingToken = useRef("");
  const unlisten = useRef(null);

  useEffect(() => {
    onRwkEvent((ev) => {
      switch (ev.kind) {
        case "settings":
          setSettings({
            wpm: ev.wpm,
            speedLocked: ev.speed_locked ?? false,
            weight: ev.weight,
            mode: ev.mode,
            paddleReverse: ev.paddle_reverse,
            toneHz: ev.tone_hz,
            toneVolume: ev.tone_volume,
            audioDevice: ev.audio_device ?? null,
            keyingPort: ev.keying_port ?? null,
            paddlePort: ev.paddle_port ?? null,
            winkeyerPort: ev.winkeyer_port ?? null,
            transport: ev.transport ?? "wan",
            relay: ev.relay ?? "",
            callsign: ev.callsign ?? "",
            keyerId: ev.keyer_id ?? "",
          });
          break;
        case "ports":
          setPorts(ev.available || []);
          break;
        case "audioDevices":
          // Each entry: { name, isDefault }.
          setAudioDevices(ev.available || []);
          break;
        case "error":
          if (ev.control === "relay") {
            // The relay did not answer: offer the built-in relay or the operator's own.
            if (ev.message) setRelayPrompt(true);
          } else {
            setErrors((e) => ({ ...e, [ev.control]: ev.message || undefined }));
          }
          break;
        case "connection":
          setConnection({ state: ev.state, path: ev.path });
          break;
        case "rtt":
          setRtt(ev.ms ?? null);
          break;
        case "keyState":
          setKeyDown(ev.down);
          break;
        case "sending":
          setSending(ev.active);
          break;
        case "advertising":
          setAdvertising(ev.active);
          break;
        case "forwardRules":
          setForwardRules(ev.rules || []);
          break;
        case "flexDiscovery":
          setFlexEnabled(ev.enabled);
          break;
        case "log":
          setLog((prev) => [...prev.slice(-500), { level: ev.level, message: ev.message, t: new Date() }]);
          break;
        default:
          break;
      }
    }).then((fn) => {
      unlisten.current = fn;
      // Beat the startup race: the backend's initial emit happens before this webview subscribed,
      // so ask for a fresh snapshot now.
      api.requestState().catch(() => {});
    });

    // Global ESC: abort sending from anywhere in the window (e.g. while a macro plays and the
    // type-ahead box is not focused).
    const onKey = (e) => {
      if (e.key === "Escape") api.abortSend().catch(() => {});
    };
    window.addEventListener("keydown", onKey);

    return () => {
      if (unlisten.current) unlisten.current();
      window.removeEventListener("keydown", onKey);
    };
  }, []);

  // Pair through the relay in settings (the built-in one until the operator changes it).
  function onPair(token) {
    pendingToken.current = token;
    api.connect(settings.relay || DEFAULT_WAN_RELAY, token).catch(() => {});
  }

  // The operator picked a relay in the dialog: remember it and pair again through it.
  function onRelayChosen(relay) {
    setRelayPrompt(false);
    setErrors((e) => ({ ...e, relay: undefined }));
    setSettings((s) => ({ ...s, relay }));
    api.connect(relay, pendingToken.current).catch(() => {});
  }

  const paired = connection.state === "connected";
  const connecting = connection.state === "connecting";
  const pathText = paired
    ? connection.path === "relay"
      ? "DERP"
      : "Direct"
    : connecting
      ? "Pairing…"
      : "Disconnected";
  const linkColor = paired ? "var(--good)" : connecting ? "var(--warn)" : "var(--text-dim)";

  return (
    <div className="app">
      <div className="statusbar" style={{ borderTop: "none", borderBottom: "1px solid var(--border)" }}>
        <strong style={{ color: "var(--accent)" }}>W1VE Remote Keyer-Router</strong>
        <span className="spring" />
        {paired && <span>RTT {rtt ?? "—"} ms</span>}
        {paired && <span style={{ margin: "0 8px", color: "var(--text-dim)" }}>|</span>}
        <span className="led" style={{ color: linkColor }}>
          ●
        </span>
        <span>{pathText}</span>
      </div>

      <div className="tabbar">
        <div className={`tab ${tab === "keyer" ? "active" : ""}`} onClick={() => setTab("keyer")}>
          Keyer
        </div>
        <div className={`tab ${tab === "router" ? "active" : ""}`} onClick={() => setTab("router")}>
          Ham Router
        </div>
      </div>

      <div className="tabbody">
        {tab === "keyer" && (
          <KeyerTab
            settings={settings}
            setSettings={setSettings}
            connection={connection}
            ports={ports}
            audioDevices={audioDevices}
            errors={errors}
            advertising={advertising}
            onPair={onPair}
          />
        )}
        {tab === "router" && (
          <RouterTab rules={forwardRules} flexEnabled={flexEnabled} connected={connection.state === "connected"} />
        )}
      </div>

      {/* The log opens above the status bar and only then adds height. */}
      {logOpen && (
        <div className="panel log-drawer">
          <LogEntries log={log} />
        </div>
      )}

      <div className="statusbar">
        <button
          type="button"
          className="log-toggle"
          aria-expanded={logOpen}
          onClick={() => setLogOpen((o) => !o)}
          style={{ width: "auto" }}
        >
          <span>Log</span>
          <span className="log-glyph">{logOpen ? "v" : "^"}</span>
        </button>
        {flexEnabled && (
          <span style={{ color: "var(--good)" }} title="FlexRadio VITA-49 discovery is being re-emitted to the Station">
            ● FlexRadio discovery on
          </span>
        )}
        <span className="spring" />
        <span style={{ color: sending ? "var(--accent)" : "var(--text-dim)" }}>{sending ? "Sending" : "Idle"}</span>
        <span style={{ color: keyDown ? "var(--key-on)" : "var(--text-dim)" }}>Key: {keyDown ? "Down" : "Up"}</span>
      </div>

      {relayPrompt && (
        <RelayDialog
          current={settings.relay}
          onConfirm={onRelayChosen}
          onCancel={() => setRelayPrompt(false)}
        />
      )}
    </div>
  );
}
