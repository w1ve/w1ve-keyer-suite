#!/usr/bin/env python3
"""Proves the C WireGuard core interoperates with boringtun, the implementation rwk-core uses.

  1. boringtun initiates, the C core responds   (the real case: desktop Client -> this station)
  2. the C core initiates, boringtun responds   (the punch case: both ends initiate)

Run:  python3 tests/interop/run_interop.py
Needs gcc, OpenSSL headers and cargo (first run downloads boringtun).
"""
import os
import secrets
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))


def run(cmd, **kw):
    return subprocess.run(cmd, check=True, text=True, capture_output=True, **kw)


def build():
    run(["gcc", "-std=gnu11", "-O1", "-g", "-Wall", "-Wextra", "-fsanitize=address,undefined",
         "-o", os.path.join(HERE, "wg_peer"),
         os.path.join(HERE, "wg_peer.c"),
         os.path.join(ROOT, "tests/host/rwk_crypto_openssl.c"),
         os.path.join(ROOT, "main/rwk/rwk_blake2s.c"),
         os.path.join(ROOT, "main/rwk/rwk_wg.c"),
         os.path.join(ROOT, "main/rwk/rwk_inner.c"),
         "-lcrypto"])
    run(["cargo", "build", "--release", "--quiet"], cwd=os.path.join(HERE, "bt_interop"))


def bt(*args):
    exe = os.path.join(HERE, "bt_interop", "target", "release", "bt_interop")
    return [exe, *args]


def pubkey(sk):
    return run(bt("pubkey", sk)).stdout.strip()


def scenario(name, c_mode, port_c, port_rs):
    sk_c, sk_r = secrets.token_hex(32), secrets.token_hex(32)
    pk_c, pk_r = pubkey(sk_c), pubkey(sk_r)
    if c_mode == "respond":
        # boringtun (initiator) dials the C core.
        c = subprocess.Popen([os.path.join(HERE, "wg_peer"), "respond", sk_c, pk_r, str(port_c)],
                             stdout=subprocess.PIPE, text=True)
        c.stdout.readline()  # ready
        r = subprocess.run(bt("initiator", sk_r, pk_c, str(port_rs), str(port_c)), text=True,
                           capture_output=True, timeout=30)
    else:
        # boringtun (responder) waits; the C core initiates.
        r_proc = subprocess.Popen(bt("responder", sk_r, pk_c, str(port_rs)), stdout=subprocess.PIPE,
                                  text=True)
        r_proc.stdout.readline()
        c = subprocess.Popen([os.path.join(HERE, "wg_peer"), "initiate", sk_c, pk_r, str(port_c),
                              str(port_rs)], stdout=subprocess.PIPE, text=True)
        c.stdout.readline()
        r = None
    c_out = c.communicate(timeout=30)[0]
    if c_mode == "respond":
        r_out = r.stdout
    else:
        r_out = r_proc.communicate(timeout=30)[0]
    ok = c.returncode == 0 and "PASS" in c_out and "PASS" in r_out
    print(f"{'PASS' if ok else 'FAIL'}  {name}")
    print(f"      c core   : {c_out.strip().splitlines()[-1] if c_out.strip() else '(no output)'}")
    print(f"      boringtun: {r_out.strip().splitlines()[-1] if r_out.strip() else '(no output)'}")
    if r is not None and r.stderr.strip():
        print(r.stderr.strip())
    return ok


def main():
    build()
    results = [
        scenario("boringtun initiates, C core responds", "respond", 47001, 47002),
        scenario("C core initiates, boringtun responds", "initiate", 47003, 47004),
    ]
    print("ALL PASS" if all(results) else "FAILED")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
