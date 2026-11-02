# Releasing the W1VE Keyer Suite

This repository builds all shippable artifacts with GitHub Actions. A single version tag
drives three workflows that build, sign (macOS, if configured), and publish everything to one
GitHub Release, plus a Docker image and a browser-based ESP32 flasher.

## What ships

| Product | Windows x64 | macOS (universal) | Linux x64 | Raspberry Pi (arm64) | Android | ESP32-S3 |
|---|:--:|:--:|:--:|:--:|:--:|:--:|
| W1VE Keyer | `.exe` | `.dmg` | `.deb` + `.AppImage` | `.deb` + `.AppImage` | — | — |
| W1VE Remote Keyer | `.exe` | `.dmg` | `.deb` + `.AppImage` | `.deb` + `.AppImage` | Phase 2 | — |
| W1VE Remote Keyer Player | `.exe` | `.dmg` | `.deb` + `.AppImage` | `.deb` + `.AppImage` | — | — |
| W1VE Remote Keyer Router | `.exe` | `.dmg` | `.deb` + `.AppImage` | `.deb` + `.AppImage` | Phase 2 | — |
| W1VE Remote Keyer Router Player | `.exe` | `.dmg` | `.deb` + `.AppImage` | `.deb` + `.AppImage` | — | — |
| W1VE ESP32 Player (firmware) | — | — | — | — | — | 4M / 8M / 16M |
| rwk-relay | Docker image `ghcr.io/w1ve/rwk-relay` (amd64 + arm64) | | | | | |

Android for the two Operator apps is a deferred Phase 2: it builds, but the engine's native
layer (audio, sockets) needs porting before the APK is functional. iOS is not targeted.

## How to cut a release

1. Make sure `main` is green and the three workspace `Cargo.lock` files are committed.
2. Bump the version in each `tauri.conf.json` and `Cargo.toml` (`version = "0.1.0"`).
3. Tag and push:

   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```

That single tag triggers:

- **`release-desktop.yml`** — builds all five products on Windows, macOS (arm64 + x64),
  Linux x64, and Raspberry Pi arm64, then attaches every artifact to the GitHub Release.
- **`release-esp32.yml`** — builds the ESP32-S3 firmware for 4M/8M/16M flash, attaches a merged
  flashable `.bin` per size to the Release, and deploys the ESP Web Tools install page to
  GitHub Pages.
- **`release-relay.yml`** — builds and pushes the multi-arch relay image to GHCR.

`workflow_dispatch` (the "Run workflow" button) runs the same builds **without** publishing, so
you can smoke-test the pipeline on a branch before tagging.

## The two-binaries-per-app detail

Three crates produce five products. Two of them build a paired Operator and Player from one
crate that share a single `tauri.conf.json`:

- `rwk-app` -> `w1ve-krr` (Operator) + `w1ve-krp` (Player)
- `wrk-app` -> `w1ve-remote-keyer` (Operator) + `w1ve-remote-keyer-player` (Player)

Windows ships the raw per-binary `.exe`s directly, so both appear with no extra work. For macOS
`.dmg` and Linux `.deb`/`.AppImage`, the bundle is named by the crate's single `productName`, so
each paired crate is **built twice** with a config patch that selects the binary and its product
identity:

| Patch file | mainBinaryName | productName | identifier |
|---|---|---|---|
| `rwk-app/tauri.operator.conf.json` | `w1ve-krr` | (base) W1VE Remote Keyer Router | `com.w1ve.rwk` |
| `rwk-app/tauri.player.conf.json` | `w1ve-krp` | W1VE Remote Keyer Router Player | `com.w1ve.rwk.player` |
| `wrk-app/tauri.operator.conf.json` | `w1ve-remote-keyer` | (base) W1VE Remote Keyer | `com.w1ve.remotekeyer` |
| `wrk-app/tauri.player.conf.json` | `w1ve-remote-keyer-player` | W1VE Remote Keyer Player | `com.w1ve.remotekeyer.player` |

These patches change only packaging metadata (and the Player's window title/size). The runtime
role is still chosen by which binary runs, exactly as before.

## macOS signing and notarization

Unsigned `.dmg` files build fine but show a Gatekeeper warning on first launch. To ship clean,
add these repository secrets (Settings -> Secrets and variables -> Actions). They are encrypted
and never visible after entry:

| Secret | What it is |
|---|---|
| `APPLE_CERTIFICATE` | base64 of your Developer ID Application `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | the `.p12` export password |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: Your Name (TEAMID)` |
| `APPLE_ID` | your Apple ID email |
| `APPLE_PASSWORD` | an app-specific password for notarization |
| `APPLE_TEAM_ID` | your 10-character Team ID |

Export the cert:

```sh
# From Keychain Access, export the Developer ID Application cert as cert.p12, then:
base64 -i cert.p12 | pbcopy   # paste into the APPLE_CERTIFICATE secret
```

With the secrets present, `tauri-action` signs and notarizes automatically. Windows binaries are
shipped unsigned for v0.1 (SmartScreen warning on first run, same as current builds).

## ESP32 firmware and the web flasher

`release-esp32.yml` builds the firmware three times, once per flash size, using Espressif's
official ESP-IDF CI container. For each size it produces a **merged** image flashable with a
single command:

```sh
esptool.py --chip esp32s3 write_flash 0x0 w1ve-esp32-player-8MB-merged.bin
```

It also publishes an [ESP Web Tools](https://esphome.github.io/esp-web-tools/) page to GitHub
Pages. Operators open the page in Chrome or Edge, connect the board over USB, pick their flash
size, and click Install — no local tools. Enable Pages once under
Settings -> Pages -> Source: GitHub Actions.

Partition layouts live beside the firmware: `partitions.4mb.csv`, `partitions.8mb.csv`,
`partitions.16mb.csv`. The app image is well under 1 MB, so all three sizes fit with headroom.

## rwk-relay Docker image

`release-relay.yml` builds `linux/rwk-relay/Dockerfile` for amd64 + arm64 and pushes to
`ghcr.io/w1ve/rwk-relay`, tagged with the version and `latest`. Run it:

```sh
docker run -d --name rwk-relay -p 9000:9000 -p 8080:8080 \
  -e RWK_ADMIN_PASSWORD=change-me -e RWK_JWT_SECRET=$(openssl rand -hex 32) \
  ghcr.io/w1ve/rwk-relay:latest
```

Or use `linux/rwk-relay/docker-compose.yml`. Put a TLS proxy (e.g. Cloudflare) in front of
port 8080; keep 9000 reachable for the relay protocol.

## Runners and cost

All desktop and firmware builds use free GitHub-hosted runners, including the free arm64 Linux
runners for the Raspberry Pi targets. No self-hosted runner or the `macpro` build box is needed
for releases; `macpro` remains for local development and manual verification.
