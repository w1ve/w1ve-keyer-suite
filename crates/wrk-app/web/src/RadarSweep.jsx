import { useEffect, useRef } from "react";

// A radar sweep ported from the macroquad "Radar Sweep" demo: a green sweep with a fading
// trail over a grid, and a red ping that the sweep refreshes each time it passes. Geometry is
// expressed in units of the radar radius, so the same drawing works at any `size`.
const TWO_PI = Math.PI * 2;
const SWEEP_SPEED = 1.5; // radians per second
const TRAIL_SEGMENTS = 30;
const TRAIL_STEP = 0.05; // radians between trail segments
const PING_X = 120 / 200; // ping position relative to the radius (macroquad: 120, -90 of 200)
const PING_Y = -90 / 200;
const PING_DECAY = 1.5; // brightness lost per second
const PING_HIT = 0.05; // angular window in which the sweep refreshes the ping

export default function RadarSweep({ size = 56 }) {
  const canvasRef = useRef(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    const ctx = canvas.getContext("2d");
    const dpr = window.devicePixelRatio || 1;
    canvas.width = size * dpr;
    canvas.height = size * dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    const cx = size / 2;
    const cy = size / 2;
    const radius = size / 2 - 2;
    const pingAngle = (Math.atan2(PING_Y, PING_X) + TWO_PI) % TWO_PI;

    let angle = 0;
    let ping = 0;
    let last = performance.now();
    let frameId = 0;

    const frame = (now) => {
      const dt = Math.min((now - last) / 1000, 0.1);
      last = now;
      angle += SWEEP_SPEED * dt;

      // The sweep refreshes the ping when it passes the ping's angle; otherwise it fades.
      const sweep = ((angle % TWO_PI) + TWO_PI) % TWO_PI;
      const diff = Math.abs(sweep - pingAngle);
      if (diff < PING_HIT || TWO_PI - diff < PING_HIT) {
        ping = 1;
      } else {
        ping = Math.max(0, ping - dt * PING_DECAY);
      }

      ctx.clearRect(0, 0, size, size);

      // Black scope face.
      ctx.fillStyle = "#000";
      ctx.beginPath();
      ctx.arc(cx, cy, radius, 0, TWO_PI);
      ctx.fill();

      // Sweep trail: older segments are more transparent.
      ctx.lineWidth = 1.5;
      for (let i = 0; i < TRAIL_SEGMENTS; i++) {
        const a = angle - i * TRAIL_STEP;
        const alpha = (1 - i / TRAIL_SEGMENTS) * 0.4;
        ctx.strokeStyle = `rgba(0, 255, 0, ${alpha})`;
        ctx.beginPath();
        ctx.moveTo(cx, cy);
        ctx.lineTo(cx + Math.cos(a) * radius, cy + Math.sin(a) * radius);
        ctx.stroke();
      }

      // Background grid.
      ctx.strokeStyle = "rgb(0, 100, 0)";
      ctx.lineWidth = 1.5;
      ctx.beginPath();
      ctx.arc(cx, cy, radius, 0, TWO_PI);
      ctx.stroke();
      ctx.lineWidth = 1;
      for (const f of [0.66, 0.33]) {
        ctx.beginPath();
        ctx.arc(cx, cy, radius * f, 0, TWO_PI);
        ctx.stroke();
      }
      ctx.beginPath();
      ctx.moveTo(cx - radius, cy);
      ctx.lineTo(cx + radius, cy);
      ctx.moveTo(cx, cy - radius);
      ctx.lineTo(cx, cy + radius);
      ctx.stroke();

      // The decaying ping, with a fading bloom ring.
      if (ping > 0) {
        const px = cx + PING_X * radius;
        const py = cy + PING_Y * radius;
        ctx.fillStyle = `rgba(255, 0, 0, ${ping})`;
        ctx.beginPath();
        ctx.arc(px, py, Math.max(1.5, radius * 0.08), 0, TWO_PI);
        ctx.fill();
        ctx.strokeStyle = `rgba(255, 77, 77, ${ping * 0.4})`;
        ctx.lineWidth = 1;
        ctx.beginPath();
        ctx.arc(px, py, Math.max(2.5, radius * 0.16), 0, TWO_PI);
        ctx.stroke();
      }

      frameId = requestAnimationFrame(frame);
    };

    frameId = requestAnimationFrame(frame);
    return () => cancelAnimationFrame(frameId);
  }, [size]);

  return (
    <canvas
      ref={canvasRef}
      role="img"
      aria-label="Pairing in progress"
      style={{ width: size, height: size, flex: "none" }}
    />
  );
}
