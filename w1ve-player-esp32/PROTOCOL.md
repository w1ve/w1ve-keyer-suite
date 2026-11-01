# W1VE v2: ESP32 Player trusted-LAN protocol

`W1VE` v2 connects a **W1VE ESP32 Player** at the radio site to a W1VE Remote Keyer or W1VE Remote Keyer Router on the same trusted LAN. It is a LAN-only transport. The Player advertises its availability, the Keyer browses the available Players, and the Keyer initiates pairing.

CW state travels in one direction:

```text
W1VE Remote Keyer or Router → W1VE ESP32 Player → KEY/PTT GPIO → radio
```

The protocol has no persistent pairing secret, pairing code, or WAN listener. Its nonce-derived session key, endpoint pinning, packet authentication, and replay window protect normal LAN operation. A device that can observe or interfere with the initial handshake can interfere with the session, so do not use this transport on an untrusted network.

## Wire conventions

- All multibyte discovery, handshake, session ID, sequence, and ping timestamp fields are big-endian unless a field explicitly says little-endian.
- `player_id` is the Player's six-byte station MAC address.
- `keyer_id` is a nonzero, stable six-byte Keyer identity.
- A packet with another magic or version is ignored.
- The Player listens on UDP 4994 for discovery queries and UDP 4995 for pairing and authenticated session packets only after it receives a non-overlapping station-LAN IPv4 address. These sockets bind to that station address; the setup access point remains configuration-only and cannot carry W1VE traffic.

## Discovery

Every 1.5 seconds, the Player broadcasts a type 1 advertisement from UDP 4995 to `255.255.255.255:4994`.

```text
magic[4]       "W1VE"
version:u8     2
type:u8        1 (Player advertisement)
player_id[6]   Player station MAC address
port:u16 BE    4995
name_len:u8    0..32
name[]         name_len display-name bytes
flags:u8       bit 0 = paired; all other bits are zero
```

A Keyer browses these advertisements and uses the source IP address plus `port` as the Player endpoint. The advertised name is display-only and must not authorize a pairing.

### Broadcast-suppression query

When a Keyer knows a Player address but does not receive its broadcasts, it may send this exact type 10 query to that Player's UDP 4994 endpoint:

```text
magic[4]       "W1VE"
version:u8     2
type:u8        10 (discovery query)
keyer_id[6]    nonzero Keyer identity
```

The Player does not change pairing state for a query. It unicasts its ordinary type 1 advertisement back to the query source, using UDP 4995 as the reply source port. The reply has the same byte layout as a broadcast advertisement.

## Pairing

The Keyer chooses an advertised Player and sends a type 2 request from the UDP endpoint it will use for the session. The Player accepts a request only when it is currently unpaired and the request names its own `player_id`.

### Type 2: Keyer-initiated pair request

```text
magic[4]             "W1VE"
version:u8           2
type:u8              2
player_id[6]         selected Player
keyer_id[6]          initiating Keyer
initiator_nonce[16]  cryptographically random
```

The packet is exactly 34 bytes. On acceptance, the Player pins the source IP address and UDP port as its only session peer.

### Type 3: Player pair accept

```text
magic[4]             "W1VE"
version:u8           2
type:u8              3
player_id[6]         selected Player
keyer_id[6]          initiating Keyer
initiator_nonce[16]  copied from type 2
responder_nonce[16]  cryptographically random Player nonce
```

The packet is exactly 50 bytes and is sent to the type 2 source endpoint. The type 2 and type 3 handshake packets are not session-authenticated. The LAN trust boundary applies before the session key exists.

Both peers derive the 32-byte session key as:

```text
SHA-256(
  "W1VE-SESSION-v2" |
  player_id |
  keyer_id |
  initiator_nonce |
  responder_nonce
)
```

The first four digest bytes, interpreted as a big-endian unsigned integer, are the `session_id`.

## Authenticated session envelope

After a successful handshake, each packet uses this envelope:

```text
magic[4]       "W1VE"
version:u8     2
type:u8        authenticated message type
player_id[6]   selected Player
session_id:u32 first four bytes of the session key
sequence:u32   nonzero, monotonically increasing per sender
payload_len:u8 0..64
payload[]       payload_len bytes
tag[16]         first 16 bytes of HMAC-SHA-256(session_key, all preceding bytes)
```

The total packet length is `37 + payload_len` bytes. The Player accepts an authenticated envelope only from the pinned endpoint, for its current `player_id` and `session_id`, with an exact encoded length and a matching tag. Only these session message types are valid:

| Type | Direction | Payload | Meaning |
| --- | --- | --- | --- |
| 4 | Keyer → Player | 16..52 bytes | Complete CW edge frame |
| 6 | Either direction | 8 bytes | Ping timestamp |
| 7 | Either direction | 8 bytes | Matching ping timestamp |
| 8 | Keyer → Player | empty | Immediate unpair |

Unsupported session message types have no protocol action.

### Replay and reordering

The Player maintains a 64-packet receive bitmap for authenticated envelopes. Sequence zero is rejected. It accepts a previously unseen sequence newer than the high-water mark, or a previously unseen sequence less than 64 positions behind that high-water mark. It rejects duplicates and packets 64 or more positions behind it.

The replay rule applies to every authenticated session message, including type 8. A valid type 8 must first pass the envelope replay window before it releases GPIO and tears down the session. Senders terminate the current session before the counter could wrap and must pair again; no implementation may send sequence zero or resume at sequence one.

## Type 4 CW edge frames

Type 4 carries a complete current-plus-history CW frame. Its payload is 16 to 52 bytes:

```text
epoch:u16 LE
count:u16 LE             1..4
entries[count]           current entry first, then older redundant entries

EdgeEntry (12 bytes):
  edge_sequence:u32 LE
  timestamp_ms:u32 LE
  state:u8               0 = KEY up; nonzero = KEY down
  flags:u8               must be 0
  reserved:u16 LE        must be 0
```

The Player processes entries oldest-to-newest. It adopts the first epoch, ignores already applied edge sequences, and disconnects on an epoch change, a timestamp regression, or a missing edge sequence that ends with KEY down. Malformed frames never refresh the key-state lease.

The selected Keyer repeats its latest complete frame in new authenticated envelopes while KEY is asserted. This allows the Player to recover from lost frames without accepting a separate state message. The Player refreshes its output lease only from a valid complete type 4 frame.

## Liveness and output safety

- The Player sends a type 6 ping once per second. The Keyer returns a type 7 pong with the identical eight-byte payload.
- A Keyer may independently send type 6 pings. The Player returns a matching type 7 pong.
- The Player ends the session after three consecutive unanswered Player-originated pings.
- When KEY is down, the Player imposes a 1500 ms lease. If no valid complete type 4 frame refreshes that lease, it unpairs and releases KEY/PTT. Pings and pongs never extend this lease.
- A fresh accepted type 8 immediately unpairs and releases KEY/PTT.
- Local configuration changes, Wi-Fi loss, and pairing loss also release KEY/PTT.

Configured KEY/PTT GPIO outputs and the optional white RGB LED show accepted Player activity. They are the physical confirmation of keying, rather than a browser-generated indication.

## Cross-language test vector

Implementations can verify the shared key derivation and authentication code with this fixed vector:

```text
player_id        01 02 03 04 05 06
keyer_id         A1 A2 A3 A4 A5 A6
initiator_nonce  00 01 02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F
responder_nonce  10 11 12 13 14 15 16 17 18 19 1A 1B 1C 1D 1E 1F
session_key      8D 9D 3A 9F 13 24 65 19 84 A3 D8 28 F9 32 3E C3
                 5E 06 BA 3D 36 35 6C 5F F1 40 D0 76 17 94 F2 25
session_id       8D 9D 3A 9F
```

For a type 6 packet with sequence `0x01020304` and payload
`01 02 03 04 05 06 07 08`, the first 16 HMAC-SHA-256 bytes are:

```text
6E 40 31 DE 64 88 4C 69 60 13 12 3B E2 87 F3 93
```

The `rwk-core` test suite verifies this vector.