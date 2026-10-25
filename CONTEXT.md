# W1VE Keyer Suite: Working Context

Current state of the suite and its projects. Written at the point the projects were consolidated
here. Keep it current when the layout, the shared engine, or the verification status changes.

## Layout

```
w1ve-keyer-suite/
  Cargo.toml          root workspace: rwk-core, rwk, rwk-relay, rwk-app
  Cargo.lock          local only (gitignored), copied from RWK-Rust to keep versions pinned
  common/rwk-core/    shared engine (keyer timing, serial keying, sidetone, paddle decode,
                      WinKeyer2 host, WireGuard tunnel, relay client, netstack)
  rwk-app/            W1VE Remote Keyer-Router: Client (w1ve-rkr) + Station (w1ve-rkr-station)
                      + Ham Router (port forwarding, Flex VITA-49 relay, SSB PTT)
  rwk-relay/          relay / rendezvous server + dashboard (VPS)
  rwk/                engine CLI (keygen, connect, selftest)
  W1VE-Keyer/         stand-alone W1VE Keyer; own workspace; crates/wk-app (w1ve-keyer)
  W1VE-Remote-Keyer/  W1VE Remote Keyer; own workspace; crates/wrk-app
                      (w1ve-remote-keyer Client + w1ve-remote-keyer-station Player/Server)
```

History: each project was imported with its full git history (`git subtree add` /
`git merge -X subtree=`). rwk-core, rwk-app, rwk-relay and rwk come from the `rust/crates/*`
paths of RWK-Rust. W1VE-Keyer and W1VE-Remote-Keyer come from their whole repos.

The original repos are untouched: `E:\AI\RWK-Rust`, `E:\AI\W1VE-Keyer`, `E:\AI\W1VE-Remote-Keyer`.
They still hold the C# RWK code, `docs/`, and the macOS provisioning notes, which were not moved.

## Projects and what each one is

| Project | Dir | Binaries | Role |
|---|---|---|---|
| W1VE Keyer | W1VE-Keyer/crates/wk-app | w1ve-keyer | Standalone WinKeyer: paddle + logger in, key line out, sidetone. No network. |
| W1VE Remote Keyer | W1VE-Remote-Keyer/crates/wrk-app | w1ve-remote-keyer, w1ve-remote-keyer-station | Client (operating position) + Player/Server (radio site) over WireGuard. |
| Remote Keyer-Router | rwk-app | w1ve-rkr, w1ve-rkr-station | Remote Keyer plus Ham Router: port-forward wizard on the operator side, forwards pushed to the Station, Flex VITA-49 discovery relay. |
| Relay | rwk-relay | rwk-relay | Key-addressed packet relay / rendezvous. Runs on a VPS; everything networked depends on it. |

## Shared WinKeyer logger host (the main recent change)

Logger input (N1MM+, DXLog and similar, speaking WinKeyer2) is implemented once, in
`common/rwk-core/src/engine/logger_host.rs`, and used by:

- the Router's Station (`rwk-app/src/station_winkeyer.rs` re-exports it),
- the Router's Client (`rwk-app/src/controller.rs`, `select_winkeyer_port`),
- the stand-alone Keyer (`W1VE-Keyer/crates/wk-app/src/winkeyer_host.rs`),
- the Remote Keyer Player (`W1VE-Remote-Keyer/crates/wrk-app/src/logger_host.rs`),
- the Remote Keyer Client (`W1VE-Remote-Keyer/crates/wrk-app/src/client_controller.rs`).

Two output modes on `LoggerHost`:

- `LoggerHost::open(port, local_key, report)`: text is keyed on a local output (Station, stand-alone Keyer, Player).
- `LoggerHost::open_text(port, sink, report)`: text goes to a `TextSink` (Client and Router Client), which pushes it into the send buffer, so it is keyed with sidetone, the key line and the tunnel.

`TextSink` covers: speed, set_speed, host_mode, send_text, abort. The Client and the Router
lock the speed slider while host mode is open (N1MM owns the speed then).

Host behaviour (follows the C# emulator's KeyerCore, which N1MM accepts):

- Admin Open replies with the version byte only (23). Sub-commands 0x10 and up consume one data byte.
- Text is batched until the logger is quiet for 5 ms; then it is echoed, followed by idle status 0xC0, and queued for CW.
- Request Status is answered at once: 0xC5 while text waits to flush, 0xC0 otherwise. Status is never held behind CW.
- CW plays on its own thread. Clear Buffer and Host Close drop the batch and the queue and abort CW in progress.
- A port read failure is reported as `HostReport::ReadFailed` (plus a log line) so the UI shows the error.
- The 1200 baud WinKeyer2 link is opened with a 5 ms read timeout.

The temporary trace logging used to debug N1MM has been removed from all projects.

## Verification

Run on the Linux build box (ssh) and cross-checked for Windows (`cargo xwin`):

- `rwk-core`: 296 lib tests + 12 integration tests + others, all pass (includes the logger host tests: admin open, status idle and busy, echo, N1MM's exact byte sequence for "CQ 7Q", clear buffer, keying and reporting, CW abort in under 500 ms).
- Suite root workspace: `rwk-core`, `rwk`, `rwk-relay` tests pass; `rwk-app` compiles for Windows including tests.
- `W1VE-Keyer` (`wk-app`) compiles for Windows including tests.
- `W1VE-Remote-Keyer` (`wrk-app`) compiles for Windows including tests (needs `web/dist` built first; Tauri embeds it).
- Web bundles build (Vite) for `rwk-app` and `wrk-app`.

Not verified:

- No hardware or N1MM run of the Router Client logger, the Router speed lock, or the Remote Keyer Client after the sink refactor (the Client was run by the user before the refactor; the refactor itself is compile- and unit-tested only).
- The rwk-relay deploy script has not been run against a VPS (the image and its compose file were verified on the build box).
- The Keyer's and Remote Keyer's Windows release binaries were not rebuilt in the suite layout.

## Known open items

1. Done: `rwk-relay` builds from the suite root (`docker build -f linux/rwk-relay/Dockerfile .`, or `docker compose -f linux/rwk-relay/docker-compose.yml up -d --build`). Verified on the build box (Docker 29.8.2): the image builds, and the container serves `/api/health` and the dashboard. `windows/rwk-relay/deploy.ps1` now runs from the suite root; its tar step was checked locally, but it has not been run against a VPS.
2. `RWK-Rust/docs/mac/provision.sh` (macOS) clones the sibling repos into one parent. The suite layout changes that; update the script when the macOS build is next used.
3. The duplicated sending logic: the Remote Keyer Client and the Router Client both use the send buffer, but the Router's buffer was changed to carry WPM per character to match the Client. Keep the two in step if the send worker changes.
4. Uncommitted, not ours (left alone): `RWK-Rust/CONTEXT.md` (modified), `W1VE-Remote-Keyer/w1ve_keyer.svg` (untracked).
5. The original repos still exist. Delete them only after the suite is accepted.
6. `Cargo.lock` is gitignored in the suite; each workspace generates its own. Consider pinning if builds should reproduce exactly.

## Build commands

```sh
# Shared workspace (rwk-core, rwk, rwk-relay, rwk-app)
cargo test -p rwk-core
cargo build --release -p rwk-relay

# Router app
( cd rwk-app/web && npm install && npm run build )
cargo build --release --features custom-protocol -p rwk-app

# Standalone apps: build from their own directory (own workspace)
( cd W1VE-Keyer/crates/wk-app/web && npm install && npm run build )   # if web/ exists
cd W1VE-Keyer && cargo build --release -p wk-app
cd W1VE-Remote-Keyer && cargo build --release -p wrk-app --features custom-protocol
```

Note: in release builds the `web/dist` bundle must exist before `cargo build`, because Tauri embeds it at compile time.

---

## Working context — session of 21 October 2026

Notes for the change currently in progress. The sections above describe the suite as a whole; this
section records the live working tree, what was fixed and verified, and the work that is next. (On
Windows `context.md` and `CONTEXT.md` are the same file, so this is appended here rather than kept
in a separate document.)

### Where the tree stands

- Branch `main`, HEAD `8ce4867` ("Add macOS and Linux app icon sizes and test the live output swap
  and peer timeout"). Nothing from this session is committed.
- Modified, all part of the port-change hardening described below:
  - `W1VE-Keyer/crates/wk-app/src/keyer_controller.rs`
  - `W1VE-Remote-Keyer/crates/wrk-app/src/server_controller.rs`
  - `common/rwk-core/src/engine/audio.rs`
  - `common/rwk-core/src/engine/keying_arbiter.rs`
  - `common/rwk-core/src/engine/replay/tracker.rs`
  - `common/rwk-core/src/engine/serial.rs`
  - `rwk-app/src/station_controller.rs`
- Untracked and not ours: `sample_about.png`.

### Port changes: what was fixed and why

Goal: a serial port or audio device (keying out, logger in, paddle in, sidetone out) can be changed
or cleared while the app is running, with no key-down glitch, no lost queued CW, and no port kept
open by a stale owner. The shared engine now carries the behaviour, so the Router and both
stand-alone apps change ports the same way.

| Area | File | Change |
|---|---|---|
| Adapter swap | `common/rwk-core/src/engine/keying_arbiter.rs` | `replace_adapter_with` no longer holds the arbiter mutex across the OS close/open. The old adapter is taken under the lock, dropped outside it, `build` runs outside it (while the arbiter points at an LED-only stand-in), then the new adapter is installed under the lock. The time-critical replay and local logger threads never block on serial syscalls. New test `replace_adapter_with_does_not_hold_lock_across_build`. |
| No key-down on open | `common/rwk-core/src/engine/serial.rs` | `SerialKeyingOutput::open` applies `(false, false)` — key up, PTT off — instead of `(true, false)`, which briefly keyed the rig on every port open/change. |
| Reconnect baseline | `common/rwk-core/src/engine/replay/tracker.rs` | Before any edge has been applied, `validate_frame` walks the frame newest-first, so the first edge validated becomes the baseline. Older edges carried redundantly in the sender's count-sized ring are discarded as duplicates instead of being scheduled as "now" (that mis-scheduling is what made a port swap or reconnect replay stale CW). New test `fresh_session_baselines_on_the_newest_edge_not_a_redundant_copy`. |
| Audio picker | `common/rwk-core/src/engine/audio.rs` | ALSA lists each card once per plugin (`hw:`, `plughw:`, `front:`, `surround…`, `dmix:`); the picker now shows only `sysdefault:` per card and labels it with the card name from `/proc/asound/cards` (the name the desktop sound panel uses). `SidetoneEngine::start_on` accepts either the label or the raw ALSA name. New helpers `listed_in_picker`, `device_label`, `alsa_card_id`, `longname_from_cards`, plus tests. |
| Standalone Keyer | `W1VE-Keyer/crates/wk-app/src/keyer_controller.rs` | The `KeyingArbiter` is now created once at start-up with an LED-only adapter and is never rebuilt: `open_keying_port` swaps the adapter in place. A cleared or failed port leaves an LED-only adapter, so the logger host and paddle sink survive a port change, and a new `keying_open` flag (not `arbiter.is_some()`) reports whether a hardware port is held. |
| Player / Server | `W1VE-Remote-Keyer/crates/wrk-app/src/server_controller.rs` | Port selection enforces exclusivity (keying, logger, paddle cannot share a port), applies a keying or logger change live through `apply_keying_live` / `apply_logger_live`, and probes a port before taking it (`probe_keying_port`, `probe_logger_port`). Removed the `KeyingOutput` wrapper; `open_logger` and `keying_adapter` are now the single paths that open ports. Tests `selecting_ports_enforces_exclusivity_and_persists`, `line_setters_reject_sharing_the_same_pin`. |
| Router Station | `rwk-app/src/station_controller.rs` | `set_key_line` / `set_ptt_line` reject using the same physical pin for both (inline error, no silent overwrite); `ptt_line_for` clamps a loaded config that has key and PTT on one line; the replay driver now uses the edge transport's real `PathHealth` (`transport.health()`) instead of a private one, so F9 sees the link it actually runs over. |

### Verification done so far (macpro build box)

All four workspaces were built and tested after the port-change edits (20 Oct, 22:52–23:02):

| Check | Result | Log |
|---|---|---|
| `cargo test -p rwk-core --lib` | 304 passed, 0 failed (2.13 s) | `/tmp/v-core-lib.log` |
| `cargo test -p rwk-app` | 16 passed, 0 failed | `/tmp/v-rwk-app.log` |
| `cd W1VE-Keyer && cargo test -p wk-app` | 4 passed, 0 failed | `/tmp/v-wk-app.log` |
| `cd W1VE-Remote-Keyer && cargo test -p wrk-app` | 10 passed, 0 failed | `/tmp/v-wrk-app.log` |
| `cargo build --release -p wrk-app` | OK, 3 m 19 s; one pre-existing `dead_code` warning (`APP_DIR_WINDOWS`, windows-only) | `/tmp/v-wrk-build.log` |

Not re-run after these edits (do this in the next verification pass):

- `common/rwk-core/tests/*.rs` (the three integration suites, including `keying_over_tunnel`).
- The Vite bundles for `rwk-app`, the stand-alone Keyer, Remote Keyer and the relay dashboard.
- Windows (`cargo xwin`) cross builds.

**Layout caveat.** `/home/gerry/build/w1ve-keyer-suite` on macpro is still the pre-`common/` layout:
its HEAD is `68f1d39`, the engine lives at `rwk-core/` in the root, and
`W1VE-Keyer/Cargo.toml` points at `../rwk-core`. The edited sources were copied in and are
byte-identical to this checkout when CRLF is normalised (md5-verified on
`keying_arbiter.rs` and `audio.rs`), so the tests exercised the same code — but against older
manifests and an older module layout. Before the next round, sync the whole tree (see "Build and
verify recipe") into a clean directory so the build matches this repository exactly.

### Outstanding: connection loss and teardown (next work)

The operator-reported behaviour to fix:

1. If the Player (radio site) is closed or loses its internet connection, the Keyer retries
   pairing **forever** and never returns to an unconnected state. It must retry for **15 seconds**
   and then settle in the disconnected state.
2. Stopping the Player and immediately reconnecting the Keyer fails with a "cannot find relay"
   style error. Stopping must release every network resource so a reconnect works at once.
3. Conversely, when the connected Keyer closes or loses its connection, the Player must disconnect
   immediately and clean up its resources.

#### Defect A — the Keyer's reconnect loop has no time bound

Where: `W1VE-Remote-Keyer/crates/wrk-app/src/client_controller.rs` — `connect` (WAN supervisor
loop), `teardown_session`, `watch_edge_liveness`; constants `PEER_TIMEOUT` (5 s) and
`MAX_RECONNECT_ATTEMPTS` (5, 2 s apart).

Cause: the supervisor resets `attempts = 0` every time a pairing *succeeds*, then on `PeerLost`
does `continue` **without incrementing** the counter. So when a pairing succeeds against a stale
relay registration and the peer is then found dead, the loop repeats forever with the UI stuck in
`connecting` ("Pairing…"). It is a count budget, not a time budget, and the count is not consumed
on the path that actually runs in this scenario.

Fix: a wall-clock reconnect window of 15 s. On peer loss, stamp `deadline = now + 15 s`; keep
attempting until the deadline, bounding each attempt with
`tokio::time::timeout(deadline - now, tunnel::connect(..))` — necessary because `PAIR_TIMEOUT` is
90 s and `PUNCH_TIMEOUT` 6 s in `engine/tunnel/connect.rs`, so a single attempt can otherwise
outlive the window. When the deadline passes: tear the session down, emit
`Connection { state: "disconnected" }`, log "Player not reachable within 15 s", stop the supervisor.
A fresh operator-initiated Connect starts a fresh window; a successful *steady* link does not
reset it (only a completely new Connect does). The LAN advertiser loop needs the same rule: re-arm
beacons only inside the window after losing the peer, then stop advertising and go disconnected.

Also: the Router's Keyer (`rwk-app/src/controller.rs`, `connect` / `drop_lost_peer` /
`spawn_rtt_heartbeat`) currently drops straight to disconnected on 5 s of silence and never
reconnects. The requirement that the two apps share code means the window logic belongs in
`rwk-core` (for example `engine/reconnect.rs`) and both Keyers call it, with unit tests using a
paused clock rather than real sleeps.

#### Defect B — the Player/Station never notices a dead Keyer

Where: `W1VE-Remote-Keyer/crates/wrk-app/src/server_controller.rs` — the heartbeat task created in
`bring_up_replay` only *sends* heartbeats and pings; nothing reads inbound traffic to judge
liveness. `rwk-app/src/station_controller.rs` (`spawn_rtt_heartbeat`) is the same. So a closed
Keyer leaves the Player showing "connected" until the operator presses Stop.

Fix: a shared silence watchdog. `EdgeTransport::recv_edge`
(`common/rwk-core/src/engine/network.rs`, around line 553) is the single receive path used by the
pump, the RTT probe and the client watch, so it is the right place to record a last-heard
timestamp (`AtomicU64`, monotonic ms). A helper (same `rwk-core` module as the reconnect window)
polls that stamp and fires `on_peer_lost` after the timeout; on the Player it runs the same
teardown as `disconnect()` and emits `Connection { state: "disconnected" }`, clears the active
session slot (so `listen` works again immediately), and forces the key and PTT up. Timeout: 1.5 s
(six missed 250 ms beats) — long enough to ride out jitter, short enough to read as "immediate" to
the operator. The local key-up path is unaffected (the replay watchdog still forces the line up on
its own schedule). Keep the number in one constant and document it.

#### Defect C — the relay hands out dead pairing partners ("cannot find relay")

Where: `common/rwk-core/src/engine/relay/server.rs` — `handle_pair` and the `Pairings` table;
constants `PAIRING_TTL` (120 s), entry type `PairWaiter { key, observed, out, since }`.

Cause: a signalling (`Pair`) connection parks a waiter in the table, but when that connection ends
nothing removes the entry — only a *later* `Pair` with the same code evicts it, and only once it is
older than `PAIRING_TTL`. `handle_pair` then matches the newcomer against that dead waiter, ignores
the failed `try_send`, and replies `Paired` with the ghost's WireGuard key and observed address.
The Keyer believes it paired, punches at an address where nothing listens, fails after the 6 s
punch timeout, and retries — exactly the "cannot find relay" symptom after stopping the Player, and
the reason the keyer can also sit in pairing forever (see Defect A). Relay route cleanup already
uses the right `same_channel` guard (in `serve_connection`); the pairing table has no equivalent.

Fix: on connection end, remove any waiter owned by that connection (compare
`out.same_channel(&out_tx)`, as routes do); when matching, skip and drop entries whose sender is
`is_closed()` instead of returning them; never answer `Paired` unless the partner's channel is
open. Optionally reap stale entries on the call path instead of relying on a 120 s TTL. Tests: a
waiter whose connection closed is not matched (the newcomer parks or times out, and the stale entry
is gone); two live waiters still pair as before.

#### Defect D — the relay client leaks its reader task and half-open socket

Where: `common/rwk-core/src/engine/relay/client.rs` — `RelayClient::connect` spawns an untracked
task that owns the TCP read half; `RelayClient` has no `Drop`, so dropping it only closes the
write half. The read half (and the task) live until the relay closes the socket, so a stopped app
can keep a registration and a socket around — the "stuck socket" the operator suspects.

Fix: keep the reader's `JoinHandle` in `RelayClient` and abort it in `Drop` (and on an explicit
`close()`), or drive the reader from a `select!` with a shutdown token so both halves drop
together. Add a test that dropping a client makes the relay see EOF and drop its route (metric
`active` returns to zero) and that a second `pair` from the same node after the drop works.

#### Teardown order to enforce everywhere

`disconnect()` / Stop / peer-lost handling, in this order: stop advertising → abort the
supervisor → detach the edge (key frames dropped, key forced up) → shut the session down (closes
the tunnel and the relay connection) → clear the transport/relay handle → clear the active-session
slot → emit `KeyState(false)`, `Ptt(false)`, `Rtt(None)`, `Connection("disconnected")` → log. It
must be idempotent (Stop pressed while already disconnected, controller dropped, peer loss racing
an operator Stop) and must leave the controller able to `connect`/`listen` again immediately.

#### UI notes

`ClientApp.jsx` maps `connection.state`: `connected` → Direct/DERP, `connecting` → "Pairing…",
anything else → "Disconnected"; a Cancel button appears while `connecting`. `ServerApp.jsx` uses
"Waiting…" for `connecting`. So the 15 s failure must end with an explicit
`Connection { state: "disconnected" }` (plus a log line) — otherwise the keyer keeps looking like
it is pairing.

### Order of work from here

1. Finish/refresh verification of the current port-change edits (integration tests, web bundles).
2. `rwk-core`: add the shared reconnect-window helper and the last-heard stamp in `EdgeTransport`,
   with unit tests on a paused clock.
3. Keyer (Remote Keyer, then Router): use the window; bound each attempt by the remaining time;
   end in `disconnected` after 15 s. Apply the same rule to the LAN advertise loop.
4. Player (Remote Keyer) and Station (Router): silence watchdog → immediate teardown and disconnect.
5. Relay: remove dead waiters on connection close, never match a closed channel; client: abort the
   reader on drop. Tests for both.
6. Re-run the whole verification matrix, then sync and re-check on macpro in a clean-layout tree.

### Build and verify recipe

```sh
# macpro (build box, per AGENTS.md). Non-login shells need PATH set explicitly.
export PATH=$HOME/.nvm/versions/node/v22.23.3/bin:$HOME/.cargo/bin:$HOME/.local/bin:$PATH

# Sync a clean copy of the whole tree (not just changed files), from this checkout:
tar czf - --exclude=target --exclude=node_modules . \
  | ssh macpro 'mkdir -p ~/build/suite && tar xzf - -C ~/build/suite'

cd ~/build/suite
cargo test -p rwk-core --all-targets          # lib + the three integration suites
cargo test -p rwk-app
cargo test -p rwk
( cd W1VE-Keyer && cargo test -p wk-app )
( cd W1VE-Remote-Keyer && cargo test -p wrk-app )
( cd rwk-app/web && npm ci && npm run build )                    # Tauri embeds dist/
( cd W1VE-Remote-Keyer/crates/wrk-app/web && npm ci && npm run build )
cargo build --release -p wrk-app --features custom-protocol
```

### Gotchas

- macpro's existing `/home/gerry/build/w1ve-keyer-suite` is the old layout and dirty against its own
  HEAD; prefer a fresh directory as above rather than patching it file by file.
- This checkout has CRLF line endings; macpro builds use LF. Compare content with `tr -d '\r'`
  before concluding two files differ.
- The desktop shell here has no ripgrep for `code_search`; use `grep -rn` through the terminal, and
  keep terminal one-shot commands short — a very long heredoc is truncated mid-file.
- Local Windows builds are unreliable (Visual Studio 18 Insiders lacks the MSVC x64 libraries);
  never treat a local link failure as a code failure.
- Commits in this repository must not carry attribution footers (see `AGENTS.md`).
