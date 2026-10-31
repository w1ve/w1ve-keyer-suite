# W1VE Keyer Suite — Working Context

**Last updated:** 30 October 2026.
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

### Verified for this change

| Check | Result |
|---|---|
| `rwk-core` tests | 315 library tests plus 12, 2 (one ignored), and 3 additional test groups passed |
| `rwk-app` tests | 17 passed |
| `wk-app` tests | 4 passed |
| `wrk-app` tests | 10 passed |
| Web builds | all three Vite bundles passed |
| Linux releases | all seven release binaries built |
| Windows builds | all seven `x86_64-pc-windows-msvc` release binaries built |
| Windows UI Automation | exact titles, headers, tabs/diagrams, menus/About, and pairing-first layouts verified |
| Linux Xvfb/Openbox | exact titles, splash dimensions/fade, tabs/diagrams, menus/About, and pairing-first layouts verified |

Verified release copies are intentionally untracked under `dist-exe/windows/` and
`dist-exe/linux/`. The canonical files are:

```text
rwk                 rwk.exe
rwk-relay           rwk-relay.exe
w1ve-keyer          w1ve-keyer.exe
w1ve-remote-keyer   w1ve-remote-keyer.exe
w1ve-remote-keyer-player  w1ve-remote-keyer-player.exe
w1ve-krr            w1ve-krr.exe
w1ve-krp            w1ve-krp.exe
```

Copied artifacts were SHA-256 checked against macpro build outputs. Windows and Linux UI review
captures are local `shots-*` artifacts and are not source.

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
| Router local build/capture scripts | `tools/build-station.sh`, `tools/capture-*.ps1`, `tools/measure-splash.ps1` |
| Relay deployment | `linux/rwk-relay/`, `windows/rwk-relay/deploy.ps1` |
