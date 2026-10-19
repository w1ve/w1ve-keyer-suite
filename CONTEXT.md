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
