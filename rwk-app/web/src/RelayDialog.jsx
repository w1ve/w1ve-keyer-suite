import { useState } from "react";

// The built-in relay both apps pair through by default.
export const DEFAULT_WAN_RELAY = "107.172.138.103:9000";

// Shown when the built-in relay (or the operator's own) did not answer. The operator picks the
// built-in relay or the address they last used, then pairs again through it.
export default function RelayDialog({ current, onConfirm, onCancel }) {
  const options = [DEFAULT_WAN_RELAY];
  if (current && current !== DEFAULT_WAN_RELAY) options.push(current);
  const [choice, setChoice] = useState(options[0]);

  return (
    <div
      style={{
        position: "fixed",
        inset: 0,
        background: "rgba(0,0,0,0.5)",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
      }}
    >
      <div className="panel" style={{ width: 420 }}>
        <h3>Relay not found</h3>
        <p style={{ color: "var(--text-dim)", fontSize: 13, marginTop: 0 }}>
          The relay did not answer. Choose a relay to pair through.
        </p>
        <div className="field">
          <label>Relay (host:port)</label>
          <select value={choice} onChange={(e) => setChoice(e.target.value)}>
            {options.map((o) => (
              <option key={o} value={o}>
                {o === DEFAULT_WAN_RELAY ? `${o} (built-in)` : `${o} (yours)`}
              </option>
            ))}
          </select>
        </div>
        <div className="row" style={{ justifyContent: "flex-end", gap: 8, marginTop: 12 }}>
          <button onClick={onCancel}>Cancel</button>
          <button
            onClick={() => onConfirm(choice)}
            style={{ background: "var(--accent-dim)", borderColor: "var(--accent)" }}
          >
            Pair via relay
          </button>
        </div>
      </div>
    </div>
  );
}
