#!/usr/bin/env python3
"""Hold the serial port open, hard-reset the chip through it, print what it says.

The ESP32-C6's native USB port maps RTS to EN, so we can reset from the same handle we
are reading from (no second process, no lost boot banner). DTR is kept deasserted so the
chip boots the application instead of the ROM downloader.

Pass "noreset" as a fourth argument to attach to a board that is already running.

    tools/idf.sh python tools/capture.py [port] [seconds] [noreset]
"""

import sys
import time

import serial


def main() -> int:
    port = sys.argv[1] if len(sys.argv) > 1 else "/dev/ttyACM0"
    seconds = float(sys.argv[2]) if len(sys.argv) > 2 else 15.0
    noreset = len(sys.argv) > 3 and sys.argv[3] == "noreset"

    try:
        link = serial.Serial(port, 115200, timeout=0.2)
    except Exception as exc:  # noqa: BLE001 - report and exit, this is a bench script
        print(f"cannot open {port}: {exc}", file=sys.stderr)
        return 1

    if not noreset:
        link.setDTR(False)      # IO0 high: normal boot
        link.setRTS(True)       # EN low: hold in reset
        time.sleep(0.15)
        link.reset_input_buffer()
        link.setRTS(False)      # release: boot

    total = 0
    deadline = time.time() + seconds
    while time.time() < deadline:
        chunk = link.read(8192)
        if chunk:
            total += len(chunk)
            # Stream as it arrives: a redirected stdout is block-buffered, and a capture
            # is useless if the log only shows up once the run has finished.
            sys.stdout.write(
                chunk.decode("utf-8", "replace").replace("\r\n", "\n").replace("\r", "\n")
            )
            sys.stdout.flush()

    link.close()
    sys.stdout.write(f"\n[captured {total} bytes in {seconds:.0f}s]\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
