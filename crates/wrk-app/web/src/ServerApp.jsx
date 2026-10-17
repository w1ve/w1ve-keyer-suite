import { useEffect, useState } from "react";
import { serverApi as api, onEvent } from "./api.js";
import TransportSlider from "./TransportSlider.jsx";
import LogPanel from "./LogPanel.jsx";

// The Remote Keyer Server: keying-output configuration, WAN/LAN choice, the pairing token to
// hand the Client, KEY/PTT indicators, and a fail-safe banner. Same look-and-feel as the
// Keyer-Router Station.
export default function ServerApp() {
  const [settings, setSettings] = useState({
    keyingPort: null,
    loggerPort: null,
    keyLine: "dtr",
    pttLine: "rts",
    transport: "wan",
    relay: "",
    token: "",
  });
  const [connection, setConnection] = useState({ state: "disconnected", path: "none" });
  const [keyDown, setKeyDown] = useState(false);
  const [rtt, setRtt] = useState(null); // round-trip ms to the Client, null until measured
  const [pttOn, setPttOn] = useState(false);
  const [lastFailSafe, setLastFailSafe] = useState(null);
  const [log, setLog] = useState([]);
  const [ports, setPorts] = useState([]);
  const [errors, setErrors] = useState({}); // control -> short red message
  const [lanPeers, setLanPeers] = useState([]);
  const [pickedKey, setPickedKey] = useState("");

  useEffect(() => {
    const unlisten = onEvent((ev) => {
      switch (ev.kind) {
        case "settings":
          setSettings({
            keyingPort: ev.keying_port,
            loggerPort: ev.logger_port,
            keyLine: ev.key_line,
            pttLine: ev.ptt_line,
            transport: ev.transport,
            relay: ev.relay,
            token: ev.token,
          });
          break;
        case "connection":
          setConnection({ state: ev.state, path: ev.path });
          if (ev.state !== "connected") setLastFailSafe(null);
          break;
        case "rtt":
          setRtt(ev.ms ?? null);
          break;
        case "keyState":
          setKeyDown(ev.down);
          break;
        case "ptt":
          setPttOn(ev.asserted);
          break;
        case "failSafe":
          setLastFailSafe({ code: ev.code, message: ev.message });
          break;
        case "lanPeers":
          setLanPeers(ev.peers || []);
          break;
        case "ports":
          setPorts(ev.available || []);
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

  // Keep the picked keyer valid as the live list changes.
  useEffect(() => {
    if (pickedKey && !lanPeers.some((p) => p.key === pickedKey)) {
      setPickedKey(lanPeers[0]?.key || "");
    } else if (!pickedKey && lanPeers.length > 0) {
      setPickedKey(lanPeers[0].key);
    }
  }, [lanPeers, pickedKey]);

  const running = connection.state === "connected";
  const connecting = connection.state === "connecting";
  const pathText =
    running ? (connection.path === "relay" ? "DERP" : "Direct") : connecting ? "Waiting…" : "Stopped";
  const linkColor = running ? "var(--good)" : connecting ? "var(--warn)" : "var(--text-dim)";

  return (
    <div className="app">
      <div className="statusbar" style={{ borderTop: "none", borderBottom: "1px solid var(--border)" }}>
        <strong style={{ color: "var(--accent)" }}>W1VE Remote Keyer — Player</strong>
        <span className="spring" />
        {running && <span>RTT {rtt ?? "—"} ms</span>}
        {running && <span style={{ margin: "0 8px", color: "var(--text-dim)" }}>|</span>}
        <span className="led" style={{ color: linkColor }}>●</span>
        <span>{pathText}</span>
      </div>

      <div className="tabbody">
        <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
          <div className="panel-row" style={{ gridTemplateColumns: "1fr 1fr" }}>
            <div className="panel">
              <h3>Keying Status</h3>
              <div className="row" style={{ gap: 24, justifyContent: "center", padding: "8px 0" }}>
                <Indicator label="KEY" on={keyDown} onColor="var(--key-on)" />
                <Indicator label="PTT" on={pttOn} onColor="var(--ptt-on)" />
              </div>
              {lastFailSafe ? (
                <div
                  style={{
                    marginTop: 8,
                    padding: "8px 10px",
                    borderRadius: 6,
                    background: "#3a2323",
                    border: "1px solid var(--bad)",
                    color: "var(--bad)",
                    fontSize: 13,
                  }}
                >
                  Fail-safe F{lastFailSafe.code}: {lastFailSafe.message}
                </div>
              ) : (
                <p style={{ color: "var(--text-dim)", fontSize: 12, textAlign: "center", margin: 0 }}>
                  {running ? "Keying the radio from the Client." : "Not running."}
                </p>
              )}
            </div>

            <div className="panel">
              <h3>Keying Output</h3>
              <div className="field">
                <label>Serial port</label>
                <select
                  value={settings.keyingPort || ""}
                  onChange={(e) => api.selectKeyingPort(e.target.value || null)}
                >
                  <option value="">(None)</option>
                  {(settings.keyingPort && !ports.includes(settings.keyingPort)
                    ? [...ports, settings.keyingPort]
                    : ports
                  ).map((p) => (
                    <option key={p} value={p}>
                      {p}
                    </option>
                  ))}
                </select>
                {errors.keying && (
                  <div style={{ color: "var(--bad, #e06c6c)", fontSize: 12, marginTop: 4 }}>
                    {errors.keying}
                  </div>
                )}
              </div>
              <div className="field">
                <label>Logger port (WinKeyer, optional)</label>
                <select
                  value={settings.loggerPort || ""}
                  onChange={(e) => api.selectLoggerPort(e.target.value || null)}
                >
                  <option value="">(None)</option>
                  {(settings.loggerPort && !ports.includes(settings.loggerPort)
                    ? [...ports, settings.loggerPort]
                    : ports
                  ).map((p) => (
                    <option key={p} value={p}>
                      {p}
                    </option>
                  ))}
                </select>
                {errors.logger && (
                  <div style={{ color: "var(--bad, #e06c6c)", fontSize: 12, marginTop: 4 }}>
                    {errors.logger}
                  </div>
                )}
              </div>
              <div className="row" style={{ gap: 10 }}>
                <div className="field" style={{ flex: 1, marginBottom: 0 }}>
                  <label>Key line</label>
                  <select
                    value={settings.keyLine}
                    onChange={(e) => api.setKeyLine(e.target.value)}
                  >
                    <option value="dtr">DTR</option>
                    <option value="rts">RTS</option>
                  </select>
                </div>
                <div className="field" style={{ flex: 1, marginBottom: 0 }}>
                  <label>PTT line</label>
                  <select
                    value={settings.pttLine}
                    onChange={(e) => api.setPttLine(e.target.value)}
                  >
                    <option value="rts">RTS</option>
                    <option value="dtr">DTR</option>
                    <option value="none">None</option>
                  </select>
                </div>
              </div>
            </div>
          </div>

          {/* ─── Pairing ───────────────────────────────────────── */}
          <div className="panel">
            <h3>Pairing</h3>
            <div className="row" style={{ gap: 12, flexWrap: "wrap", alignItems: "flex-end" }}>
              <div>
                <label style={{ display: "block", fontSize: 12, color: "var(--text-dim)", marginBottom: 4 }}>
                  Transport
                </label>
                <TransportSlider
                  value={settings.transport}
                  disabled={running || connecting}
                  onChange={(t) => {
                    api.setTransport(t);
                    if (t === "lan") api.lanBrowse();
                  }}
                />
              </div>

              {settings.transport === "wan" ? (
                <>
                  <div>
                    <label style={{ display: "block", fontSize: 12, color: "var(--text-dim)", marginBottom: 4 }}>
                      Pairing token (give this to the Client)
                    </label>
                    <div className="row" style={{ gap: 8 }}>
                      <span className="token">{settings.token || "…"}</span>
                      <button onClick={() => navigator.clipboard?.writeText(settings.token)} title="Copy">
                        Copy
                      </button>
                      <button onClick={() => api.regenerateToken()} disabled={running || connecting}>
                        New
                      </button>
                    </div>
                  </div>
                  <div style={{ alignSelf: "flex-end" }}>
                    {running || connecting ? (
                      <button onClick={() => api.disconnect()} style={{ background: "#5a2d2d" }}>
                        {connecting ? "Cancel" : "Stop"}
                      </button>
                    ) : (
                      <button
                        onClick={() => api.listen()}
                        style={{ background: "var(--accent-dim)", borderColor: "var(--accent)" }}
                      >
                        Start
                      </button>
                    )}
                  </div>
                </>
              ) : (
                <>
                  <div className="field" style={{ marginBottom: 0, minWidth: 220 }}>
                    <label>Available keyers</label>
                    <select
                      value={pickedKey}
                      disabled={running || connecting}
                      onChange={(e) => setPickedKey(e.target.value)}
                    >
                      {lanPeers.length === 0 && <option value="">(searching…)</option>}
                      {lanPeers.map((p) => (
                        <option key={p.key} value={p.key}>
                          {p.name}
                        </option>
                      ))}
                    </select>
                  </div>
                  <div style={{ alignSelf: "flex-end" }}>
                    <button onClick={() => api.lanBrowse()} disabled={running || connecting}>
                      Refresh
                    </button>
                  </div>
                  <div style={{ alignSelf: "flex-end" }}>
                    {running || connecting ? (
                      <button onClick={() => api.disconnect()} style={{ background: "#5a2d2d" }}>
                        {connecting ? "Cancel" : "Stop"}
                      </button>
                    ) : (
                      <button
                        onClick={() => {
                          const peer = lanPeers.find((p) => p.key === pickedKey);
                          if (peer) api.connectLan(peer.key, peer.endpoint);
                        }}
                        disabled={!pickedKey}
                        style={{ background: "var(--accent-dim)", borderColor: "var(--accent)" }}
                      >
                        Connect
                      </button>
                    )}
                  </div>
                </>
              )}
            </div>
            <p style={{ color: "var(--text-dim)", fontSize: 12, marginBottom: 0, marginTop: 8 }}>
              {settings.transport === "wan"
                ? "Start and give the token to the Client. A keying port is optional — without one the KEY LED still follows the Client; pick or change the port any time."
                : "Pick a keyer advertising on your LAN, then Connect. A keying port is optional and can be set or changed any time. No relay, no internet."}
            </p>
          </div>

          <LogPanel log={log} />
        </div>
      </div>

      <div className="statusbar">
        <span className="spring" />
        <span style={{ color: keyDown ? "var(--key-on)" : "var(--text-dim)" }}>
          Key: {keyDown ? "Down" : "Up"}
        </span>
        <span style={{ color: pttOn ? "var(--ptt-on)" : "var(--text-dim)" }}>
          PTT: {pttOn ? "On" : "Off"}
        </span>
      </div>
    </div>
  );
}

function Indicator({ label, on, onColor }) {
  return (
    <div style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 6 }}>
      <div
        style={{
          width: 44,
          height: 44,
          borderRadius: "50%",
          background: on ? onColor : "var(--bg-panel-2)",
          border: `2px solid ${on ? onColor : "var(--border)"}`,
          boxShadow: on ? `0 0 14px ${onColor}` : "none",
          transition: "background 40ms, box-shadow 40ms",
        }}
      />
      <span style={{ fontSize: 13, fontWeight: 700, color: on ? onColor : "var(--text-dim)" }}>
        {label}
      </span>
    </div>
  );
}
