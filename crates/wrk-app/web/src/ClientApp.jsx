import { useEffect, useState } from "react";
import { clientApi as api, onEvent } from "./api.js";
import TransportSlider from "./TransportSlider.jsx";

// Is this platform event-driven for device changes? On Windows the backend pushes updated
// device/port lists on WM_DEVICECHANGE, so no manual refresh is needed. Elsewhere we expose a
// Refresh button. navigator.userAgent carries the OS in the webview.
const IS_WINDOWS = /windows/i.test(navigator.userAgent);

const MODES = [
  ["iambicB", "Iambic B"],
  ["iambicA", "Iambic A"],
  ["ultimatic", "Ultimatic"],
  ["bug", "Bug"],
  ["straight", "Straight"],
];

// The Remote Keyer Client: keyer settings, sidetone, input ports, and a WAN/LAN pairing panel
// where the operator enters the token the Server shows. Same look-and-feel as the Keyer-Router.
export default function ClientApp() {
  const [settings, setSettings] = useState({
    wpm: 20,
    weight: 50,
    mode: "iambicB",
    paddleReverse: false,
    toneHz: 600,
    toneVolume: 70,
    audioDevice: null,
    transport: "wan",
    relay: "",
    callsign: "",
    keyerId: "",
  });
  const [connection, setConnection] = useState({ state: "disconnected", path: "none" });
  const [keyDown, setKeyDown] = useState(false);
  const [sending, setSending] = useState(false);
  const [advertising, setAdvertising] = useState(false);
  const [log, setLog] = useState([]);

  const [ports, setPorts] = useState([]);
  const [audioDevices, setAudioDevices] = useState([]);
  const [errors, setErrors] = useState({}); // control -> short red message
  const [keyingPort, setKeyingPort] = useState("");
  const [paddlePort, setPaddlePort] = useState("");
  const [token, setToken] = useState("");
  const [typeAhead, setTypeAhead] = useState("");

  useEffect(() => {
    const unlisten = onEvent((ev) => {
      switch (ev.kind) {
        case "settings":
          setSettings({
            wpm: ev.wpm,
            weight: ev.weight,
            mode: ev.mode,
            paddleReverse: ev.paddle_reverse,
            toneHz: ev.tone_hz,
            toneVolume: ev.tone_volume,
            audioDevice: ev.audio_device ?? null,
            transport: ev.transport,
            relay: ev.relay,
            callsign: ev.callsign ?? "",
            keyerId: ev.keyer_id ?? "",
          });
          break;
        case "connection":
          setConnection({ state: ev.state, path: ev.path });
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
        case "ports":
          setPorts(ev.available || []);
          break;
        case "audioDevices":
          // Each entry: { name, isDefault }.
          setAudioDevices(ev.available || []);
          break;
        case "error":
          setErrors((e) => ({ ...e, [ev.control]: ev.message || undefined }));
          break;
        case "log":
          setLog((p) => [...p.slice(-400), { level: ev.level, message: ev.message, t: new Date() }]);
          break;
        default:
          break;
      }
    });
    // Pull current state once subscribed (startup push races the webview and is otherwise lost).
    unlisten.then(() => api.requestState().catch(() => {}));
    return () => unlisten.then((f) => f());
  }, []);

  // Global ESC: abort sending from anywhere in the window (e.g. while a macro plays and the
  // type-ahead box is not focused). The type-ahead box also handles ESC to clear itself.
  useEffect(() => {
    const onKey = (e) => {
      if (e.key === "Escape") {
        api.abortSend();
        setTypeAhead("");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // Type-ahead: any text added to the box — typed OR pasted — is appended to the send buffer
  // and keyed with proper CW spacing. We diff against what we have already sent so a paste of a
  // whole sentence enqueues the whole sentence (not just one key). ESC aborts + clears.
  function onTypeAheadChange(e) {
    const next = e.target.value;
    if (next.length > typeAhead.length && next.startsWith(typeAhead)) {
      const added = next.slice(typeAhead.length);
      api.sendCw(added.toUpperCase());
    } else if (next.length > typeAhead.length) {
      api.sendCw(next.toUpperCase());
    }
    setTypeAhead(next);
  }

  function onTypeAheadKey(e) {
    if (e.key === "Escape") {
      e.preventDefault();
      api.abortSend();
      setTypeAhead("");
    }
  }

  const paired = connection.state === "connected";
  const connecting = connection.state === "connecting";
  const pathText =
    paired ? (connection.path === "relay" ? "Relay" : "Direct") : connecting ? "Pairing…" : "Disconnected";
  const linkColor = paired ? "var(--good)" : connecting ? "var(--warn)" : "var(--text-dim)";

  // Audio selection model: settings.audioDevice is null (follow system default), "" (off), or a
  // device name. When null, show the actual default device by name so the user sees what they
  // are hearing through. "(None)" = off = "".
  const defaultDevice = audioDevices.find((d) => d.isDefault)?.name || "";
  const audioValue = settings.audioDevice == null ? defaultDevice : settings.audioDevice;

  return (
    <div className="app">
      <div className="statusbar" style={{ borderTop: "none", borderBottom: "1px solid var(--border)" }}>
        <strong style={{ color: "var(--accent)" }}>W1VE Remote Keyer</strong>
        <span className="spring" />
        <span className="led" style={{ color: linkColor }}>●</span>
        <span>{pathText}</span>
      </div>

      <div className="tabbody">
        <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
          <div className="panel-row" style={{ gridTemplateColumns: "1.2fr 1fr" }}>
            <div className="panel">
              <h3>Keyer</h3>
              <div className="wpm">
                {settings.wpm} <small>WPM</small>
              </div>
              <input
                type="range"
                min={5}
                max={60}
                value={settings.wpm}
                onChange={(e) => {
                  const v = Number(e.target.value);
                  setSettings((s) => ({ ...s, wpm: v }));
                  api.setSpeed(v);
                }}
              />
              <div className="field" style={{ marginTop: 10 }}>
                <label>Weight: {settings.weight}%</label>
                <input
                  type="range"
                  min={25}
                  max={75}
                  value={settings.weight}
                  onChange={(e) => {
                    const v = Number(e.target.value);
                    setSettings((s) => ({ ...s, weight: v }));
                    api.setWeight(v);
                  }}
                />
              </div>
              <div className="field">
                <label>Mode</label>
                <select
                  value={settings.mode}
                  onChange={(e) => {
                    setSettings((s) => ({ ...s, mode: e.target.value }));
                    api.setMode(e.target.value);
                  }}
                >
                  {MODES.map(([v, l]) => (
                    <option key={v} value={v}>
                      {l}
                    </option>
                  ))}
                </select>
              </div>
              <label className="row" style={{ gap: 6 }}>
                <input
                  type="checkbox"
                  checked={settings.paddleReverse}
                  onChange={(e) => {
                    setSettings((s) => ({ ...s, paddleReverse: e.target.checked }));
                    api.setPaddleReverse(e.target.checked);
                  }}
                />
                Paddle reverse
              </label>
              <div className="row" style={{ marginTop: 10, gap: 6 }}>
                <input
                  type="text"
                  placeholder="Type or paste CW (sent as entered) — ESC to stop…"
                  value={typeAhead}
                  onChange={onTypeAheadChange}
                  onKeyDown={onTypeAheadKey}
                  style={{ flex: 1 }}
                />
                <button onClick={() => api.testTx()}>Test TX</button>
              </div>
            </div>

            <div className="panel">
              <h3>Sidetone & Inputs</h3>
              <div className="field">
                <label className="row spread">
                  <span>Sidetone device</span>
                  {!IS_WINDOWS && (
                    <button className="mini" onClick={() => api.refreshAudioDevices()}>
                      Refresh
                    </button>
                  )}
                </label>
                <select
                  value={audioValue}
                  onChange={(e) => {
                    const v = e.target.value;
                    // "" -> off; the default device name -> null (follow default); else that name.
                    const device = v === "" ? "" : v === defaultDevice ? null : v;
                    setSettings((s) => ({ ...s, audioDevice: device }));
                    api.selectAudioDevice(device);
                  }}
                >
                  <option value="">(None — sidetone off)</option>
                  {audioDevices.map((d) => (
                    <option key={d.name} value={d.name}>
                      {d.isDefault ? `${d.name} (default)` : d.name}
                    </option>
                  ))}
                  {/* keep a stale named selection visible until the next refresh */}
                  {settings.audioDevice &&
                    !audioDevices.some((d) => d.name === settings.audioDevice) && (
                      <option value={settings.audioDevice}>{settings.audioDevice}</option>
                    )}
                </select>
                <ErrorLine msg={errors.audio} />
              </div>
              <div className="field">
                <label>Frequency: {settings.toneHz} Hz</label>
                <input
                  type="range"
                  min={400}
                  max={1000}
                  step={10}
                  value={settings.toneHz}
                  onChange={(e) => {
                    const v = Number(e.target.value);
                    setSettings((s) => ({ ...s, toneHz: v }));
                    api.setToneFrequency(v);
                  }}
                />
              </div>
              <div className="field">
                <label>Volume: {settings.toneVolume}%</label>
                <input
                  type="range"
                  min={0}
                  max={100}
                  value={settings.toneVolume}
                  onChange={(e) => {
                    const v = Number(e.target.value);
                    setSettings((s) => ({ ...s, toneVolume: v }));
                    api.setToneVolume(v);
                  }}
                />
              </div>
              <PortSelect
                label="Paddle input (serial)"
                value={paddlePort}
                ports={ports}
                error={errors.paddle}
                onChange={(p) => {
                  setPaddlePort(p);
                  api.selectPaddlePort(p || null);
                }}
              />
              <PortSelect
                label="Local key line (optional)"
                value={keyingPort}
                ports={ports}
                error={errors.keying}
                onChange={(p) => {
                  setKeyingPort(p);
                  api.selectKeyingPort(p || null);
                }}
              />
            </div>
          </div>

          {/* ─── Pairing ───────────────────────────────────────── */}
          <div className="panel">
            <h3>Pair with Server</h3>
            <div className="row" style={{ gap: 12, flexWrap: "wrap", alignItems: "flex-end" }}>
              <div>
                <label style={{ display: "block", fontSize: 12, color: "var(--text-dim)", marginBottom: 4 }}>
                  Transport
                </label>
                <TransportSlider
                  value={settings.transport}
                  disabled={paired || connecting}
                  onChange={(t) => {
                    setSettings((s) => ({ ...s, transport: t }));
                    api.setTransport(t);
                  }}
                />
              </div>

              {settings.transport === "wan" ? (
                <>
                  <div className="field" style={{ marginBottom: 0 }}>
                    <label>Pairing token (from Server)</label>
                    <input
                      type="text"
                      value={token}
                      onChange={(e) => setToken(e.target.value)}
                      disabled={paired || connecting}
                      style={{ width: 200 }}
                    />
                  </div>
                  <div>
                    {paired || connecting ? (
                      <button onClick={() => api.disconnect()} style={{ background: "#5a2d2d" }}>
                        {connecting ? "Cancel" : "Unpair"}
                      </button>
                    ) : (
                      <button
                        onClick={() => api.connect(token)}
                        disabled={!token.trim()}
                        style={{ background: "var(--accent-dim)", borderColor: "var(--accent)" }}
                      >
                        Pair
                      </button>
                    )}
                  </div>
                </>
              ) : (
                <>
                  <div className="field" style={{ marginBottom: 0 }}>
                    <label>Station Callsign</label>
                    <input
                      type="text"
                      value={settings.callsign}
                      placeholder="Station Callsign"
                      disabled={paired || connecting}
                      onChange={(e) => {
                        setSettings((s) => ({ ...s, callsign: e.target.value }));
                        api.setCallsign(e.target.value);
                      }}
                      style={{ width: 130 }}
                    />
                  </div>
                  <div className="field" style={{ marginBottom: 0 }}>
                    <label>Keyer ID</label>
                    <input
                      type="text"
                      value={settings.keyerId}
                      placeholder="Keyer ID (20m, etc.)"
                      disabled={paired || connecting}
                      onChange={(e) => {
                        setSettings((s) => ({ ...s, keyerId: e.target.value }));
                        api.setKeyerId(e.target.value);
                      }}
                      style={{ width: 150 }}
                    />
                  </div>
                  <div>
                    {paired || connecting ? (
                      <button onClick={() => api.lanAdvertise(false)} style={{ background: "#5a2d2d" }}>
                        {connecting ? "Stop" : "Unpair"}
                      </button>
                    ) : (
                      <button
                        onClick={() => api.lanAdvertise(true)}
                        disabled={!settings.callsign.trim()}
                        style={{ background: "var(--accent-dim)", borderColor: "var(--accent)" }}
                      >
                        Advertise
                      </button>
                    )}
                  </div>
                </>
              )}
            </div>
            {settings.transport === "lan" && (
              <p style={{ color: "var(--text-dim)", fontSize: 12, marginBottom: 0, marginTop: 8 }}>
                {advertising
                  ? `Advertising as ${lanName(settings)} — pick this keyer on the Server.`
                  : "Enter your callsign (and an optional ID), then Advertise so the Server can find and pick this keyer. No relay, no internet."}
              </p>
            )}
          </div>

          <LogPanel log={log} />
        </div>
      </div>

      <div className="statusbar">
        <span className="spring" />
        <span style={{ color: sending ? "var(--accent)" : "var(--text-dim)" }}>
          {sending ? "Sending" : "Idle"}
        </span>
        <span style={{ color: keyDown ? "var(--key-on)" : "var(--text-dim)" }}>
          Key: {keyDown ? "Down" : "Up"}
        </span>
      </div>
    </div>
  );
}

// The display name a keyer advertises: callsign-keyerId (or just callsign).
function lanName(settings) {
  const c = (settings.callsign || "").trim();
  const id = (settings.keyerId || "").trim();
  if (c && id) return `${c}-${id}`;
  return c || id || "(unnamed)";
}

function PortSelect({ label, value, ports, error, onChange }) {
  // Keep the current value visible even if it is not in the live list (busy/hidden port).
  const list = value && !ports.includes(value) ? [...ports, value] : ports;
  return (
    <div className="field">
      <label>{label}</label>
      <select value={value} onChange={(e) => onChange(e.target.value)}>
        <option value="">(None)</option>
        {list.map((p) => (
          <option key={p} value={p}>
            {p}
          </option>
        ))}
      </select>
      <ErrorLine msg={error} />
    </div>
  );
}

function ErrorLine({ msg }) {
  if (!msg) return null;
  return (
    <div style={{ color: "var(--bad, #e06c6c)", fontSize: 12, marginTop: 4 }}>{msg}</div>
  );
}

function LogPanel({ log }) {
  return (
    <div className="panel">
      <h3>Log</h3>
      <div
        style={{
          fontFamily: "Consolas, monospace",
          fontSize: 12,
          whiteSpace: "pre-wrap",
          color: "var(--text-dim)",
          maxHeight: 140,
          overflow: "auto",
        }}
      >
        {log.length === 0
          ? "(no messages yet)"
          : log.map((l, i) => (
              <div key={i} style={{ color: l.level === "warn" ? "var(--warn)" : undefined }}>
                {l.t.toLocaleTimeString()} [{l.level}] {l.message}
              </div>
            ))}
      </div>
    </div>
  );
}
