import { useState } from "react";

// The Connections tab matches the Remote Keyer Router Player: straight-key and paddle wiring,
// with diagrams that can be clicked to zoom for close inspection.
const DIAGRAMS = [
  {
    key: "straight",
    title: "Straight key",
    src: "/straightkey-image.png",
    alt: "Straight key wiring to a serial port",
    text: [
      "Connect the key's two contacts to a 3.5 mm (or 1/4 inch) plug: one contact to the tip and the other to the sleeve (ground).",
      "Plug the key into the keying input on the rig's cable or interface, which runs to the Keyer's serial port.",
      "Choose that serial port under Serial Ports on the Main tab. Key line selects DTR or RTS, whichever the interface uses for keying.",
    ],
  },
  {
    key: "paddle",
    title: "Paddle",
    src: "/paddle-image.png",
    alt: "Paddle wiring to a serial port",
    text: [
      "A paddle has two contacts, one for dit and one for dah, plus a common (ground) return. Wire the dit and dah contacts to the tip and ring of a stereo plug, and the common to the sleeve.",
      "Plug the paddle into the Keyer's paddle-input interface. The serial port carries the key and PTT lines; set Key line and PTT line on the Main tab to match your interface.",
      "If the paddle is reversed (dit and dah swapped), swap the two signal contacts at the plug.",
    ],
  },
];

export default function ConnectionsTab() {
  const [zoomed, setZoomed] = useState({});

  return (
    <div className="body connections-body">
      <div className="panel" style={{ marginBottom: 12 }}>
        <h3>Connections</h3>
        <p style={{ color: "var(--text-dim)", fontSize: 12, margin: 0 }}>
          Wire your key or paddle to the keyer interface, then select the serial port on the Main tab. Click a
          diagram to zoom in; click it again to shrink it.
        </p>
      </div>

      {DIAGRAMS.map((diagram) => (
        <div key={diagram.key} className="panel" style={{ marginBottom: 12 }}>
          <h3>{diagram.title}</h3>
          <div style={{ overflowX: "auto" }}>
            <img
              src={diagram.src}
              alt={diagram.alt}
              title="Click to zoom"
              onClick={() => setZoomed((current) => ({ ...current, [diagram.key]: !current[diagram.key] }))}
              style={{
                display: "block",
                width: zoomed[diagram.key] ? "auto" : "100%",
                maxWidth: zoomed[diagram.key] ? "none" : "100%",
                maxHeight: zoomed[diagram.key] ? "none" : 260,
                objectFit: "contain",
                margin: "0 auto",
                borderRadius: 6,
                cursor: zoomed[diagram.key] ? "zoom-out" : "zoom-in",
              }}
            />
          </div>
          <div style={{ marginTop: 10, fontSize: 13, color: "var(--text)", lineHeight: 1.5 }}>
            {diagram.text.map((text) => (
              <p key={text} style={{ margin: "0 0 6px" }}>
                {text}
              </p>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}
