## W1VE Touch Keyer

Station-side CW keyer firmware for the **Waveshare ESP32-C6-Touch-LCD-1.47**: a touch-screen
iambic keyer that owns the rig's KEY/PTT line, with a browser UI, LAN pairing to a W1VE Remote
Keyer, and an optional WAN link to a remote Keyer over the Internet through the RWK relay.

### Install

Flash from your browser (Chrome/Edge, USB): **https://w1ve.github.io/w1ve-keyer-suite/touch-keyer/**

Or download `w1ve-touch-keyer-8MB-merged.bin` from this release and write it at offset `0x0`:

```
esptool.py --chip esp32c6 write_flash 0x0 w1ve-touch-keyer-8MB-merged.bin
```

After flashing, the board comes up as its own access point **W1VE-Keyer** (password `w1vekeyer`)
at `192.168.4.1`; open that page to join it to your Wi-Fi.

### What's in this build

- Iambic A/B and straight keyer, 5–50 WPM, weight 25–75%, GPIO KEY/PTT with a PTT tail.
- 320×172 touch panel: name plate, live KEY/PTT, five macro keys, and a settings page with a
  single transport toggle (**OFF → LAN → WAN**).
- Browser UI on port 80: speed, mode, weight, macros, Wi-Fi join, WebAudio sidetone over `/ws`.
- **LAN Player** (W1VE v2 trusted-LAN protocol): a Remote Keyer on the LAN pairs and keys the rig.
- **WAN transport** (new): reach a remote Keyer over the Internet through the RWK relay, using the
  same WireGuard transport as the desktop suite. Direct hole-punched path when the NATs allow,
  relay fallback otherwise. A per-device random pairing code; the private key never leaves the box.
- Macros honor the remote keyer's speed during a session.

### Pairing code

Each device generates a random 9-digit pairing code on first boot. Give it to the remote Keyer
(both ends enter the same code). Change it to anything memorable on the web page's Remote keyer
card; clearing the field rolls a fresh random code.

### Documentation

Full operator and developer documentation is attached to this release and lives in the project:

- `README.md` — features, wiring, build and flash, first-boot checks.
- `WAN.md` — WAN architecture, the RWK/WireGuard protocol analysis, and the compatibility report.
- `CONTEXT.md` — the complete map of the firmware for developers.

### Compatibility

The WAN transport interoperates with the existing W1VE Keyer Suite relay (`rwk-relay`) and desktop
RWK clients; existing LAN behavior is unchanged.
