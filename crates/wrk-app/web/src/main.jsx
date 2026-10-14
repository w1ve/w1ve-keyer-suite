import React, { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import { detectRole } from "./api.js";
import ClientApp from "./ClientApp.jsx";
import ServerApp from "./ServerApp.jsx";
import "./styles.css";

// One web bundle serves both executables. The Server binary exposes `server_app_role`; the
// Client does not. We probe it once and mount the matching UI.
function Root() {
  const [role, setRole] = useState(null);

  useEffect(() => {
    let cancelled = false;
    detectRole().then((r) => {
      if (!cancelled) setRole(r);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  if (role === null) {
    return (
      <div
        style={{
          height: "100%",
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          color: "var(--text-dim)",
          fontFamily: "Segoe UI, system-ui, sans-serif",
        }}
      >
        Starting…
      </div>
    );
  }

  return role === "server" ? <ServerApp /> : <ClientApp />;
}

ReactDOM.createRoot(document.getElementById("root")).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>,
);
