import { useEffect, useRef, useState } from "react";
import { onRwkEvent, stationApi } from "./station-api.js";
import StationTab from "./StationTab.jsx";
import RelayDialog, { DEFAULT_WAN_RELAY } from "./RelayDialog.jsx";
import { LogEntries } from "./LogPanel.jsx";

const DEFAULT_SETTINGS = {
  keyingPort: null,
  keyLine: "dtr",
  pttLine: "rts",
  pttHangMs: 200,
  loggerPort: null,
  relay: "",
  pairingCode: "",
  transport: "wan",
};

// The Station application shell: the Remote Keyer Player's status bar (RTT, then Disconnected /
// Pairing / Direct / DERP at the top right), the station body, and a log drawer. The bottom strip
// shows the KEY and PTT state. It shares the `rwk://event` stream with the Client.
export default function StationApp() {
  const [settings, setSettings] = useState(DEFAULT_SETTINGS);
  const [connection, setConnection] = useState({ state: "disconnected", path: "none" });
  const [rtt, setRtt] = useState(null);
  const [keyDown, setKeyDown] = useState(false);
  const [pttOn, setPttOn] = useState(false);
  const [lastFailSafe, setLastFailSafe] = useState(null);
  const [lanPeers, setLanPeers] = useState([]);
  const [ports, setPorts] = useState([]);
  const [errors, setErrors] = useState({}); // control -> short red message
  const [rules, setRules] = useState([]); // port forwards pushed from the Operator (read-only)
  const [flexEnabled, setFlexEnabled] = useState(false); // FlexRadio discovery is being relayed
  const [log, setLog] = useState([]);
  const [logOpen, setLogOpen] = useState(false);
  const [relayPrompt, setRelayPrompt] = useState(false);
  const unlisten = useRef(null);

  useEffect(() => {
    onRwkEvent((ev) => {
      switch (ev.kind) {
        case "settings":
          setSettings({
            keyingPort: ev.keying_port ?? null,
            keyLine: ev.key_line,
            pttLine: ev.ptt_line,
            pttHangMs: ev.ptt_hang_ms ?? DEFAULT_SETTINGS.pttHangMs,
            loggerPort: ev.logger_port ?? null,
            relay: ev.relay ?? "",
            pairingCode: ev.pairing_code ?? "",
            transport: ev.transport ?? "wan",
          });
          break;
        case "ports":
          setPorts(ev.available || []);
          break;
        case "error":
          if (ev.control === "relay") {
            // The relay did not answer: offer the built-in relay or the operator's own.
            if (ev.message) setRelayPrompt(true);
          } else {
            setErrors((e) => ({ ...e, [ev.control]: ev.message || undefined }));
          }
          break;
        case "lanPeers":
          setLanPeers(ev.peers || []);
          break;
        case "connection":
          setConnection({ state: ev.state, path: ev.path });
          // A fresh pairing clears any stale fail-safe banner.
          if (ev.state === "connecting" || ev.state === "disconnected") {
            setLastFailSafe(null);
          }
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
        case "forwardRules":
          setRules(ev.rules || []);
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
      // Beat the startup race: ask for a fresh settings + ports snapshot now that we subscribed.
      stationApi.requestState().catch(() => {});
    });
    return () => {
      if (unlisten.current) unlisten.current();
    };
  }, []);

  // Pair through the relay in settings (the built-in one until the operator changes it).
  function onListen() {
    stationApi.listen(settings.relay || DEFAULT_WAN_RELAY, settings.pairingCode).catch(() => {});
  }

  // The operator picked a relay in the dialog: remember it and pair again through it.
  function onRelayChosen(relay) {
    setRelayPrompt(false);
    setErrors((e) => ({ ...e, relay: undefined }));
    setSettings((s) => ({ ...s, relay }));
    stationApi.listen(relay, settings.pairingCode).catch(() => {});
  }

  const running = connection.state === "connected";
  const connecting = connection.state === "connecting";
  const pathText = running
    ? connection.path === "relay"
      ? "DERP"
      : "Direct"
    : connecting
      ? "Pairing…"
      : "Disconnected";
  const linkColor = running ? "var(--good)" : connecting ? "var(--warn)" : "var(--text-dim)";

  return (
    <div className="app">
      <div className="statusbar" style={{ borderTop: "none", borderBottom: "1px solid var(--border)" }}>
        <strong style={{ color: "var(--accent)" }}>W1VE Remote Keyer-Router — Station</strong>
        <span className="spring" />
        {running && <span>RTT {rtt ?? "—"} ms</span>}
        {running && <span style={{ margin: "0 8px", color: "var(--text-dim)" }}>|</span>}
        <span className="led" style={{ color: linkColor }}>
          ●
        </span>
        <span>{pathText}</span>
      </div>

      <StationTab
        settings={settings}
        setSettings={setSettings}
        connection={connection}
        keyDown={keyDown}
        pttOn={pttOn}
        lastFailSafe={lastFailSafe}
        lanPeers={lanPeers}
        ports={ports}
        errors={errors}
        rules={rules}
        onListen={onListen}
      />

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
          <span style={{ color: "var(--good)" }} title="FlexRadio VITA-49 discovery is being relayed to the Client">
            ● FlexRadio discovery on
          </span>
        )}
        <span className="spring" />
        <span style={{ color: keyDown ? "var(--key-on)" : "var(--text-dim)" }}>Key: {keyDown ? "Down" : "Up"}</span>
        <span style={{ color: pttOn ? "var(--ptt-on)" : "var(--text-dim)" }}>PTT: {pttOn ? "On" : "Off"}</span>
      </div>

      {relayPrompt && (
        <RelayDialog current={settings.relay} onConfirm={onRelayChosen} onCancel={() => setRelayPrompt(false)} />
      )}
    </div>
  );
}
