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

// The whole W1VE Keyer UI: one compact window. Keyer + Sidetone across the top, the three
// serial ports and keyer macros/type-ahead below, and a KEY/PTT/SENDING status strip.
export default function App() {
  // Settings mirror the backend echo (single source of truth for ports + keyer config).
  const [settings, setSettings] = useState(null);
  const [ports, setPorts] = useState([]);
  const [keyDown, setKeyDown] = useState(false);
  const [pttOn, setPttOn] = useState(false);
  const [sending, setSending] = useState(false);
  const [log, setLog] = useState([]);
  const [showLog, setShowLog] = useState(false);

  const [macros, setMacros] = useState(loadMacros);
  const [editing, setEditing] = useState(false);
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
          setLog((l) => [`${ev.level.toUpperCase()}  ${ev.message}`, ...l].slice(0, 200));
          break;
        default:
          break;
      }
    }).then((fn) => {
      unlisten.current = fn;
    });
    // Prime the port list immediately (the backend also pushes it).
    api.serialPorts().then(setPorts).catch(() => {});
    return () => {
      if (unlisten.current) unlisten.current();
    };
  }, []);

  function saveMacros(next) {
    setMacros(next);
    localStorage.setItem("wk.macros", JSON.stringify(next));
  }

  function onTypeAheadKey(e) {
    const ch = e.key;
    if (ch.length === 1 && ch >= " " && ch <= "~") {
      api.sendCw(ch.toUpperCase());
    }
  }

  if (!settings) {
    return (
      <div className="app loading">
        Starting…
      </div>
    );
  }

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
              onChange={(e) => api.setSpeed(Number(e.target.value))}
            />
            <div className="field" style={{ marginTop: 8, marginBottom: 8 }}>
              <label>Weight: {settings.weight}%</label>
              <input
                type="range"
                min={25}
                max={75}
                value={settings.weight}
                onChange={(e) => api.setWeight(Number(e.target.value))}
              />
            </div>
            <div className="row" style={{ gap: 8 }}>
              <div className="field" style={{ flex: 1, marginBottom: 0 }}>
                <label>Mode</label>
                <select
                  value={settings.mode}
                  onChange={(e) => api.setMode(e.target.value)}
                >
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
                  onChange={(e) => api.setPaddleReverse(e.target.checked)}
                />
                Reverse
              </label>
            </div>
          </div>

          {/* ─── Sidetone ──────────────────────────────── */}
          <div className="panel">
            <h3>Sidetone</h3>
            <div className="field" style={{ marginBottom: 8 }}>
              <label>Frequency: {settings.toneHz} Hz</label>
              <input
                type="range"
                min={300}
                max={1200}
                step={10}
                value={settings.toneHz}
                onChange={(e) => api.setToneFrequency(Number(e.target.value))}
              />
            </div>
            <div className="field" style={{ marginBottom: 0 }}>
              <label>Volume: {settings.toneVolume}%</label>
              <input
                type="range"
                min={0}
                max={100}
                value={settings.toneVolume}
                onChange={(e) => api.setToneVolume(Number(e.target.value))}
              />
            </div>
          </div>
        </div>

        {/* ─── Serial ports (the three I/O legs) ───────── */}
        <div className="panel">
          <h3>Serial Ports</h3>
          <div className="ports">
            <PortSelect
              label="Paddle input"
              value={settings.paddlePort}
              ports={ports}
              onChange={(p) => api.selectPaddlePort(p)}
            />
            <div className="field" style={{ marginBottom: 0 }}>
              <label className="row spread">
                <span>Logger (WinKeyer)</span>
                <label className="row toggle" style={{ gap: 4 }}>
                  <input
                    type="checkbox"
                    checked={settings.loggerEnabled}
                    onChange={(e) => api.setLoggerEnabled(e.target.checked)}
                  />
                  on
                </label>
              </label>
              <select
                value={settings.loggerPort || ""}
                disabled={!settings.loggerEnabled}
                onChange={(e) => api.selectLoggerPort(e.target.value || null)}
              >
                <option value="">(None)</option>
                {portOptions(ports, settings.loggerPort, [settings.paddlePort, settings.keyingPort])}
              </select>
            </div>
            <PortSelect
              label="Keying output"
              value={settings.keyingPort}
              ports={ports}
              exclude={[settings.paddlePort, settings.loggerPort]}
              onChange={(p) => api.selectKeyingPort(p)}
            />
            <div className="row" style={{ gap: 8 }}>
              <div className="field" style={{ flex: 1, marginBottom: 0 }}>
                <label>Key line</label>
                <select value={settings.keyLine} onChange={(e) => api.setKeyLine(e.target.value)}>
                  <option value="dtr">DTR</option>
                  <option value="rts">RTS</option>
                </select>
              </div>
              <div className="field" style={{ flex: 1, marginBottom: 0 }}>
                <label>PTT line</label>
                <select value={settings.pttLine} onChange={(e) => api.setPttLine(e.target.value)}>
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
              placeholder="Type CW (sent as you type)…"
              onKeyPress={onTypeAheadKey}
              style={{ flex: 1 }}
            />
            <button onClick={() => api.testTx()}>Test</button>
          </div>
        </div>

        {showLog && (
          <div className="panel log">
            {log.length === 0 ? "(no messages yet)" : log.join("\n")}
          </div>
        )}
      </div>

      {/* ─── Status strip ──────────────────────────────── */}
      <div className="statusbar">
        <Led label="KEY" on={keyDown} color="var(--key-on)" />
        <Led label="PTT" on={pttOn} color="var(--ptt-on)" />
        <Led label="SEND" on={sending} color="var(--accent)" />
        <span className="spring" />
        <button className="linklike" onClick={() => setShowLog((v) => !v)}>
          {showLog ? "Hide log" : "Log"}
        </button>
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

// Renders <option>s for a port <select>: always (None), the live ports, plus the current
// value if it is somehow absent (so a mid-removal render never loses the label). `exclude`
// hides ports already chosen for another leg, but never hides the current value.
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

function PortSelect({ label, value, ports, exclude = [], onChange }) {
  return (
    <div className="field" style={{ marginBottom: 0 }}>
      <label>{label}</label>
      <select value={value || ""} onChange={(e) => onChange(e.target.value || null)}>
        <option value="">(None)</option>
        {portOptions(ports, value, exclude)}
      </select>
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
