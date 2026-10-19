import { Fragment, useState } from "react";
import { api } from "./api.js";
import TransportSlider from "./TransportSlider.jsx";
import RadarSweep from "./RadarSweep.jsx";

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

// The eight F-key macros. Same defaults as the Remote Keyer, kept under this app's own key so the
// two apps do not share edits.
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
    const raw = localStorage.getItem("rwk.macros");
    if (raw) return JSON.parse(raw);
  } catch {
    /* fall through to defaults */
  }
  return DEFAULT_MACROS;
}

// The Client's keyer body: keyer settings, inputs, sidetone, CW macros, and the pairing panel
// (WAN token or LAN advertise). State lives in App; this renders it and sends commands.
export default function KeyerTab({ settings, setSettings, connection, ports, audioDevices, errors, advertising, onPair }) {
  const [token, setToken] = useState("");
  const [typeAhead, setTypeAhead] = useState("");
  const [macros, setMacros] = useState(loadMacros);
  const [editing, setEditing] = useState(false);

  const paired = connection.state === "connected";
  const connecting = connection.state === "connecting";

  // Audio selection model: settings.audioDevice is null (follow system default), "" (off), or a
  // device name. When null, show the actual default device by name so the user sees what they
  // are hearing through. "(None)" = off = "".
  const defaultDevice = audioDevices.find((d) => d.isDefault)?.name || "";
  const audioValue = settings.audioDevice == null ? defaultDevice : settings.audioDevice;

  // Type-ahead: any text added to the box, typed or pasted, is appended to the send buffer and
  // keyed with proper CW spacing. We diff against what we have already sent so a paste of a whole
  // sentence enqueues the whole sentence. ESC aborts and clears.
  function onTypeAheadChange(e) {
    const next = e.target.value;
    if (next.length > typeAhead.length && next.startsWith(typeAhead)) {
      api.sendCw(next.slice(typeAhead.length).toUpperCase());
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
    localStorage.setItem("rwk.macros", JSON.stringify(next));
  }

  return (
    <div className="keyer-body">
      <div className="keyer-top">
        {/* ─── Keyer ─────────────────────────────────────── */}
        <div className="panel">
          <h3>Keyer</h3>
          <div className="wpm">
            {settings.wpm} <small>WPM</small>
          </div>
          {settings.speedLocked && <small className="hint">Set by N1MM while it has the logger open</small>}
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
              value={settings.paddlePort || ""}
              ports={ports}
              error={errors.paddle}
              onChange={(p) => {
                setSettings((s) => ({ ...s, paddlePort: p || null }));
                api.selectPaddlePort(p || null);
              }}
            />
            <PortSelect
              label="Logger (WinKeyer, optional)"
              value={settings.winkeyerPort || ""}
              ports={ports}
              error={errors.winkeyer}
              onChange={(p) => {
                setSettings((s) => ({ ...s, winkeyerPort: p || null }));
                api.selectWinkeyerPort(p || null);
              }}
            />
            <PortSelect
              label="Keying output (rig key, optional)"
              value={settings.keyingPort || ""}
              ports={ports}
              error={errors.keying}
              onChange={(p) => {
                setSettings((s) => ({ ...s, keyingPort: p || null }));
                api.selectKeyingPort(p || null);
              }}
            />
          </div>
        </div>

        {/* ─── Sidetone + CW Macros (right column) ───────── */}
        <div className="keyer-side">
          <div className="panel">
            <h3>Sidetone</h3>
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

      {/* ─── Pairing (full width) ──────────────────────── */}
      <div className="panel">
        <h3>Pair with Station</h3>
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
                <label>Pairing token (from Station)</label>
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
                    onClick={() => onPair(token.trim())}
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
          {connecting && !paired && (
            <div className="row" style={{ gap: 8, alignSelf: "flex-end" }}>
              <RadarSweep size={56} />
              <span style={{ fontSize: 13, color: "var(--text-dim)" }}>Pairing...</span>
            </div>
          )}
        </div>
        {settings.transport === "lan" && (
          <p style={{ color: "var(--text-dim)", fontSize: 12, marginBottom: 0, marginTop: 8 }}>
            {advertising
              ? `Advertising as ${lanName(settings)} — pick this keyer on the Station.`
              : "Enter your callsign (and an optional ID), then Advertise so the Station can find and pick this keyer. No relay, no internet."}
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
  const update = (i, field, v) => setDraft((d) => d.map((m, j) => (i === j ? { ...m, [field]: v } : m)));

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
              <input type="text" value={m.name} onChange={(e) => update(i, "name", e.target.value)} />
              <input type="text" value={m.text} onChange={(e) => update(i, "text", e.target.value)} />
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
