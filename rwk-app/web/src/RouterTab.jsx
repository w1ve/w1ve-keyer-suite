import { useState } from "react";
import { api } from "./api.js";
import { WIZARD_CATALOG } from "./wizardCatalog.js";

// The Ham Router tab: the forward-rule table, add/remove/enable controls, a port-forward
// wizard, and the FlexRadio discovery re-emission toggle. Rules are managed on the Client and
// pushed to the Station over the control channel, so the Station needs no manual config.
export default function RouterTab({ rules, flexEnabled, connected }) {
  const [wizardOpen, setWizardOpen] = useState(false);
  const [manualOpen, setManualOpen] = useState(false);
  // The rule being edited in the form, or null when adding a new one.
  const [editRule, setEditRule] = useState(null);

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
      <div className="panel">
        <div className="row spread" style={{ marginBottom: 10 }}>
          <h3 style={{ margin: 0 }}>Port Forwarding</h3>
          <div className="row" style={{ gap: 8 }}>
            <button onClick={() => setWizardOpen(true)}>Wizard…</button>
            <button
              onClick={() => {
                setEditRule(null);
                setManualOpen(true);
              }}
            >
              Add Rule…
            </button>
          </div>
        </div>

        {rules.length === 0 ? (
          <p style={{ color: "var(--text-dim)", fontSize: 13 }}>
            No forward rules yet. Use the Wizard for a radio/service preset, or Add Rule for a
            single port. Rules are sent to the Station automatically when you pair.
          </p>
        ) : (
          <table className="rules">
            <thead>
              <tr>
                <th>On</th>
                <th>Name</th>
                <th>Proto</th>
                <th>Client</th>
                <th>→ Station</th>
                <th>Station target</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {rules.map((r) => (
                <tr key={r.id}>
                  <td>
                    <input
                      type="checkbox"
                      checked={r.enabled}
                      onChange={(e) => api.setForwardRuleEnabled(r.id, e.target.checked)}
                    />
                  </td>
                  <td>{r.name}</td>
                  <td>{r.protocol.toUpperCase()}</td>
                  <td>{r.clientPort}</td>
                  <td>{r.stationPort}</td>
                  <td style={{ color: r.stationTargetAddress ? undefined : "var(--warn)" }}>
                    {r.stationTargetAddress || "(not set)"}
                  </td>
                  <td>
                    <button
                      onClick={() => {
                        setEditRule(r);
                        setManualOpen(true);
                      }}
                      style={{ padding: "2px 8px", marginRight: 4 }}
                      title="Edit rule"
                    >
                      Edit
                    </button>
                    <button
                      onClick={() => api.removeForwardRule(r.id)}
                      style={{ padding: "2px 8px", background: "#5a2d2d" }}
                      title="Remove rule"
                    >
                      ✕
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <p style={{ color: "var(--text-dim)", fontSize: 12, marginBottom: 0, marginTop: 8 }}>
          On the Client, each enabled rule listens on <code>127.0.0.1:&lt;client port&gt;</code>;
          point your software there. The Station dials the <em>Station target</em> address.
          {connected ? "" : " Pair with the Station to activate the rules."}
        </p>
      </div>

      {/* ─── FlexRadio discovery ─────────────────────────────────────────── */}
      <div className="panel">
        <h3>FlexRadio Discovery</h3>
        <label className="row" style={{ gap: 8 }}>
          <input
            type="checkbox"
            checked={flexEnabled}
            onChange={(e) => api.setFlexDiscovery(e.target.checked)}
          />
          Enable FlexRadio discovery re-emission
        </label>
        <p style={{ color: "var(--text-dim)", fontSize: 12, marginBottom: 0, marginTop: 8 }}>
          Relays SmartSDR discovery from the Station LAN so your local SmartSDR finds the
          remote radio. Enabling this creates the Flex forward rules (TCP 4992, UDP 4991)
          automatically; the radio's real address is learned from the first announcement.
        </p>
      </div>

      {wizardOpen && <Wizard onClose={() => setWizardOpen(false)} />}
      {manualOpen && (
        <ManualRule
          rule={editRule}
          onClose={() => {
            setManualOpen(false);
            setEditRule(null);
          }}
        />
      )}
    </div>
  );
}

// The preset wizard: pick a radio/service, enter the Station target address, and it creates
// all the preset's forward rules at once.
function Wizard({ onClose }) {
  const [id, setId] = useState(WIZARD_CATALOG[0].id);
  const [target, setTarget] = useState("127.0.0.1");
  const preset = WIZARD_CATALOG.find((p) => p.id === id);

  // Group presets by vendor for the dropdown.
  const vendors = [...new Set(WIZARD_CATALOG.map((p) => p.vendor))];

  function create() {
    for (const f of preset.forwards) {
      api.addForwardRule(f.label, f.proto, f.port, f.port, target);
    }
    onClose();
  }

  return (
    <Modal title="Port-Forward Wizard" onClose={onClose} width={520}>
      <div className="field">
        <label>Radio / service</label>
        <select value={id} onChange={(e) => setId(e.target.value)}>
          {vendors.map((v) => (
            <optgroup key={v} label={v}>
              {WIZARD_CATALOG.filter((p) => p.vendor === v).map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </optgroup>
          ))}
        </select>
      </div>

      <div className="field">
        <label>{preset.targetLabel}</label>
        <input type="text" value={target} onChange={(e) => setTarget(e.target.value)} />
      </div>

      <div style={{ marginBottom: 10 }}>
        <label style={{ color: "var(--text-dim)", fontSize: 12 }}>Rules this creates:</label>
        <ul style={{ margin: "6px 0", fontSize: 13 }}>
          {preset.forwards.map((f) => (
            <li key={`${f.proto}-${f.port}`}>
              {f.label} — {f.proto.toUpperCase()} {f.port}
            </li>
          ))}
        </ul>
      </div>

      <div className="row" style={{ justifyContent: "flex-end", gap: 8 }}>
        <button onClick={onClose}>Cancel</button>
        <button
          onClick={create}
          disabled={!target.trim()}
          style={{ background: "var(--accent-dim)", borderColor: "var(--accent)" }}
        >
          Create {preset.forwards.length} rule{preset.forwards.length > 1 ? "s" : ""}
        </button>
      </div>
    </Modal>
  );
}

// A single hand-entered forward rule. With `rule` it edits that rule in place; the edit is pushed
// to the Station live, whether or not the link is up.
function ManualRule({ rule, onClose }) {
  const editing = Boolean(rule);
  const [name, setName] = useState(rule?.name ?? "");
  const [proto, setProto] = useState(rule?.protocol ?? "tcp");
  const [clientPort, setClientPort] = useState(rule?.clientPort ?? 8000);
  const [stationPort, setStationPort] = useState(rule?.stationPort ?? 8000);
  const [target, setTarget] = useState(rule?.stationTargetAddress ?? "127.0.0.1");

  function save() {
    const finalName = name || `${proto.toUpperCase()}-${clientPort}`;
    if (editing) {
      api.updateForwardRule(
        rule.id,
        finalName,
        proto,
        Number(clientPort),
        Number(stationPort),
        target,
      );
    } else {
      api.addForwardRule(finalName, proto, Number(clientPort), Number(stationPort), target);
    }
    onClose();
  }

  return (
    <Modal title={editing ? "Edit Forward Rule" : "Add Forward Rule"} onClose={onClose} width={460}>
      <div className="field">
        <label>Name</label>
        <input type="text" value={name} onChange={(e) => setName(e.target.value)} placeholder="optional" />
      </div>
      <div className="row" style={{ gap: 10 }}>
        <div className="field" style={{ flex: 1 }}>
          <label>Protocol</label>
          <select value={proto} onChange={(e) => setProto(e.target.value)}>
            <option value="tcp">TCP</option>
            <option value="udp">UDP</option>
          </select>
        </div>
        <div className="field" style={{ flex: 1 }}>
          <label>Client port</label>
          <input
            type="number"
            value={clientPort}
            onChange={(e) => setClientPort(e.target.value)}
          />
        </div>
        <div className="field" style={{ flex: 1 }}>
          <label>Station port</label>
          <input
            type="number"
            value={stationPort}
            onChange={(e) => setStationPort(e.target.value)}
          />
        </div>
      </div>
      <div className="field">
        <label>Station target address</label>
        <input type="text" value={target} onChange={(e) => setTarget(e.target.value)} />
      </div>
      <div className="row" style={{ justifyContent: "flex-end", gap: 8 }}>
        <button onClick={onClose}>Cancel</button>
        <button
          onClick={save}
          disabled={!target.trim()}
          style={{ background: "var(--accent-dim)", borderColor: "var(--accent)" }}
        >
          {editing ? "Save" : "Add"}
        </button>
      </div>
    </Modal>
  );
}

function Modal({ title, children, onClose, width = 480 }) {
  return (
    <div
      style={{
        position: "fixed",
        inset: 0,
        background: "rgba(0,0,0,0.5)",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        zIndex: 10,
      }}
      onClick={onClose}
    >
      <div className="panel" style={{ width }} onClick={(e) => e.stopPropagation()}>
        <h3>{title}</h3>
        {children}
      </div>
    </div>
  );
}
