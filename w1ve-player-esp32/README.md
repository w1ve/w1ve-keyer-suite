# W1VE ESP32 Player

The **W1VE ESP32 Player** is a LAN-only ESP32-S3 radio-site Player for W1VE Remote Keyer. It receives authenticated CW frames from a selected Keyer, drives configurable KEY/PTT GPIO outputs, reports state through its browser UI, and does not host WinKeyer, paddle, relay, port-forwarding, or WAN traffic.

```text
W1VE Remote Keyer or Router ── authenticated LAN UDP ── W1VE ESP32 Player ── KEY/PTT GPIO ── radio
                                                                  │
                                                                  ├─ RGB activity LED
                                                                  └─ WebSocket ── status and log UI
```

## Discovery and pairing

The Player advertises itself every 1.5 seconds on the home LAN. A W1VE Remote Keyer or W1VE Remote Keyer Router browses the available Player list, chooses this Player, and initiates pairing. The Player never browses, selects, or stores a Keyer endpoint. Raw W1VE discovery, pairing, and session sockets bind only to the joined station subnet. The always-available setup access point remains for configuration, but an AP-side device cannot discover, pair with, or key this Player.

When LAN equipment suppresses broadcasts, a Keyer can send a type 10 discovery query to the Player. The Player replies with its ordinary advertisement. Discovery only identifies a candidate endpoint; pairing remains Keyer-initiated.

The ESP32 Player transport has no pairing code, PIN, or persistent session secret. It is designed for a trusted LAN. Session nonces, endpoint pinning, authenticated envelopes, and replay protection reject unrelated traffic after pairing, but a device that can observe or interfere with the initial LAN handshake can interfere with the session. Do not expose this transport to an untrusted network.

See [PROTOCOL.md](PROTOCOL.md) for the W1VE v2 byte-level contract.

## First boot

1. Power the ESP32-S3.
2. Join `W1VE-Player-XXXX` using the setup password printed on the serial console.
3. Browse to `http://192.168.4.1`.
4. Select the home Wi-Fi network, enter its password, and choose safe KEY/PTT GPIOs and an optional RGB LED GPIO.
5. Save the configuration. The Player stores it in NVS and joins the home LAN.
6. In W1VE Remote Keyer or W1VE Remote Keyer Router, browse the LAN Player list and choose this **W1VE ESP32 Player**.

If joining the saved Wi-Fi fails, the setup access point stays available. Use the hardware reset procedure only when a full factory reset is required.

## GPIO and activity safety

The default generic ESP32-S3 profile uses GPIO 4 for KEY, GPIO 5 for PTT, and GPIO 48 for an optional WS2812-compatible RGB LED. The firmware permits only its board-profile safe list by default. KEY, PTT, and an enabled RGB output must use distinct pins.

KEY and PTT start inactive. The Player releases them on configuration changes, Wi-Fi loss, unpair, failed link liveness, and an output-lease timeout. Output polarity is configurable for an optocoupler, transistor, or rated relay interface. Do not connect a radio key line directly to an ESP32 pin.

The selected Keyer repeats its latest complete CW frame while KEY is asserted. The Player imposes a 1500 ms lease on that asserted state. A valid complete frame refreshes the lease; pings and pongs do not. If frame refresh stops, the Player unpairs and releases KEY/PTT.

Configured GPIO outputs and the optional RGB LED are the activity confirmation:

- **Red:** no home Wi-Fi connection.
- **Blue:** home Wi-Fi connected and waiting for a Keyer.
- **Green:** paired with a Keyer.
- **White:** accepted KEY down.

## Browser UI and logs

The web UI is titled **W1VE ESP32 Player**. It configures Wi-Fi, GPIO polarity, PTT tail, the RGB LED, and browser log streaming. It shows Player availability, the paired Keyer endpoint, link state, KEY/PTT state, and the disconnect reason.

The Player has no browser keying controls. Browser status does not establish a session or drive radio outputs. The log switch controls browser streaming and the stored runtime log display. Important configuration and fail-safe events also remain available on the serial console.

## Bluetooth

ESP32-S3 supports BLE, not Bluetooth Classic. This project reserves BLE for possible later Wi-Fi provisioning and a secured GATT transport. Wi-Fi LAN is the current operating transport.

## Build

ESP-IDF 6.1 is installed on the development machine under `E:\Espressif`.

```powershell
$env:IDF_TOOLS_PATH = 'E:\Espressif'
$env:IDF_PYTHON_ENV_PATH = 'E:\Espressif\python_env\idf6.1_py3.13_env'
& 'E:\Espressif\frameworks\esp-idf-v6.1\export.ps1'

idf.py set-target esp32s3
idf.py build
```
