# W1VE Remote Keyer

A focused **Player + Operator CW keyer pair** for remote Morse operation over the internet (WAN)
or a local network (LAN). Native Rust + Tauri v2, same look-and-feel as **W1VE Remote
Keyer Router**, and built on the *same* engine — this is the Keyer Router's keying path on its
own, with no port forwarding or FlexRadio router.

* **Client** (`w1ve-remote-keyer`) — the operating position: paddle/keyer, sidetone, serial
  key line, type-ahead CW. Sends keyed edges to the Player.
* **Player** (`w1ve-remote-keyer-player`) — the remote radio site: receives the keyed edges
  and keys the radio's control line, with the full replay jitter buffer and fail-safes.

## Transport: WAN / LAN

A segmented slider on each app chooses how the two ends meet:

* **WAN** — the public W1VE relay on the internet. Zero network setup: no port forwarding, no
  static IP. The WireGuard tunnel goes peer-to-peer when NATs allow, else over the relay.
* **LAN** — the desktop radio-site Player advertises its persisted callsign and optional Player
  ID, such as `W1VE-20m`, in an RWKP v2 beacon with CW capability. It waits as the WireGuard
  responder. The operating-position Client browses available desktop Players, validates the
  selected identity and endpoint, and initiates the tunnel. A **W1VE ESP32 Player** advertises
  separately; the Client lists it alongside desktop Players and runs one bounded, paced unicast
  discovery pass when browsing starts for Wi-Fi networks that suppress broadcasts.

WAN pairing uses a short pairing token. LAN selection uses no ESP PIN or persistent ESP secret;
the ESP32 path is for trusted local networks only.

## Architecture

```
W1VE-Remote-Keyer/
  Cargo.toml                     workspace (reuses ../common/rwk-core)
  crates/wrk-app/
    src/pairing.rs               WAN relay/token config and LAN boundary
    src/client_controller.rs     Client: keyer, sidetone, Player browsing, edge sending
    src/server_controller.rs     Player: LAN advertisement and radio replay driver
    src/app_paths.rs             per-user config.json (stable WireGuard identity)
    src/lib.rs                   two Tauri binaries (wrk-client, wrk-server) + commands
    web/                         React UI (ClientApp, ServerApp, WAN/LAN slider)
```

The engine (`rwk-core`) is reused unchanged via a path dependency; this project adds only the
Player/Operator controllers, the pairing/token model, and the UI.

## Build

Reuses the sibling Keyer-Router's toolchain notes. On this dev machine source its
`build-env.ps1` once per shell (clang/lld + xwin MSVC CRT on `E:`), then:

```powershell
cd crates/wrk-app/web
npm install
npm run build                 # produces web/dist

cd ../../..
# standalone build needs the custom-protocol feature so the webview serves web/dist:
cargo build --release -p wrk-app --features custom-protocol \
  --bin w1ve-remote-keyer --bin w1ve-remote-keyer-player
# → target/release/w1ve-remote-keyer.exe and w1ve-remote-keyer-player.exe
```

On a normal MSVC "Desktop development with C++" install, plain `cargo build` works without the
build-env step.
