# WAN transport for the W1VE Touch Keyer: architecture and protocol analysis

Phase 1 report. Source of truth is the code in `w1ve-keyer-suite` (shallow clone at
`.toolchain/suite-ref`, commit `b3d4970`), not its documentation.

## 1. What RWK WAN actually is

The prompt asks for "the RWK transport protocol, not a conventional WireGuard IP VPN client".
The source shows these are the same thing, so one premise needs correcting:

* RWK WAN **is** WireGuard. `common/rwk-core/src/engine/tunnel/mod.rs` wraps `boringtun::noise::Tunn`
  (Noise_IKpsk2, X25519, ChaCha20-Poly1305, BLAKE2s, no PSK). The relay's payloads are standard
  WireGuard datagrams ("Send: 32-byte destination key + WireGuard datagram"). There is no
  RWK-specific cipher or handshake to port instead.
* What RWK does **not** use is a kernel/lwIP network interface. Inside the tunnel, `smoltcp` runs a
  userspace IP stack on a fixed point-to-point pair: Client `10.6.0.1`, Station `10.6.0.2`
  (`config.rs::Role::inner_address`). The keying edge is one UDP flow, port **5555** both ends.

Consequence for the ESP32: implement the WireGuard **wire protocol** (handshake + transport data) as
a small purpose-built endpoint, with no netif and no inner IP stack. The only inner traffic the ESP
must understand is IPv4/UDP 10.6.0.1:5555 <-> 10.6.0.2:5555. This honours the intent (no general
VPN, no lwIP netif, tiny footprint) while staying byte-compatible with boringtun. Anything else would
be a second, incompatible protocol, which the prompt forbids.

## 2. Layers, bottom to top

| Layer | Spec (from source) | ESP32 plan |
| --- | --- | --- |
| Rendezvous/relay TCP framing | `relay/frame.rs`: `type:1 | len:4 BE | payload`, `MAX_PAYLOAD` 64 KiB | `rwk_frame.c` |
| Frame types | Register 0x02, Send 0x04, Recv 0x05, KeepAlive 0x06, Pair 0x20, Paired 0x21 | all six |
| Pair body | `key[32] | udp_port u16 BE | code (UTF-8, remainder)` | |
| Paired body | `peer_key[32] | tag(0/4/6) [addr][port u16 BE]` | |
| WireGuard | standard; boringtun `Tunn::new(sk, peer_pk, None, keepalive, index, None)` | `rwk_wg.c` |
| Inner packet | IPv4 + UDP, checksums emitted (rx not verified by smoltcp config `Checksum::Tx`) | hand-built 28-byte header |
| Edge channel | UDP 5555; `RwkPaddleFrame` = epoch u16 LE, count u16 LE, 1..4 x 12 B entries | identical to LAN type-4 payload |
| Liveness | heartbeat = 4 bytes `RWKH`; ping `RWKP`+8 B stamp, pong `RWKQ`+same; every 250 ms; peer silent 1500 ms = lost | |

Edge payload equals the LAN type-4 payload, so the existing edge applier in `remote.c`
(sequence/timestamp/epoch checks, clock-offset learning, lease, speed inference) is reused unchanged.

## 3. Connection lifecycle (Player/Station side, which is what the ESP is)

1. `connect_paired`: bind a UDP socket, learn its local port.
2. TCP to relay, send `Pair{my_pubkey, udp_port, code}` as the **first** frame. The relay treats the
   connection as signalling-only, parks it (TTL 120 s) until a second node presents the same code,
   then sends `Paired{peer_key, peer_addr}` to both; the client then closes.
3. Direct attempt (6 s): both ends fire a WireGuard initiation every 250 ms at `peer_addr` over the
   same UDP socket and answer the other's. The Client is the intended initiator; the Station also
   initiates here (punching).
4. On failure: new TCP connection, `Register{my_pubkey}` as first frame; datagrams travel as
   `Send{dst_key, wg}` / `Recv{src_key, wg}`. The Client re-initiates for up to 20 s; the Station
   waits up to 60 s.
5. Session: WireGuard transport carries the edge UDP flow. Both ends send heartbeat+ping every 250 ms.
   Silence of 1500 ms drops the session. Desktop budget: retry for 15 s, then report disconnected.
   One retry of rendezvous after 750 ms.
6. Station role: on peer loss, re-listen immediately (new Pair).

Roles: the ESP is `Role::Station` (inner 10.6.0.2). It never needs to be a TCP/IP initiator inside
the tunnel.

## 4. Identity, keys, authentication

* Identity is a raw X25519 private key (32 random bytes, clamped at use). Public key = X25519
  base point multiply. This is also the relay address. `config.rs` stores it base64 in `config.json`.
* **The pairing code is the only authentication.** The relay hands each side the other's key; the
  WireGuard handshake then pins that key for the session. There is no PSK, no key pre-sharing, and
  nothing stops a third party who learns/guesses the code from racing in. This is existing
  behaviour; I keep it (compatibility) and will use ≥12 hex chars (desktop `generate_token`) and
  only accept the one peer key introduced for the current attempt. Optional first-key pinning is a
  possible later hardening, not a protocol change.
* ESP keys: generated with the hardware TRNG (`esp_fill_random` after Wi-Fi/BT RNG source is live),
  stored in NVS (`wan_sk`). The private key is never returned by any API, never logged, and the
  stack copy is zeroed after use. NVS is not encrypted on this board (no flash encryption
  configured), so physical flash readout would reveal it; stated limitation.

## 5. Crypto the ESP must support, and what ESP-IDF provides

| Operation | Needed for | ESP-IDF v5.3.4 |
| --- | --- | --- |
| X25519 | static key, handshake DH | mbedTLS `ECDH` Curve25519 (already enabled). No HW accel (ECC block is P-192/256 only); software, cost measured in Phase 2 |
| ChaCha20-Poly1305 (IETF) | handshake AEAD and data | mbedTLS `chachapoly`, needs `CONFIG_MBEDTLS_CHACHA20_C/POLY1305_C` (currently off) |
| BLAKE2s-256, HMAC-BLAKE2s, HKDF | Noise hashing/KDF | **not in mbedTLS**; small self-contained implementation (~150 lines), verified against RFC 7693 vectors |
| TAI64N timestamp | initiation only | only if the ESP initiates; needs SNTP (lwIP SNTP present). Responder-only needs none |
| XChaCha20 cookie reply | DoS mitigation | not implemented; a peer under load may send a cookie reply, which we ignore |

Hardware AES/SHA are irrelevant here. Handshake CPU is bounded (~4 scalar mults) and happens once
per session on a low-priority task; per-packet cost (≈50-byte ChaCha20-Poly1305) is negligible.

## 6. Is `handle_pair()` sufficient for NAT traversal?

No, not in general. `relay/server.rs::handle_pair` builds the introduced address as
`SocketAddr::new(observed_tcp_peer_ip, udp_port_reported_by_client)`, and desktop clients report
`socket.local_addr().port()` (`connect_paired`: "the local port the NAT will map"). That assumes:

1. The NAT maps UDP to the same external port as the local one (port preservation). Many home
   routers do, many (CGNAT, symmetric, port-collision) do not.
2. The public IP seen on the TCP connection equals the UDP source IP (multi-WAN/CGNAT can differ).
3. TCP and UDP are not hairpinned (both ends behind one NAT get its public IP).

Backward-compatible fix for the ESP: before pairing, send an RFC 5389 STUN Binding Request from the
**same UDP socket** to a public STUN server and report the *mapped* port in `Pair.udp_port`. The
relay protocol and desktop clients are untouched (the field simply carries a truer value). Inbound,
the ESP adopts the source address of any datagram that authenticates (WireGuard-style roaming) and
replies there, so a desktop peer with a remapped port still connects when the ESP's NAT is
full/address-restricted cone. Symmetric NAT on either side falls through to the relay, as it does
for desktops today. A server-side fix (relay learns UDP mapping) would help desktop peers too but
needs a relay change; deliberately out of scope ("no breaking changes").

## 7. Reuse versus port

| Component | Reuse | Port |
| --- | --- | --- |
| Frame codec, relay client | no | port to C (small) |
| WireGuard | no (boringtun is Rust, `std`) | port protocol to C using mbedTLS + BLAKE2s |
| smoltcp netstack | not needed | hand-built IPv4/UDP for one flow |
| Edge applier / lease / speed | **yes, `remote.c`** | add attach/detach hooks only |
| Relay server | **yes, unmodified** | n/a |

## 8. Target architecture on the ESP

```
 keyer.c  <-- keyer_remote_edge()  <--  remote.c (edge applier, status, lease)  <-- LAN transport (unchanged)
                                              ^
                                              | rwk_edge_ingest(payload)
                                         wan.c  (state machine, FreeRTOS task, prio below keyer)
                                              |
        +-------------------+-----------------+------------------+
        | rwk_rdv (TCP Pair)| rwk_path        | rwk_relay (TCP)  |
        +-------------------+  direct UDP     +------------------+
                                    | datagram in/out
                               rwk_wg (Noise IK + transport)   rwk_frame (TCP codec)
                                    | crypto vtable
                          mbedTLS backend | blake2s | host OpenSSL backend (tests)
```

Portable pieces (`rwk_frame`, `rwk_wg`, `rwk_blake2s`, `rwk_inner`) have no ESP-IDF includes and
are compiled and tested on the host. The keying path never waits on WAN: `wan.c` runs in its own
task (priority 3, below the keyer and the LAN task), uses non-blocking sockets and bounded static
buffers, and hands edges to the same non-blocking `keyer_remote_edge()`.

Arbitration: one remote session owns the line at a time. Whichever of LAN or WAN pairs first holds
it; the other is refused until it ends. A local paddle always wins, as today.

## 9. State machine

`Disabled → WifiDown → Rendezvous → Pairing → {Direct | Relay} → Reconnecting → Error`, with
bounded exponential backoff (1 s, 2 s, 4 s ... capped at 30 s), a 5 s stability threshold before
the backoff resets (copying `ReconnectBudget`), and teardown of sockets on every transition.

## 10. Deviations and open decisions

1. **Premise corrected:** WAN transport is WireGuard; I implement the WG wire protocol, not a netif.
2. Relay address on desktops is parsed as `IP:port` only; the ESP will accept a hostname too (superset).
3. The suite's static relay is `DEFAULT_WAN_RELAY = "107.172.138.103:9000"`
   (`W1VE-Remote-Keyer/crates/wrk-app/src/pairing.rs`). The ESP uses it as the default relay, shown
   in an editable field on the web page (host or IP, port). An empty field restores the default.
   WAN itself stays **off** until the operator enables it, so nothing leaves the box unprompted.
4. STUN uses a third-party server (default `stun.l.google.com:19302`, configurable, can be
   disabled). It reveals only the device's public IP/port.
5. "Status is the Cryptokey value": I read this as WAN mode showing the device's **public** key
   (abbreviated on the panel, full on the web page). Private key never shown.
6. Panel toggle semantics assumed: the "LAN: On/Off" switch becomes a LAN / WAN mode switch; in LAN
   mode status reads "Advertising", in WAN mode the cryptokey fingerprint or the WAN state.

---

## 11. Compatibility report (Phase 8)

Tested on 4 Nov 2026 against **genuine suite code**, not a reimplementation:

* The real relay, `rwk-relay` from `w1ve-keyer-suite` (commit `b3d4970`), built from source and run
  locally on `0.0.0.0:9000`.
* A real RWK **Keyer** (Client role) built on the suite's own `rwk-core`
  (`tests/interop/rwk_keyer/`): it calls `tunnel::connect`, `Session::start`,
  `EdgeTransport::over_tunnel` and sends `RwkPaddleFrame`s — the same crate and code paths the
  desktop Remote Keyer uses.

Results:

| Check | Result |
|---|---|
| Rendezvous (Pair/Paired) with the real relay | pass — relay introduced both ends |
| WireGuard handshake, C core (ESP role) ↔ boringtun (Keyer) | pass, over the relayed path |
| CW edge frames delivered and keyed on the ESP | pass — `keying:true`, 248 frames in a 20 s run |
| Remote speed inferred | pass — `peer_wpm:30` for a 30 WPM stream |
| Authenticated PING/PONG both ways | pass — Keyer saw RTT, ESP answered |
| Session drop on Keyer silence | pass — ESP returned to rendezvous, KEY/PTT released, 1.5 s |
| Relay fallback | pass — this was the path taken (same-host punch declined, see note) |
| Direct UDP punch | not exercised on this bench (both ends on one host hairpin to the relay); the code path is covered by the `boringtun`-responds interop test and the STUN unit tests |
| WAN disabled → LAN unchanged | pass — `pair_test.py` and `edges_test.py` both `ALL PASS` |

Also verified earlier, offline:

* `tests/host` — 1421 assertions, ASan+UBSan clean, including BLAKE2s/HMAC against OpenSSL, X25519
  against RFC 7748, ChaCha20-Poly1305 round trips, the relay frame codec and incremental parser,
  inner IPv4/UDP, the STUN codec, the jitter scheduler, and the shared edge-applier safety rules.
* `tests/interop/run_interop.py` — the C WireGuard core interoperates with **boringtun 0.7.1** in
  both roles (boringtun initiates / C responds, and C initiates / boringtun responds).

### Deviations found and resolved

* None in the wire protocol. The ESP speaks the relay framing, the WireGuard handshake/transport,
  the inner UDP flow and the edge/liveness datagrams exactly as the suite does.

### Known limitations (unchanged from the design)

* Direct NAT traversal still depends on the relay's `handle_pair()` combining the TCP-observed IP
  with a reported UDP port; the ESP reports its STUN-mapped port to make that more often correct,
  but a symmetric NAT on either end falls back to the relay (as desktops do).
* The pairing code is the only admission control, per the existing design.
* The private key lives in plaintext NVS (no flash encryption configured on this board).

### How to reproduce

```bash
# build the real relay and keyer (needs the stub pkg-config shim for the unused serial/audio deps)
ln -sfn "$PWD/.toolchain/stublibs" /tmp/w1ve_stub
export PKG_CONFIG_PATH=/tmp/w1ve_stub/pc LD_LIBRARY_PATH=/tmp/w1ve_stub/lib RUSTFLAGS=-L/tmp/w1ve_stub/lib
( cd .toolchain/suite-ref && cargo build --release -p rwk-relay )
( cd tests/interop/rwk_keyer && cargo build --release )

# run the relay
RWK_RELAY_ADDR=0.0.0.0:9000 RWK_HTTP_ADDR=127.0.0.1:8099 \
  RWK_ADMIN_PASSWORD=testpass123 RWK_JWT_SECRET=testsecrettestsecret \
  .toolchain/suite-ref/target/release/rwk-relay &

# point the board at it and enable WAN (replace the IP with this host's LAN address)
curl -d 'relay=<host-ip>:9000' http://<board-ip>/api/wan/relay
curl -d 'code=INTEROP-42'       http://<board-ip>/api/wan/code
curl -d 'on=1'                  http://<board-ip>/api/wan

# key the board from a real RWK Keyer
tests/interop/rwk_keyer/target/release/rwk_keyer <host-ip>:9000 INTEROP-42 30 15
```
