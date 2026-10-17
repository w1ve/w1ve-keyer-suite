// The WAN/LAN segmented slider shared by both roles. WAN = the W1VE relay over the internet;
// LAN = a relay on the local network (zero internet dependency once paired).
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
