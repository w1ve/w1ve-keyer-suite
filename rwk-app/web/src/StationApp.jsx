import { useEffect, useRef, useState } from "react";
import { onRwkEvent, stationApi } from "./station-api.js";
import StationTab from "./StationTab.jsx";
import RelayDialog, { DEFAULT_WAN_RELAY } from "./RelayDialog.jsx";
import { LogEntries } from "./LogPanel.jsx";
import ConnectionsTab from "./ConnectionsTab.jsx";
import AboutDialog from "./AboutDialog.jsx";
import { listen } from "@tauri-apps/api/event";

// The splash is a separate BORDERLESS POPUP window containing only the artwork — not an overlay
// inside this window, and not a full-window black page. It is created natively (see
// `station_show_splash` in src/lib.rs), sized to the artwork, centred over the main window and
// hidden again after SPLASH_MS. This component only decides when, from the UI's own load state:
//
//   1. the document has finished loading (document.readyState === "complete"),
//   2. the webfonts are ready,
//   3. the first settings snapshot is applied (token, keying config, transport), and
//   4. the serial-port inventory has arrived, so the port pickers are populated.
//
// The popup is shown the moment all four hold — i.e. when the main UI itself has rendered — and
// there is no startup timer or safety net to race that: if the backend never answers, the popup
// never appears rather than playing over a half-built window.
const SPLASH_MS = 5000;

/// True once the main UI is fully loaded: document ready, fonts settled, and the first settings
/// and port snapshots applied. The splash waits on this rather than on elapsed time.
function useUiLoaded(settingsLoaded, portsLoaded) {
  const [documentLoaded, setDocumentLoaded] = useState(
    () => typeof document !== "undefined" && document.readyState === "complete",
  );
  const [fontsLoaded, setFontsLoaded] = useState(false);

  useEffect(() => {
    if (documentLoaded) return undefined;
    const onLoad = () => setDocumentLoaded(true);
    window.addEventListener("load", onLoad, { once: true });
    // The load event can fire between the initial state and this effect: re-check once.
    if (document.readyState === "complete") setDocumentLoaded(true);
    return () => window.removeEventListener("load", onLoad);
  }, [documentLoaded]);

  useEffect(() => {
    let cancelled = false;
    const done = () => {
      if (!cancelled) setFontsLoaded(true);
    };
    // `fonts.ready` resolves once layout-affecting fonts are loaded, and rejects never: the
    // rejection handler is the same one-shot settle so a font failure cannot block the splash.
    if (document.fonts?.ready) document.fonts.ready.then(done, done);
    else done();
    return () => {
      cancelled = true;
    };
  }, []);

  return documentLoaded && fontsLoaded && settingsLoaded && portsLoaded;
}

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

// The Station application shell: a compact window with a Main tab (keying, forwards, pairing) and
// a Log tab. The header shows the link state and the bottom strip shows KEY and PTT on both tabs.
// It shares the `rwk://event` stream with the Client.
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
  const [tab, setTab] = useState("main"); // "main" | "log"
  const [relayPrompt, setRelayPrompt] = useState(false);
  const unlisten = useRef(null);
  const [aboutOpen, setAboutOpen] = useState(false);
  // The two snapshots that make the main UI complete: settings (token, keying config, transport)
  // and the serial-port inventory behind the port pickers.
  const [settingsLoaded, setSettingsLoaded] = useState(false);
  const [portsLoaded, setPortsLoaded] = useState(false);
  const uiLoaded = useUiLoaded(settingsLoaded, portsLoaded);

  // Splash popup: the window is revealed and the popup shown the instant the main UI has finished
  // rendering, then the popup is hidden five seconds later.
  useEffect(() => {
    if (!uiLoaded) return undefined;
    let timer;
    let cancelled = false;
    stationApi
      .uiReady()
      .catch(() => {})
      .then(() => stationApi.showSplash())
      .then(() => {
        // The timer starts once the popup is actually up, not when the UI finished rendering:
        // anchoring it to `uiLoaded` would spend part of the five seconds on the IPC round trip
        // and leave the splash visible for slightly less than five seconds.
        if (cancelled) return;
        timer = window.setTimeout(() => {
          stationApi.hideSplash().catch(() => {});
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

  useEffect(() => {
    onRwkEvent((ev) => {
      switch (ev.kind) {
        case "settings":
          setSettingsLoaded(true);
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
          setPortsLoaded(true);
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
    <div className="app station">
      <div className="statusbar">
        <strong style={{ color: "var(--accent)" }}>RWK Station</strong>
        <span className="spring" />
        {running && <span>RTT {rtt ?? "—"} ms</span>}
        <span className="led" style={{ color: linkColor }}>
          ●
        </span>
        <span>{pathText}</span>
      </div>

      <div className="tabbar">
        <button type="button" className={`tab${tab === "main" ? " active" : ""}`} onClick={() => setTab("main")}>
          Main
        </button>
        <button type="button" className={`tab${tab === "connections" ? " active" : ""}`} onClick={() => setTab("connections")}>
          Connections
        </button>
        <button type="button" className={`tab${tab === "log" ? " active" : ""}`} onClick={() => setTab("log")}>
          Log
        </button>
      </div>

      {tab === "connections" ? (
        <ConnectionsTab />
      ) : tab === "main" ? (
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
      ) : (
        <div className="tabbody">
          <div className="panel">
            <LogEntries log={log} fill />
          </div>
        </div>
      )}

      <div className="statusbar">
        {flexEnabled && (
          <span style={{ color: "var(--good)" }} title="FlexRadio VITA-49 discovery is being relayed to the Client">
            ● FlexRadio
          </span>
        )}
        <span className="spring" />
        <span style={{ color: keyDown ? "var(--key-on)" : "var(--text-dim)" }}>Key: {keyDown ? "Down" : "Up"}</span>
        <span style={{ color: pttOn ? "var(--ptt-on)" : "var(--text-dim)" }}>PTT: {pttOn ? "On" : "Off"}</span>
      </div>

      {aboutOpen && <AboutDialog onClose={() => setAboutOpen(false)} />}

      {relayPrompt && (
        <RelayDialog current={settings.relay} onConfirm={onRelayChosen} onCancel={() => setRelayPrompt(false)} />
      )}
    </div>
  );
}
