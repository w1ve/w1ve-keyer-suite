import React, { useEffect } from "react";
import ReactDOM from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import App from "./App.jsx";
import { APP_NAME, APP_VERSION } from "./app-info.js";
import "./styles.css";
// Vite-imported splash artwork. A root-relative "/splash-hero.png" from the public dir does not
// load in the separate splash WebviewWindow on Windows, leaving the splash a blank dark box; the
// bundled asset URL resolves in every window.
import heroArt from "./assets/splash-hero.png";

// The splash popup loads this same entry point and shows the artwork alone. The window is
// identified by its Tauri label — "splash" — set by the window builder in src/lib.rs (a
// `?splash` query is accepted as a browser-preview fallback).
const SPLASH_ONLY =
  window.__TAURI_INTERNALS__?.metadata?.currentWindow?.label === "splash" ||
  new URLSearchParams(window.location.search).has("splash");

function SplashArtwork() {
  return (
    <div style={{ position: "fixed", inset: 0, background: "#0d1721" }}>
      <img
        src={heroArt}
        alt=""
        style={{ display: "block", width: "100vw", height: "100vh" }}
      />
      <div
        style={{
          position: "absolute",
          left: 8,
          top: 7,
          padding: "3px 6px",
          borderRadius: 4,
          background: "rgba(5, 12, 19, 0.82)",
          color: "#fff",
          font: "600 10px Segoe UI, sans-serif",
          lineHeight: 1.35,
        }}
      >
        <div>{APP_NAME}</div>
        <div style={{ color: "#9ecfff", fontSize: 9 }}>Version {APP_VERSION}</div>
      </div>
    </div>
  );
}

// The main window is created hidden (see `ui_ready` in src/lib.rs). App.jsx reveals it and runs
// the splash once its own UI has loaded; this timer is the safety net that still reveals it if
// that call never comes.
function Root() {
  useEffect(() => {
    const t = window.setTimeout(() => invoke("ui_ready").catch(() => {}), 3000);
    return () => window.clearTimeout(t);
  }, []);
  return <App />;
}

ReactDOM.createRoot(document.getElementById("root")).render(
  <React.StrictMode>{SPLASH_ONLY ? <SplashArtwork /> : <Root />}</React.StrictMode>,
);
