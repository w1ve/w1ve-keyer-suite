# W1VE Touch Keyer — station keyer firmware

**Version 0.1.1**

Firmware for the **Waveshare ESP32-C6-Touch-LCD-1.47**: a station-side CW keyer with a
touch UI, paddles, a LAN link to a W1VE Remote Keyer at the operator position, and an optional
WAN link to a remote Keyer over the Internet through the RWK relay.

Design and roadmap: [SPEC.md](SPEC.md).

## What this slice does

* 320×172 landscape panel used as a name plate: the title in red, the SSID and address in green,
  then `REMOTE: NN WPM` while a Keyer holds a live session (its own measured speed) or
  `LOCAL: NN WPM`, KEY/PTT dots on the right edge, and five blue macro keys plus a `Set...` key
  that opens the settings page. Everything else lives on the web page.
* **Status rail** on the right edge: red dot when the KEY line is down, blue dot when PTT is
  down. Both reflect the real post-arbiter line state.
* **Macro keys**: five blue keys on the panel beside `Set...`, two rows of three with a 7 px
  gutter so a press can only land on one. Labels and CW text are edited on the web page and stored in NVS; the
  defaults are CQ (`CQ DE W1VE`), 599 (`5NN,`), TU, 73, W1VE and an empty sixth slot. A key
  refuses a press while the line is busy, and a sender that overruns its own length releases KEY
  and PTT rather than leaving the rig keyed.
* **Settings page** (`Set...`): a 5–50 WPM slider, one LAN switch that turns advertising on and
  off, and the transport state under it — `Advertising`, `Paired`, `No network` or `LAN off`. A
  Keyer that goes silent for three seconds is dropped, KEY is released and the box advertises
  again by itself; the switch shows the stored setting, so it survives a reboot along with the
  Player name, station SSID and passphrase in NVS.
* **Status LED**: the onboard WS2812 (GPIO8) shows where the network stands — blue with only
  the setup access point up, green once joined to the station Wi-Fi, orange while a Keyer holds
  a session. The same three colours appear on a dot beside the speed line, and `POST /api/led`
  with `r=`, `g=`, `b=` drives the part directly for a wiring check.
* **Keyer engine**: iambic A, iambic B and straight; 5–50 WPM slider, weight 25–75 %;
  GPIO paddles with debounce; KEY and PTT outputs with a 500 ms PTT tail.
* **Access point by default**: the box always comes up as `W1VE-Keyer` (password
  `w1vekeyer`, address 192.168.4.1) so it can be reached before it has ever seen a network.
  The address is in the page header; the panel line under the product name belongs to the
  paired keyer.
* **Web UI** on port 80: live KEY/PTT state, speed 5–50 WPM (the remote's while it holds the session, read-only), weight 25–75 % (50 % centred),
  macro labels and text (with Send),
  mode (Iambic A, Iambic B, straight), and network scan/join with a passphrase field. Credentials persist to NVS

* **Player card on the page**: the display name browsing Keyers see, the LAN transport on/off
  switch, the paired Keyer's identity and round-trip time, Unpair, and the reason the last session
  ended.
  (`w1ve` / `ssid`, `pass`); the access point stays up as the way back in.
* **Sidetone over WebSocket**: `/ws` streams keying edges as `[[t,down],...]`, and the page
  schedules them 50 ms ahead on the firmware's own clock, so browser timer jitter cannot smear
  the elements. A single oscillator runs throughout and a 2 ms gain envelope keys it, which is
  what keeps 40 WPM elements clean. The anchor re-arms after a reboot or a long pause, edges that
  are not newer than the last one played are dropped, and a suspended context is resumed when the
  tab comes forward. The line under the button shows the live lead, or `suspended` when the
  browser has stopped audio. **Test key** keys the station engine from the page, so the whole
  audio path can be proven without a Keyer at the other end. 400-1000 Hz with a volume control; a
  tone that misses its release lets go on its own. The browser is the only audio path; losing the
  socket costs silence and nothing else.

* **W1VE v2 Player, LAN only**: this box is the *Player* in the suite's trusted-LAN protocol
  (`w1ve-player-esp32/PROTOCOL.md`). It advertises on udp/4994 every 1.5 s from udp/4995 with its
  station MAC and the name set on the page; a W1VE Remote Keyer browses that list, pairs to it
  (nonce handshake, SHA-256 session key, HMAC-SHA-256 envelope) and its type 4 edge frames drive
  the rig. Every entry keeps the Keyer's own timestamp, mapped onto the station clock with an
  offset learned per burst, so a history frame of up to four edges reaches the sidetone with the
  spacing it was keyed at. A complete frame is the 1500 ms KEY lease, three missed pings end the
  session, a bad tag or replayed packet is dropped, and the local paddle always outranks the
  remote. No relay, no WireGuard, no tunnel: same subnet, nothing to configure.

* **WAN link over the Internet (optional)**: the box can also be keyed by a remote W1VE Keyer
  that is not on the LAN, through the RWK relay the desktop suite uses. This is standard
  WireGuard (not a bespoke protocol): the box pairs with the Keyer through the relay using a
  shared code, tries a direct hole-punched path and falls back to relaying, then carries the same
  CW edges inside the tunnel. WAN is off until enabled and is mutually exclusive with a LAN
  session — whichever Keyer arrives first holds the line, and a local paddle still overrides both.
  Configure it on the web page's **Remote keyer** card (choose **Internet**): the relay (defaults
  to the suite's public relay, editable; empty restores the default) and a **pairing code**. See
  [WAN.md](WAN.md).

### Pairing code

The pairing code is a short shared secret that the relay uses to introduce the two ends to each
other: this station and the remote Keyer both enter the **same** code, and the relay matches them
and hands each the other's WireGuard key. It is the one thing you have to coordinate with the
remote operator; nothing else about the connection is configured by hand.

* Each device generates a **random 9-digit code** on first boot and keeps it across reboots, so two
  stations never share a default on a public relay. You can change it to anything memorable (your
  callsign, e.g. `W1VE-1`) — any string up to 32 characters works.
* Set it on the web page's **Remote keyer** card (Internet mode). Clearing the field and saving
  rolls a fresh random code. Clicking **Copy** puts it on the clipboard to paste into the remote
  Keyer.
* The code is only a rendezvous key, so treat it like a meeting-room name: anyone who knows it and
  is pointed at the same relay at the same moment could attempt to pair, so pick something not
  trivially guessable and change it if you suspect it has leaked.

The long value shown as **this station's key fingerprint** (e.g. `1260 e953 86b8`) is *not* the
pairing code. It is a short form of this box's WireGuard public key, shown only so the remote
operator can confirm they are connected to the right station. The private key never leaves the
device and is never displayed.

## Wiring

| Function | ESP32-C6 pin | Notes |
| --- | --- | --- |
| Dit paddle | GPIO7 | to GND, internal pull-up; connected to nothing else on the board |
| Dah paddle | GPIO4 | to GND, internal pull-up; TF-card CS, the slot is unused |
| KEY out | GPIO3 | active high by default; TF-card MISO, the slot is unused |
| PTT out | GPIO0 | active high by default; BAT_ADC tap, the divider is harmless here |

GPIO5 and GPIO6 are deliberately **not** used: they are the QMI8658A interrupt outputs, and
the IMU can hold them low after reset, which would look like a permanently pressed paddle.
The TF card slot is sacrificed instead — this product has no use for it.

Paddle common goes to GND. Never connect a rig key line directly to an ESP32 pin.

`KEYER_KEY_ACTIVE_HIGH` / `KEYER_PTT_ACTIVE_HIGH` in `main/include/board.h` invert the
outputs for an inverting interface.

Display and touch are wired internally on this board (SPI: SCK 1, MOSI 2, CS 14, DC 15,
RST 22, BL 23; I2C touch: SDA 18, SCL 19, RST 20, INT 21 at 0x63).

## Build

The toolchain is project-local, so nothing outside this folder is modified:

```bash
tools/setup-idf.sh                 # ESP-IDF v5.3.4 + esp32c6 toolchain (~2 GB, one time)
tools/idf.sh idf.py set-target esp32c6
tools/idf.sh idf.py build
```

## Flash

```bash
tools/idf.sh idf.py -p /dev/ttyACM0 flash monitor
```

The C6 enumerates as a USB CDC device (`/dev/ttyACM0`); if the port is not detected, hold
BOOT (GPIO8) while plugging in.

## First-boot checks

1. **Display.** Expect the idle page. If the image is negative, set `LCD_INVERT_COLOR` to 0
   in `board.h`; if red and blue are swapped, change `LCD_MADCTL` to `0x08`.
2. **Touch.** The serial log prints `raw (x,y) -> (x,y)` for the first touches. If the
   coordinates are transposed or mirrored, set `TOUCH_SWAP_XY`, `TOUCH_MIRROR_X` or
   `TOUCH_MIRROR_Y` in `board.h` to match.
3. **Key line.** Put a meter or scope on GPIO3 (KEY) and GPIO0 (PTT) and confirm the dots and
   the pins agree: a dit should be 1200/WPM milliseconds.
4. **Paddles.** Dot and dash contacts to GND on GPIO7 and GPIO4; held paddles should repeat,
   both together should squeeze.

## Layout

```
main/
  app_main.c        boot sequence and the LVGL loop
  board_display.c   SPI, JD9853 panel init, LVGL display driver
  board_touch.c     AXS5106L I2C touch + LVGL pointer device
  keyer.c           iambic engine, paddles, KEY/PTT outputs, PTT tail
  ui.c              idle page, status rail, touch paddles, speed and mode labels
  net.c             Wi-Fi station and access point, credentials from NVS
  remote.c          W1VE v2 LAN Player: advertisement, pairing, HMAC session, CW edges
  wan.c             WAN transport: RWK relay rendezvous + WireGuard, feeds the edge applier
  rwk/              portable transport core (WireGuard, relay framing, crypto, edge applier)
  include/board.h   pin map and geometry
```

Tests for the WAN core run on the build host, not the board:

```bash
make -C tests/host test            # crypto vs OpenSSL/RFC vectors, codecs, edge safety rules
python3 tests/interop/run_interop.py   # the C WireGuard core vs boringtun, both roles
```
