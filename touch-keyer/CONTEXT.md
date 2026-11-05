# CONTEXT — W1VE Touch Keyer (station side)

**Firmware version 0.1.1.** Bump it in the top-level `CMakeLists.txt` (`W1VE_FW_VERSION`); it is
compiled into the `main` component and shown in the boot log, the web page header, and
`/api/status` (`fw` key).

Working context for anyone (human or agent) picking this firmware up. It covers what the box is,
how it is built and flashed, how the pieces fit together at runtime, the wire protocol, the
persistence model, the test scripts, and the traps that have already cost time here.

Design rationale and forward plan live in [SPEC.md](SPEC.md); the operator-facing summary is
[README.md](README.md). This file is the map of the code as it exists today.

---

## 1. What this is

Firmware for the **Waveshare ESP32-C6-Touch-LCD-1.47** (ESP32-C6FH8, 320×172 touch panel, no PSRAM,
no DAC). It is the **station** end of the operator position:

* an iambic keyer that owns the rig's KEY/PTT line, usable with no network at all;
* a 320×172 touch panel: name plate, live line state, five macro keys and a settings page;
* a browser UI on port 80 for everything that does not belong on a 1.47" screen, plus a WebAudio
  sidetone fed over `/ws`;
* the **Player** role of the W1VE v2 trusted-LAN protocol, so a W1VE Remote Keyer upstairs can key
  this station and fire its macros;
* an optional **WAN transport** (`main/wan.c` + `main/rwk/`) that reaches a remote W1VE Keyer over
  the Internet through the RWK relay and WireGuard, interoperating with the existing desktop RWK
  clients and relay. See §14 and [WAN.md](WAN.md).

Status: a complete, working station keyer, a complete v2 LAN Player, and a working WAN transport
verified against the real `rwk-relay` and a real `rwk-core` Keyer. The remaining protocol work is
the remote macro trigger and a Keyer advertisement (see §9 of SPEC.md).

---

## 2. Hardware

### Keyer and status pins (`main/include/board.h`)

| Function | GPIO | Why this pin |
| --- | --- | --- |
| Paddle dit | 7 | free, documented as connected to nothing |
| Paddle dah | 4 | TF-card CS — card slot unused |
| KEY out | 3 | TF-card MISO — card slot unused |
| PTT out | 0 | BAT_ADC tap; the divider is harmless for an output |
| Status LED | 8 | WS2812/NeoPixel, GRB order, also the BOOT strap |

GPIO5/GPIO6 are the QMI8658A interrupt outputs and the IMU can hold them low after reset, which
would read as a permanently pressed paddle. They are avoided on purpose. GPIO8 is a boot strap read
by the ROM before any of this code runs, so driving the LED does not disturb flashing or boot.

Outputs are active high (`KEYER_KEY_ACTIVE_HIGH`, `KEYER_PTT_ACTIVE_HIGH`); set either to 0 for an
inverting interface. Paddles switch to GND with internal pull-ups. Debounce is 4 ms — shorter than
any real element (a 40 WPM dit is 30 ms). PTT hangs 500 ms after the last element.

### Display and touch

* JD9853 over SPI: SCK 1, MOSI 2, CS 14, DC 15, RST 22, BL 23 (LEDC PWM, 10-bit).
* Landscape 320×172. `LCD_MADCTL 0x28` (MV + BGR), `LCD_INVERT_COLOR 0`, `LCD_Y_OFFSET 34` because
  the visible 172 px window is centred in the controller's 240 px of line RAM.
* AXS5106L capacitive touch on I2C (SDA 18, SCL 19, RST 20, INT 21) at address 0x63, 400 kHz.
  It reports in the panel's native portrait space, so `TOUCH_SWAP_XY 1`, no mirroring. The firmware
  logs raw → mapped coordinates for the first eight touches as a calibration aid.
* LVGL 8.4, 16-bit colour, two 320×32 partial buffers (~20 KB each). No PSRAM, so no full-screen
  animation and the panel refreshes at 4 Hz or less.

Board bring-up switches in `board.h`: `UI_ORIENT_SELFTEST` (cycles four MADCTL values, 3 s each —
off by default) and `KEYER_BOOT_SELFTEST` (dry-run element timing at boot, pins untouched).

---

## 3. Build, flash, capture

The toolchain is project-local: ESP-IDF v5.3.4 in `.toolchain/esp-idf`, tools in
`.toolchain/espressif`, Python interpreter recorded in `.toolchain/python_bin`. Nothing outside this
folder is modified, and `tools/idf.sh` wires all of that up.

```bash
tools/setup-idf.sh                                  # one time, ~2 GB
tools/idf.sh idf.py build
tools/idf.py -p /dev/ttyACM0 flash                  # via tools/idf.sh
tools/idf.sh python -u tools/capture.py /dev/ttyACM0 40
tools/idf.sh python tools/capture.py /dev/ttyACM0 15 noreset   # attach to a running board
```

`tools/capture.py` holds the port open, resets the chip through RTS (the C6's native USB maps RTS to
EN), and streams the log as it arrives; `DTR` is held deasserted so the chip boots the application,
not the ROM downloader.

Practical rules learned here:

* **Free the port first.** `capture.py`, `idf.py monitor` and a stray `idf.py flash` all contend for
  `/dev/ttyACM0`:
  ```bash
  pids=$(fuser /dev/ttyACM0 2>/dev/null | tr -s ' ' '\n' | grep -E '^[0-9]+$')
  for p in $pids; do [ "$p" != "$$" ] && kill "$p" 2>/dev/null; done
  ```
* **Opening the port resets the chip.** After a capture or a flash, allow 12–15 s before expecting
  HTTP: Wi-Fi association, sockets and the HTTP server all come up after boot.
* Tool-call timeouts of 300 s mean a flash + reboot + network test must be split, or the HTTP wait
  has to be a bounded poll rather than a fixed sleep.
* Partition table: `nvs` 0x9000 (24 KB), `phy_init`, single `factory` app at 0x10000 (1.5 MB).
  Flash is 8 MB, CPU 160 MHz, `CONFIG_FREERTOS_HZ=1000`, `CONFIG_LWIP_MAX_SOCKETS=20`.

---

## 4. Runtime layout

`app_main()` in `main/app_main.c` is the whole boot order:

```
nvs_flash_init → board_display_init → board_touch_init → board_led_init
              → keyer_init → macro_init → [keyer_selftest] → ui_init
              → net_init → remote_init → web_init
              → loop { lv_timer_handler(); delay(10 ms); }
```

The keyer runs on its own high-priority task with `esp_timer` element deadlines. The display is a
view of that, never part of the keying path: a wedged LVGL call cannot delay a dit.

| File | Responsibility |
| --- | --- |
| `app_main.c` | boot sequence, LVGL tick loop |
| `board_display.c` | SPI, JD9853 init, LVGL display driver, backlight PWM, MADCTL |
| `board_touch.c` | AXS5106L I2C touch, LVGL pointer device, raw→display mapping |
| `board_led.c` | WS2812 on GPIO8 over RMT at 10 MHz, GRB; writes only on colour change |
| `keyer.c` | iambic A/B + straight engine, paddle ISRs, debounce, weight, PTT tail, macro text sender, keytest, remote-edge entry points |
| `macro.c` | six NVS-backed slots (label + CW text) |
| `ui.c` | both panel pages, status dots, speed/LAN controls, LED state machine |
| `net.c` | Wi-Fi station + setup AP, scan/join/forget, NVS credentials |
| `remote.c` | W1VE v2 LAN Player: advertisement, pairing, HMAC envelope, replay window, edge frames, ping, lease, measured peer speed |
| `wan.c` | WAN transport: RWK relay rendezvous, WireGuard session (direct or relayed), state machine, STUN, feeds the shared edge applier |
| `rwk/` | portable, host-testable transport core: `rwk_wg` (WireGuard), `rwk_frame` (relay framing), `rwk_inner` (inner IPv4/UDP), `rwk_blake2s`, `rwk_crypto` (mbedTLS backend), `rwk_stun`, `rwk_jitter`, `rwk_edge` (shared CW edge applier), `rwk_lineclaim` (LAN/WAN arbitration) |
| `web.c` | HTTP server, static page and JS, `/ws` sidetone stream, all `/api/*` handlers (LAN and WAN) |
| `include/*.h` | pin map, and the status structs that are the module contracts |

Key contracts:

* `keyer_status_t { key_down, ptt_down, wpm, weight, mode }` — post-arbiter line state, so the dots
  show what the rig sees, not what a source asked for.
* `remote_status_t { paired, alive, available, lan_enabled, active, keying, keyer[18], peer[24],
  session_id, rtt_ms, peer_wpm, reason[64] }` — `active` is the single "remote owns the line" flag
  used by the panel, the page and the LED, so those three cannot disagree.
* `keyer_edge_cb_t(bool down, uint32_t ms)` — the keyer task calls this on every KEY transition; the
  web sidetone feeds off it. It must not block.

---

## 5. Panel (320×172)

**Page 1** — the name plate:

```
W1VE Remote Player/Keyer                        ●   ← title red; KEY dot (red when down)
W1VE Maine   192.168.88.168                     ●   ← SSID + address green; PTT dot (blue)
REMOTE: 24 WPM  /  LOCAL: 20 WPM  /  REMOTE: -- WPM
[ CQ ] [ 599 ] [ TU ]        ← five blue macro keys, 98×44, 7 px gutter
[ 73 ] [ W1VE ] [ Set... ]
```

* The speed line follows whoever owns the line. With no session it is `LOCAL: NN WPM` (the station
  setting). With a live session it is `REMOTE: NN WPM`, or `REMOTE: -- WPM` until that Keyer has
  actually sent CW — the station speed is never shown under a REMOTE label.
* The speed line, the page 2 state text and the status LED are all driven from the same two flags.
* Slots 0–4 appear as buttons; slot 5 is edited and fired from the web page only.

**Page 2** (`Set...`, back with `Back`): 5–50 WPM slider, one LAN switch, and the transport state
under it — `Advertising`, `Paired`, `No network`, `LAN off`. Both pages keep the KEY/PTT dots.

* While a Keyer holds the session the remote owns the speed: the slider greys out, stops taking
  touch, and the log says `remote up: speed line REMOTE, speed slider locked`.
* The LAN switch shows the **stored** setting, not whether the socket happens to be bound, so it
  stays honest with the station LAN down. Turning it off drops any session immediately.

**Status LED** (WS2812, GPIO8):

| Colour | Meaning |
| --- | --- |
| Blue | only the setup access point is up |
| Green | joined the station Wi-Fi |
| Orange | a Keyer holds a session |

Because the part may not be fitted on every variant (and is not always in sight), the same colour is
repeated on a dot beside the speed line, and `POST /api/led` with `r=`, `g=`, `b=` drives it directly
for a wiring check.

Do **not** copy the reference Player's LED scheme (red = no Wi-Fi, blue = waiting, green = paired,
white = KEY down). This product's colours are defined by SPEC.md §4 — blue = access point only,
green = station Wi-Fi, orange = Keyer session — and the LED, the on-screen dot and the page 2 state
text must stay on the same flags.

---

## 6. Web UI (port 80)

One static document plus a small form-encoded API — the firmware needs no JSON parser for two
integers, and the page needs no request serialiser. The page polls `/api/status` every 400 ms.

| Endpoint | Method | Purpose |
| --- | --- | --- |
| `/` | GET | the page |
| `/api/status` | GET | full status JSON (see below) |
| `/api/wpm` | POST | `wpm=5..50` |
| `/api/mode` | POST | `mode=0..2` |
| `/api/weight` | POST | `weight=25..75` |
| `/api/scan` | GET | visible networks |
| `/api/join` | POST | `ssid=…&pass=…` |
| `/api/identity` | POST | `name=…`, the name browsing Keyers see |
| `/api/advertise` | POST | `on=0/1`, the LAN transport switch |
| `/api/unpair` | POST | drop the session |
| `/api/led` | POST | `r=&g=&b=` 0..255, drive the status LED |
| `/api/keytest` | POST | key a few elements from the station engine |
| `/api/macros` | GET | six slots as JSON |
| `/api/macro` | POST | `idx=0..5&label=…&text=…`, saved to NVS |
| `/api/macro/play` | POST | `idx=0..5`, send that slot as CW |
| `/api/wan` | POST | `on=0/1`, the WAN transport switch |
| `/api/wan/relay` | POST | `relay=host:port`; empty restores the default relay |
| `/api/wan/code` | POST | `code=…`, the pairing code shared with the remote Keyer |
| `/api/wan/disconnect` | POST | drop the WAN session |
| `/ws` | GET (WebSocket) | sidetone edge frames |

`/api/status` keys: `network, address, wpm, mode, weight, key, ptt, jitter, remote, speed, name, pid,
lan, keyer, peer, peer_alive, peer_paired, peer_wpm, session, rtt, reason`, plus a nested `wan`
object: `enabled, state, paired, active, keying, direct, relay, code, key (public-key fingerprint),
peer_wpm, rtt, jitter, reason`. `remote` and `speed` follow whichever of LAN or WAN owns the line.

* `wpm` is the station setting; `speed` is the **effective** speed — the remote's measured speed while
  `remote` is true and it has keyed, otherwise `wpm`. Every readout uses `speed`.
* While `remote` is true the page disables its own speed slider and mode buttons (greyed via
  `.seg button:disabled, input[type=range]:disabled`), shows "speed and mode are set by the remote
  Keyer", and ignores stale input/click events, so a page that has not caught up cannot fight the
  Keyer.

The box always comes up as its own access point, `W1VE-Keyer` / `w1vekeyer` on 192.168.4.1, so the
page is reachable before it has ever joined a network. The access point is kept up even after the
station joins, as the way back in.

### Sidetone over `/ws`

There is no onboard audio: the browser is the only audio path, and losing the socket costs silence
and nothing else.

* One frame per tick: `[[t, down], ...]`, `t` from the firmware's own millisecond clock.
* The page maps that clock onto `AudioContext.currentTime` with a **50 ms** lead and schedules each
  edge at `anchor + t/1000`. One oscillator runs continuously and a 2 ms gain envelope gates it, so
  fast elements never restart a tone or jump phase.
* The anchor re-arms after a reboot (timestamps going backwards), after a long silence, or when a
  frame would land behind `now + 20 ms`. Re-anchoring the whole frame keeps intra-burst spacing
  exact; clamping individual edges shortened gaps and made 40 WPM sound rough.
* An edge not newer than the last one played is dropped, so a repeated frame cannot key twice.
* A suspended context is resumed on `visibilitychange` and by a 1.5 s watchdog that also releases a
  tone whose release edge was lost.
* The firmware keeps **four** subscribers and one slot per fd: the handshake path can be reached
  more than once for the same fd, and registering it twice wrote every frame down that socket twice
  (heard as a 1 ms click after each element). Frames go out `MSG_DONTWAIT`; a client that refuses
  40 frames in a row is dropped so a sleeping page cannot hold the HTTP server lock and wedge port 80.

---

## 7. W1VE v2 Player protocol (`main/remote.c`)

Byte-level contract: `.toolchain/ref/PROTOCOL.md` — the W1VE v2 spec for the ESP32 Player, which this
box mirrors — with the reference Player implementation beside it in `.toolchain/ref/`
(`player_transport.c`, `player_config.c`, `verify_live_pair.py`). That document also carries a
cross-language test vector: session key `8D 9D 3A 9F 13 24 65 19 …`, session id `8D 9D 3A 9F`, and the
first 16 HMAC bytes for a type 6 packet. It is the cheapest way to re-check any future change to the
pairing or envelope code.

Ports: discovery/broadcast **udp/4994**, transport **udp/4995**. No relay, no WireGuard, no tunnel;
same subnet only. Both sockets bind only to the station address, so the setup access point can never
carry W1VE traffic, and a Wi-Fi change closes them and drops the session.

| Type | Name | Payload |
| --- | --- | --- |
| 1 | Advertise | `"W1VE" ver=2 type=1 player_id[6] port u16BE name_len u8 name[] flags u8` (flags bit 0 = paired), broadcast to udp/4994 every 1500 ms from 4995 |
| 2 | Pair request | 34 B: `"W1VE" 2 2 player_id[6] keyer_id[6] nonce[16]` |
| 3 | Pair accept | 50 B: Player nonce in bytes 34..49 |
| 4 | CW edges | `epoch u16LE, count u16LE, count×(sequence u32, timestamp u32, state u8, flags u8, reserved u16)`, newest entry first, 12 B per entry, 1–4 entries |
| 6 | Ping | 8-byte timestamp |
| 7 | Pong | the identical 8 bytes |
| 8 | Unpair | empty |
| 10 | Query | 12 B (six non-zero bytes) to 4994 for Players that cannot hear broadcasts; answered with a unicast advertisement |

* **Pairing.** Both ends derive `SHA-256("W1VE-SESSION-v2" | player_id | keyer_id | initiator_nonce |
  responder_nonce)`; the first four bytes are the session id, the whole digest the HMAC key. The
  source endpoint is pinned and only that address/port is obeyed for the life of the session.
* **Envelope.** `"W1VE" ver type player_id[6] session_id u32 sequence u32 payload_len u8 payload
  tag[16]` (`payload_len` 0..64, total length 37 + payload_len), tag = first 16 bytes of HMAC-SHA-256
  over everything before it. A 64-packet replay window
  rejects duplicates, sequence 0 and anything more than 64 behind the high-water mark. A bad tag is
  dropped silently. Unpair clears the replay window first, so a replayed unpair cannot tear down a
  newer stream.
* **Lease.** A complete type 4 frame is the 1500 ms KEY lease; a line held with no frame for that
  long drops the session and releases KEY/PTT.
* **Liveness.** The Player pings once a second; three unanswered pings end the session. The station
  also clears the session after `SESSION_SILENT_MS` (3 s) with no packet at all and goes back to
  `Advertising` by itself — that is the "client dropped" behaviour, pinned by `pair_test.py`.
* **Edge safety.** Already-applied sequences are skipped; a timestamp regression or an epoch change
  ends the session; a sequence gap that ends with KEY down is unsafe and also ends it.
* **Timestamps.** Edge entries keep the Keyer's own clock. The Player maps them onto its own with an
  offset learned when a burst starts and held for the burst, re-learned only when a packet drifts
  more than 60 ms ahead or 200 ms behind that prediction. Stamping the *arrival* instead collapsed a
  history frame onto one instant and scattered late frames — on the page that is clean CW turning
  into clicks.

### How the remote speed is obtained (there is no speed field)

The protocol carries no WPM anywhere: `PROTOCOL.md` defines no speed field, and types 1, 2, 3, 4, 6,
7, 8 and 10 have nowhere to put one. The only source is the timing of the type 4 edges:

* A key-up completes an element. `held = up_timestamp − down_timestamp` on the **Keyer's** clock; the
  interval is accepted when it is between 8 and 400 ms.
* The shortest accepted element in a run is the dit: `wpm = 1200 / dit_ms`, clamped to 5..50. A pause
  of more than `SPEED_REMEASURE_MS` (2 s) starts a fresh baseline, so an operator changing speed is
  followed rather than pinned to the old value.
* The result is **held for the whole session** and exposed as `remote_status_t.peer_wpm` /
  `remote_wpm`. It used to expire a few seconds after the last element, which made the panel and the
  page fall back to the station speed and read as "the remote speed is wrong".
* **Consequence:** the remote speed appears only once the Keyer has actually sent CW. Until then the
  panel reads `REMOTE: -- WPM` and `/api/status` reports `speed` = the station `wpm` with
  `remote` true. This is by design, not a fault: with a fresh session the number shows up as soon as
  the first character is keyed.

---

## 8. Keyer engine and arbitration

* Modes: iambic A, iambic B, straight. Speed 5–50 WPM (`dit_us = 1_200_000 / wpm`), weight 25–75 %
  of each element period, PTT tail 500 ms.
* Paddle ISRs only set flags; a high-priority task does every transition in task context, arming the
  next deadline from `esp_timer_get_time()` so a slow tick never accumulates.
* `keyer_take_max_jitter_us()` reports the worst boundary lateness — the number that says whether
  keying is actually clean.
* Macros expand to dit/dah/3-unit/7-unit segments at the current speed and play from their own task
  with PTT asserted for the run; every boundary is measured against the timer and the sender carries a
  hard deadline, so an overrun releases KEY and PTT instead of leaving the rig keyed. A second press
  is refused while anything else owns the line.
* Arbitration, highest first: local paddle (no lease), local macro, remote edge / remote macro (1500 ms
  lease). `keyer_remote_edge()` returns false when a local paddle owns the line, and a remote edge
  arriving mid-element drops the session rather than fighting the operator. Every exit path — unpair,
  Wi-Fi loss, lease expiry, LAN switch off, reboot — leaves KEY and PTT released.
* `keyer_keytest()` drives the real pins from the station engine; it is how the whole chain (element
  machine → KEY pad → edge stream → browser tone) is proven with no Keyer at the other end.

---

## 9. Persistence (NVS)

Namespace `w1ve` throughout (`nvs` partition, 24 KB). Defaults are restored per key when absent.

| Key | Written by | Meaning |
| --- | --- | --- |
| `ssid`, `pass` | `net.c` | station credentials; the AP stays up as the way back in |
| `pname` | `remote.c` | Player display name (≤32 bytes), default `W1VE Keyer` |
| `lan` | `remote.c` | stored LAN switch, survives reboot |
| `ml0..ml5`, `mt0..mt5` | `macro.c` | macro label (≤10) and CW text (≤48) per slot |
| `wan_sk` | `wan.c` | WAN WireGuard private key (32-byte blob), generated once from the TRNG; never exposed by any API or log |
| `wan_relay`, `wan_code`, `wan_stun`, `wan_en` | `wan.c` | relay endpoint, pairing code, STUN server, WAN enable flag |

Keyer **speed, mode and weight are RAM only** and come back at defaults (20 WPM, Iambic A, 50 %)
after a reboot. The Player identity (`player_id`) is the station MAC, not stored.

---

## 10. Test and bench scripts

`.toolchain/` holds the working scripts and the captured logs they produced. They all talk to a live
board at its station address (`DEV = 192.168.88.168` in the scripts).

| Script | What it proves | Expected |
| --- | --- | --- |
| `.toolchain/pair_test.py` | pair → four pongs with RTT → silent drop after ~3 s → discovery query reports `paired=False` → re-pair accepted → unpair | prints `ALL PASS` |
| `.toolchain/edges_test.py` | speed inference end to end: a synthetic Keyer keys "ET" at 30 WPM on an exact timestamp grid, then idles 8 s, then unpairs | `peer_wpm=30 speed=30 remote=True local=20`, still 30 after the idle, `speed == wpm` after unpair, `ALL PASS` |
| `.toolchain/hold_session.py [secs]` | keeps a real session open (keys three dits at 24 WPM, then services pings) so a browser page can be inspected while a remote owns the line | prints `holding session …` / `closing` |
| `tools/capture.py` | serial log, boot banner, panel/LED/remote log lines | see the log lines below |
| `POST /api/led`, `POST /api/keytest` | the LED part and the audio chain, without waiting for a network event | `ok` |
| `make -C tests/host test` | the portable WAN core (crypto vs OpenSSL/RFC vectors, relay codec, inner UDP, STUN, jitter, edge safety) | `ALL PASS` (1421 checks) |
| `python3 tests/interop/run_interop.py` | the C WireGuard core interoperates with boringtun 0.7.1 both ways | `ALL PASS` |
| `tests/interop/rwk_keyer` + `rwk-relay` | the board keyed by a real `rwk-core` Keyer through the real relay (see WAN.md §11) | `PASS ... pongs` |

Useful log lines to grep for in a capture: `panel up: page 1 name/speed + 5 macros + Set, page 2
speed/LAN, 320x172`, `remote up: speed line REMOTE, speed slider locked`, `status LED: blue|green|
orange …`, `keyer up: dit=7 dah=4 key=3 ptt=0, 20 wpm`, and the reason strings (`Keyer went silent`,
`local paddle has the line`, `edge epoch changed`, `edge timestamp regressed`).

**Timing matters when writing these scripts.** The speed estimator measures the interval between a
key-down and its key-up, so an "element" is a *down-duration*. Sending `[dit, dit/3, …]` as down
durations produces 13 ms elements that legitimately measure above the 50 WPM clamp. Stamp edge
timestamps on an exact grid (`base + offset`) and send the packets as close to their slots as
possible; the device only needs monotonic timestamps and a plausible arrival time.

---

## 11. Traps already paid for

* **lwIP socket budget.** `CONFIG_LWIP_MAX_SOCKETS=20` and httpd `max_open_sockets = 10` are load
  bearing. At the default of 10, `accept()` failed with `ENFILE` and the web UI went dead for good
  while the box still answered pings; raising httpd's limit alone makes `httpd_start` fail with
  "max allowed 7".
* **`main/web.c` is large and was once truncated to 0 bytes** by a bulk edit. Back it up before
  rewriting it (`cp main/web.c .toolchain/web.c.bak`) and prefer small anchored edits.
* **Anchors must be unique.** `char body[768];` appears twice (status and scan), and the CSS lives in
  one long template string, so a "one occurrence" assertion is worth having — a stale anchor silently
  skipping an edit is how the `speed` field went missing for a build.
* **`REMOTE: -- WPM` is not a bug** — it means the Keyer has not keyed since pairing. §7 explains the
  measurement and why no speed field exists to read.
* **The panel and page must never show the station speed under a REMOTE label**; use
  `remote_status_t.active` and `peer_wpm`, and keep the LED, panel and page on the same flags.
* **Don't call LVGL from the keyer task**; use `lv_async_call` and let the UI task write labels.
* **The vendor's Arduino sample uses ESP32-S3 pin numbers** (45/47/38/39) that do not exist on the C6.
  `board.h` follows Waveshare's pin table for this SKU instead.
* **Panel orientation** is settled: MADCTL 0x28, inversion off, Y offset 34; MX (0x68) and MY (0xA8)
  both came up mirrored or upside down on this part.
* **GPIO8 is both the LED and the BOOT strap.** Driving it at runtime is safe; do not move the LED to
  an IMU interrupt pin.
* **HTTP is only up ~12–15 s after a reset**, and any serial tool resets the chip. Poll with a bounded
  loop instead of sleeping a fixed short time.

---

## 14. WAN transport (RWK relay + WireGuard)

The optional WAN path lets a remote W1VE Keyer key this station over the Internet. It is the Player
end of the desktop RWK WAN flow and is byte-compatible with the existing Linux/Windows RWK clients
and relay. Full design and the compatibility report are in [WAN.md](WAN.md); the essentials:

* **It is WireGuard, not a bespoke protocol.** RWK WAN carries CW inside a standard WireGuard tunnel
  (boringtun on the desktop). The ESP implements the WireGuard wire protocol for one peer in
  `main/rwk/rwk_wg.c` using mbedTLS (Curve25519, ChaCha20-Poly1305) plus a small BLAKE2s. There is
  **no** lwIP netif and no inner IP stack: the only inner traffic is one UDP flow, Client `10.6.0.1`
  ↔ Station `10.6.0.2` on port 5555, built and parsed by `rwk_inner.c`.
* **Rendezvous and relay** use the RWK relay's TCP framing (`rwk_frame.c`): `Pair`/`Paired` to find
  the peer, then either a direct hole-punched UDP path or `Register`/`Send`/`Recv` forwarding when
  the punch fails. The relay never sees plaintext.
* **The edge stream is identical to the LAN type-4 payload**, so `rwk_edge.c` — the shared CW edge
  applier with the epoch/sequence/timestamp safety rules, 1500 ms lease and dit speed inference —
  serves both transports. **CW timing is scheduled, not applied on arrival, on the WAN path.**
  `rwk_edge` anchors the sender's clock to local time plus a per-path buffer (direct 60 ms, relay
  120 ms) chosen once per burst, and hands each edge an absolute play time. The LAN path uses a
  zero buffer and applies immediately; the WAN path enqueues each edge and a dedicated playout task
  (`wan.c`, priority 6, above the network task, below the keyer) fires it at that instant. This is
  why CW stays clean over a bunching relay/DERP path: the network task never touches the key, so a
  burst of frames delivered together is still played with the sender's exact element spacing. A
  verified 38 WPM relay session fires every edge within ±1 ms of schedule.
* **Arbitration.** `rwk_lineclaim.c` is an atomic claim on the single remote slot: LAN and WAN
  cannot both key at once, and a local paddle still overrides either inside the keyer. The task runs
  at priority 3, below the keyer (20) and the LAN Player (5); it never touches the keying path
  except through the same non-blocking `keyer_remote_edge()` the LAN path uses.
* **Identity and secrecy.** The WireGuard private key is generated once from the hardware TRNG and
  stored in NVS (`wan_sk`). It is never returned by any API and never logged; only a short public-key
  fingerprint is shown (web page and, when WAN is idle, the panel). Flash is not encrypted on this
  board, so physical readout would expose it — a stated limitation.
* **Config.** The relay defaults to the suite's static relay (`107.172.138.103:9000`,
  `DEFAULT_WAN_RELAY` in the desktop `pairing.rs`) and is editable on the web page; an empty field
  restores the default. The pairing code defaults to a **random 9-digit number generated once per
  device** (`random_code` in `wan.c`) and persisted, so two boxes never share a default on a public
  relay; the operator can change it to any string shared with the remote Keyer, and clearing the
  field rolls a fresh random code. WAN stays off until the operator enables it, so nothing leaves
  the box unprompted.
* **The pairing code is not the key.** The code is the relay rendezvous secret both ends type; the
  key fingerprint shown on the glass and the page is this box's WireGuard public key, for the remote
  operator to verify. They are different things and the UI keeps them clearly separate.
* **UI.** The panel's page-2 button is a single transport toggle cycling **OFF → LAN → WAN → OFF**
  (the two are mutually exclusive); the state line under it shows what that transport is doing, and
  for WAN-idle it shows the key fingerprint. Back sits at the right. The speed line, the slider lock
  and the status LED follow whichever transport owns the line. The web page has one **Remote keyer**
  card with an Off/LAN/Internet selector that reveals the LAN (name, pair status) or WAN (pairing
  code — large, bold, with Copy — relay, disconnect, key fingerprint) controls; choosing one
  transport turns the other off, so they are never both on.

Traps specific to WAN:

* **ChaCha20-Poly1305 must be enabled in sdkconfig** (`CONFIG_MBEDTLS_CHACHA20_C`, `POLY1305_C`,
  `CHACHAPOLY_C`). They are in `sdkconfig.defaults`; a `fullclean` that regenerates `sdkconfig`
  without them makes the handshake fail at runtime, not build time.
* **BLAKE2s is not in mbedTLS** — it is `main/rwk/rwk_blake2s.c`, checked against RFC 7693 vectors in
  both the boot self-test and the host tests. Do not replace it with a SHA-256 call; WireGuard's KDF
  is BLAKE2s-specific.
* **X25519 is software** (~95 ms per scalar mult on the C6; the ECC accelerator is P-192/256 only), so
  a handshake costs a few hundred ms on a low-priority task. That is fine for a once-per-session
  event and must stay off the keying path.
* **Keep `rwk_edge.c` and `remote.c`'s edge handling in step.** They implement the same safety rules;
  `remote.c` predates the shared applier and still carries its own copy. A change to one must be
  mirrored in the other (or `remote.c` finally switched to `rwk_edge.c`).

## 12. Conventions in this codebase

* Comments explain **why**, especially where a value looks arbitrary (lead, lease, debounce, window,
  clamp). Keep that style; the numbers are the design.
* The firmware never grows a JSON parser or a template engine: form-encoded POSTs, one static HTML
  string with inline JS, `snprintf` for the one JSON response.
* The keyer engine and the panel are the things that must always work; network, page and sidetone are
  optional and must fail silently rather than block the keying path.
* Module boundaries are the structs in `include/`: a module publishes a `*_status_t` snapshot and
  hides its locking. `remote.c` and `keyer.c` each take their own mutex and return copies.
* Everything adjustable that is not a two-position switch belongs on the web page, not on the glass.
* **Keep this file honest.** It describes behaviour, not intentions: when a constant, endpoint, log
  line or flag meaning changes, change the matching paragraph here in the same edit. Where SPEC.md and
  CONTEXT.md disagree, the code in `main/` wins and one of the two documents is stale.

---

## 13. Open items

Known documentation drift: the last sentence of README.md still says "Macros are still to come", but
the macro slots, panel keys and `/api/macro*` endpoints are all implemented and in NVS.

From SPEC.md §9, still live: the Keyer advertisement (type 11) or a mirrored-state alternative for
display-side pairing, the remote macro trigger, the touch-square release policy (complete the
in-flight element vs stop immediately), and whether a real PTT output with a tail is wanted.

Not yet built anywhere: an explicit **speed report** in the protocol. A Keyer that told the Player its
WPM — a field in the ping payload or a small status message — would let the panel show a speed the
moment the session opens instead of after the first character. Until then §7's edge-timing inference
is the only mechanism, and the `-- WPM` placeholder is its honest output.
