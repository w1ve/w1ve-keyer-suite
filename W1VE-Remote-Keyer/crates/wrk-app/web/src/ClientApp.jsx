import { Fragment, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { clientApi as api, shellApi, onEvent } from "./api.js";
import TransportSlider from "./TransportSlider.jsx";
import RadarSweep from "./RadarSweep.jsx";
import { LogEntries } from "./LogPanel.jsx";
import ConnectionsTab from "./ConnectionsTab.jsx";
import AboutDialog from "./AboutDialog.jsx";
import { APP_NAME_CLIENT } from "./app-info.js";

// How long the splash popup stays up once the main UI has loaded.
const SPLASH_MS = 5000;

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

// The eight F-key macros. Same defaults and storage shape as the Keyer-Router, kept under its
// own localStorage key so the two apps do not share edits.
const DEFAULT_MACROS = [
  { name: "CQ", text: "CQ DE MYCALL" },
  { name: "599", text: "599" },
  { name: "TU", text: "TU" },
  { name: "73", text: "73" },
  { name: "MYCALL", text: "MYCALL" },
  { name: "QRL?", text: "QRL?" },
  { name: "?", text: "?" },
  { name: "QRX", text: "QRX" },
];

function loadMacros() {
  try {
    const raw = localStorage.getItem("wrk.macros");
    if (raw) return JSON.parse(raw);
  } catch {
    /* fall through to defaults */
  }
  return DEFAULT_MACROS;
}

// The Remote Keyer Client: keyer settings, sidetone, input ports, CW macros, and WAN/LAN
// pairing controls. On LAN it browses and selects desktop or W1VE ESP32 Players.
export default function ClientApp() {
  const [settings, setSettings] = useState({
    wpm: 20,
    speedLocked: false,
    weight: 50,
    mode: "iambicB",
    paddleReverse: false,
    toneHz: 600,
    toneVolume: 70,
    audioDevice: null,
    transport: "wan",
    relay: "",
  });
  const [connection, setConnection] = useState({ state: "disconnected", path: "none" });
  const [keyDown, setKeyDown] = useState(false);
  const [rtt, setRtt] = useState(null); // round-trip ms to the Player, null until measured
  const [sending, setSending] = useState(false);
  const [lanPlayers, setLanPlayers] = useState([]);
  const [pickedSelection, setPickedSelection] = useState(null);
  const [log, setLog] = useState([]);

  const [ports, setPorts] = useState([]);
  const [audioDevices, setAudioDevices] = useState([]);
  const [errors, setErrors] = useState({}); // control -> short red message
  const [loggerPort, setLoggerPort] = useState("");
  const [paddlePort, setPaddlePort] = useState("");
  const [token, setToken] = useState("");
  const [typeAhead, setTypeAhead] = useState("");
  const [macros, setMacros] = useState(loadMacros);
  const [editing, setEditing] = useState(false);
  const [logOpen, setLogOpen] = useState(false);
  const [tab, setTab] = useState("main"); // "main" | "connections"
  const [aboutOpen, setAboutOpen] = useState(false);
  const [settingsLoaded, setSettingsLoaded] = useState(false);
  const [portsLoaded, setPortsLoaded] = useState(false);

  useEffect(() => {
    const unlisten = onEvent((ev) => {
      switch (ev.kind) {
        case "settings":
          setSettingsLoaded(true);
          setSettings({
            wpm: ev.wpm,
            speedLocked: ev.speed_locked ?? false,
            weight: ev.weight,
            mode: ev.mode,
            paddleReverse: ev.paddle_reverse,
            toneHz: ev.tone_hz,
            toneVolume: ev.tone_volume,
            audioDevice: ev.audio_device ?? null,
            transport: ev.transport,
            relay: ev.relay,
          });
          setPaddlePort(ev.paddle_port ?? "");
          setLoggerPort(ev.logger_port ?? "");
          // Pre-fill the remembered token, but never overwrite what the operator is typing.
          setToken((current) => current || ev.token || "");
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
        case "lanPlayers":
          setLanPlayers(ev.players || []);
          break;
        case "ports":
          setPorts(ev.available || []);
          setPortsLoaded(true);
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

  // Splash popup: reveal the window and show the popup once the main UI has loaded (first
  // settings + ports snapshots applied), then hide it five seconds later.
  const uiLoaded = settingsLoaded && portsLoaded;
  useEffect(() => {
    if (!uiLoaded) return undefined;
    let timer;
    let cancelled = false;
    shellApi
      .uiReady()
      .catch(() => {})
      .then(() => shellApi.showSplash())
      .then(() => {
        if (cancelled) return;
        timer = window.setTimeout(() => {
          shellApi.hideSplash().catch(() => {});
        }, SPLASH_MS);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [uiLoaded]);

  // File > About... from the native menu.
  useEffect(() => {
    const un = listen("rwk://menu", (e) => {
      if (e.payload === "about") setAboutOpen(true);
    });
    return () => {
      un.then((fn) => fn());
    };
  }, []);

  // Start one fresh combined discovery pass whenever LAN becomes the selected transport. The
  // backend owns the bounded query cadence and does not create a session until Connect.
  useEffect(() => {
    if (settingsLoaded && settings.transport === "lan") {
      api.lanBrowse().catch(() => {});
    }
  }, [settingsLoaded, settings.transport]);

  // Keep the tagged selection valid as discovery records arrive, expire, or report ESP32 busy.
  useEffect(() => {
    const selected = lanPlayers.find((player) => sameSelection(player, pickedSelection));
    if (selected && !(selected.kind === "esp" && selected.busy)) return;
    const next = lanPlayers.find((player) => player.kind !== "esp" || !player.busy);
    setPickedSelection(next ? selectionForPlayer(next) : null);
  }, [lanPlayers, pickedSelection]);

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

  function saveMacros(next) {
    setMacros(next);
    localStorage.setItem("wrk.macros", JSON.stringify(next));
  }

  const paired = connection.state === "connected";
  const connecting = connection.state === "connecting";
  const pathText =
    paired
      ? connection.path === "relay"
        ? "DERP"
        : connection.path === "esp"
          ? "W1VE ESP32 Player LAN"
          : "Direct"
      : connecting
        ? "Pairing…"
        : "Disconnected";
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
        {paired && <span>RTT {rtt ?? "—"} ms</span>}
        {paired && <span style={{ margin: "0 8px", color: "var(--text-dim)" }}>|</span>}
        <span className="led" style={{ color: linkColor }}>●</span>
        <span>{pathText}</span>
      </div>

      <div className="tabbar">
        <button type="button" className={`tab${tab === "main" ? " active" : ""}`} onClick={() => setTab("main")}>
          Main
        </button>
        <button
          type="button"
          className={`tab${tab === "connections" ? " active" : ""}`}
          onClick={() => setTab("connections")}
        >
          Connections
        </button>
      </div>

      {tab === "connections" ? (
        <ConnectionsTab />
      ) : (
      <div className="tabbody keyer-body">
        <div className="keyer-top">
          {/* ─── Keyer ─────────────────────────────────────── */}
          <div className="panel">
            <h3>Keyer</h3>
            <div className="wpm">
              {settings.wpm} <small>WPM</small>
            </div>
            {settings.speedLocked && (
              <small className="hint">Set by N1MM while it has the logger open</small>
            )}
            <input
              type="range"
              min={5}
              max={60}
              value={settings.wpm}
              disabled={settings.speedLocked}
              onChange={(e) => {
                const v = Number(e.target.value);
                setSettings((s) => ({ ...s, wpm: v }));
                api.setSpeed(v);
              }}
            />
            <div className="field" style={{ marginTop: 8 }}>
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
            <div className="row keyer-mode">
              <label style={{ fontSize: 12, color: "var(--text-dim)" }}>Mode</label>
              <select
                value={settings.mode}
                style={{ minWidth: 125 }}
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
            </div>
            <div className="keyer-inputs">
              <PortSelect
                label="Paddle input"
                value={paddlePort}
                ports={ports}
                error={errors.paddle}
                onChange={(p) => {
                  setPaddlePort(p);
                  api.selectPaddlePort(p || null);
                }}
              />
              <PortSelect
                label="Logger (WinKeyer, optional)"
                value={loggerPort}
                ports={ports}
                error={errors.logger}
                onChange={(p) => {
                  setLoggerPort(p);
                  api.selectLoggerPort(p || null);
                }}
              />
            </div>
          </div>

          {/* ─── Sidetone + CW Macros (right column) ───────── */}
          <div className="keyer-side">
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
              <div className="row two-up">
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
              </div>
            </div>

            <div className="panel">
              <div className="row spread" style={{ marginBottom: 6 }}>
                <h3 style={{ margin: 0 }}>CW Macros</h3>
                <button onClick={() => setEditing(true)} style={{ padding: "2px 10px" }}>
                  Edit
                </button>
              </div>
              <div className="macros">
                {macros.map((m, i) => (
                  <button key={i} onClick={() => m.text && api.sendCw(m.text)} title={m.text}>
                    {m.name || `F${i + 1}`}
                  </button>
                ))}
              </div>
              {/* Type-ahead: single-line CW keyed as you type or paste */}
              <div className="row typeahead">
                <input
                  type="text"
                  placeholder="Type or paste CW (sent as entered) — ESC to stop…"
                  value={typeAhead}
                  onChange={onTypeAheadChange}
                  onKeyDown={onTypeAheadKey}
                />
                <button onClick={() => api.testTx()}>Test TX</button>
              </div>
            </div>
          </div>
        </div>

        <div className="panel pairing-panel">
          <h3>Pair with Player</h3>
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
                  <label>Pairing token (from Player)</label>
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
                <div className="field" style={{ marginBottom: 0, minWidth: 280 }}>
                  <label>Available Players</label>
                  <select
                    value={pickedSelection ? selectionKey(pickedSelection) : ""}
                    disabled={paired || connecting}
                    onChange={(e) => {
                      const player = lanPlayers.find((item) => selectionKey(item) === e.target.value);
                      setPickedSelection(player ? selectionForPlayer(player) : null);
                    }}
                  >
                    <option value="">{lanPlayers.length ? "(choose a Player)" : "(searching…)"}</option>
                    {lanPlayers.map((player) => (
                      <option
                        key={selectionKey(player)}
                        value={selectionKey(player)}
                        disabled={player.kind === "esp" && player.busy}
                      >
                        {playerOptionLabel(player)}
                      </option>
                    ))}
                  </select>
                </div>
                <div style={{ alignSelf: "flex-end" }}>
                  <button onClick={() => api.lanBrowse()} disabled={paired || connecting}>
                    Refresh
                  </button>
                </div>
                <div style={{ alignSelf: "flex-end" }}>
                  {paired || connecting ? (
                    <button onClick={() => api.disconnect()} style={{ background: "#5a2d2d" }}>
                      {connecting ? "Cancel" : "Unpair"}
                    </button>
                  ) : (
                    <button
                      onClick={() => pickedSelection && api.connectLan(pickedSelection)}
                      disabled={!pickedSelection}
                      style={{ background: "var(--accent-dim)", borderColor: "var(--accent)" }}
                    >
                      Connect
                    </button>
                  )}
                </div>
              </>
            )}
            {connecting && !paired && (
              <div className="row" style={{ gap: 8, alignSelf: "flex-end" }}>
                <RadarSweep size={56} />
                <span style={{ fontSize: 13, color: "var(--text-dim)" }}>Pairing…</span>
              </div>
            )}
          </div>
          {settings.transport === "lan" && (
            <p style={{ color: "var(--text-dim)", fontSize: 12, marginBottom: 0, marginTop: 8 }}>
              Choose an available desktop Player or W1VE ESP32 Player advertising on your LAN, then Connect.
              W1VE ESP32 Players marked busy cannot be selected. No relay, no internet.
            </p>
          )}
        </div>

        {editing && (
          <MacroEditor
            macros={macros}
            onSave={(next) => {
              saveMacros(next);
              setEditing(false);
            }}
            onCancel={() => setEditing(false)}
          />
        )}
      </div>
      )}

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
        <span className="spring" />
        <span style={{ color: sending ? "var(--accent)" : "var(--text-dim)" }}>
          {sending ? "Sending" : "Idle"}
        </span>
        <span style={{ color: keyDown ? "var(--key-on)" : "var(--text-dim)" }}>
          Key: {keyDown ? "Down" : "Up"}
        </span>
      </div>

      {aboutOpen && <AboutDialog name={APP_NAME_CLIENT} onClose={() => setAboutOpen(false)} />}
    </div>
  );
}

// The selected value remains a tagged model, so a desktop tunnel identity can never be confused
// with a W1VE ESP32 Player identity in the bridge call.
function selectionForPlayer(player) {
  return { kind: player.kind, id: player.id, endpoint: player.endpoint };
}

function sameSelection(player, selection) {
  return Boolean(
    selection &&
      player.kind === selection.kind &&
      player.id === selection.id &&
      player.endpoint === selection.endpoint,
  );
}

function selectionKey(selection) {
  return `${selection.kind}|${selection.id}|${selection.endpoint}`;
}

function playerOptionLabel(player) {
  if (player.kind === "esp") {
    return `W1VE ESP32 Player — ${player.name}${player.busy ? " (busy)" : ""}`;
  }
  return `Desktop Player — ${player.name}`;
}

function PortSelect({ label, value, ports, error, onChange }) {
  // Keep the current value visible even if it is not in the live list (busy/hidden port).
  const list = value && !ports.includes(value) ? [...ports, value] : ports;
  return (
    <div className="field" style={{ marginBottom: 0 }}>
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
  return <div style={{ color: "var(--bad, #e06c6c)", fontSize: 12, marginTop: 4 }}>{msg}</div>;
}

// Edit the F-key macros: a name for the button and the CW text it sends.
function MacroEditor({ macros, onSave, onCancel }) {
  const [draft, setDraft] = useState(macros.map((m) => ({ ...m })));
  const update = (i, field, v) =>
    setDraft((d) => d.map((m, j) => (i === j ? { ...m, [field]: v } : m)));

  return (
    <div
      style={{
        position: "fixed",
        inset: 0,
        background: "rgba(0,0,0,0.5)",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
      }}
    >
      <div className="panel" style={{ width: 440 }}>
        <h3>Edit CW Macros</h3>
        <div className="macro-edit-grid">
          {draft.map((m, i) => (
            <Fragment key={i}>
              <span style={{ color: "var(--text-dim)" }}>F{i + 1}</span>
              <input
                type="text"
                value={m.name}
                onChange={(e) => update(i, "name", e.target.value)}
              />
              <input
                type="text"
                value={m.text}
                onChange={(e) => update(i, "text", e.target.value)}
              />
            </Fragment>
          ))}
        </div>
        <div className="row" style={{ justifyContent: "flex-end", gap: 8, marginTop: 12 }}>
          <button onClick={onCancel}>Cancel</button>
          <button
            onClick={() => onSave(draft)}
            style={{ background: "var(--accent-dim)", borderColor: "var(--accent)" }}
          >
            Save
          </button>
        </div>
      </div>
    </div>
  );
}
