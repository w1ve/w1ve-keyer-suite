import React, { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import App from "./App.jsx";
import StationApp from "./StationApp.jsx";
import "./styles.css";

// The splash popup loads this same entry point and shows the artwork alone, so it uses the asset
// pipeline that is known to work (a standalone splash.html rendered as an empty window). The
// window is identified by its Tauri label — "splash" — rather than by a query string, because the
// label is guaranteed by the window builder that creates it.
const SPLASH_ONLY =
  window.__TAURI_INTERNALS__?.metadata?.currentWindow?.label === "splash" ||
  new URLSearchParams(window.location.search).has("splash");

function SplashArtwork() {
  return (
    <div style={{ position: "fixed", inset: 0, background: "#0d1721" }}>
      <img
        src="/hero.png"
        alt=""
        style={{ display: "block", width: "100vw", height: "100vh" }}
      />
    </div>
  );
}

// One web bundle serves both executables. The Station binary exposes a `station_app_role`
// command; the Client binary does not. We probe it once at startup: if it resolves we are the
// Station and mount StationApp, otherwise we are the Client and mount App. This avoids a
// build-time split and keeps a single Vite bundle for both apps.
function Root() {
  const [role, setRole] = useState(null); // null = probing, "client" | "station" otherwise

  useEffect(() => {
    let cancelled = false;
    invoke("station_app_role")
      .then((r) => {
        if (!cancelled) setRole(r === "station" ? "station" : "client");
      })
      .catch(() => {
        // The command is absent in the Client binary: that is how we know we are the Client.
        if (!cancelled) setRole("client");
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // The window is created hidden (see `ui_ready` in src/lib.rs), so nothing needs to be shown here
  // while the role probe runs. Whichever UI mounts reveals the window once it has painted; this
  // timer is the safety net that still reveals it if that call never comes.
  useEffect(() => {
    const t = window.setTimeout(() => invoke("ui_ready").catch(() => {}), 3000);
    return () => window.clearTimeout(t);
  }, []);

  if (role === null) {
    return <div style={{ height: "100%" }} />;
  }

  return role === "station" ? <StationApp /> : <App />;
}

ReactDOM.createRoot(document.getElementById("root")).render(
  <React.StrictMode>{SPLASH_ONLY ? <SplashArtwork /> : <Root />}</React.StrictMode>,
);
