// Transport selector shared by both roles. WAN uses the W1VE relay. On LAN, the desktop Player
// advertises itself while the operating Keyer browses desktop and W1VE ESP32 Players locally.
export default function TransportSlider({ value, onChange, disabled }) {
  return (
    <div className="segmented" style={disabled ? { opacity: 0.5, pointerEvents: "none" } : null}>
      <button className={value === "wan" ? "active" : ""} onClick={() => onChange("wan")}>
        WAN
      </button>
      <button className={value === "lan" ? "active" : ""} onClick={() => onChange("lan")}>
        LAN
      </button>
    </div>
  );
}
