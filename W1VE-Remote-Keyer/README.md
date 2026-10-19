# W1VE Remote Keyer

A focused **Server + Client CW keyer pair** for remote Morse operation over the internet (WAN)
or a local network (LAN). Native Rust + Tauri v2, same look-and-feel as **W1VE Remote
Keyer-Router**, and built on the *same* engine — this is the Keyer-Router's keying path on its
own, with no port forwarding or FlexRadio router.

* **Client** (`w1ve-remote-keyer`) — the operating position: paddle/keyer, sidetone, serial
  key line, type-ahead CW. Sends keyed edges to the Server.
* **Server** (`w1ve-remote-keyer-station`) — the remote radio site: receives the keyed edges
  and keys the radio's control line, with the full replay jitter buffer and fail-safes.

## Transport: WAN / LAN

A segmented slider on both apps chooses how the two ends meet:

* **WAN** — the public W1VE relay on the internet. Zero network setup: no port forwarding, no
  static IP. The WireGuard tunnel goes peer-to-peer when NATs allow, else over the relay.
* **LAN** — a relay reachable on the local network (often the Server itself). After the brief
  rendezvous the two hole-punch and all keyed traffic stays on the LAN.

The Server shows a short **pairing token**; the Client enters it. No key copying by hand.

## Architecture

```
W1VE-Remote-Keyer/
  Cargo.toml                     workspace (reuses ../common/rwk-core)
  crates/wrk-app/
    src/pairing.rs               WAN/LAN transport + pairing token → rwk-core Config
    src/client_controller.rs     Client: keyer, sidetone, paddle, send edges over the tunnel
    src/server_controller.rs     Server: replay driver keys the radio from received edges
    src/app_paths.rs             per-user config.json (stable WireGuard identity)
    src/lib.rs                   two Tauri binaries (wrk-client, wrk-server) + commands
    web/                         React UI (ClientApp, ServerApp, WAN/LAN slider)
```

The engine (`rwk-core`) is reused unchanged via a path dependency; this project adds only the
Server/Client controllers, the pairing/token model, and the UI.

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
  --bin w1ve-remote-keyer --bin w1ve-remote-keyer-station
# → target/release/w1ve-remote-keyer.exe and w1ve-remote-keyer-station.exe
```

On a normal MSVC "Desktop development with C++" install, plain `cargo build` works without the
build-env step.
