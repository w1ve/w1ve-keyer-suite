# W1VE Keyer Suite — Working Context

**Last updated:** 31 October 2026.
**Purpose:** operational handoff for the unified W1VE Keyer desktop suite. Use `git status -sb`
and `git log -1 --oneline` for the authoritative branch and commit state.

---

## 1. Product names and binaries

Use these names in user-facing UI, documentation, build output, and distribution artifacts:

| Product / role | Binary | Visible product name |
|---|---|---|
| Standalone keyer | `w1ve-keyer` | W1VE Keyer |
| Remote Keyer operator | `w1ve-remote-keyer` | W1VE Remote Keyer |
| Remote Keyer radio site | `w1ve-remote-keyer-player` | W1VE Remote Keyer Player |
| Remote Keyer Router operator | `w1ve-krr` | W1VE Remote Keyer Router |
| Remote Keyer Router radio site | `w1ve-krp` | W1VE Remote Keyer Router Player |
| Engine CLI | `rwk` | RWK engine CLI |
| Relay | `rwk-relay` | RWK relay |

Use **Operator/Client** for the operating-position product and **Player** for the radio-site
product. Existing technical APIs and implementation symbols retain `Station` or `Server` where
changing them would affect protocol/persistence compatibility. In particular,
`station_app_role`, `StationController`, `server_controller`, and `run_server` are internal names.

The Router's per-user configuration path intentionally retains the legacy
`w1ve-remote-keyer-router` name so existing pairing and hardware settings remain available after
the product rename. Do not rename it without an explicit migration.

### Repository layout

```text
w1ve-keyer-suite/
  Cargo.toml                 root workspace: rwk-core, rwk, rwk-relay, rwk-app
  common/rwk-core/           shared keyer engine, tunnel, relay, replay, reconnect policy
  rwk-app/                   W1VE Remote Keyer Router Operator + Player
  rwk/                       engine CLI
  rwk-relay/                 relay / rendezvous server and dashboard
  W1VE-Keyer/                standalone W1VE Keyer workspace
  W1VE-Remote-Keyer/         focused Remote Keyer Operator + Player workspace
  linux/rwk-relay/           relay Docker image and compose files
  windows/rwk-relay/         relay deployment script
  tools/                     supported local build and splash-capture helpers
```

The original sibling repositories remain untouched. Work only in this suite checkout.

### What belongs in source control

Commit source, manifests, required public images, documentation, and maintained helpers. Do **not**
commit local build/capture material: `dist-exe/`, `dist-win/`, `shots*/`,
`rwk-app/dist-local/`, local `_check*`/`_about_test*` scripts and logs, or root capture PNGs.
`dist-exe/windows/` and `dist-exe/linux/` hold verified release copies for the operator but remain
intentionally untracked.

Commit messages have no AI/tool attribution footer. See `AGENTS.md`.

---

## 2. Desktop applications

All desktop applications are Tauri v2 shells over `rwk-core`.

| Product | Main UI |
|---|---|
| W1VE Keyer | Main and Connections tabs; local paddle/logger/keying/sidetone |
| Remote Keyer Operator | Main and Connections tabs; paired keying sender and sidetone |
| Remote Keyer Player | Main and Connections tabs; remote replay/keying fail-safe end |
| Remote Keyer Router Operator | Keyer and Ham Router tabs; forwarding/Flex support |
| Remote Keyer Router Player | Main, Connections, and Log tabs; compact radio-site monitor |

The focused Remote Keyer and Router Operator show their **Pair with Player** panel before operating
controls. This is intentional: connect first, then operate. The Router Player uses a compact
512×700 logical window (480×560 minimum). Other requested logical sizes/minimums are:

| Binary | Initial size | Minimum |
|---|---:|---:|
| `w1ve-keyer` | 580×760 | 540×620 |
| `w1ve-remote-keyer` | 760×560 | 680×480 |
| `w1ve-remote-keyer-player` | 760×560 | 680×480 |
| `w1ve-krr` | 980×640 | 900×560 |
| `w1ve-krp` | 512×700 | 480×560 |

### Main and Connections tabs

The standalone Keyer, focused Remote Keyer, and Router Player use the same Connections-tab model:
straight-key and paddle wiring diagrams, instructions, and click-to-zoom images. Assets are in each
app's `web/public/` directory. The standalone Keyer wording points back to its **Serial Ports**
controls; networked Player wording points to **Keying Output**.

### Settings persistence

Every operator-adjustable setting is saved on change and restored at startup, on Windows and Linux.

| App | File | What is restored |
|---|---|---|
| W1VE Keyer | `settings.json` (`keyer_controller.rs` `Settings`) | keyer, sidetone, lines, all ports |
| Remote Keyer Operator / Router Keyer | `client.json`, `Config.keyer` + `Config.transport` | WPM, weight, mode, paddle reverse, tone Hz/volume, audio device, paddle/logger(/keying) ports, LAN/WAN, relay, last WAN token (`pairing_code`) |
| Remote Keyer Player / Router Player | `server.json` / `station.json`, `Config.station` + `transport` | keying/logger ports, key/PTT lines, PTT hang, callsign, label, LAN/WAN, relay, WAN token |

`KeyerSettings` fields are all `Option` with `serde(default)`, so older files load with app
defaults. Saved ports are reopened at startup; a missing device reports "not available" and keeps
the selection. A USB unplug while running (`reconcile_ports`) still clears and saves that port as
`(None)`. Macros live in WebView `localStorage` (`wrk.macros` / Router key). Do not lock
`config` twice inside one `emit(...)` expression: the temporary guard lives to the end of the
statement and deadlocks.

---

## 3. Native shell, splash, About, and menu

Every desktop binary has the same shell conventions:

- **File menu:** About… opens a role-specific React About dialog. Exit closes the standalone Keyer
  or disconnects a paired networked application before exiting.
- **Splash:** a native `splash` window reusing `index.html`, identified by its Tauri label. It is
  borderless, not focusable, skipped from the taskbar, centred over the main window, and contains
  only the 300×168 artwork plus the product/role name and version.
- **Timing:** the UI waits for initial state and port snapshots, reveals the complete main surface,
  shows the splash, holds it for five seconds, then hides it.
- **Fade:** Windows uses layered-window alpha (`SetLayeredWindowAttributes`) over roughly 200 ms.
  Linux uses GTK top-level opacity on the GTK main thread over the same duration. macOS currently
  closes directly because this tree has no equivalent native fade implementation.
- **Linux WebKitGTK:** the main config permits navigation at startup. Setup makes the Linux window
  transparent until `ui_ready`; other platforms hide it during setup. `ui_ready` restores opacity
  or shows/focuses the completed UI. This prevents an unfinished startup frame while allowing
  WebKitGTK to load.
- **Windows startup (no white flash):** each app has a `tauri.windows.conf.json` overlay that
  creates the main window with `"visible": false` and `"backgroundColor": "#0f1115"`. A
  configured-visible window is shown before `setup` can hide it, which painted a bare white
  WebView2 surface for about a second. The overlay is a JSON merge patch, so it repeats the full
  `app.windows` entry; keep it in sync with `tauri.conf.json`.
- **Linux geometry:** GTK geometry hints keep the native splash at exactly 300×168 rather than its
  default 200-pixel height floor.

Important files:

| Product | Rust shell | Web shell |
|---|---|---|
| W1VE Keyer | `W1VE-Keyer/crates/wk-app/src/lib.rs` | `web/src/App.jsx`, `main.jsx`, `AboutDialog.jsx`, `ConnectionsTab.jsx`, `app-info.js` |
| Remote Keyer | `W1VE-Remote-Keyer/crates/wrk-app/src/lib.rs` | `web/src/ClientApp.jsx`, `ServerApp.jsx`, `main.jsx`, `AboutDialog.jsx`, `ConnectionsTab.jsx`, `app-info.js` |
| Router | `rwk-app/src/lib.rs` | `web/src/App.jsx`, `StationApp.jsx`, `main.jsx`, `AboutDialog.jsx`, `ConnectionsTab.jsx`, `app-info.js` |

`WebviewWindowBuilder` splash creation remains an **async** Tauri command because a synchronous
builder invocation can deadlock on Windows. Build `web/dist` before any release build: Tauri embeds
that output.

---

## 4. Reconnection and teardown

`common/rwk-core/src/engine/reconnect.rs` is the shared policy and behavior seam.

| Item | Value / API |
|---|---|
| Peer silence | `PEER_SILENCE_TIMEOUT = 1500 ms` |
| WAN reconnect budget | `RECONNECT_WINDOW = 15 s` |
| Stable link threshold | `LINK_STABLE_THRESHOLD = 5 s` |
| Heartbeat interval | 250 ms |
| RTT event cadence | every 4 heartbeats |
| Relay retry | one retry after 750 ms |
| Shared liveness watcher | `watch_edge_liveness` |
| Shared budget | `ReconnectBudget` |
| Shared rendezvous helper | `connect_with_relay_retry` |

`EdgeTransport` stamps a monotonic last-heard value for every accepted inbound frame, heartbeat,
ping, or pong. A peer silent for 1.5 seconds is lost. Both Clients use the shared liveness watcher
and supervised WAN reconnect loop. They retry within the 15-second budget, stop after five
consecutive connect failures, and re-establish the transport after `PeerLost`.

`ReconnectBudget` fixes the pair-then-die flap: a short successful pairing does **not** reset its
current 15-second budget. The budget only resets once the link stayed up for at least five seconds.

The Router Client (`rwk-app/src/controller.rs`) now follows the focused Remote Keyer Client
supervisor model. Its `bring_up_client` recreates the control channel and forwarded rules after each
successful pairing. The two Player listen paths use `connect_with_relay_retry`, avoiding the relay
restart race on both sides.

Player/Station watchdogs retain their replay fail-safes: peer silence or F2 takes the active session
slot first, forces key/PTT up, stops logger/pump/control work, emits UI reset state before awaiting
shutdown, and releases the slot so Listen can restart immediately.

### Deliberate or remaining limits

- LAN advertise/responder loops remain unbounded after peer loss. This supports recovery after a
  Player reboot but should be revisited only with an explicit operator policy decision.
- Closed pairing waiters are filtered when a new pair arrives but are not proactively reaped on
  connection end.
- The supervisor/watchdog tasks do not yet have a paused-clock fake-transport harness.
- Real-WAN reconnection matrix, N1MM plus physical-rig regression, relay VPS deployment, and macOS
  build/provisioning remain manual verification work.

---

## 5. Build and verification

### Build box

Use **macpro** (`ssh macpro`, `192.168.88.99`, user `gerry`) for real Linux builds, tests, and
Windows cross-builds. Never use `big-linux`.

```sh
export PATH=$HOME/.nvm/versions/node/v22.23.3/bin:$HOME/.cargo/bin:$HOME/.local/bin:$PATH
```

Sync with tar over SSH, excluding `target/` and `node_modules/`:

```sh
tar czf - --exclude=target --exclude=node_modules . \
  | ssh macpro 'rm -rf ~/build/suite && mkdir -p ~/build/suite && tar xzf - -C ~/build/suite'
```

Build each web bundle before the matching Tauri release build:

```sh
( cd rwk-app/web && npm ci && npm run build )
( cd W1VE-Keyer/crates/wk-app/web && npm ci && npm run build )
( cd W1VE-Remote-Keyer/crates/wrk-app/web && npm ci && npm run build )

cargo build --release --features custom-protocol -p rwk-app
( cd W1VE-Keyer && cargo build --release --features custom-protocol -p wk-app )
( cd W1VE-Remote-Keyer && cargo build --release --features custom-protocol -p wrk-app )
```

Use `cargo xwin build --release --target x86_64-pc-windows-msvc --features custom-protocol` for
Windows Tauri binaries. The local Windows Visual Studio installation is unreliable for full Rust
linking; use macpro rather than relying on it.

### ESP32 Player hardware workflow

The ESP32 firmware is the explicit hardware-target exception to the desktop Rust build-box rule:
use the local Windows ESP-IDF installation for ESP32-S3 firmware builds and COM-port operations.
The current `macpro` build box does not have ESP-IDF installed.

| Item | Verified value |
|---|---|
| ESP-IDF tools home | `E:\Espressif` |
| ESP-IDF | `E:\Espressif\frameworks\esp-idf-v6.1` |
| ESP-IDF Python environment | `E:\Espressif\python_env\idf6.1_py3.13_env` |
| Firmware project | `w1ve-player-esp32` |
| Target | `esp32s3` |
| Attached board | `COM13` — USB VID `303A`, PID `1001` |

Start a PowerShell firmware shell with:

```powershell
$env:IDF_TOOLS_PATH = 'E:\Espressif'
$env:IDF_PYTHON_ENV_PATH = 'E:\Espressif\python_env\idf6.1_py3.13_env'
& 'E:\Espressif\frameworks\esp-idf-v6.1\export.ps1'
```

From `w1ve-player-esp32`, use `idf.py build` to build and `idf.py -p COM13 flash` to write the
attached board. Recheck that COM13 is the intended ESP32-S3 before flashing. Flashing changes
hardware state, so run it only after the user explicitly approves that action.

### Verified for the W1VE v2 Player change

| Check | Result |
|---|---|
| `rwk-core` tests | 322 library tests plus 12, 2 (one ignored), and 3 additional integration-test groups passed on macpro |
| Router `rwk-app` tests | 21 passed on macpro |
| Focused `wrk-app` tests | 17 passed on macpro |
| Web builds | focused Remote Keyer and Router Vite production bundles passed on macpro |
| Linux releases | `w1ve-krr`, `w1ve-krp`, `w1ve-remote-keyer`, and `w1ve-remote-keyer-player` built on macpro |
| Windows releases | the same four `x86_64-pc-windows-msvc` executables built with `cargo xwin` on macpro |
| ESP32 Player firmware | current W1VE v2 firmware built locally with ESP-IDF 6.1; it has not been flashed to COM13 |

The current verified W1VE v2 desktop release copies are intentionally untracked under
`dist-exe/w1ve-v2/linux/` and `dist-exe/w1ve-v2/windows/`. Their SHA-256 hashes match the
macpro build outputs.

---

## 6. Quick file map

| Concern | Where |
|---|---|
| Shared reconnect policy/supervisor helpers | `common/rwk-core/src/engine/reconnect.rs` |
| Peer last-heard and path stats | `common/rwk-core/src/engine/network.rs` (`EdgeTransport`) |
| Tunnel pair/punch timeouts | `common/rwk-core/src/engine/tunnel/connect.rs` |
| Relay pairing table | `common/rwk-core/src/engine/relay/server.rs` |
| Relay client reader cleanup | `common/rwk-core/src/engine/relay/client.rs` |
| Focused Operator supervisor | `W1VE-Remote-Keyer/crates/wrk-app/src/client_controller.rs` |
| Focused Player watchdog | `W1VE-Remote-Keyer/crates/wrk-app/src/server_controller.rs` |
| Router Operator supervisor | `rwk-app/src/controller.rs` |
| Router Player watchdog/F2 | `rwk-app/src/station_controller.rs` |
| Standalone Keyer web UI | `W1VE-Keyer/crates/wk-app/web/src/App.jsx` |
| Focused Remote Keyer UIs | `W1VE-Remote-Keyer/crates/wrk-app/web/src/ClientApp.jsx`, `ServerApp.jsx` |
| Router UIs | `rwk-app/web/src/App.jsx`, `StationApp.jsx`, `KeyerTab.jsx` |
| Shared diagram UI pattern | each app's `web/src/ConnectionsTab.jsx` |
| Native menus/splash/GTK fade | each app's `src/lib.rs` |
| ESP Player protocol, transport, and setup UI | `w1ve-player-esp32/PROTOCOL.md`, `main/player_transport.c`, `main/player_web.c`, `main/web/index.html` |
| ESP Player local build/flash environment | `E:\Espressif` (`IDF_TOOLS_PATH`), ESP-IDF v6.1; attached ESP32-S3 on `COM13` |
| Router local build/capture scripts | `tools/build-station.sh`, `tools/capture-*.ps1`, `tools/measure-splash.ps1` |
| Relay deployment | `linux/rwk-relay/`, `windows/rwk-relay/deploy.ps1` |


---

## 7. W1VE v2 LAN Player transport

**Current design, confirmed 31 October 2026:** radio-site Players advertise their availability.
Operating-position Keyers browse the LAN list and explicitly choose one. This applies to desktop
Players, the **W1VE ESP32 Player**, W1VE Remote Keyer, and W1VE Remote Keyer Router.

- Desktop Players advertise `RWKP` v2 beacons. A selected operating Keyer initiates the direct
  WireGuard connection. Router desktop Players advertise both CW and Router capability.
- W1VE ESP32 Players advertise W1VE v2 type-1 records from UDP 4995 to discovery port 4994.
  A Keyer may run one bounded, paced `/24` type-10 unicast discovery pass when cross-band Wi-Fi
  suppresses broadcast. The Keyer sends type 2; the selected Player returns type 3.
- The W1VE v2 session key is
  `SHA-256("W1VE-SESSION-v2" | player_id | keyer_id | initiator_nonce | responder_nonce)`.
  Authenticated type-4 edge frames, type-6/7 PING/PONG, and type-8 unpair use a 64-packet replay
  window. The sender serializes envelopes and terminates before sequence rollover.
- ESP CW is strictly Keyer → Player. The Keyer re-sends the latest complete `RWK-PADDLE` frame at
  250 ms intervals. PING/PONG runs independently every second and ends a session after three
  missed matching PONGs. Only a valid full type-4 frame refreshes the ESP's 1500 ms asserted-KEY
  lease.
- The focused Client owns and waits for its cancelled edge drain before sending ESP unpair, so a
  queued type-4 frame cannot start after the operator presses Stop. `EdgeSession` also drops
  buffered callbacks at detach and includes a regression test.
- Router ESP selection is CW-only. It does not start a WireGuard `Session`, control link,
  forwarding, Flex discovery, or Router UI controls.
- ESP browser sidetone and Web Audio are removed. GPIO KEY/PTT and the optional LED are the
  activity confirmation. The ESP Player web UI only provides configuration, status, logs, and
  local Disconnect.
- ESP web live updates: ESP-IDF v6.1 completes the `/ws` handshake without calling the URI
  handler, so the page sends a `hello` text frame on open and `player_web.c` registers the
  client from that frame. The page also polls `/api/status` every 2 s while the socket is down.
- LAN Player display fields are UTF-8-safe at the core 32-byte wire limit. A multi-byte character
  is never split into an invalid advertisement.
- The desktop UI lists **W1VE ESP32 Player** entries alongside desktop Players. Busy ESP32 Players
  cannot be selected. WAN remains the existing relay/shared-token flow and is not used by ESP32.

### Live LAN facts

- ESP32-S3 MAC: `1c:db:d4:4d:3a:40`; known station IP before the v2 flash: `192.168.88.130`.
- PC station IP: `192.168.88.215`; both devices use the `W1VE Maine` SSID, with the PC on 5 GHz
  and ESP on 2.4 GHz.
- Same-subnet unicast works. Limited and directed IPv4 broadcasts are suppressed between those
  bands, which is why v2 Keyers use bounded type-10 unicast fallback queries.
- First-boot/recovery setup AP remains `W1VE-Player-XXXX` at `192.168.4.1`. The current firmware
  binds raw W1VE discovery, pairing, and session sockets only to the joined non-overlapping
  station subnet, not directly to the setup AP subnet.

### Current validation and handoff

- `common/rwk-core/src/engine/esp_player.rs` contains W1VE v2 advertisement, pairing, envelope,
  reordering, counter-exhaustion, and cross-language HMAC test coverage.
- `common/rwk-core/src/engine/edge_session.rs` contains shared FIFO, latest-frame refresh, first
  frame, and detach-cancellation regression coverage.
- `w1ve-player-esp32/tools/verify_esp_player.py` defaults to no-CW discovery/pairing/ping/replay/
  safe-unpair checks. With `--esp`, it starts with a direct type-10 query so it also works on the
  known cross-band broadcast-suppressed Wi-Fi path. `--exercise-cw` is an explicit
  controlled-hardware test only.
- The latest W1VE v2 ESP-IDF image (`0xDC280`, with the `/ws` hello registration fix) was built
  and flashed to COM13; live pushes of paired/peer status to an open page were verified. After the Wi-Fi
  event-stack correction, `http://192.168.88.130/api/status` stays reachable with
  `networkConnected: true`, `transportAvailable: true`, and safe inactive KEY/PTT output.
- The live default no-CW ESP diagnostic passed against `192.168.88.130`: direct query/reply,
  Keyer-initiated pairing, bidirectional matching PING/PONG, bounded reordering, replayed-unpair
  rejection, and safe type-8 unpair.
- Current desktop v2 releases are staged at `dist-exe/w1ve-v2/` (not tracked in git). The
  1 November 2026 builds of `w1ve-remote-keyer`, `w1ve-remote-keyer-player`, `w1ve-krr`, and
  `w1ve-krp` (Windows and Linux) include the Windows UDP 10054 discovery fix, the no-white-flash
  window overlay, and full settings persistence. `w1ve-keyer` needed no persistence change.
  The operator confirmed all apps working on 1 November 2026.
- Windows build rule: run `npm run build` in each app's `web/` immediately before
  `cargo xwin build`. Tauri embeds `web/dist`, and a stale bundle produced a window that never
  called `ui_ready`.
- The Windows discovery sweep treats UDP `WSAECONNRESET` (10054) from an ICMP
  port-unreachable reply as a transient negative result, not a browser failure.
- Launch the staged Windows Keyer and Player binaries manually to verify the Player-selection UI.
  Do not run the `--exercise-cw` diagnostic unless the radio connection is safe for controlled
  key/PTT output.

### Open decision: serial port unplug

`reconcile_ports` (all apps) clears a vanished serial selection to `(None)` and saves it, so an
unplugged USB adapter must be re-selected. Remembering the selection and reopening it on re-plug
was offered and not yet chosen.

### Deferred security decision

The setup AP still uses its existing fixed provisioning password. It cannot directly carry raw
W1VE keying traffic because station-only sockets are enforced, but an AP client can still change
station Wi-Fi configuration. A per-device setup credential or a post-provisioning management
restriction remains deferred by the user while the current trusted-LAN/no-PIN flow is evaluated.
