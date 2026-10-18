import { Fragment, useEffect, useRef, useState } from "react";
import { api, onEvent } from "./api.js";

const MODES = [
  ["iambicB", "Iambic B"],
  ["iambicA", "Iambic A"],
  ["ultimatic", "Ultimatic"],
  ["bug", "Bug"],
  ["straight", "Straight"],
];

const DEFAULT_MACROS = [
  { name: "CQ", text: "CQ DE W1VE" },
  { name: "599", text: "599" },
  { name: "TU", text: "TU" },
  { name: "73", text: "73" },
  { name: "MYCALL", text: "W1VE" },
  { name: "QRL?", text: "QRL?" },
  { name: "?", text: "?" },
  { name: "AGN", text: "AGN" },
];

function loadMacros() {
  try {
    const raw = localStorage.getItem("wk.macros");
    if (raw) return JSON.parse(raw);
  } catch {
    /* fall through to defaults */
  }
  return DEFAULT_MACROS;
}

// Is this platform event-driven for device changes? On Windows the backend pushes updated
// device/port lists on WM_DEVICECHANGE, so no manual refresh is needed. Elsewhere we expose a
// Refresh button. navigator.userAgent carries the OS in the webview.
const IS_WINDOWS = /windows/i.test(navigator.userAgent);

// The whole W1VE Keyer UI: one compact window. Keyer + Sidetone across the top, the three
// serial ports and keyer macros/type-ahead below, and a KEY/PTT/SEND status strip.
export default function App() {
  const [settings, setSettings] = useState(null);
  const [ports, setPorts] = useState([]);
  const [audioDevices, setAudioDevices] = useState([]);
  const [errors, setErrors] = useState({}); // control -> short red message
  const [keyDown, setKeyDown] = useState(false);
  const [pttOn, setPttOn] = useState(false);
  const [sending, setSending] = useState(false);
  const [log, setLog] = useState([]);

  const [macros, setMacros] = useState(loadMacros);
  const [editing, setEditing] = useState(false);
  const [typeAhead, setTypeAhead] = useState("");
  const unlisten = useRef(null);

  useEffect(() => {
    onEvent((ev) => {
      switch (ev.kind) {
        case "settings":
          setSettings(ev);
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
        case "keyState":
          setKeyDown(ev.down);
          break;
        case "pttState":
          setPttOn(ev.on);
          break;
        case "sending":
          setSending(ev.active);
          break;
        case "log":
          setLog((l) => [...l, `${ev.level.toUpperCase()}  ${ev.message}`].slice(-500));
          break;
        default:
          break;
      }
    }).then((fn) => {
      unlisten.current = fn;
      api.requestState().catch(() => {});
    });
    return () => {
      if (unlisten.current) unlisten.current();
    };
  }, []);

  // Global ESC: abort sending from anywhere in the window (e.g. while a macro is playing and
  // the type-ahead box is not focused). The type-ahead box also handles ESC to clear itself.
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

  function saveMacros(next) {
    setMacros(next);
    localStorage.setItem("wk.macros", JSON.stringify(next));
  }

  // Type-ahead: any text added to the box — typed OR pasted — is appended to the send buffer
  // and keyed with proper CW spacing. We diff against what we have already sent so a paste of
  // a whole sentence enqueues the whole sentence (not just one key). ESC aborts + clears.
  function onTypeAheadChange(e) {
    const next = e.target.value;
    // Characters appended since last time: send them. (If the user edits earlier text we only
    // act on a growing tail, which is the normal type/paste case.)
    if (next.length > typeAhead.length && next.startsWith(typeAhead)) {
      const added = next.slice(typeAhead.length);
      api.sendCw(added.toUpperCase());
    } else if (next.length > typeAhead.length) {
      // Non-append edit (rare): send the whole new value's added remainder conservatively.
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

  // A controlled numeric setting: render from `settings` but allow immediate local override so
  // the slider + readout move instantly, before the next backend echo.
  function patch(field, value) {
    setSettings((s) => (s ? { ...s, [field]: value } : s));
  }

  if (!settings) {
    return <div className="app loading">Starting…</div>;
  }

  // Audio selection model: settings.audioDevice is null (follow system default), "" (off), or
  // a device name. When null, show the actual default device by name so the user sees what
  // they are hearing through. "(None)" = off = "".
  const defaultDevice = audioDevices.find((d) => d.isDefault)?.name || "";
  const audioValue = settings.audioDevice == null ? defaultDevice : settings.audioDevice;

  return (
    <div className="app">
      <div className="body">
        <div className="col2">
          {/* ─── Keyer ─────────────────────────────────── */}
          <div className="panel">
            <h3>Keyer</h3>
            <div className="row spread" style={{ alignItems: "flex-end", marginBottom: 6 }}>
              <div className="wpm">
                {settings.wpm} <small>WPM</small>
              </div>
            </div>
            <input
              type="range"
              min={5}
              max={60}
              value={settings.wpm}
              onChange={(e) => {
                const v = Number(e.target.value);
                patch("wpm", v);
                api.setSpeed(v);
              }}
            />
            <div className="field" style={{ marginTop: 8, marginBottom: 8 }}>
              <label>Weight: {settings.weight}%</label>
              <input
                type="range"
                min={25}
                max={75}
                value={settings.weight}
                onChange={(e) => {
                  const v = Number(e.target.value);
                  patch("weight", v);
                  api.setWeight(v);
                }}
              />
            </div>
            <div className="row" style={{ gap: 8 }}>
              <div className="field" style={{ flex: 1, marginBottom: 0 }}>
                <label>Mode</label>
                <select value={settings.mode} onChange={(e) => { patch("mode", e.target.value); api.setMode(e.target.value); }}>
                  {MODES.map(([val, label]) => (
                    <option key={val} value={val}>
                      {label}
                    </option>
                  ))}
                </select>
              </div>
              <label className="row" style={{ gap: 6, alignSelf: "flex-end", paddingBottom: 6 }}>
                <input
                  type="checkbox"
                  checked={settings.paddleReverse}
                  onChange={(e) => { patch("paddleReverse", e.target.checked); api.setPaddleReverse(e.target.checked); }}
                />
                Reverse
              </label>
            </div>
          </div>

          {/* ─── Sidetone ──────────────────────────────── */}
          <div className="panel">
            <h3>Sidetone</h3>
            <div className="field" style={{ marginBottom: 8 }}>
              <label className="row spread">
                <span>Device</span>
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
                  // "" -> off; a name equal to the default device -> null (follow default);
                  // any other name -> that device.
                  const device = v === "" ? "" : v === defaultDevice ? null : v;
                  patch("audioDevice", device);
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
            <div className="field" style={{ marginBottom: 8 }}>
              <label>Frequency: {settings.toneHz} Hz</label>
              <input
                type="range"
                min={300}
                max={1200}
                step={10}
                value={settings.toneHz}
                onChange={(e) => {
                  const v = Number(e.target.value);
                  patch("toneHz", v);
                  api.setToneFrequency(v);
                }}
              />
            </div>
            <div className="field" style={{ marginBottom: 0 }}>
              <label>Volume: {settings.toneVolume}%</label>
              <input
                type="range"
                min={0}
                max={100}
                value={settings.toneVolume}
                onChange={(e) => {
                  const v = Number(e.target.value);
                  patch("toneVolume", v);
                  api.setToneVolume(v);
                }}
              />
            </div>
          </div>
        </div>

        {/* ─── Serial ports (the three I/O legs) ───────── */}
        <div className="panel">
          <div className="row spread" style={{ marginBottom: 8 }}>
            <h3 style={{ margin: 0 }}>Serial Ports</h3>
            {!IS_WINDOWS && (
              <button className="mini" onClick={() => api.serialPorts().then(setPorts)}>
                Refresh
              </button>
            )}
          </div>
          <div className="ports">
            <PortSelect
              label="Paddle input"
              value={settings.paddlePort}
              ports={ports}
              error={errors.paddle}
              exclude={[settings.loggerPort, settings.keyingPort]}
              onChange={(p) => { patch("paddlePort", p); api.selectPaddlePort(p); }}
            />
            <PortSelect
              label="Logger Input (WinKeyer)"
              value={settings.loggerPort}
              ports={ports}
              error={errors.logger}
              exclude={[settings.paddlePort, settings.keyingPort]}
              onChange={(p) => { patch("loggerPort", p); api.selectLoggerPort(p); }}
            />
            <PortSelect
              label="Keying output"
              value={settings.keyingPort}
              ports={ports}
              error={errors.keying}
              exclude={[settings.paddlePort, settings.loggerPort]}
              onChange={(p) => { patch("keyingPort", p); api.selectKeyingPort(p); }}
            />
            <div className="row" style={{ gap: 8 }}>
              <div className="field" style={{ flex: 1, marginBottom: 0 }}>
                <label>Key line</label>
                <select value={settings.keyLine} onChange={(e) => { patch("keyLine", e.target.value); api.setKeyLine(e.target.value); }}>
                  <option value="dtr">DTR</option>
                  <option value="rts">RTS</option>
                </select>
              </div>
              <div className="field" style={{ flex: 1, marginBottom: 0 }}>
                <label>PTT line</label>
                <select value={settings.pttLine} onChange={(e) => { patch("pttLine", e.target.value); api.setPttLine(e.target.value); }}>
                  <option value="none">None</option>
                  <option value="rts">RTS</option>
                  <option value="dtr">DTR</option>
                </select>
              </div>
            </div>
          </div>
        </div>

        {/* ─── Macros + type-ahead ─────────────────────── */}
        <div className="panel">
          <div className="row spread" style={{ marginBottom: 8 }}>
            <h3 style={{ margin: 0 }}>Macros</h3>
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
          <div className="row" style={{ marginTop: 8, gap: 6 }}>
            <input
              type="text"
              placeholder="Type or paste CW (sent as entered) — ESC to stop…"
              value={typeAhead}
              onChange={onTypeAheadChange}
              onKeyDown={onTypeAheadKey}
              style={{ flex: 1 }}
            />
            <button onClick={() => api.testTx()}>Test</button>
          </div>
        </div>

        {/* ─── Log (10-line scrollable) ────────────────── */}
        <div className="panel">
          <div className="row spread" style={{ marginBottom: 6 }}>
            <h3 style={{ margin: 0 }}>Log</h3>
            <button className="mini" onClick={() => setLog([])}>
              Clear
            </button>
          </div>
          <LogView lines={log} />
        </div>
      </div>

      {/* ─── Status strip ──────────────────────────────── */}
      <div className="statusbar">
        <Led label="KEY" on={keyDown} color="var(--key-on)" />
        <Led label="PTT" on={pttOn} color="var(--ptt-on)" />
        <Led label="SEND" on={sending} color="var(--accent)" />
        <span className="spring" />
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

// A scrolling, read-only 10-line log. Auto-sticks to the bottom as new lines arrive.
function LogView({ lines }) {
  const ref = useRef(null);
  useEffect(() => {
    const el = ref.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [lines]);
  return (
    <textarea className="log" ref={ref} readOnly rows={10} value={lines.join("\n")} />
  );
}

function ErrorLine({ msg }) {
  if (!msg) return null;
  return <div className="err">{msg}</div>;
}

// Renders <option>s for a port <select>: always (None), the live ports, minus any excluded
// (chosen for another leg), plus the current value if absent so it never loses the label.
function portOptions(ports, value, exclude = []) {
  const ex = exclude.filter((p) => p && p !== value);
  const list = ports.filter((p) => !ex.includes(p));
  if (value && !list.includes(value)) list.push(value);
  return list.map((p) => (
    <option key={p} value={p}>
      {p}
    </option>
  ));
}

function PortSelect({ label, value, ports, exclude = [], error, onChange }) {
  return (
    <div className="field" style={{ marginBottom: 0 }}>
      <label>{label}</label>
      <select value={value || ""} onChange={(e) => onChange(e.target.value || null)}>
        <option value="">(None)</option>
        {portOptions(ports, value, exclude)}
      </select>
      <ErrorLine msg={error} />
    </div>
  );
}

function Led({ label, on, color }) {
  return (
    <span className="row" style={{ gap: 6 }}>
      <span
        style={{
          width: 12,
          height: 12,
          borderRadius: "50%",
          background: on ? color : "var(--bg-panel-2)",
          border: `1px solid ${on ? color : "var(--border)"}`,
          boxShadow: on ? `0 0 8px ${color}` : "none",
        }}
      />
      <span style={{ fontSize: 12, fontWeight: 700, color: on ? color : "var(--text-dim)" }}>
        {label}
      </span>
    </span>
  );
}

function MacroEditor({ macros, onSave, onCancel }) {
  const [draft, setDraft] = useState(macros.map((m) => ({ ...m })));
  const update = (i, field, v) =>
    setDraft((d) => d.map((m, j) => (i === j ? { ...m, [field]: v } : m)));

  return (
    <div className="modal">
      <div className="panel" style={{ width: 420 }}>
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
