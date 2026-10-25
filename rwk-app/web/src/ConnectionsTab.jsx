import { useState } from "react";

// The Connections tab: how to wire a straight key or a paddle to the Station's serial port.
// Each diagram is shown at a comfortable width; click it to view it at full size (click again to shrink).
const DIAGRAMS = [
  {
    key: "straight",
    title: "Straight key",
    src: "/straightkey-image.png",
    alt: "Straight key wiring to a serial port",
    text: [
      "Connect the key's two contacts to a 3.5 mm (or 1/4 inch) plug: one contact to the tip and the other to the sleeve (ground).",
      "Plug the key into the keying input on the rig's cable or interface, which runs to the Station's serial port.",
      "Choose that serial port under Keying Output on the Main tab. Key line selects DTR or RTS, whichever the interface uses for keying.",
    ],
  },
  {
    key: "paddle",
    title: "Paddle",
    src: "/paddle-image.png",
    alt: "Paddle wiring to a serial port",
    text: [
      "A paddle has two contacts, one for dit and one for dah, plus a common (ground) return. Wire the dit and dah contacts to the tip and ring of a stereo plug, and the common to the sleeve.",
      "Plug the paddle into the keyer interface. The serial port carries the key and PTT lines; set Key line and PTT line on the Main tab to match your interface.",
      "If the paddle is reversed (dit and dah swapped), swap the two signal contacts at the plug.",
    ],
  },
];

export default function ConnectionsTab() {
  const [zoomed, setZoomed] = useState({});

  return (
    <div className="tabbody">
      <div className="panel" style={{ marginBottom: 12 }}>
        <h3>Connections</h3>
        <p style={{ color: "var(--text-dim)", fontSize: 12, margin: 0 }}>
          Wire your key or paddle to the keyer interface, then select the serial port on the Main tab. Click a
          diagram to zoom in; click it again to shrink it.
        </p>
      </div>

      {DIAGRAMS.map((d) => (
        <div key={d.key} className="panel" style={{ marginBottom: 12 }}>
          <h3>{d.title}</h3>
          <div style={{ overflowX: "auto" }}>
            <img
              src={d.src}
              alt={d.alt}
              title="Click to zoom"
              onClick={() => setZoomed((z) => ({ ...z, [d.key]: !z[d.key] }))}
              style={{
                display: "block",
                width: zoomed[d.key] ? "auto" : "100%",
                maxWidth: zoomed[d.key] ? "none" : "100%",
                maxHeight: zoomed[d.key] ? "none" : 260,
                objectFit: "contain",
                margin: "0 auto",
                borderRadius: 6,
                cursor: zoomed[d.key] ? "zoom-out" : "zoom-in",
              }}
            />
          </div>
          <div style={{ marginTop: 10, fontSize: 13, color: "var(--text)", lineHeight: 1.5 }}>
            {d.text.map((t) => (
              <p key={t} style={{ margin: "0 0 6px" }}>
                {t}
              </p>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}
