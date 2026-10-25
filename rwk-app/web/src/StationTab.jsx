import { useEffect, useState } from "react";
import { stationApi as api } from "./station-api.js";
import TransportSlider from "./TransportSlider.jsx";
import RadarSweep from "./RadarSweep.jsx";

// The Station body: keying status (KEY/PTT and the fail-safe banner), keying output (the rig's
// serial port, its key and PTT lines, the PTT hang, and the logger's WinKeyer COM port), the
// port-forward table the Operator pushes (read-only), and the pairing panel. Nothing here is
// gated on a keying port: with none selected the KEY/PTT LEDs still follow the Client.
export default function StationTab({
  settings,
  setSettings,
  connection,
  keyDown,
  pttOn,
  lastFailSafe,
  lanPeers,
  ports,
  errors,
  rules,
  onListen,
}) {
  const running = connection.state === "connected";
  const connecting = connection.state === "connecting";
  const [pickedKey, setPickedKey] = useState("");
  // The PTT hang is bound when a pairing starts, so it is locked while linked. Ports and lines
  // take effect at once.
  const hangLocked = running || connecting;
  const pttNone = settings.pttLine === "none";
  const hangDisabled = hangLocked || pttNone;

  // Keep the picked keyer valid as the live list changes.
  useEffect(() => {
    if (pickedKey && !lanPeers.some((p) => p.key === pickedKey)) {
      setPickedKey(lanPeers[0]?.key || "");
    } else if (!pickedKey && lanPeers.length > 0) {
      setPickedKey(lanPeers[0].key);
    }
  }, [lanPeers, pickedKey]);

  return (
    <div className="tabbody">
      <div style={{ display: "flex", flexDirection: "column", gap: 12, position: "relative", minHeight: "100%" }}>
        <div style={{ position: "relative", display: "flex", flexDirection: "column", gap: 12, flex: 1, minHeight: 0 }}>
        {!running && (
          <div
            aria-hidden="true"
            style={{ position: "absolute", inset: 0, zIndex: 1, background: "rgba(255, 255, 255, 0.5)", borderRadius: 6 }}
          />
        )}
        <div className="station-row">
          <div className="panel">
            <h3>Keying Status</h3>
            <div className="row" style={{ gap: 18, justifyContent: "center", padding: "2px 0" }}>
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
            <div className="row" style={{ gap: 10, alignItems: "flex-end" }}>
              <div style={{ flex: 1.6 }}>
                <PortSelect
                  label="Serial port (rig key line)"
                  value={settings.keyingPort || ""}
                  ports={ports}
                  error={errors.keying}
                  onChange={(p) => api.selectKeyingPort(p || null)}
                />
              </div>
              <div className="field" style={{ flex: 1, marginBottom: 0 }}>
                <label>Key line</label>                  <select
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
              </div>
              <div className="field" style={{ flex: "0 0 170px", marginBottom: 0, opacity: pttNone ? 0.45 : 1 }}>
              <label>PTT hang: {settings.pttHangMs} ms</label>
              <input
                type="range"
                min={75}
                max={600}
                step={5}
                value={settings.pttHangMs}
                disabled={hangDisabled}
                onChange={(e) => {
                  const v = Number(e.target.value);
                  setSettings((s) => ({ ...s, pttHangMs: v }));
                }}
                onPointerUp={(e) => api.setPttHang(Number(e.currentTarget.value))}
                onKeyUp={(e) => api.setPttHang(Number(e.currentTarget.value))}
              />
              </div>
            </div>
            <PortSelect
              label="Logger COM port (WinKeyer, optional)"
              value={settings.loggerPort || ""}
              ports={ports}
              error={errors.logger}
              onChange={(p) => api.setLoggerPort(p || null)}
            />
            {hangLocked && (
              <p style={{ color: "var(--text-dim)", fontSize: 12, margin: "8px 0 0" }}>
                PTT hang applies at the next pairing.
              </p>
            )}
          </div>
        </div>

        {/* ─── Port forwards pushed from the Operator (read-only) ───────── */}
        <div className="panel" style={{ flex: 1, minHeight: 120, overflowY: "auto", display: "flex", flexDirection: "column" }}>
          <h3>Port Forwards (from Operator)</h3>
          {rules.length === 0 ? (
            <div style={{ flex: 1, display: "flex", alignItems: "center", justifyContent: "center", color: "var(--text-dim)", fontSize: 13 }}>
              (Port Forwards show here.)
            </div>
          ) : (
            <table className="rules">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Protocol</th>
                  <th>Operator port</th>
                  <th>Station target</th>
                  <th>Status</th>
                </tr>
              </thead>
              <tbody>
                {rules.map((r) => (
                  <tr key={r.id}>
                    <td>{r.name}</td>
                    <td>{String(r.protocol).toUpperCase()}</td>
                    <td>{r.client_port}</td>
                    <td>
                      {r.station_target_address}:{r.station_port}
                    </td>
                    <td style={{ color: r.enabled ? "var(--good)" : "var(--text-dim)" }}>
                      {r.enabled ? "Enabled" : "Disabled"}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>

        </div>

        {/* ─── Pairing (first on the page; overlays the rest until linked) ─── */}
        <div className="panel" style={{ order: -1, position: "relative", zIndex: 2, background: "var(--bg-panel)" }}>
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
                    <span className="token">{settings.pairingCode || "…"}</span>
                    <button onClick={() => navigator.clipboard?.writeText(settings.pairingCode || "")} title="Copy">
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
                      onClick={() => onListen()}
                      disabled={!settings.pairingCode}
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
                  <select value={pickedKey} disabled={running || connecting} onChange={(e) => setPickedKey(e.target.value)}>
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
            {connecting && !running && (
              <div className="row" style={{ gap: 8, alignSelf: "flex-end" }}>
                <RadarSweep size={56} />
                <span style={{ fontSize: 13, color: "var(--text-dim)" }}>Pairing...</span>
              </div>
            )}
          </div>
          <p style={{ color: "var(--text-dim)", fontSize: 12, marginBottom: 0, marginTop: 8 }}>
            {settings.transport === "wan"
              ? "Start and give the token to the Client. A keying port is optional — without one the KEY LED still follows the Client. Choose the keying port and PTT line before you pair."
              : "Pick a keyer advertising on your LAN, then Connect. A keying port is optional and is chosen before you pair. No relay, no internet."}
          </p>
        </div>
      </div>
    </div>
  );
}

function Indicator({ label, on, onColor }) {
  return (
    <div style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 6 }}>
      <div
        style={{
          width: 26,
          height: 26,
          borderRadius: "50%",
          background: on ? onColor : "var(--bg-panel-2)",
          border: `2px solid ${on ? onColor : "var(--border)"}`,
          boxShadow: on ? `0 0 14px ${onColor}` : "none",
          transition: "background 40ms, box-shadow 40ms",
        }}
      />
      <span style={{ fontSize: 11, fontWeight: 700, color: on ? onColor : "var(--text-dim)" }}>{label}</span>
    </div>
  );
}

// A serial-port picker with "(None)" first. A selected port that is not in the live list (busy or
// hidden) stays visible so the operator can see what is chosen.
function PortSelect({ label, value, ports, error, onChange, disabled }) {
  const list = value && !ports.includes(value) ? [...ports, value] : ports;
  return (
    <div className="field" style={{ marginBottom: 0 }}>
      <label>{label}</label>
      <select value={value} disabled={disabled} onChange={(e) => onChange(e.target.value)}>
        <option value="">(None)</option>
        {list.map((p) => (
          <option key={p} value={p}>
            {p}
          </option>
        ))}
      </select>
      {error && <div style={{ color: "var(--bad, #e06c6c)", fontSize: 12, marginTop: 4 }}>{error}</div>}
    </div>
  );
}

