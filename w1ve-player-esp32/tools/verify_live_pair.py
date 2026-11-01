#!/usr/bin/env python3
"""Run a synthetic W1VE v2 ESP32 Player for desktop Keyer diagnostics.

This script advertises a synthetic W1VE ESP32 Player, answers Keyer type-10 discovery queries,
accepts a Keyer-initiated type-2 pairing request, and checks matching ping/pong exchanges in both
directions. It never sends a type-4 CW frame and has no GPIO access. It is useful for checking a
desktop Keyer's W1VE v2 Player browsing and pairing flow without connecting radio hardware.
"""

from __future__ import annotations

import argparse
import hashlib
import hmac
import os
import select
import socket
import time
from dataclasses import dataclass

MAGIC = b"W1VE"
VERSION = 2
PLAYER_ADVERTISEMENT = 1
PAIR_REQUEST = 2
PAIR_ACCEPT = 3
EDGE = 4
PING = 6
PONG = 7
UNPAIR = 8
DISCOVERY_QUERY = 10
DISCOVERY_PORT = 4994
TRANSPORT_PORT = 4995
HEADER_LEN = 21
TAG_LEN = 16
REPLAY_WINDOW = 64


class ProtocolError(RuntimeError):
    """Raised when a Keyer packet does not match the W1VE v2 contract."""


@dataclass
class ReplayWindow:
    highest: int | None = None
    bits: int = 0

    def accept(self, sequence: int) -> bool:
        if sequence == 0:
            return False
        if self.highest is None:
            self.highest = sequence
            self.bits = 1
            return True
        if sequence > self.highest:
            shift = sequence - self.highest
            self.bits = 1 if shift >= REPLAY_WINDOW else ((self.bits << shift) | 1)
            self.bits &= (1 << REPLAY_WINDOW) - 1
            self.highest = sequence
            return True
        distance = self.highest - sequence
        if distance >= REPLAY_WINDOW:
            return False
        bit = 1 << distance
        if self.bits & bit:
            return False
        self.bits |= bit
        return True


@dataclass
class SyntheticPlayerSession:
    sock: socket.socket
    endpoint: tuple[str, int]
    player_id: bytes
    keyer_id: bytes
    key: bytes
    session_id: bytes
    next_sequence: int = 0
    replay: ReplayWindow | None = None

    def __post_init__(self) -> None:
        if self.replay is None:
            self.replay = ReplayWindow()

    def send(self, packet_type: int, payload: bytes = b"") -> None:
        self.next_sequence += 1
        self.sock.sendto(
            authenticated_packet(
                self.player_id,
                self.key,
                self.session_id,
                self.next_sequence,
                packet_type,
                payload,
            ),
            self.endpoint,
        )


def hmac16(key: bytes, message: bytes) -> bytes:
    return hmac.new(key, message, hashlib.sha256).digest()[:TAG_LEN]


def derive_session_key(
    player_id: bytes, keyer_id: bytes, initiator_nonce: bytes, responder_nonce: bytes
) -> bytes:
    return hashlib.sha256(
        b"W1VE-SESSION-v2" + player_id + keyer_id + initiator_nonce + responder_nonce
    ).digest()


def player_advertisement(player_id: bytes, name: str, paired: bool) -> bytes:
    encoded_name = name.encode("utf-8")
    if len(encoded_name) > 32:
        raise ValueError("Player display name exceeds 32 UTF-8 bytes")
    return (
        MAGIC
        + bytes([VERSION, PLAYER_ADVERTISEMENT])
        + player_id
        + TRANSPORT_PORT.to_bytes(2, "big")
        + bytes([len(encoded_name)])
        + encoded_name
        + bytes([1 if paired else 0])
    )


def parse_discovery_query(packet: bytes) -> bytes:
    if len(packet) != 12 or packet[:6] != MAGIC + bytes([VERSION, DISCOVERY_QUERY]):
        raise ProtocolError("not a W1VE v2 type-10 discovery query")
    keyer_id = packet[6:12]
    if keyer_id == b"\0" * 6:
        raise ProtocolError("type-10 query has a zero Keyer ID")
    return keyer_id


def parse_pair_request(packet: bytes, player_id: bytes) -> tuple[bytes, bytes]:
    if len(packet) != 34 or packet[:6] != MAGIC + bytes([VERSION, PAIR_REQUEST]):
        raise ProtocolError("not a W1VE v2 type-2 pair request")
    if packet[6:12] != player_id:
        raise ProtocolError("type-2 request targets another Player")
    keyer_id = packet[12:18]
    if keyer_id == b"\0" * 6:
        raise ProtocolError("type-2 request has a zero Keyer ID")
    return keyer_id, packet[18:34]


def pair_accept(player_id: bytes, keyer_id: bytes, initiator_nonce: bytes, responder_nonce: bytes) -> bytes:
    return (
        MAGIC
        + bytes([VERSION, PAIR_ACCEPT])
        + player_id
        + keyer_id
        + initiator_nonce
        + responder_nonce
    )


def authenticated_packet(
    player_id: bytes,
    key: bytes,
    session_id: bytes,
    sequence: int,
    packet_type: int,
    payload: bytes = b"",
) -> bytes:
    if not 0 < sequence <= 0xFFFFFFFF:
        raise ValueError("W1VE sequence is outside the wire range")
    if len(payload) > 64:
        raise ValueError("W1VE payload exceeds 64 bytes")
    message = (
        MAGIC
        + bytes([VERSION, packet_type])
        + player_id
        + session_id
        + sequence.to_bytes(4, "big")
        + bytes([len(payload)])
        + payload
    )
    return message + hmac16(key, message)


def parse_authenticated(
    packet: bytes, player_id: bytes, key: bytes, session_id: bytes
) -> tuple[int, int, bytes]:
    if len(packet) < HEADER_LEN + TAG_LEN or packet[:4] != MAGIC or packet[4] != VERSION:
        raise ProtocolError("malformed W1VE v2 authenticated packet")
    if packet[6:12] != player_id or packet[12:16] != session_id:
        raise ProtocolError("authenticated packet targets another Player or session")
    payload_len = packet[20]
    unsigned_len = HEADER_LEN + payload_len
    if len(packet) != unsigned_len + TAG_LEN:
        raise ProtocolError("authenticated packet has an invalid encoded length")
    if not hmac.compare_digest(hmac16(key, packet[:unsigned_len]), packet[unsigned_len:]):
        raise ProtocolError("authenticated packet tag did not verify")
    return packet[5], int.from_bytes(packet[16:20], "big"), packet[HEADER_LEN:unsigned_len]


def make_udp_socket(bind_address: str, port: int, broadcast: bool = False) -> socket.socket:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    if broadcast:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    sock.bind((bind_address, port))
    sock.setblocking(False)
    return sock


def parse_player_id(value: str) -> bytes:
    compact = value.replace(":", "").replace("-", "")
    try:
        player_id = bytes.fromhex(compact)
    except ValueError as error:
        raise argparse.ArgumentTypeError("Player ID must be six hexadecimal bytes") from error
    if len(player_id) != 6 or player_id == b"\0" * 6:
        raise argparse.ArgumentTypeError("Player ID must be six nonzero bytes")
    return player_id


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--bind",
        default="0.0.0.0",
        help="local IPv4 address to bind (default: all local interfaces)",
    )
    parser.add_argument("--name", default="W1VE ESP32 Player Diagnostic", help="advertised Player name")
    parser.add_argument(
        "--player-id",
        type=parse_player_id,
        default=parse_player_id("02:57:31:56:45:02"),
        help="six-byte synthetic Player ID (default: 02:57:31:56:45:02)",
    )
    parser.add_argument("--timeout", type=float, default=20.0, help="overall diagnostic timeout in seconds")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")

    transport = make_udp_socket(args.bind, TRANSPORT_PORT, broadcast=True)
    discovery = make_udp_socket(args.bind, DISCOVERY_PORT)
    session: SyntheticPlayerSession | None = None
    saw_keyer_ping = False
    saw_player_pong = False
    pending_player_ping: bytes | None = None
    next_advertisement = 0.0
    next_ping = 0.0
    deadline = time.monotonic() + args.timeout

    print(f"Synthetic W1VE ESP32 Player listening on {args.bind}:{TRANSPORT_PORT}")
    print(f"Advertising type-1 on 255.255.255.255:{DISCOVERY_PORT}; this diagnostic never sends CW frames")
    try:
        while time.monotonic() < deadline:
            now = time.monotonic()
            if now >= next_advertisement:
                transport.sendto(
                    player_advertisement(args.player_id, args.name, session is not None),
                    ("255.255.255.255", DISCOVERY_PORT),
                )
                next_advertisement = now + 1.5

            if session is not None and now >= next_ping:
                pending_player_ping = os.urandom(8)
                session.send(PING, pending_player_ping)
                next_ping = now + 1.0

            readable, _, _ = select.select([discovery, transport], [], [], 0.1)
            for ready in readable:
                packet, source = ready.recvfrom(128)
                if ready is discovery:
                    try:
                        query_keyer_id = parse_discovery_query(packet)
                    except ProtocolError:
                        continue
                    transport.sendto(
                        player_advertisement(args.player_id, args.name, session is not None), source
                    )
                    print(f"Answered type-10 query from Keyer {query_keyer_id.hex(':')} at {source[0]}:{source[1]}")
                    continue

                if session is None:
                    try:
                        keyer_id, initiator_nonce = parse_pair_request(packet, args.player_id)
                    except ProtocolError:
                        continue
                    responder_nonce = os.urandom(16)
                    key = derive_session_key(args.player_id, keyer_id, initiator_nonce, responder_nonce)
                    session = SyntheticPlayerSession(
                        transport,
                        source,
                        args.player_id,
                        keyer_id,
                        key,
                        key[:4],
                    )
                    transport.sendto(
                        pair_accept(args.player_id, keyer_id, initiator_nonce, responder_nonce), source
                    )
                    next_ping = time.monotonic()
                    print(f"Accepted Keyer type-2 pairing from {source[0]}:{source[1]}")
                    continue

                if source != session.endpoint:
                    continue
                try:
                    packet_type, sequence, payload = parse_authenticated(
                        packet, session.player_id, session.key, session.session_id
                    )
                except ProtocolError:
                    continue
                if packet_type not in (PING, PONG, UNPAIR):
                    if packet_type == EDGE:
                        raise ProtocolError("received a type-4 CW frame during a no-CW diagnostic")
                    raise ProtocolError(f"received unsupported session type {packet_type}")
                if packet_type == UNPAIR and payload:
                    raise ProtocolError("type-8 unpair payload must be empty")
                if not session.replay or not session.replay.accept(sequence):
                    continue
                if packet_type == PING:
                    if len(payload) != 8:
                        raise ProtocolError("type-6 ping payload must be eight bytes")
                    session.send(PONG, payload)
                    saw_keyer_ping = True
                    print("Returned a matching type-7 pong for the Keyer ping")
                elif packet_type == PONG:
                    if len(payload) != 8:
                        raise ProtocolError("type-7 pong payload must be eight bytes")
                    if pending_player_ping is not None and payload == pending_player_ping:
                        saw_player_pong = True
                        print("Received a matching type-7 pong for the synthetic Player ping")
                else:
                    print("Keyer requested a safe type-8 unpair")
                    return 0

            if session is not None and saw_keyer_ping and saw_player_pong:
                session.send(UNPAIR)
                print("PASS: desktop Keyer browsed a v2 Player, paired, and completed bidirectional ping/pong")
                print("Sent a safe type-8 unpair; no CW frame was generated")
                return 0
    finally:
        discovery.close()
        transport.close()

    raise RuntimeError("timed out waiting for a complete W1VE v2 Keyer pairing and control exchange")


if __name__ == "__main__":
    raise SystemExit(main())
