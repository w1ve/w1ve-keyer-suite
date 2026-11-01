// The WAN/LAN segmented slider shared by both roles. WAN uses the W1VE relay over the internet;
// LAN discovers a direct Player on the local network and needs no relay or internet.
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
