import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { serverApi as api, shellApi, onEvent } from "./api.js";
import TransportSlider from "./TransportSlider.jsx";
import LogPanel from "./LogPanel.jsx";
import ConnectionsTab from "./ConnectionsTab.jsx";
import AboutDialog from "./AboutDialog.jsx";
import { APP_NAME_SERVER } from "./app-info.js";

// How long the splash popup stays up once the main UI has loaded.
const SPLASH_MS = 5000;

// The Remote Keyer Player: keying-output configuration, WAN/LAN choice, the pairing token to
// hand the Client, the persisted LAN display identity it advertises, KEY/PTT indicators, and a
// fail-safe banner.
export default function ServerApp() {
  const [settings, setSettings] = useState({
    keyingPort: null,
    loggerPort: null,
    keyLine: "dtr",
    pttLine: "rts",
    pttHangMs: 200,
    transport: "wan",
    relay: "",
    token: "",
    callsign: "",
    keyerId: "",
  });
  const [connection, setConnection] = useState({ state: "disconnected", path: "none" });
  const [keyDown, setKeyDown] = useState(false);
  const [rtt, setRtt] = useState(null); // round-trip ms to the Client, null until measured
  const [pttOn, setPttOn] = useState(false);
  const [lastFailSafe, setLastFailSafe] = useState(null);
  const [log, setLog] = useState([]);
  const [ports, setPorts] = useState([]);
  const [errors, setErrors] = useState({}); // control -> short red message
  const [advertising, setAdvertising] = useState(false);
  const [tab, setTab] = useState("main"); // "main" | "connections"
  const [aboutOpen, setAboutOpen] = useState(false);
  // The two snapshots that make the main UI complete: first settings snapshot and ports.
  const [settingsLoaded, setSettingsLoaded] = useState(false);
  const [portsLoaded, setPortsLoaded] = useState(false);

  useEffect(() => {
    const unlisten = onEvent((ev) => {
      switch (ev.kind) {
        case "settings":
          setSettingsLoaded(true);
          setSettings({
            keyingPort: ev.keying_port,
            loggerPort: ev.logger_port,
            keyLine: ev.key_line,
            pttLine: ev.ptt_line,
            pttHangMs: ev.ptt_hang_ms,
            transport: ev.transport,
            relay: ev.relay,
            token: ev.token,
            callsign: ev.callsign ?? "",
            keyerId: ev.keyer_id ?? "",
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
        case "advertising":
          setAdvertising(ev.active);
          break;
        case "ports":
          setPorts(ev.available || []);
          setPortsLoaded(true);
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

  const running = connection.state === "connected";
  const connecting = connection.state === "connecting";
  const hasPlayerIdentity = Boolean(settings.callsign.trim() || settings.keyerId.trim());
  const pathText =
    running ? (connection.path === "relay" ? "DERP" : "Direct") : connecting ? "Waiting…" : "Stopped";
  const linkColor = running ? "var(--good)" : connecting ? "var(--warn)" : "var(--text-dim)";

  return (
    <div className="app">
      <div className="statusbar" style={{ borderTop: "none", borderBottom: "1px solid var(--border)" }}>
        <strong style={{ color: "var(--accent)" }}>W1VE Remote Keyer Player</strong>
        <span className="spring" />
        {running && <span>RTT {rtt ?? "—"} ms</span>}
        {running && <span style={{ margin: "0 8px", color: "var(--text-dim)" }}>|</span>}
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
                    <option value="dtr" disabled={settings.pttLine === "dtr"}>DTR</option>
                    <option value="rts" disabled={settings.pttLine === "rts"}>RTS</option>
                  </select>
                </div>
                <div className="field" style={{ flex: 1, marginBottom: 0 }}>
                  <label>PTT line</label>
                  <select
                    value={settings.pttLine}
                    onChange={(e) => api.setPttLine(e.target.value)}
                  >
                    <option value="rts" disabled={settings.keyLine === "rts"}>RTS</option>
                    <option value="dtr" disabled={settings.keyLine === "dtr"}>DTR</option>
                    <option value="none">None</option>
                  </select>
                  <label style={{ marginTop: 8 }}>PTT hang: {settings.pttHangMs} ms</label>
                  <input
                    type="range"
                    min={75}
                    max={600}
                    step={5}
                    value={settings.pttHangMs}
                    onChange={(e) => {
                      const v = Number(e.target.value);
                      setSettings((s) => ({ ...s, pttHangMs: v }));
                    }}
                    onPointerUp={(e) => api.setPttHang(Number(e.currentTarget.value))}
                    onKeyUp={(e) => api.setPttHang(Number(e.currentTarget.value))}
                  />
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
                    setSettings((s) => ({ ...s, transport: t }));
                    api.setTransport(t);
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
                  <div className="field" style={{ marginBottom: 0 }}>
                    <label>Player callsign</label>
                    <input
                      type="text"
                      value={settings.callsign}
                      placeholder="Station callsign"
                      disabled={running || connecting}
                      onChange={(e) => {
                        setSettings((s) => ({ ...s, callsign: e.target.value }));
                        api.setCallsign(e.target.value);
                      }}
                      style={{ width: 150 }}
                    />
                  </div>
                  <div className="field" style={{ marginBottom: 0 }}>
                    <label>Player ID</label>
                    <input
                      type="text"
                      value={settings.keyerId}
                      placeholder="20m, etc."
                      disabled={running || connecting}
                      onChange={(e) => {
                        setSettings((s) => ({ ...s, keyerId: e.target.value }));
                        api.setKeyerId(e.target.value);
                      }}
                      style={{ width: 130 }}
                    />
                  </div>
                  <div style={{ alignSelf: "flex-end" }}>
                    {running || connecting ? (
                      <button onClick={() => api.lanAdvertise(false)} style={{ background: "#5a2d2d" }}>
                        {connecting ? "Stop" : "Unpair"}
                      </button>
                    ) : (
                      <button
                        onClick={() => api.lanAdvertise(true)}
                        disabled={!hasPlayerIdentity}
                        style={{ background: "var(--accent-dim)", borderColor: "var(--accent)" }}
                      >
                        Advertise
                      </button>
                    )}
                  </div>
                </>
              )}
            </div>
            <p style={{ color: "var(--text-dim)", fontSize: 12, marginBottom: 0, marginTop: 8 }}>
              {settings.transport === "wan"
                ? "Start and give the token to the Client. A keying port is optional — without one the KEY LED still follows the Client; pick or change the port any time."
                : advertising
                  ? `Advertising ${playerName(settings)} as a desktop Player. The operating Keyer can select it on this LAN.`
                  : "Enter a callsign and optional Player ID, then Advertise. The operating Keyer selects this desktop Player; no relay or internet is needed."}
            </p>
          </div>

          <LogPanel log={log} />
        </div>
      </div>
      )}

      <div className="statusbar">
        <span className="spring" />
        <span style={{ color: keyDown ? "var(--key-on)" : "var(--text-dim)" }}>
          Key: {keyDown ? "Down" : "Up"}
        </span>
        <span style={{ color: pttOn ? "var(--ptt-on)" : "var(--text-dim)" }}>
          PTT: {pttOn ? "On" : "Off"}
        </span>
      </div>

      {aboutOpen && <AboutDialog name={APP_NAME_SERVER} onClose={() => setAboutOpen(false)} />}
    </div>
  );
}

function playerName(settings) {
  const callsign = settings.callsign.trim();
  const keyerId = settings.keyerId.trim();
  if (callsign && keyerId) return `${callsign}-${keyerId}`;
  return callsign || keyerId || "(unnamed Player)";
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
