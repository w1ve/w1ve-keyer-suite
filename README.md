<h1 align="center">W1VE Keyer Suite</h1>

<p align="center">
  <strong>Robust K1EL WinKeyer emulation in pure software — local or networked, on any platform.</strong>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
  <img src="https://img.shields.io/badge/language-Rust-dea584.svg" alt="Rust">
  <img src="https://img.shields.io/badge/UI-Tauri%20v2-24c8db.svg" alt="Tauri v2">
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey.svg" alt="Platforms">
</p>

---

## What this is

The W1VE Keyer Suite is a family of amateur-radio CW keyer applications that do in **pure
software** what a hardware [K1EL WinKeyer](https://www.hamcrafters2.com/) does in silicon:
turn a paddle, a keyboard, or a logging program's macros into precisely timed Morse code on a
radio's key line — with no dependency beyond the availability of a serial port.

The suite spans the full range of how an operator keys a radio:

- **Locally**, with the paddle, the rig, and the logger all on one desk.
- **Across a network**, with the operator in one place and the radio in another, keyed over an
  encrypted link with the timing preserved to within a couple of milliseconds.

All of it is built on **one shared Rust engine** (`rwk-core`): the same keyer timing, serial
keying, sidetone, paddle decoder, and WinKeyer2 protocol host power every app. Each
application is a thin controller plus a [Tauri v2](https://tauri.app/) desktop UI over that
engine, so they look and behave consistently and a fix to the engine benefits all of them.

## Design goals

1. **Pure software WinKeyer.** Faithful K1EL WinKeyer2 host emulation, so logging programs
   (N1MM+, DXLog, and friends) drive it exactly as they drive a hardware chip.
2. **No dependencies but a serial port.** No cloud account, no coordination server, no sidecar
   process, no runtime to install. One self-contained executable per app.
3. **Timing you can trust.** Element timing runs off the UI thread on a time-critical,
   high-resolution clock, so CW stays clean under load — locally and across a jittery link.
4. **One engine, many front-ends.** Local, remote, and router variants share `rwk-core`
   rather than forking it.
5. **Cross-platform.** Windows today; macOS and Linux share the same engine and UI, with
   platform specifics (thread priority, device detection) isolated to one seam.

## The applications

| App | Binaries | What it does |
|-----|----------|--------------|
| **W1VE Keyer** | `w1ve-keyer` | A standalone software WinKeyer. Paddle + logger (WinKeyer2) in, serial key-line out, local sidetone. No networking — a direct replacement for a hardware WinKeyer on the desk. |
| **W1VE Remote Keyer** | `w1ve-remote-keyer`, `w1ve-remote-keyer-station` | A focused Client/Station pair that keys a radio at a remote site over an encrypted link. The operating position sends hand-sent CW; the radio-site station replays it on the key line with fail-safe protection. |
| **W1VE Remote Keyer-Router** | `w1ve-rkr`, `w1ve-rkr-station` | The Remote Keyer plus a "Ham Router": TCP/UDP port forwarding, SSB PTT control, and FlexRadio discovery relay — CAT control, audio, and rig discovery tunneled alongside the CW. |
| **Relay** (server) | `rwk-relay` | An optional, key-addressed packet relay for NAT traversal when the two ends of a networked link have no direct path. A Linux server/container, not a desktop app. |

Both networked apps offer a **WAN** mode (rendezvous over a relay with a shared pairing code)
and a relay-free **LAN** mode (the station browses keyers advertising themselves on the local
network and the operator picks one). The transport is a native WireGuard tunnel
([boringtun](https://github.com/cloudflare/boringtun)) with a userspace IP stack — no kernel
driver, no admin rights, no third-party mesh.

## Heritage

This suite is the current chapter of a longer line of work:

1. **RWK Router/Keyer (C#).** The original Windows application — `.NET 9` for the UI and
   keyer, with a Go [Tailscale](https://tailscale.com/) `tsnet` sidecar for the encrypted mesh
   transport. It established the core ideas the suite still rests on: separating the *timing
   decision* from the *physical keying*, a fail-safe-protected edge replayer at the radio site,
   WinKeyer2 host emulation for loggers, and port forwarding for CAT/audio.

2. **The Rust port (`rwk-core`).** A ground-up rewrite of that engine into native Rust,
   collapsing the .NET app *and* its Go/Tailscale sidecar into **one self-contained executable
   with no child process**. The WireGuard transport became native (boringtun + a `smoltcp`
   userspace netstack + a minimal key-addressed relay) in place of Tailscale, and the UI moved
   to Tauri v2. The byte-exact wire formats, the keyer timing maths, and the fail-safe
   F-numbering were carried over faithfully from the C# original.

3. **The suite.** From that shared engine, the keying path was factored into the three
   applications above — local, remote, and remote-plus-router — so each operator picks the one
   that matches their station without carrying features they do not need.

The `rwk` crate prefix throughout the engine is a nod to that **R**emote **W**in**K**ey
lineage.

## Repository layout

> **Note on structure.** The suite is being consolidated into a single workspace. During the
> transition the applications live in sibling repositories that share the engine by path; the
> shared engine is `rwk-core`.

```
w1ve-keyer-suite/
  crates/
    rwk-core/        the shared engine: keyer timing, serial keying, sidetone,
                     paddle decode, WinKeyer2 host, WireGuard tunnel, relay, netstack
    wk-app/          W1VE Keyer            (standalone, no networking)
    wrk-app/         W1VE Remote Keyer     (Client + Station)
    rwk-app/         W1VE Remote Keyer-Router (Client + Station + Ham Router)
    rwk-relay/       the relay server + dashboard
    rwk/             an engine CLI (keygen, connect, selftest, …)
```

## Building

Each app is a Rust workspace member with a Tauri v2 front-end. In broad strokes:

```sh
# Build an app's web bundle, then the release binary:
( cd crates/<app>/web && npm install && npm run build )
cargo build --release --features custom-protocol -p <app>
```

On a stock toolchain (`cargo` + Node + a platform C toolchain) a plain `cargo build` works.
Full platform build notes — including the macOS build/sign/notarize path — live with the
engine project.

## License

[MIT](LICENSE) © 2026 Gerry Hull, W1VE
