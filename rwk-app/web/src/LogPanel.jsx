import { useState } from "react";

// The scrolling list of log entries, shared by the Keyer and the Player.
// `fill` lets the list take the height of its tab instead of the 140px drawer height.
export function LogEntries({ log, fill = false }) {
  return (
    <div
      className={fill ? "log-entries" : undefined}
      style={{
        fontFamily: "Consolas, monospace",
        fontSize: 12,
        whiteSpace: "pre-wrap",
        color: "var(--text-dim)",
        maxHeight: fill ? undefined : 140,
        overflow: "auto",
      }}
    >
      {log.length === 0
        ? "(no messages yet)"
        : log.map((l, i) => (
            <div key={i} style={{ color: l.level === "warn" ? "var(--warn)" : undefined }}>
              {l.t.toLocaleTimeString()} [{l.level}] {l.message}
            </div>
          ))}
    </div>
  );
}

// The Player's Log panel. It starts collapsed; the header's ^ glyph opens it and the v glyph
// closes it. Log entries keep accumulating while it is closed, so opening it shows the full history.
export default function LogPanel({ log }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="panel">
      <button
        type="button"
        className="log-toggle"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
        style={{ marginBottom: open ? 10 : 0 }}
      >
        <span>Log</span>
        <span className="log-glyph">{open ? "v" : "^"}</span>
      </button>
      {open && <LogEntries log={log} />}
    </div>
  );
}
