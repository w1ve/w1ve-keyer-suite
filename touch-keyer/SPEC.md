# W1VE Touch Keyer — ESP32-C6-Touch-LCD-1.47

Station-side CW keyer with a touch UI, paddles, macros, optional LAN pairing to a
W1VE Remote Keyer at the operator position.

Status: design agreed; no firmware written yet.

## 1. Role and topology

The box lives at the **station**, owns the rig's KEY/PTT line, and holds the paddles and
the macro set. It is usable with no network at all.

```
paddles (GPIO) ─┐
touch squares  ─┼─→ keyer engine (iambic A/B, straight, WPM, macros) ─→ arbiter ─→ KEY/PTT → rig
laptop edges   ─┘                                                          │
                                                                     status → display
                                                                     status → web page
```

* The operator upstairs runs **W1VE Remote Keyer** (client/operator role) on a laptop on the
  same LAN. He keys from there and fires **the station keyer's own macros**, so he never has to
  walk down to the shack.
* Remote keying is optional. If nothing is paired, or Wi-Fi is down, the station keyer behaves
  exactly as a stand-alone keyer.
* This box is the **Player** side of the W1VE v2 protocol — the same side as
  `w1ve-player-esp32` — so its transport, envelope, replay window and output lease are reused.

### Reused from `w1ve-player-esp32`

* `player_transport.c` — pairing, authenticated envelope, HMAC-SHA-256, 64-packet replay window,
  ping/pong, unpair.
* The **1500 ms output lease** and frame-refresh-while-keyed rules; release on Wi-Fi loss,
  unpair, config change, lease expiry.
* `player_config.c` (NVS), `player_wifi.c`, `player_log.c`, `player_json.c`, and the
  `player_web.c` browser UI for everything that does not belong on a 1.47" screen.

### New work

1. C6 board support: JD9853 LCD, AXS5106L touch, LVGL, backlight, battery ADC.
2. Keyer engine: paddle ISRs, debounce, iambic A/B + straight, WPM, macro store/playback.
3. Arbiter: one key line, several possible drivers (see §6).
4. Touch UI (§4).
5. Two small protocol additions (§7).

## 2. Hardware

ESP32-C6FH8 — single HP RISC-V core to 160 MHz, 512 KB HP SRAM, 8 MB in-package flash, **no
PSRAM**, no DAC. 1.47" IPS 172×320, JD9853, capacitance touch via AXS5106L.

### Onboard pin usage (from the Waveshare pin table, not the S3-derived Arduino sample)

| Function | GPIO |
| --- | --- |
| LCD SCK / SD SCK | 1 |
| LCD MOSI (LCD_SDA) / SD MOSI | 2 |
| SD MISO | 3 |
| SD CS | 4 |
| IMU INT1 / INT2 | 5 / 6 |
| BOOT strap | 8 |
| USB D- / D+ | 12 / 13 |
| LCD CS | 14 |
| LCD DC | 15 |
| UART0 TX / RX | 16 / 17 |
| Touch + IMU I2C SDA / SCL | 18 / 19 |
| Touch RST / INT | 20 / 21 |
| LCD RST | 22 |
| LCD backlight | 23 |
| BAT_ADC | 0 |

### Keyer pin assignment

| Signal | GPIO | Notes |
| --- | --- | --- |
| Paddle dit | 7 | free — documented as connected to nothing |
| Paddle dah | 4 | TF CS (card slot unused) |
| KEY out | 3 | TF MISO (card slot unused) |
| PTT out | 0 | BAT_ADC tap; divider is harmless for an output |

GPIO5 and GPIO6 are avoided: they are the QMI8658A interrupt outputs and the IMU can hold
them low after reset, which reads as a stuck paddle. The TF card slot is the cheaper thing
to give up.

Four pins, no sacrifices: **TF card slot and UART0 stay usable.** Sidetone is not on board
(§5), so there is no PWM pin and no amplifier.

Switches wire to GND with internal pull-ups. KEY/PTT drive an optocoupler or transistor — never
the rig key line directly.

## 3. Keyer engine

* Dedicated FreeRTOS task at high priority; element deadlines from a hardware timer, not from
  LVGL, not from a Wi-Fi callback.
* Paddle inputs: GPIO interrupt sets a flag; debounce ~5 ms; a release shorter than ~5 ms is
  ignored. **No minimum press time** — a dit at 40 WPM is 30 ms.
* Iambic A and B, plus straight-key mode. Squeeze = both paddles.
* WPM 5–50, adjustable from the touch slider, the web page, and (if enabled) the paired client.
* Text/macro generation and paddle keying share one edge timeline, so the arbiter, the display
  dots and the web sidetone all see one consistent stream.
* Macros: 8 slots, each an editable text string, played through the same engine at the current
  WPM. Fired by an on-screen button, the web page, or a paired client (§7).

## 4. On-device UI (320×172 landscape)

Black background, 8 px margins. Layout as built:

The panel is a name plate with five macro keys and a Set key that opens a second page. Anything
adjustable is on the web page instead, because a 172 px row of controls is worse than a browser at
the operating position.

```
┌──────────────────────────────────────────────────────┐
│ W1VE Remote Player/Keyer                       ●     │  title, red; ● KEY (red)
│ W1VE Maine   192.168.88.168                    ●     │  SSID and address, green; ● PTT (blue)
│ REMOTE: 40 WPM                                       │  REMOTE when a Keyer is live, else LOCAL
│ ┌──────────┐ ┌──────────┐ ┌──────────┐               │
│ │    CQ    │ │   599    │ │    TU    │               │  five blue keys + Set, 98×44, 7 apart
│ └──────────┘ └──────────┘ └──────────┘               │
│ ┌──────────┐ ┌──────────┐ ┌──────────┐               │
│ │    73    │ │   W1VE   │ │  Set...  │               │
│ └──────────┘ └──────────┘ └──────────┘               │
└──────────────────────────────────────────────────────┘
```

The speed line follows the source: while a Keyer holds a live session the panel shows that
Keyer's own measured speed (`REMOTE: NN WPM`); with no session it shows the station setting
(`LOCAL: NN WPM`).

### Page 2 (Set...)

Reached with the `Set...` key, and left with `Back`. Both pages carry the KEY and PTT dots, so a
macro going out is visible wherever the operator is.

* **Speed** — a 5–50 WPM slider that sets the station speed, and follows a change made from the
  web page while no finger is on it. While a Keyer holds a live session the remote owns the
  speed: the slider goes grey and stops taking touch, and page 1 reads `REMOTE: NN WPM` (or `REMOTE: -- WPM` until the Keyer has keyed), and the
  web page shows that same speed as its own big number, with its speed slider and mode
  buttons greyed and disabled. The measurement is held for the whole session, not for a
  few seconds after the last element.
* **LAN** — one switch that turns LAN advertising on and off. It shows the *stored* setting, so it
  stays honest with the station LAN down; turning it off drops any session immediately.
* **Advertising / Paired** — under the switch. `Advertising` while the box is up for pairing,
  `Paired` while a Keyer holds a session, `No network` when the station LAN is down, `LAN off`
  when the switch is off.
* A Keyer that stops answering for three seconds is dropped and the box returns to `Advertising`
  on its own, releasing KEY first; the next pair request is accepted normally.

### Status LED

The board's addressable RGB LED (WS2812, GPIO8, GRB order) repeats the transport state so it can
be read from across the shack:

| Colour | Meaning |
| --- | --- |
| Blue | only the setup access point is up |
| Green | joined the station Wi-Fi, advertising |
| Orange | a Keyer holds a session |

It is driven from the same two flags as the speed line and the page 2 state text, so the LED and
the glass cannot disagree, and it is written only when the state changes.

The same three colours are repeated on a dot at the right of the speed line, because the part may
not be fitted on every variant and is not always in sight from the operating position.
`POST /api/led` with `r=`, `g=` and `b=` drives the LED directly, which is how the part and its
wiring are proven without waiting for a network change.

## 4a. Web UI (port 80)

Served by `esp_http_server`; the page is a single static document plus a small API:

| Endpoint | Method | Purpose |
| --- | --- | --- |
| `/` | GET | the page |
| `/api/status` | GET | network, address, WPM, mode, KEY, PTT |
| `/api/wpm` | POST | `wpm=5..50` |
| `/api/mode` | POST | `mode=0..2` |
| `/api/weight` | POST | `weight=25..75` |
| `/api/macros` | GET | the six macro slots as JSON |
| `/api/macro` | POST | `idx=0..5&label=…&text=…`, saved to NVS |
| `/api/macro/play` | POST | `idx=0..5`, sends that slot as CW |
| `/api/led` | POST | `r=&g=&b=` 0..255, drives the status LED directly |
| `/api/scan` | GET | visible networks as JSON |
| `/api/join` | POST | `ssid=…&pass=…` |
| `/ws` | WebSocket | keying edges `{t,down}` for the sidetone |

The box comes up as its own access point, `W1VE-Keyer` / `w1vekeyer` on 192.168.4.1, so the
page is reachable before it has ever joined a network.

### Socket budget

`CONFIG_LWIP_MAX_SOCKETS` is raised to 20 in `sdkconfig.defaults`, and httpd is given
`max_open_sockets = 10`. The Player's two UDP sockets and every accepted HTTP connection draw
from the same lwIP pool: at the stock limit of 10, `accept()` started failing with `ENFILE`
(`httpd: httpd_accept_conn: error in accept (23)`) and the web UI stopped answering for good while
the box still answered pings. The default limit also caps httpd near 7 sockets, which is why the
old attempt to raise `max_open_sockets` alone made `httpd_start` fail with "max allowed 7".

### Status rail (right edge)

* Two 14 px dots, **KEY above PTT**, both always present so position carries the meaning as well
  as colour (colour-blind safe).
* Driven by the **post-arbiter line state** — what the rig is actually seeing, not what a source
  requested. Both can be on at once (PTT asserted, KEY up between elements).
* Updated on change from the keyer task via `lv_async_call`; the UI never writes key state.
* Optional extra state on the same rail: dim = idle, bright = asserted.

### Macro keys (two rows of three)

Five 98×44 blue keys and the Set key, three per row with a 7 px gutter, so a finger can land on
one key only.
Each shows its label; the labels and the CW text behind them are edited on the web page and kept
in NVS under the `w1ve` namespace (`ml0..5` and `mt0..5`).

* Panel defaults: CQ (`CQ DE W1VE`), 599 (`5NN,`), TU, 73 and W1VE. The sixth slot is kept in NVS
  and is edited and fired from the web page, which is where a sixth key would have been.
* A press expands the text at the current speed into key-down and key-up segments — dit, dah,
  three units between characters, seven between words — and plays them from its own task with
  PTT asserted for the run.
* A second press is refused while a macro plays, as is a press while the paddle or a remote Keyer
  owns the line.
* Every boundary is measured against `esp_timer_get_time()`, and the sender carries a hard
  deadline: if it overruns, it releases KEY and PTT instead of leaving the rig keyed.

### Pages

1. **Idle** — product icon, "W1VE Remote Player/Keyer", Wi-Fi status (SSID, RSSI), battery, a
   `Join Network` button that opens page 2 and **returns here when done**, plus `Pair` and
   `Macros` entries.
2. **Network** — scanned networks, saved networks, join/forget, back.
   *WPA passphrase entry on a 320 px keyboard is painful; the web page remains the recommended
   path, and the display reflects the result.*
3. **Player** — session state: Player name, whether the LAN transport is advertising, the paired
   Keyer's identity, the measured speed and round-trip time, and Unpair. Pairing itself is
   Keyer-initiated in v2, so the page and panel show state rather than offering a Player list.
4. **Operating** — WPM slider (finger-sized), keyer mode, macro buttons, Local/Remote indicator,
   source owner, link state/RTT.
5. **Macros** — six slots, fire and (for short edits) rename; full text editing via the web page.

### Display budget

512 KB SRAM, no PSRAM. LVGL partial buffer 320×32×2 ≈ 20 KB ×2, no full-screen animation,
status refresh ≤ 4 Hz, and the SPI flush never runs ahead of the keyer task.

## 5. Sidetone over the web page (no onboard audio)

The page generates the monitor tone with WebAudio, triggered over the WebSocket the player web
UI already runs.

* **Enable Sidetone button.** An `AudioContext` is created/resumed inside that click handler;
  without a gesture browsers keep it suspended. Once running, tones play for the life of the
  page. Re-check `ctx.state` on `visibilitychange` (iOS Safari suspends it when backgrounded)
  and show the state on the button.
* **Schedule, don't react.** The firmware posts every KEY transition as `[t, down]` in one
  frame per tick (`[[t,down],...]`), timestamped from its own millisecond clock. The page maps
  that clock onto `ctx.currentTime` with a 50 ms anchor and schedules each edge at
  `anchor + t/1000`, so browser timer jitter and one late frame cannot smear the elements. One
  oscillator runs continuously and a gain envelope gates it (2 ms ramps), so fast elements never
  restart a tone or jump phase. The anchor re-arms after a reboot (timestamps going backwards),
  after a long silence, or when a frame would land behind `now + 20 ms`: re-anchoring the whole
  frame keeps intra-burst spacing exact, where clamping edges one at a time shortened the gaps
  and made 40 WPM sound rough. An edge whose `t` is not newer than the last one already played is
  dropped, so a repeated frame can never key the tone a second time. A suspended `AudioContext`
  is resumed on `visibilitychange` and by a 1.5 s watchdog, which also releases a tone whose
  release edge was lost; the readout under the button shows the live lead or says `suspended`.
* **One socket, one slot.** The firmware keeps four `/ws` subscribers and writes each frame once
  per subscribed socket. A socket may hold exactly one slot: the handshake path can be reached
  more than once for the same fd, and registering it twice wrote every edge frame down that socket
  twice — the page heard a 1 ms click after each element. Frames go out with `MSG_DONTWAIT`, and
  a client that refuses 40 frames in a row is dropped, so a page that stops draining cannot hold
  the HTTP server lock and wedge port 80.
* **Test key.** `POST /api/keytest` (page button) runs the element machine for a few dits with
  the pins live, so the whole chain — element machine, KEY pad, edge stream, browser tone — can
  be proven with no Keyer attached. The serial log reports how many times the KEY pad actually
  followed, which is how the GPIO read-back bug was found.

## 6. Keying arbitration

One key line, four possible drivers:

| Source | Lease | Priority |
| --- | --- | --- |
| Local paddle (GPIO or touch squares) | none | highest — the operator is standing there |
| Local macro (screen or web) | none | high |
| Remote macro trigger | 1500 ms refresh | medium |
| Remote edge frame | 1500 ms refresh | medium |

* Local paddle input aborts a remote macro in progress and takes the line.
* Remote sources are masked while the local keyer is transmitting — the mirror of the engine's
  existing two-source `keying_arbiter` (remote replay vs local logger), here applied to the
  station's own keyer.
* Every exit path (unpair, Wi-Fi loss, lease expiry, reboot, config change) leaves KEY and PTT
  released.

## 7. Remote link (W1VE v2, LAN only)

The station is the **Player** in the suite's trusted-LAN protocol (`w1ve-player-esp32/PROTOCOL.md`):
a Keyer browses for Players, pairs to this box, and the box drives KEY and PTT. No relay, no
WireGuard, no tunnel — one unauthenticated nonce handshake and then HMAC-authenticated UDP on the
same subnet.

**Implemented today.** This station is a complete v2 Player.

* **Advertisement** — type 1 from udp/4995 to the subnet broadcast on udp/4994 every 1500 ms:
  `"W1VE" | version u8 (2) | type u8 (1) | player_id[6] | port u16 BE | name_len u8 | name[] |
  flags u8`. `player_id` is the station MAC and the name is the display name set on the page, so a
  Keyer's Player list shows this box by name. A Keyer that cannot hear broadcasts sends a type 10
  query to udp/4994 and gets the same advertisement back, unicast.
* **Pairing** — the Keyer sends type 2 (its six-byte identity and a random 16-byte nonce) to
  udp/4995; the Player answers type 3 with its own nonce. Both derive
  `SHA-256("W1VE-SESSION-v2" | player_id | keyer_id | initiator_nonce | responder_nonce)`: the
  first four bytes are the session id, the whole digest the HMAC-SHA-256 key. The source endpoint
  is pinned, and only that address and port are obeyed for the life of the session.
* **Session envelope** — `"W1VE" | version | type | player_id[6] | session_id u32 | sequence u32 |
  payload_len u8 | payload | tag[16]`, the tag being the first 16 bytes of HMAC-SHA-256 over
  everything before it. A 64-packet replay window rejects duplicates, sequence zero and anything
  more than 64 behind the high-water mark; a bad tag is dropped silently.
* **CW edges** — type 4 payload `epoch u16 LE | count u16 LE | count x (sequence u32, timestamp
  u32, state u8, flags u8, reserved u16)`, newest entry first. Entries are replayed oldest to
  newest; already-applied sequences are skipped, a timestamp regression or an epoch change ends
  the session, and a sequence gap that ends with KEY down is unsafe and also ends it. A **complete
  frame is the lease**: only valid type 4 frames refresh it, and a line held down with no frame
  for 1500 ms drops the session and releases KEY/PTT.

  Entry timestamps are on the Keyer's own clock. The Player maps them onto its own with an offset
  learned when a burst starts and held for its duration, re-learned only when a packet drifts more
  than 60 ms ahead or 200 ms behind that prediction. Every transition therefore reaches the
  sidetone carrying the instant the operator keyed it, whether it arrived inside a history frame
  of up to four entries or alone. Stamping the moment a packet was handled — which is what the
  Player used to do — collapsed a whole history frame onto a single millisecond and scattered a
  late run of frames at random; on the page that is clean CW turning into clicks.
* **Liveness** — the Player pings (type 6) once a second with an eight-byte timestamp and expects
  the identical payload back as type 7; three unanswered pings end the session. Type 8 unpairs at
  once, after clearing the same replay window.
* **Binding** — both sockets bind only to the station address, so the setup access point can never
  carry W1VE traffic; a Wi-Fi change closes them and drops the session.

**Arbitration is unchanged** (§6): the local paddle outranks remote edges, and if the local keyer
is mid-element when an edge arrives the session is dropped rather than fighting the operator.
Every exit path releases KEY and PTT.

**Panel** shows the paired Keyer's identity and the speed measured from that Keyer's own edge
timestamps (1200 ms divided by the shortest dit in the last 5 s), or the Player name while
advertising.

**Still to add.** A remote macro trigger, and a Keyer advertisement for display-side pairing (§9).

## 8. Build order

1. **C6 BSP + LVGL** — LCD, touch, backlight, battery, idle page with branding and Wi-Fi status.
2. **Keyer engine** — paddles, debounce, iambic, WPM, KEY/PTT out, macro playback, status dots.
3. **Touch UI** — pages 1–5, touch square paddles, WPM slider.
4. **Web page** — config/logs carried over from the player, plus the sidetone scheduler.
5. **Remote link** — W1VE v2 transport behind the arbiter: advertise, pair, receive edges,
   lease, unpair. Local keying must keep working with the socket dead.
6. **Protocol additions** — macro trigger, and the keyer advertisement or the mirrored-state
   alternative.

Steps 1–4 are a complete, shippable station keyer. Step 5 makes it a Player. Step 6 is the
remote-macro capability.

## 9. Open decisions

1. **Keyer advertisement (type 11)** — v2 Players never browse, so listing remote keyers on the
   panel needs an added message; until then pairing stays Keyer-initiated and the display mirrors
   the session.
2. **Touch square release** — complete the in-flight element (default) or stop immediately?
3. **PTT** — drive a real PTT output with a tail, or key-only with the rig handling its own
   sequencing? (Dropping PTT saves a pin.)
4. **Macro storage** — NVS text slots (simple) or allow a small SD-card file of macros?
