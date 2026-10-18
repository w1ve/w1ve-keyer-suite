# W1VE Keyer — design plan (not yet implemented)

A **local, no-networking** CW keyer for Windows. It is the Server and the Client of W1VE
Remote Keyer fused into one app, with the tunnel removed: paddle/key in → keyer timing →
sidetone + serial key line out, all on one machine. Think "a WinKeyer in software" — the
same engine that powers the Remote Keyer and the Keyer-Router, minus everything to do with the
network.

> Status: **plan only.** No code written. This document is the brief for building it next.

## 1. What it is

One Tauri window, one process. The operator plugs a paddle (or straight key) into a serial
port, picks a keying-output port for the rig, and keys CW with:

- a **paddle** (iambic A/B, ultimatic, bug, straight), read from serial control lines;
- **type-ahead / macros** (text → CW);
- an optional **WinKeyer protocol host** so a logging program on the *same* PC can drive the
  keyer over a (virtual) COM port, exactly as the Keyer-Router Station's logger input does.

Output is the local sidetone and a serial DTR/RTS key line to the rig. There is no Server, no
Client, no pairing, no relay, no tunnel — the keyer core runs in-process and the edges go
straight to the hardware.

## 2. Why it is trivial to build on what exists

Everything the keyer needs is already in `rwk-core` and already proven by the two shipping
apps. W1VE Keyer is a *subtraction*, not new engine work:

| Need | Reuse (unchanged) |
|---|---|
| Element timing, text→edges, iambic decider | `engine::keying` (`KeyerElementTiming`, `EdgeScheduleBuilder`, `PaddleElementEngine`) |
| Live paddle keyer with Iambic B squeeze memory | `engine::paddle::PaddleKeyer` |
| Serial paddle input (CTS/DSR/DCD, DTR source) | `engine::paddle_input::PaddleInputPort` |
| Contact debounce | `engine::debounce::ContactDebouncer` |
| Serial key/PTT output (DTR/RTS, inversion, PTT lead/tail) | `engine::serial` |
| Sidetone (cpal, raised-cosine envelope) | `engine::audio::SidetoneEngine` |
| WinKeyer host (logger drives the keyer) | `protocol::winkeyer_host` + the Station's `StationWinKeyerHost` pattern |
| Two-source arbitration (paddle vs logger) | `engine::keying_arbiter::KeyingArbiter` |
| Thread priority + 1 ms timer | `platform` + `timing::HybridWaiter` |

**Removed:** `engine::{tunnel, relay, netstack, netstack_driver, session, control,
flex_discovery, edge_sender, edge_station}` and `protocol::{edge, flex_vita}` play no part. No
`Role`, no `Config.peer/relay/pairing_code`, no tunnel transport.

Net new code is small: one controller that wires paddle + type-ahead + (optional) WinKeyer
host through the arbiter to the local sidetone and serial output, plus a single-view UI.

## 3. Architecture

```
W1VE-Keyer/                         (new sibling project, like W1VE-Remote-Keyer)
  Cargo.toml                        workspace; path dep on ../RWK-Rust/rust/crates/rwk-core
  crates/wk-app/
    src/keyer_controller.rs         THE app: owns the keyer, arbiter, sidetone, serial I/O
    src/app_paths.rs                %APPDATA%\W1VE Keyer\settings.json (no identity needed)
    src/lib.rs                      ONE Tauri binary + commands
    src/main.rs
    web/                            one-view React UI (reuse styles.css look-and-feel)
```

Only **one** executable (`wk.exe`) — there is no client/server split to select at startup, so
no role probe. The window title is simply "W1VE Keyer".

### Signal flow (all in-process, no network)

```
          ┌───────────── paddle poller thread (1 ms, time-critical) ──────────┐
paddle →  │ PaddleInputPort → ContactDebouncer → PaddleKeyer → edges          │
          └───────────────────────────────────────────────────────────────────┘
                                                   │
type-ahead / macros → EdgeScheduleBuilder → edges ─┤
                                                   │        ┌─ sidetone (cpal thread)
WinKeyer host (logger COM) → winkeyer_host ────────┤──▶ arbiter ──▶ serial KEY/PTT (DTR/RTS)
   → PaddleKeyer/EdgeSchedule for its text         │        └─ KEY/PTT indicator events → UI
```

The **arbiter** is the same `KeyingArbiter` the Station uses: two logical sources (here "paddle
/ type-ahead" vs "logger"), local-priority interlock, single physical serial writer, fault
drain for the F6 fail-safe equivalent. For a purely local keyer the fail-safe/replay machinery
is not needed (there is no jittered network to protect against), so edges drive the output
directly — no `EdgeReplayer`, no jitter buffer.

### Threads

- **Paddle poller** — `ThreadPriorityGuard::raise_time_critical()` + 1 ms timer, identical to
  the Remote Keyer Client's `paddle_loop`, but the per-edge sink drives sidetone + serial only
  (no tunnel transport).
- **Sidetone** — the cpal-owning thread (cpal is `!Send`), driven by a channel, as today.
- **Type-ahead / macro** — a transient thread per message running `EdgeSchedule::play`.
- **WinKeyer host** — the serial-reader thread from `StationWinKeyerHost`, keying through the
  arbiter's local source.

## 4. UI (single view)

Reuse the shared `styles.css`. One panel layout, no tabs needed (or a slim Keyer | Log split):

- **Keyer**: WPM, weight, mode, paddle-reverse; eight CW macros + type-ahead box + Test TX
  (lifted from the Keyer-Router's `KeyerTab`).
- **Sidetone**: frequency, volume (+ device picker once device selection lands).
- **Ports**: paddle input port, keying output port + key line (DTR/RTS) + PTT line, and
  "Enable logger input" + its COM port (excluding the keying port) — the Station's logger UI.
- **Status strip**: KEY and PTT indicators.

Persist all settings to `settings.json` (no WireGuard identity — there is no tunnel). The
settings struct is the keyer/sidetone/port fields only.

## 5. Build & packaging

Same toolchain and pattern as the other two apps: `cargo build --release -p wk-app
--features custom-protocol` → one `wk.exe` that serves its bundled `web/dist`. No relay, no
second binary, no sidecar.

## 6. Scope boundaries / explicit non-goals

- **No networking of any kind.** If the operator wants remote, that is the Remote Keyer app.
- **No fail-safe/replay engine.** Those exist to protect a *remote* key over a jittery link;
  a local key needs none.
- **Reuse, don't fork.** `rwk-core` is a path dependency, as in W1VE-Remote-Keyer; any fix to
  the keyer/serial/sidetone/winkeyer code benefits all three apps.

## 7. Estimated effort

Small. The controller is a trimmed `client_controller.rs` (drop `connect`/`disconnect`,
`transport`, `session`, `pairing`; add the WinKeyer host + arbiter wiring from the Station).
The UI is `KeyerTab` + the Station's logger panel in one view. One binary, one window.
A day or so once prioritised — the risk is low because every moving part already runs in
production in the two sibling apps.
