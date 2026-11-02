# W1VE Keyer Suite

A suite of CW (Morse code) keyer applications for amateur radio: a standalone keyer, a
remote keyer pair (operator + radio-site player), a remote keyer router, and an ESP32 radio-site
player. Desktop apps run on Windows, macOS, Linux, and Raspberry Pi.

Pick the application you need, download the file for your platform, and follow the instructions
for that platform below.

---

## Which application do I want?

| Application | What it does |
|---|---|
| **W1VE Keyer** | Standalone keyer at your operating position: paddle/straight key, sidetone, logger. No network. |
| **W1VE Remote Keyer** | Operator side of a remote pair. Keys a Player at the radio site over the network. |
| **W1VE Remote Keyer Player** | Radio-site side. Receives keying and keys the radio. Pairs with W1VE Remote Keyer. |
| **W1VE Remote Keyer Router** | Operator with ham-router forwarding and Flex support. Good for portable operation. |
| **W1VE Remote Keyer Router Player** | Compact radio-site monitor that pairs with the Router. |
| **W1VE ESP32 Player** | A low-cost ESP32-S3 board acting as a radio-site Player. Keys the radio over GPIO. Pairs with W1VE Remote Keyer or the Router. |
| **RWK relay** | Optional server that lets an operator and player find each other over the internet (WAN). Most LAN users don't need it. |

---

## Download the right file

Each application has a build per platform. File names start with the binary name:

| Application | File starts with |
|---|---|
| W1VE Keyer | `w1ve-keyer` |
| W1VE Remote Keyer | `w1ve-remote-keyer` |
| W1VE Remote Keyer Player | `w1ve-remote-keyer-player` |
| W1VE Remote Keyer Router | `w1ve-krr` |
| W1VE Remote Keyer Router Player | `w1ve-krp` |

Platform suffixes:

- **Windows:** `...-x64.exe` or the `.exe` under the Windows assets
- **macOS (Intel or Apple Silicon):** `...-universal.dmg`
- **Linux desktop:** `..._amd64.deb` or `...-x86_64.AppImage`
- **Raspberry Pi (64-bit):** `...-arm64.deb` or `...-arm64.AppImage`

---

## Windows

1. Download the `.exe` for the application you want.
2. Double-click it to run. There is no installer; the `.exe` is the whole app.
3. On first launch, Windows SmartScreen may warn that the publisher is unknown (these v0.1
   builds are not code-signed). Click **More info**, then **Run anyway**.

To uninstall, just delete the `.exe`.

---

## macOS (Intel and Apple Silicon)

The `.dmg` is universal: the same file runs on both Intel and Apple Silicon Macs.

1. Download the `...-universal.dmg` for the application you want.
2. Double-click the `.dmg` to open it, then drag the app into your **Applications** folder.
3. **First launch (important):** these v0.1 builds are not yet notarized, so macOS Gatekeeper
   will refuse a normal double-click. Do this once:
   - In **Applications**, right-click (or Control-click) the app and choose **Open**.
   - In the dialog, click **Open** again.
   - If macOS still blocks it, open **System Settings -> Privacy & Security**, scroll to the
     Security section, and click **Open Anyway** next to the app's name, then launch it again.
   After the first successful open, it launches normally.

Signed and notarized builds are planned so this step will not be needed in a future release.

---

## Linux (desktop, 64-bit x86)

Two formats are provided. Use whichever suits your distribution.

### .deb (Debian, Ubuntu, and derivatives)

```sh
sudo apt install ./w1ve-keyer_0.1.0_amd64.deb      # use the file you downloaded
```

Then launch it from your applications menu or by running its command name
(e.g. `w1ve-keyer`).

### .AppImage (any distribution, no install)

```sh
chmod +x ./w1ve-keyer_0.1.0_amd64.AppImage
./w1ve-keyer_0.1.0_amd64.AppImage
```

If the AppImage does not start, install FUSE (`sudo apt install libfuse2` on Debian/Ubuntu).

Serial keying needs permission to access serial ports. Add yourself to the `dialout` group once,
then log out and back in:

```sh
sudo usermod -aG dialout "$USER"
```

---

## Raspberry Pi (64-bit Raspberry Pi OS)

Use the `-arm64` files. Raspberry Pi OS is Debian-based, so the `.deb` is the simplest:

```sh
sudo apt install ./w1ve-keyer_0.1.0-arm64.deb      # use the file you downloaded
```

An `-arm64.AppImage` is also provided for other 64-bit ARM distributions. These are 64-bit
builds; use a 64-bit Raspberry Pi OS on a Pi 3, 4, 5, or newer. As on desktop Linux, add yourself
to the `dialout` group for serial keying.

---

## W1VE ESP32 Player (firmware)

The ESP32 Player turns an ESP32-S3 board into a radio-site Player that keys the radio over GPIO.

### Easiest: flash from your browser

1. Connect the ESP32-S3 to your computer with a USB **data** cable.
2. In **Chrome or Edge** (Web Serial is required; Safari and Firefox will not work), open the
   flasher page:

   **https://w1ve.github.io/w1ve-keyer-suite/**

3. Pick the card matching your board's flash size (4 MB, 8 MB, or 16 MB) and click **Install**.
4. Choose the board's serial port. If none appears, hold **BOOT**, tap **RESET**, release
   **BOOT**, and retry.
5. After flashing, the board reboots into its setup Wi-Fi access point `W1VE-Player-XXXX`.
   Join it (the setup password prints on the serial console), browse to `http://192.168.4.1`,
   and configure your home Wi-Fi and the KEY/PTT GPIO pins.

### Command line

Download the merged image for your flash size (`w1ve-esp32-player-8MB-merged.bin`, etc.) and
flash it with esptool:

```sh
pip install esptool
esptool.py --chip esp32s3 write_flash 0x0 w1ve-esp32-player-8MB-merged.bin
```

Not sure of your flash size? Start with the smaller image; flashing a layout larger than the
physical flash fails safely.

Safety: KEY and PTT start inactive and are released on Wi-Fi loss, unpair, or lost link. Do not
connect a radio key line directly to an ESP32 pin; use an optocoupler, transistor, or rated
relay interface.

---

## RWK relay (optional WAN server)

Most users operating on the same LAN do **not** need the relay. It is only needed to connect an
operator and a player across the internet. Run it on a public host with Docker:

```sh
docker run -d --name rwk-relay -p 9000:9000 -p 8080:8080 \
  -e RWK_ADMIN_PASSWORD=change-me \
  -e RWK_JWT_SECRET=$(openssl rand -hex 32) \
  ghcr.io/w1ve/rwk-relay:latest
```

- Port **9000** is the relay protocol; it must be reachable from the internet.
- Port **8080** is the dashboard/API; put a TLS proxy (such as Cloudflare) in front of it.

The image is multi-architecture (amd64 and arm64), so it runs on a regular VPS or an ARM host.

---

## Getting started after install

- **Standalone W1VE Keyer:** open it, pick your serial ports on the Connections tab, and key.
- **Remote pair (Remote Keyer / Router + a Player):** start the Player at the radio site first,
  then open the operator app, browse the LAN Player list, and select your Player to pair. The
  operator app shows its pairing panel before the operating controls by design: connect first,
  then operate.
- **ESP32 Player:** once configured and on your Wi-Fi, it appears in the operator's LAN Player
  list as **W1VE ESP32 Player**. Select it to pair.

---

## Notes for this release (v0.1.0)

- First public release. Desktop apps for Windows, macOS (universal), Linux, and Raspberry Pi;
  ESP32-S3 firmware for 4 MB / 8 MB / 16 MB boards; and the relay Docker image.
- macOS and Windows builds are **not yet code-signed**, so you will see a one-time security
  prompt on first launch (see the platform instructions above). Signing is planned.
- Android for the two operator apps and iOS are not included in this release.
- Verify your radio keying setup carefully before connecting to a live rig.
