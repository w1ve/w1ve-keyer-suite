# W1VE Keyer Suite — Working Context

**Last updated:** 30 October 2026.
**Code HEAD:** `8844f0e` (27 Oct 2026), branch `main`, remote `origin`
(https://github.com/w1ve/w1ve-keyer-suite.git). This document is committed on top of that.
**Purpose:** everything needed for another engineer (or agent) to pick this up without the
conversation that produced it. It describes the suite as it stands, what changed most recently,
how the reconnection/teardown logic actually works, what was and was not verified, and what is
still open.

---

## 1. Where the tree stands

- Branch `main`, code HEAD `8844f0e` — *"Show the Station splash in its own window and fade it
  out"*. `origin/main` is the source of truth; check `git status -sb` for ahead/behind.
- The tracked tree is clean at that commit. Everything from the recent work is committed.
- **Untracked, deliberately not committed:** local capture/build scratch — `shots*/`,
  `dist-exe/`, `dist-win/`, `rwk-app/dist-local/`, `_check.log`, `_about_test.ps1`, and the
  stray root-level PNGs (`about.png`, `paddle-image.png`, `straightkey-image.png`,
  `station-about*.png`, `sample_about.png`). These are not source. Do not stage them; add
  ignore rules only if they are no longer useful.
- **Commit messages carry no attribution footer** — see [AGENTS.md](AGENTS.md). No
  "Generated with", no `Co-Authored-By`.
- `Cargo.lock` is gitignored; each workspace generates its own. If reproducible builds matter,
  reconsider that.

Recent history (newest first):

| Commit | Date | Subject |
|---|---|---|
| `8844f0e` | 27 Oct | Show the Station splash in its own window and fade it out |
| `d1f61ae` | 25 Oct | Redesign Station UI with Connections tab, splash, and compact window |
| `fec5eba` | 25 Oct | Unpair the Station when the Client goes silent or F2 closes the session |
| `d1dae17` | 24 Oct | Drop the Client when its heartbeats stop so the radio is never left keyed |
| `eb21161` | 24 Oct | Shorten peer loss to 1.5 s and bound Keyer reconnects to a 15 s window |
| `675b8c9` | 24 Oct | Let serial and audio ports change live without glitches or stale owners |
| `8ce4867` | 23 Oct | Add macOS and Linux app icon sizes and test the live output swap and peer timeout |

Naming: the binaries are **`w1ve-rkr`** (Client / operating position) and
**`w1ve-rkr-station`** (Station / radio site). Gerry often writes these as "w1ve-rwk" and
"w1ve-rwk-station"; they are the same two executables.

---

## 2. Layout and applications

```
w1ve-keyer-suite/
  Cargo.toml            root workspace: rwk-core, rwk, rwk-relay, rwk-app
  common/rwk-core/      shared engine (timing, serial, sidetone, paddle, WinKeyer2 host,
                        WireGuard tunnel, relay client, netstack, reconnect policy)
  rwk-app/              W1VE Remote Keyer-Router: Client + Station + Ham Router
  rwk-relay/            relay / rendezvous server + dashboard
  rwk/                  engine CLI (keygen, connect, selftest)
  W1VE-Keyer/           stand-alone W1VE Keyer; own workspace; crates/wk-app
  W1VE-Remote-Keyer/    W1VE Remote Keyer; own workspace; crates/wrk-app
  linux/rwk-relay/      relay Dockerfile + compose
  windows/rwk-relay/    relay deploy script (run from Windows)
  tools/                local build workaround + splash capture/measurement scripts
```

| App | Binaries | Role | Latest code change |
|---|---|---|---|
| W1VE Keyer | `w1ve-keyer` | Standalone software WinKeyer: paddle + WinKeyer2 logger in, serial key line out, local sidetone. No network. | `675b8c9` (live port changes) |
| W1VE Remote Keyer | `w1ve-remote-keyer`, `w1ve-remote-keyer-station` | Focused Client/Station pair over the WireGuard tunnel. | `fec5eba` / `d1dae17` / `eb21161` |
| W1VE Remote Keyer-Router | `w1ve-rkr`, `w1ve-rkr-station` | Remote Keyer plus port forwarding, SSB PTT, Flex VITA-49 discovery relay. | Station: `8844f0e`; Client: `fec5eba` |
| Relay | `rwk-relay` | Key-addressed packet relay / rendezvous + dashboard. VPS container. | `eb21161` |
| Engine CLI | `rwk` | Engine diagnostics (keygen, connect, selftest). | unchanged |

Heritage: the C# RWK Router/Keyer was ported to Rust `rwk-core` (native boringtun +
smoltcp + a minimal relay, replacing Tailscale), then split into these apps. The `rwk`
prefix is a nod to that **R**emote **W**in**K**ey lineage.

Every app is a Tauri v2 shell over `rwk-core`. The Router Client and Station share one Vite
bundle and one Tauri config; `main.jsx` probes the `station_app_role` command to decide which UI
to mount. Only the Router Station has the redesigned Station UI described below.

---

## 3. Latest work in detail

### 3.1 Station splash as its own window — `8844f0e` (27 Oct)

The splash used to be a React overlay inside the Station's main window. That painted the whole
window black around a 300×168 graphic and, on exit, the panel blinked out in one frame. The
current design:

- **The popup is a native Tauri window**, label `"splash"`, 300×168 (the exact size of
  `hero.png`), borderless, non-resizable, always on top, skipped from the taskbar, not focused,
  no shadow, opaque background `#0d1721` matching the artwork, centred over the main window,
  and with the File menu removed (otherwise it carries a menu bar and is taller than the art).
- **The main window is created hidden** (`"visible": false` in `rwk-app/tauri.conf.json`) and
  revealed by the `ui_ready` command. The first thing the operator sees is the finished UI, not
  an empty webview. `main.jsx` also has a 3 s safety timer that calls `ui_ready` if the role
  probe never settles.
- **When it shows:** `StationApp` waits until all four of these hold — `document.readyState ===
  "complete"`, `document.fonts.ready`, the first `settings` snapshot, and the first `ports`
  snapshot. Only then does it call `station_api.uiReady()` and `station_api.showSplash()`. If
  the backend never answers, the splash never appears rather than playing over a half-built
  window.
- **Timing:** the 5 s timer (`SPLASH_MS = 5000`) starts only after `showSplash()` resolves, so
  the popup is visible for the full five seconds. Then `hideSplash()`.
- **Fade:** `station_hide_splash` calls `fade_splash_out` (Windows only) before closing. It adds
  `WS_EX_LAYERED` and steps `SetLayeredWindowAttributes` alpha from 255 to 0 by elapsed time
  over 200 ms, sleeping 8 ms between steps. Elapsed-time stepping (not a fixed step count)
  keeps the fade near 200 ms even though Windows timer resolution rounds sleeps up (~15.6 ms).
  Off Windows there is no equivalent one-call window alpha; the popup simply closes.
- **Why the window fades, not the artwork:** Windows composites a top-level window as one
  surface. CSS opacity inside the webview could only dissolve the artwork into the popup's own
  background, and the plate would still vanish in one frame. Layered-window alpha fades the
  artwork and the plate together.
- **Must be async:** `station_show_splash` is an `async` Tauri command because
  `WebviewWindowBuilder` deadlocks on Windows when it runs inside a synchronous command or event
  handler (tauri-apps/tauri#13092). The symptom is a window whose webview never initialises.
- **Transparent windows do not work here:** making the popup transparent rendered nothing at
  all on Windows. It is opaque with the artwork's own background colour instead.

Supporting pieces added by the same commit:

- `rwk-app/web/src/AboutDialog.jsx` — the About artwork as the dialog background, product name
  and version in the top band, copyright / MIT / repository lines below. Click anywhere closes.
- `rwk-app/web/src/app-info.js` — `APP_NAME`, `APP_VERSION` moved out of the deleted
  `SplashScreen.jsx`; both the About dialog and any future surface use it.
- `rwk-app/web/station-preview.html` — a browser-only dev harness that stubs the Tauri IPC
  (`station_app_role`, ports, token) so `StationApp` can be previewed in Vite without the native
  shell. It is a convenience, not a substitute for a Tauri run.
- `rwk-app/web/public/about.png` — About artwork.

### 3.2 Station UI redesign — `d1f61ae` (25 Oct)

- Three tabs: **Main** (keying status, keying output/ports, read-only forwarded rules, pairing
  panel), **Connections**, **Log** (`LogEntries` filling the tab).
- **Connections** tab shows straight-key and paddle wiring diagrams
  (`public/straightkey-image.png`, `public/paddle-image.png`) at a readable width, click to zoom
  in/out, with short wiring instructions and a pointer back to the Main tab's port/line pickers.
- **Compact site window:** `run_station` overrides the shared 980×640 config window to
  512×700 with a 480×560 minimum, sized for a small site monitor. Title:
  `W1VE Remote Keyer-Router Station — Any Rig, Any Internet, Anytime`.
- The Station header shows link state (Direct / DERP / Pairing… / Disconnected), RTT, and the
  bottom strip shows KEY and PTT on every tab.
- `LogPanel.jsx` now exports `LogEntries` so the Client and Station share it.
- The Station still does not require a keying port to pair: with none selected the KEY/PTT LEDs
  follow the Client and the serial output is LED-only.

### 3.3 Reconnection and teardown — `eb21161`, `d1dae17`, `fec5eba`

This is the work most likely to need continuation; it is documented in full in §5. Summary:

- One shared policy in `common/rwk-core/src/engine/reconnect.rs`: peer silence 1.5 s, reconnect
  window 15 s.
- `EdgeTransport` records a monotonic last-heard timestamp on every accepted datagram.
- The Remote Keyer Client retries within the 15 s window and ends explicitly disconnected.
- The Remote Keyer Station (Player) and Router Station notice a silent Client within 1.5 s,
  force the key/PTT up, unpair, and free the session slot so Listen works immediately.
- The Router Client notices a silent Station within 1.5 s and unpairs (no automatic reconnect —
  see §5.11).
- The relay no longer hands out a pairing waiter whose signalling connection has closed, and
  `RelayClient` aborts its reader task on drop so the socket is released.

### 3.4 Live port changes — `675b8c9` (24 Oct)

Goal: change or clear a serial/audio port while running with no key-down glitch, no lost queued
CW, and no port left open by a stale owner. Shared engine changes apply to all apps:

- `keying_arbiter.rs`: `replace_adapter_with` takes the old adapter under the lock, drops it
  outside, builds the new one outside (arbiter temporarily on an LED-only stand-in), then
  installs it under the lock. Replay and logger threads never block on serial syscalls. Test
  `replace_adapter_with_does_not_hold_lock_across_build`.
- `serial.rs`: opening a port applies `(false, false)` (key up, PTT off), not `(true, false)`;
  opening no longer briefly keys the rig.
- `replay/tracker.rs`: before any edge has been applied, `validate_frame` walks newest-first so
  the first validated edge becomes the baseline; older redundant edges in the count-sized ring
  are discarded instead of scheduled as "now". This is what stopped stale CW after a port swap
  or reconnect. Test `fresh_session_baselines_on_the_newest_edge_not_a_redundant_copy`.
- `audio.rs`: ALSA lists each card once per plugin; the picker shows one `sysdefault:` entry per
  card, labelled with the name from `/proc/asound/cards`. `SidetoneEngine::start_on` accepts
  either the label or the raw ALSA name.
- Stand-alone Keyer: one `KeyingArbiter` for the process, created with an LED-only adapter;
  `open_keying_port` swaps in place. A failed/cleared port leaves LED-only, so logger and paddle
  survive. `keying_open` (not `arbiter.is_some()`) reports whether hardware is held.
- Remote Keyer Station: exclusivity for keying/logger/paddle ports, live apply through
  `apply_keying_live`/`apply_logger_live`, probe before taking a port. Removed the
  `KeyingOutput` wrapper. Tests for exclusivity and pin sharing.
- Router Station: `set_key_line`/`set_ptt_line` reject the same physical pin for key and PTT;
  `ptt_line_for` clamps a loaded config that has both on one line; the replay driver uses the
  edge transport's real `PathHealth`.

### 3.5 Icons and tests — `8ce4867` (23 Oct)

Added `icon.icns` and 32/128/256 px PNGs to all three Tauri bundles and listed them in each
config. Added tests for the arbiter's live output swap and the Client's peer timeout.

---

## 4. Application state

### W1VE Keyer (`w1ve-keyer`, `W1VE-Keyer/crates/wk-app`)

Standalone, no networking. Paddle and WinKeyer2 logger input, serial key line out, local
sidetone. Latest code is the live-port work in `675b8c9`. It uses the shared `LoggerHost` and
`KeyingArbiter` exactly as the networked apps do. Build from its own workspace; `web/dist` must
exist before a release build because Tauri embeds it.

### W1VE Remote Keyer (`w1ve-remote-keyer`, `w1ve-remote-keyer-station`)

The focused Client/Station pair. The Client owns paddle, sidetone, type-ahead buffer and the
WAN/LAN transport; the Station owns the replay jitter buffer, serial keying and fail-safes. The
reconnection work landed here first (`eb21161`, `d1dae17`) and the teardown ordering in
`fec5eba`. This is the simplest pair to use when debugging link behaviour, because there is no
forwarding or Flex discovery in the way.

### W1VE Remote Keyer-Router Client (`w1ve-rkr`)

The operating-position app with the Ham Router. Latest changes:

- `fec5eba`: `connect` retries the relay rendezvous once after 750 ms before surfacing "relay
  not reachable"; `drop_lost_peer` emits the disconnected UI state before awaiting the session
  shutdown; `disconnect` still aborts advertiser/control/session.
- `eb21161`: `PEER_TIMEOUT` comes from the shared 1.5 s policy.
- Liveness: `spawn_rtt_heartbeat` runs two tasks — one drains `recv_edge` and tracks inbound
  traffic, the other sends a heartbeat + ping every 250 ms and emits RTT every fourth tick.
  After 1.5 s of silence it calls `drop_lost_peer`.
- There is **no automatic reconnect** after a lost WAN link. The UI goes Disconnected and the
  operator presses Connect. See §5.11 item 1.
- Local release binary built 26 Oct 11:53 (`target/release/w1ve-rkr.exe`).

### W1VE Remote Keyer-Router Station (`w1ve-rkr-station`)

The radio-site app. The most recent work is all here:

- Redesigned compact UI with Main / Connections / Log tabs (`d1f61ae`).
- Native splash popup with a 200 ms layered-window fade (`8844f0e`).
- File menu: About… (opens `AboutDialog`) and Exit (disconnects the pairing, then exits).
- Silent-Client and F2 watchdog (`fec5eba`): the replay bus forwarder sets `session_closed`
  when fail-safe F2 is raised, and the RTT probe sees it; either that or 1.5 s of peer silence
  takes the active session, stops the logger/bus, aborts the pump and control task, emits every
  UI reset (KEY up, PTT off, logger not sending, forward rules cleared, Flex discovery off, RTT
  none, disconnected), logs, and shuts the session down. The slot is taken first so Listen works
  again immediately.
- Session watchdog details are in `spawn_rtt_probe` (`rwk-app/src/station_controller.rs`).
- Local release binary built 27 Oct 14:17; the operator reviewed the UI and splash captures and
  approved the Station.

### Relay (`rwk-relay`)

Key-addressed packet relay / rendezvous with a dashboard. Every WAN pairing depends on it.
Latest fixes in `eb21161` (closed pairing waiters, client reader drop). Deploy artifacts:
`linux/rwk-relay/Dockerfile`, `linux/rwk-relay/docker-compose.yml`,
`windows/rwk-relay/deploy.ps1`. The image/compose were verified on the build box; the deploy
script has not been run against a VPS.

### Engine CLI (`rwk`)

Unchanged. Useful for key generation and diagnostics (`rwk keygen`, `rwk connect`,
`rwk selftest`).

---

## 5. Reconnection and teardown — the full picture

### 5.1 The three operator-reported problems

1. **Keyer retries forever.** If the Player is closed or loses its internet connection, the
   Keyer retried pairing forever and never returned to the unconnected state. It must retry for
   **15 s**, then settle Disconnected.
2. **Immediate reconnect fails.** Stopping the Player and immediately reconnecting the Keyer
   failed with a "cannot find relay" style error. Stopping must release every network resource
   so a reconnect works at once.
3. **Player never notices a dead Keyer.** When the connected Keyer closed or lost its link, the
   Player kept showing "connected" until the operator pressed Stop.

### 5.2 Shared policy

`common/rwk-core/src/engine/reconnect.rs` is the single source of truth:

| Item | Value / API |
|---|---|
| `PEER_SILENCE_TIMEOUT` | **1500 ms** — six missed 250 ms heartbeats |
| `RECONNECT_WINDOW` | **15 s** |
| `peer_silent(last_heard, now)` | true once silence ≥ the timeout |
| `ReconnectWindow` | `open(now)`, `with_span(now, span)`, `expired(now)`, `remaining(now)` |

The window is a wall-clock budget, not an attempt count. Tests cover the timeout boundary,
the 15 s constant, countdown to zero, and a fresh window restarting the budget.

`EdgeTransport` (`common/rwk-core/src/engine/network.rs`) carries
`last_heard_nanos: AtomicU64`:

- initialised at transport creation, so the silence clock starts even before the first datagram;
- stamped in `recv_edge` after the peer-source gate for **any** accepted datagram — heartbeat,
  ping/pong, or keying frame;
- read through `peer_silent_for()`;
- test `peer_silence_resets_on_any_accepted_datagram`.

The exact 4-byte heartbeat magic is classified before frame parsing, so a heartbeat cannot be
mistaken for a malformed frame.

### 5.3 Keyer WAN supervisor — Remote Keyer Client

File: `W1VE-Remote-Keyer/crates/wrk-app/src/client_controller.rs` (`connect`,
`watch_edge_liveness`, `teardown_session`, `lan_advertise_and_await`). Constants:
`MAX_RECONNECT_ATTEMPTS = 5`, 2 s between attempts, `RECONNECT_BUDGET = RECONNECT_WINDOW`.

Flow of the supervised WAN connect:

1. Operator Connect calls `disconnect()` first (so a second Connect never leaves two loops),
   then emits `Connection { state: "connecting" }`.
2. Loop opens a `ReconnectWindow` lazily. If `remaining` is zero, `fail()` emits Disconnected
   and logs "Could not reach the Player within 15s; disconnected. Reconnect to try again."
3. Each `tunnel::connect` is wrapped in `tokio::time::timeout(remaining, ...)`. This is
   essential: `PAIR_TIMEOUT` is 90 s and `PUNCH_TIMEOUT` 6 s inside
   `engine/tunnel/connect.rs`, so one attempt could otherwise outlive the 15 s budget.
4. On a successful tunnel, `bring_up_keying` installs the edge. Success clears both the attempt
   counter and the budget (`attempts = 0; budget = None`), then `watch_edge_liveness` runs.
5. `watch_edge_liveness` sends a heartbeat + ping every 250 ms and waits up to
   `PEER_SILENCE_TIMEOUT` for any inbound datagram. It returns `PeerLost` on silence or `Closed`
   when the edge stream ends (a deliberate unpair/teardown).
6. `teardown_session` drops the transport and edge and shuts the session. `Closed` returns from
   the supervisor. `PeerLost` logs "Player stopped responding; reconnecting…", emits
   `connecting`, and loops; the next pass opens a fresh 15 s window.
7. Connect failures (relay unreachable, handshake failure) increment `attempts`; at 5 the
   supervisor calls `fail()` with "after 5 attempts; giving up. Reconnect to try again."
   Otherwise it logs "Reconnect attempt n/5…" and sleeps 2 s.

Budget semantics to understand before changing it:

- The `ReconnectWindow` is opened lazily on the first loop pass, so the **initial** pairing from
  the operator's Connect is itself bounded by 15 s (rendezvous + handshake), and a fresh one is
  opened after a previously successful link is lost.
- A *successful pairing* clears the budget (`budget = None`). Therefore a peer that completes a
  handshake and then dies instantly gets a fresh 15 s window each time — the pathological
  flapping case can keep the UI in "Pairing…" indefinitely, one 15 s episode after another.
  `MAX_RECONNECT_ATTEMPTS` does not bound this path because the counter is only incremented on
  connect failures. Fixing this properly needs a stability threshold (for example: only clear
  the budget once the link has stayed up for ≥ 5 s) or a single deadline fixed at Connect. See
  §5.11 item 3.
- The peer-loss path deliberately does **not** increment `attempts`; the 15 s window is the
  budget that matters there.

### 5.4 Keyer LAN responder loops

Both Clients have a LAN mode that arms once and then acts as a persistent responder:

- Remote Keyer: `lan_advertise_and_await` in `client_controller.rs`.
- Router: `lan_advertise_and_await` in `rwk-app/src/controller.rs`.

Each cycle binds a fresh WireGuard socket, advertises the beacon, waits for the Station to
initiate, stops the beacon during the handshake, brings the edge up, watches liveness, tears the
session down, and loops to re-advertise. The re-bound port is always advertised, so a Station
resolves the current endpoint from the live beacon.

**The 15 s window is not applied here.** After peer loss the LAN loop re-advertises forever
until the operator unarms it. That is useful for a Station that reboots, but it is a divergence
from the WAN rule and from the original requirement. Decide explicitly whether LAN should be
bounded; if yes, use `ReconnectWindow` around the post-loss re-advertise phase and stop with
`Connection("disconnected")` + `Advertising(false)` on expiry. See §5.11 item 2.

### 5.5 Player / Remote Keyer Station watchdog

File: `W1VE-Remote-Keyer/crates/wrk-app/src/server_controller.rs` (`bring_up_replay`,
`drop_lost_peer`, `disconnect`). `HEARTBEAT_INTERVAL = 250 ms`, `RTT_EVERY_TICKS = 4`.

- The heartbeat task created at bring-up sends a heartbeat and a ping each tick and emits RTT
  every fourth tick.
- Before sending, it checks `edge.peer_silent_for() >= PEER_SILENCE_TIMEOUT`. On silence it calls
  `drop_lost_peer` and returns.
- `drop_lost_peer` takes the active session out of the slot **first** (so a later `listen` or
  `connect_lan` can start at once; a second call is a no-op), stops the bus and logger, aborts
  the pump, emits KEY up, PTT off, logger-not-sending, RTT none and Disconnected, logs "The
  Client stopped responding; disconnected.", then shuts the session down.
- `disconnect` (operator Stop) does the same teardown but emits before blocking on shutdown, so
  a stuck shutdown cannot leave the UI showing a live link. `Drop` calls `disconnect`.
- The key line is forced up by session shutdown and the replay driver's own fail-safes, so the
  radio is not left keyed.

### 5.6 Station watchdog — Router (`w1ve-rkr-station`)

File: `rwk-app/src/station_controller.rs` (`bring_up_station`, `spawn_rtt_probe`,
`spawn_bus_forwarder`, `drop` path).

- `spawn_rtt_probe` runs every 250 ms. It checks `stop` (normal teardown), then
  `session_closed` (set by the bus forwarder on fail-safe **F2**) or 1.5 s of peer silence.
- On either trigger it takes the `ActiveSession` (slot cleared first), sets the stop flag, stops
  the logger, aborts the pump and control task, emits `KeyState(false)`, `Ptt(false)`,
  `LoggerSending(false)`, `ForwardRules([])`, `FlexDiscovery(false)`, `Rtt(None)`,
  `Connection(disconnected)`, logs "The Client stopped responding; unpaired.", then awaits
  `session.shutdown()`. It returns, ending the probe.
- F2 handling was previously invisible to the pairing state; the bus forwarder now sets
  `session_closed` when `CoreEvent::FailSafe { code: 2 }` arrives, so the UI returns to Start
  instead of showing a dead pairing.

### 5.7 Router Client liveness (`w1ve-rkr`)

File: `rwk-app/src/controller.rs` (`connect`, `spawn_rtt_heartbeat`, `drop_lost_peer`,
`disconnect`, `lan_advertise_and_await`).

- `connect` retries the relay rendezvous once (750 ms apart) before emitting the relay error;
  this addresses the transient relay race after a Station restart.
- `spawn_rtt_heartbeat` runs a receive-drain task (1 s timeout on `recv_edge`, tracks
  `last_heard`) and a 250 ms heartbeat/ping sender. On 1.5 s silence `drop_lost_peer` runs:
  transport cleared, edge detached, control task and sender cleared, RTT none + Disconnected
  emitted **before** awaiting session shutdown, log "The Station stopped responding; unpaired."
- No automatic reconnect. The operator presses Connect. This is the biggest behavioural
  difference from the Remote Keyer Client.

### 5.8 Relay fixes

**Ghost pairing partners** — `common/rwk-core/src/engine/relay/server.rs`, `handle_pair`:

- `PAIRING_TTL` is 120 s.
- A `Pair` first evicts an entry older than the TTL.
- It then does `table.remove(code).filter(|partner| !partner.out.is_closed())`. A waiter whose
  signalling connection has closed is discarded, never handed to the newcomer. The newcomer
  parks as the new waiter and times out normally, rather than being introduced to a ghost and
  failing a hole punch against a dead address.
- Test `a_closed_pairing_waiter_is_not_handed_out` covers exactly the "stop the Player, then
  reconnect" sequence.
- Residual nuance: a closed waiter is not proactively removed at connection close; it is
  filtered on the next `Pair` (or reaped by TTL). That is sufficient for correctness but leaves
  a small amount of dead state. Route cleanup on connection end already uses the right
  `same_channel` guard; the pairing table could use the same treatment. §5.11 item 5.

**Leaked relay client socket** — `common/rwk-core/src/engine/relay/client.rs`:

- `RelayClient` now owns `reader: JoinHandle<()>` and implements `Drop` to abort it, so dropping
  the client releases the read half (and the socket) even while a `RelaySender` clone still
  holds the write half.
- Test `dropping_a_client_releases_its_relay_route` asserts the relay sees EOF, `metrics.active`
  returns to zero, and the same node can pair again.
- `pair()` signalling remains a one-shot short-lived connection; the normal registered client is
  a separate connection.

### 5.9 Teardown order (enforce everywhere)

The intended order, from the original defect analysis:

1. Stop advertising / cancel the pending reconnect.
2. Abort the supervisor and the heartbeat/probe task.
3. Detach the edge (key frames are dropped; the replay watchdog forces the line up).
4. Shut the session down (closes the tunnel and relay connection).
5. Clear the transport, control channel and relay handle.
6. Clear the active-session slot.
7. Emit `KeyState(false)`, `Ptt(false)`, `Rtt(None)`, `Connection("disconnected")`.
8. Log the reason.

Implementations differ in the exact order, but after `fec5eba` every path emits the UI reset
**before** awaiting `session.shutdown()`. That is deliberate: a blocked shutdown must not leave
a stale "connected" state on screen. Idempotency comes from taking the session out of its slot;
a second Stop, a controller drop, or a peer loss racing an operator Stop sees `None` and does
nothing. Every path must leave the controller able to `connect`/`listen` again immediately.

### 5.10 Manual reconnection test matrix

These are the operator scenarios the fixes target. Run them on a real WAN pair (or two machines
on separate networks) and record the result before calling the work done:

| # | Scenario | Expected |
|---|---|---|
| 1 | Stop the Player while the Keyer is connected | Keyer shows Pairing…/connecting for at most 15 s, then Disconnected with the 15 s log line. It must not stay in Pairing forever. |
| 2 | Restart the Player within 15 s | Keyer reconnects automatically and keys again. |
| 3 | Close the Keyer while the Player waits | Player returns to Start/Waiting within ~1.5 s; Listen works immediately. Radio key/PTT are up. |
| 4 | Stop the Player, immediately start it again, press Listen, then Connect on the Keyer | Pairing succeeds; no "cannot find relay" ghost. |
| 5 | Pull the network cable/Wi-Fi on either end | Both ends go disconnected within ~1.5 s (the far end within its own timeout). |
| 6 | Press F2 on the Station while paired | Station unpairs to Start; Keyer sees silence and reconnects (Remote Keyer) or goes Disconnected (Router Client). |
| 7 | Rapid Stop/Connect/Stop cycles | No stuck sockets, no duplicate supervisors, relay `metrics.active` returns to 0. |
| 8 | LAN mode: stop the Player, leave Advertise armed | Client re-advertises and the Player can reconnect; confirm this is the intended unbounded behaviour. |

### 5.11 Remaining reconnection gaps (prioritised)

1. **Router Client has no WAN auto-reconnect.** `w1ve-rkr` drops to Disconnected after 1.5 s and
   waits for the operator. The Remote Keyer Client supervisor is the model. Port it (or extract
   a shared helper in `rwk-core`) so both keyers behave the same. Remember `bring_up_client`
   also owns the control channel and forward rules, so reconnect must re-establish those.
2. **LAN loops are unbounded.** Both `lan_advertise_and_await` implementations re-advertise
   forever. If that is intended (Station reboot recovery), document it in the UI; if not, apply
   `ReconnectWindow` and stop with an explicit disconnected state.
3. **The reconnect budget resets on every successful pairing.** A peer that pairs then dies
   immediately can keep the Keyer in Pairing indefinitely, one 15 s episode at a time. Only
   clear the budget after the link has been stable for a few seconds, or keep one deadline from
   the operator's Connect.
4. **Station-side relay retry asymmetry.** The Router Client retries the relay rendezvous once;
   the Router Station's `listen` and the Remote Keyer Server's `listen` do not. A transient
   relay race after a restart therefore fails the Station immediately. Add the same 750 ms
   retry (or a shared helper) to both `listen` implementations.
5. **Proactively reap closed pairing waiters.** `handle_pair` filters closed senders at match
   time; a connection-end hook using `out.same_channel(&out_tx)` would remove them at once and
   match the route cleanup already in `serve_connection`.
6. **No automated coverage of the supervisors/watchdogs.** The `ReconnectWindow` policy, the
   last-heard stamp, the relay ghost path and client drop are unit-tested. The 15 s supervisor,
   `watch_edge_liveness`, and the Station/Client watchdogs are not. They need a paused-clock
   test harness and a fake edge/transport; real sleeps would make the suite slow and flaky.
7. **No end-to-end WAN run at this HEAD.** Everything above is reasoned from code and covered
   in pieces; the actual bug scenarios must be re-run on a real network after the next build.
8. **N1MM / hardware regression.** The logger sink and port-change work was compile- and
   unit-tested but not re-run with N1MM and a real rig after the latest changes.

---

## 6. Station UI / splash reference

Files and responsibilities:

| Concern | File |
|---|---|
| Splash popup build, show, fade, close | `rwk-app/src/lib.rs` (`station_show_splash`, `build_splash_window`, `station_hide_splash`, `fade_splash_out`) |
| File menu (About/Exit) | `rwk-app/src/lib.rs` (`run_station`) |
| Hidden main window / reveal | `rwk-app/tauri.conf.json` (`visible: false`), `ui_ready` in `lib.rs`, `main.jsx` safety timer |
| Splash artwork-only render | `rwk-app/web/src/main.jsx` (`SPLASH_ONLY`, `SplashArtwork`) |
| Splash timing / UI-loaded gate | `rwk-app/web/src/StationApp.jsx` (`SPLASH_MS`, `useUiLoaded`) |
| Station API bridge | `rwk-app/web/src/station-api.js` |
| Tabs and layout | `StationApp.jsx`, `StationTab.jsx`, `ConnectionsTab.jsx`, `LogPanel.jsx` |
| About dialog | `AboutDialog.jsx`, `app-info.js`, `public/about.png` |
| Styling | `rwk-app/web/src/styles.css` |
| Browser preview harness | `rwk-app/web/station-preview.html` |

Lifecycle in one line:

```
role probe → StationApp mounts → settings + ports snapshots arrive → uiReady() reveals main
window → showSplash() creates 300x168 popup → 5000 ms → hideSplash() → 200 ms fade → close
```

Important build/run caveats:

- `web/dist` must be built before `cargo build --release`; Tauri embeds it. Vite is configured
  to copy `hero.png`, `about.png` and the connection diagrams from `public/`.
- The splash reuses `index.html` and distinguishes its window by the Tauri label `"splash"` (or
  a `?splash` query for browser preview). Do not rely on a separate HTML entry.
- The window must be shown from an **async** command on Windows (Tauri deadlock #13092).
- `SetLayeredWindowAttributes` is Windows-only. The non-Windows branch simply closes; if a
  Linux/macOS fade is wanted, it needs a separate implementation (for example an animated
  webview alpha, which has the plate problem described in §3.1).
- The main window is deliberately invisible until `ui_ready`; if that command is removed, the
  app starts with no visible window.
- The File menu is attached to the application; the splash explicitly removes it so the popup
  stays 300×168.

Measurement/capture tooling (all in `tools/`, all hardcode `E:\AI\w1ve-keyer-suite` and
`target\release\w1ve-rkr-station.exe`):

| Script | What it does |
|---|---|
| `build-station.sh` | Builds the Station release binary with explicit MSVC 14.44 + Windows SDK include/lib paths (see §7). |
| `capture-splash.ps1` | Captures the splash popup by following the process's window. |
| `capture-visible.ps1` | Captures the process's visible main window over a cold start. |
| `capture-visible-main.ps1` | Finds the main window by title once, keeps its rect for the whole run, and copies that screen area each frame — so the splash appears inside the frames instead of replacing them. Also logs every visible window's rect per frame. |
| `capture-splash-fade.ps1` | Captures the splash at short intervals to inspect the fade frames and per-frame mean brightness. |
| `measure-splash.ps1` | Polls the process's top-level windows every 20 ms and reports first/last sighting of the 300×168 popup, bounding the visible duration to ~20 ms. |

The scripts stop any running `w1ve-rkr-station`, launch the release EXE, and are safe to rerun.
They were used to validate the splash position, five-second duration and fade; the operator
reviewed the captures and approved the result.

---

## 7. Build and verify

### Build box

Per `AGENTS.md`, use **macpro** (`ssh macpro`, `192.168.88.99`, user `gerry`, Ubuntu Linux) for
real builds and tests, both Linux and Windows targets. Never use `big-linux` (unreachable).

```sh
export PATH=$HOME/.nvm/versions/node/v22.23.3/bin:$HOME/.cargo/bin:$HOME/.local/bin:$PATH
```

macpro has Rust + cargo-xwin/cargo-tauri/cargo-zigbuild, the `x86_64-pc-windows-msvc` target,
`libasound2-dev`, `libudev-dev`, `pkg-config`, `clang`, `lld`, `llvm-21`, plus user symlinks
`~/.local/bin/llvm-lib` and `~/.local/bin/llvm-rc` to the `-21` binaries. Keep `~/.local/bin`
on PATH.

### Clean sync and full verification

```sh
# From this checkout (Windows), sync a clean copy — not just changed files:
tar czf - --exclude=target --exclude=node_modules . \
  | ssh macpro 'rm -rf ~/build/suite && mkdir -p ~/build/suite && tar xzf - -C ~/build/suite'

ssh macpro
export PATH=$HOME/.nvm/versions/node/v22.23.3/bin:$HOME/.cargo/bin:$HOME/.local/bin:$PATH
cd ~/build/suite

cargo test -p rwk-core --all-targets      # lib + the integration suites
cargo test -p rwk-app
cargo test -p rwk
( cd W1VE-Keyer && cargo test -p wk-app )
( cd W1VE-Remote-Keyer && cargo test -p wrk-app )

( cd rwk-app/web && npm ci && npm run build )
( cd W1VE-Remote-Keyer/crates/wrk-app/web && npm ci && npm run build )
( cd W1VE-Keyer/crates/wk-app/web && npm ci && npm run build )

cargo build --release --features custom-protocol -p rwk-app
( cd W1VE-Remote-Keyer && cargo build --release --features custom-protocol -p wrk-app )
```

Cross-build Windows with `cargo xwin build --release --target x86_64-pc-windows-msvc ...` (or
the project's cargo-xwin wrapper). Build release only after `web/dist` exists in every app.

### Local Windows build (this machine)

Local builds are normally unreliable: the Visual Studio 18 Insiders install at
`E:\Program Files\Microsoft Visual Studio\18\Insiders` is missing the MSVC x64 libraries and
`excpt.h`. A plain `cargo build` fails in `vswhom-sys` (see `rwk-app/_check.log`). A complete
MSVC 14.44 toolchain and Windows 10 SDK are on disk, so `tools/build-station.sh` points
`INCLUDE`/`LIB` at them:

```sh
bash tools/build-station.sh
# → target/release/w1ve-rkr-station.exe
```

Remove the script once the Insiders install is repaired (Visual Studio Installer → Repair), at
which point plain `cargo build` works. The script only builds the Station. The Client can be
built the same way by changing `-p rwk-app --bin` (or use macpro).

`web/dist` is already generated in this checkout for local runs; regenerate it after UI edits:

```sh
cd rwk-app/web && npm install && npm run build
```

### Gotchas

- This checkout has **CRLF**; macpro builds use LF. Compare with `tr -d '\r'` before concluding
  files differ — copies are byte-identical after normalisation.
- The desktop shell has no ripgrep for `code_search`; use `grep -rn` in the terminal.
- Keep one-shot terminal commands short — a very long heredoc is truncated mid-file.
- `CONTEXT.md` and `context.md` are the same file on Windows; this is the root `CONTEXT.md`.
- Tauri embeds `web/dist` at compile time. A release build with a stale `dist` ships a stale UI.
- The `station-preview.html` harness stubs only a handful of commands; it cannot exercise the
  native splash, menu, or serial paths.
- Do not add attribution footers to commits (`AGENTS.md`).

---

## 8. Verification status

### Last recorded full matrix (macpro, 20 Oct 2026, at the port-change work)

| Check | Result |
|---|---|
| `cargo test -p rwk-core --lib` | 304 passed, 0 failed (2.13 s) |
| `cargo test -p rwk-app` | 16 passed, 0 failed |
| `cd W1VE-Keyer && cargo test -p wk-app` | 4 passed, 0 failed |
| `cd W1VE-Remote-Keyer && cargo test -p wrk-app` | 10 passed, 0 failed |
| `cargo build --release -p wrk-app` | OK, 3 m 19 s; one pre-existing `dead_code` warning (`APP_DIR_WINDOWS`) |

Since that run, tests were added (live output swap, peer timeout, reconnect policy, relay ghost
waiter, relay client drop) and the Station UI/splash changed. **The full matrix has not been
re-run at `8844f0e`.** Do not claim current totals until it is.

### Known verified

- Station release binary built and run locally on Windows on 27 Oct (via
  `tools/build-station.sh`), with splash captures and operator review of the main window,
  splash position/duration, fade, Connections tab and About dialog.
- `ReconnectWindow`/`peer_silent` unit tests, `EdgeTransport` last-heard test, relay
  `a_closed_pairing_waiter_is_not_handed_out`, and `dropping_a_client_releases_its_relay_route`
  are in the tree and were part of the committed work. They have not been re-executed in this
  session.
- The rwk-relay image/compose built and served `/api/health` + dashboard on the build box in an
  earlier session.

### Not verified / limitations

- Full test suite and Windows cross-build at HEAD `8844f0e`.
- Real-WAN reconnection scenarios 1–8 in §5.10.
- LAN unbounded re-advertise behaviour under a long outage.
- Non-Windows splash (there is no fade; it simply closes).
- Relay deploy script against a VPS.
- N1MM/hardware regression after the logger/port changes.
- macOS build/provisioning; `RWK-Rust/docs/mac/provision.sh` still assumes the old sibling-repo
  layout.

---

## 9. Order of work from here

1. Sync macpro and run the full test matrix + web builds at HEAD; record real counts.
2. Decide and implement the LAN reconnection policy (§5.11 item 2), or document the choice.
3. Port the 15 s supervisor to the Router Client (§5.11 item 1). This is the largest remaining
   behavioural gap between the two apps.
4. Fix the budget-reset flapping edge (§5.11 item 3) and add a paused-clock test for the
   supervisor.
5. Add the relay retry to both Station `listen` paths (§5.11 item 4).
6. Reap closed pairing waiters at connection end (§5.11 item 5).
7. Build Windows release binaries from the clean tree and run the manual WAN matrix in §5.10.
8. Run N1MM + rig smoke test (logger host, port change, fail-safes).
9. Update `RWK-Rust/docs/mac/provision.sh` for the suite layout when macOS is next built.
10. Remove or ignore scratch directories before a release tag.

---

## 10. Quick file map

| Concern | Where |
|---|---|
| Reconnect policy, constants | `common/rwk-core/src/engine/reconnect.rs` |
| Last-heard clock, heartbeat, path stats | `common/rwk-core/src/engine/network.rs` (`EdgeTransport`) |
| Tunnel connect / pair / punch timeouts | `common/rwk-core/src/engine/tunnel/connect.rs` |
| Relay server, pairing table | `common/rwk-core/src/engine/relay/server.rs` |
| Relay client, reader drop | `common/rwk-core/src/engine/relay/client.rs` |
| Remote Keyer Client supervisor | `W1VE-Remote-Keyer/crates/wrk-app/src/client_controller.rs` |
| Remote Keyer Station watchdog | `W1VE-Remote-Keyer/crates/wrk-app/src/server_controller.rs` |
| Router Client liveness/reconnect | `rwk-app/src/controller.rs` |
| Router Station watchdog/F2 | `rwk-app/src/station_controller.rs` |
| Router Station UI | `rwk-app/web/src/StationApp.jsx`, `StationTab.jsx`, `ConnectionsTab.jsx`, `styles.css` |
| Splash, menu, window | `rwk-app/src/lib.rs`, `rwk-app/tauri.conf.json`, `rwk-app/web/src/main.jsx` |
| Station API bridge | `rwk-app/web/src/station-api.js` |
| About | `rwk-app/web/src/AboutDialog.jsx`, `app-info.js`, `public/about.png` |
| Logger host (WinKeyer2) | `common/rwk-core/src/engine/logger_host.rs` |
| Keying arbiter / serial / replay | `common/rwk-core/src/engine/keying_arbiter.rs`, `serial.rs`, `replay/` |
| Relay deploy | `linux/rwk-relay/`, `windows/rwk-relay/deploy.ps1` |
| Local build/capture tools | `tools/` |
