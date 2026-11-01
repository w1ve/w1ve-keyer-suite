#!/usr/bin/env python3
"""Run a W1VE v2 live diagnostic against one W1VE ESP32 Player.

The default checks never send a CW frame. It receives a Player type-1 advertisement, sends a
Keyer type-10 query and validates the direct type-1 reply, initiates pairing, checks matching
ping/pong exchanges in both directions, checks bounded reordering and replay rejection with
non-keying packets, and verifies safe unpair/status.

Use --exercise-cw only with the radio disconnected or on a controlled test load. That opt-in
sends authenticated CW key-down frames and can operate the configured KEY/PTT outputs.
"""

from __future__ import annotations

import argparse
import hashlib
import hmac
import json
import os
import socket
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Callable

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
MAX_PAYLOAD_LEN = 64
REPLAY_WINDOW = 64
PLAYER_KEY_LEASE_SECONDS = 1.5


class ProtocolError(RuntimeError):
    """Raised when a peer packet does not match the W1VE v2 contract."""


@dataclass(frozen=True)
class PlayerAdvertisement:
    player_id: bytes
    port: int
    name: str
    paired: bool


@dataclass
class KeyerSession:
    sock: socket.socket
    endpoint: tuple[str, int]
    player_id: bytes
    key: bytes
    session_id: bytes
    sequence: int = 0

    def send(self, packet_type: int, payload: bytes = b"") -> int:
        self.sequence += 1
        self.send_sequence(self.sequence, packet_type, payload)
        return self.sequence

    def send_sequence(self, sequence: int, packet_type: int, payload: bytes = b"") -> None:
        if sequence <= 0:
            raise ValueError("W1VE sequence must be nonzero")
        if sequence > self.sequence:
            self.sequence = sequence
        self.sock.sendto(
            authenticated_packet(
                self.player_id,
                self.key,
                self.session_id,
                sequence,
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


def authenticated_packet(
    player_id: bytes,
    key: bytes,
    session_id: bytes,
    sequence: int,
    packet_type: int,
    payload: bytes = b"",
) -> bytes:
    if len(player_id) != 6 or len(session_id) != 4:
        raise ValueError("invalid W1VE Player ID or session ID")
    if not 0 < sequence <= 0xFFFFFFFF:
        raise ValueError("W1VE sequence is outside the wire range")
    if len(payload) > MAX_PAYLOAD_LEN:
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


def parse_player_advertisement(packet: bytes) -> PlayerAdvertisement:
    if len(packet) < 16 or packet[:6] != MAGIC + bytes([VERSION, PLAYER_ADVERTISEMENT]):
        raise ProtocolError("not a W1VE v2 Player advertisement")
    player_id = packet[6:12]
    port = int.from_bytes(packet[12:14], "big")
    name_len = packet[14]
    if player_id == b"\0" * 6 or port == 0 or name_len > 32 or len(packet) != 16 + name_len:
        raise ProtocolError("malformed W1VE v2 Player advertisement")
    flags = packet[15 + name_len]
    if flags & ~0x01:
        raise ProtocolError("Player advertisement has unsupported flags")
    try:
        name = packet[15 : 15 + name_len].decode("utf-8", "strict")
    except UnicodeDecodeError as error:
        raise ProtocolError("Player advertisement name is not UTF-8") from error
    return PlayerAdvertisement(player_id, port, name, bool(flags & 0x01))


def pair_request(player_id: bytes, keyer_id: bytes, initiator_nonce: bytes) -> bytes:
    if len(player_id) != 6 or len(keyer_id) != 6 or len(initiator_nonce) != 16:
        raise ValueError("invalid type-2 fields")
    return MAGIC + bytes([VERSION, PAIR_REQUEST]) + player_id + keyer_id + initiator_nonce


def parse_pair_accept(
    packet: bytes, player_id: bytes, keyer_id: bytes, initiator_nonce: bytes
) -> bytes:
    if len(packet) != 50 or packet[:6] != MAGIC + bytes([VERSION, PAIR_ACCEPT]):
        raise ProtocolError("Player did not return a valid W1VE v2 type-3 pair accept")
    if packet[6:12] != player_id or packet[12:18] != keyer_id or packet[18:34] != initiator_nonce:
        raise ProtocolError("type-3 pair accept does not match the type-2 request")
    return packet[34:50]


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


def receive_or_none(sock: socket.socket) -> tuple[bytes, tuple[str, int]] | None:
    try:
        return sock.recvfrom(128)
    except socket.timeout:
        return None


def make_udp_socket(bind_address: str, port: int = 0) -> socket.socket:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind((bind_address, port))
    return sock


def wait_for_advertisement(
    listener: socket.socket,
    expected_ip: str | None,
    expected_name: str | None,
    timeout: float,
) -> tuple[PlayerAdvertisement, tuple[str, int]]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        received = receive_or_none(listener)
        if not received:
            continue
        packet, source = received
        if expected_ip and source[0] != expected_ip:
            continue
        try:
            advertisement = parse_player_advertisement(packet)
        except ProtocolError:
            continue
        if expected_name and advertisement.name != expected_name:
            continue
        return advertisement, source
    target = expected_ip or expected_name or "a W1VE ESP32 Player"
    raise RuntimeError(f"timed out waiting for a type-1 advertisement from {target}")


def wait_for_query_reply(
    query_socket: socket.socket,
    player_ip: str,
    expected: PlayerAdvertisement,
    timeout: float,
) -> PlayerAdvertisement:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        received = receive_or_none(query_socket)
        if not received:
            continue
        packet, source = received
        if source[0] != player_ip or source[1] != expected.port:
            continue
        try:
            reply = parse_player_advertisement(packet)
        except ProtocolError:
            continue
        if reply.player_id != expected.player_id or reply.port != expected.port:
            raise ProtocolError("type-10 reply identifies a different Player endpoint")
        return reply
    raise RuntimeError("timed out waiting for the direct type-1 query reply")


def receive_pair_accept(
    session_socket: socket.socket,
    endpoint: tuple[str, int],
    player_id: bytes,
    keyer_id: bytes,
    initiator_nonce: bytes,
    timeout: float,
) -> bytes:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        received = receive_or_none(session_socket)
        if not received:
            continue
        packet, source = received
        if source != endpoint:
            continue
        return parse_pair_accept(packet, player_id, keyer_id, initiator_nonce)
    raise RuntimeError("timed out waiting for the Player type-3 pair accept")


def get_status(player_ip: str, http_port: int) -> dict:
    url = f"http://{player_ip}:{http_port}/api/status"
    with urllib.request.urlopen(url, timeout=3) as response:
        status = json.load(response)
    if not isinstance(status, dict):
        raise ProtocolError("Player status endpoint did not return an object")
    return status


def wait_until(label: str, predicate: Callable[[], bool], timeout: float) -> None:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
        except (OSError, urllib.error.URLError, json.JSONDecodeError, ProtocolError) as error:
            last_error = error
        time.sleep(0.1)
    detail = f": {last_error}" if last_error else ""
    raise RuntimeError(f"timed out waiting for {label}{detail}")


def status_is_paired_and_idle(player_ip: str, http_port: int) -> bool:
    status = get_status(player_ip, http_port)
    return bool(status.get("paired")) and not status.get("keyDown") and not status.get("pttOn")


def status_is_key_down(player_ip: str, http_port: int) -> bool:
    status = get_status(player_ip, http_port)
    return bool(status.get("paired")) and bool(status.get("keyDown"))


def status_is_key_up(player_ip: str, http_port: int) -> bool:
    status = get_status(player_ip, http_port)
    return bool(status.get("paired")) and not status.get("keyDown")


def status_is_safely_unpaired(player_ip: str, http_port: int) -> bool:
    status = get_status(player_ip, http_port)
    return not status.get("paired") and not status.get("keyDown") and not status.get("pttOn")


def handle_player_packet(session: KeyerSession, packet: bytes, source: tuple[str, int]) -> tuple[str, bytes] | None:
    if source != session.endpoint:
        return None
    packet_type, _, payload = parse_authenticated(packet, session.player_id, session.key, session.session_id)
    if packet_type == PING:
        if len(payload) != 8:
            raise ProtocolError("Player type-6 ping payload is not eight bytes")
        session.send(PONG, payload)
        return "player-ping", payload
    if packet_type == PONG:
        if len(payload) != 8:
            raise ProtocolError("Player type-7 pong payload is not eight bytes")
        return "player-pong", payload
    if packet_type == UNPAIR:
        raise ProtocolError("Player unexpectedly sent a type-8 unpair")
    raise ProtocolError(f"Player sent unsupported session type {packet_type}")


def wait_for_control_exchange(
    session: KeyerSession,
    expected_pongs: set[bytes],
    require_player_ping: bool,
    timeout: float,
) -> None:
    seen_pongs: set[bytes] = set()
    saw_player_ping = False
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        received = receive_or_none(session.sock)
        if not received:
            continue
        event = handle_player_packet(session, *received)
        if event is None:
            continue
        kind, payload = event
        if kind == "player-ping":
            saw_player_ping = True
        elif kind == "player-pong" and payload in expected_pongs:
            seen_pongs.add(payload)
        if seen_pongs == expected_pongs and (saw_player_ping or not require_player_ping):
            return
    missing = len(expected_pongs - seen_pongs)
    raise RuntimeError(
        f"incomplete control exchange: {missing} matching Keyer pong(s) missing; "
        f"Player ping seen={saw_player_ping}"
    )


def expect_no_matching_pong(session: KeyerSession, stamp: bytes, timeout: float, label: str) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        received = receive_or_none(session.sock)
        if not received:
            continue
        event = handle_player_packet(session, *received)
        if event and event == ("player-pong", stamp):
            raise RuntimeError(f"Player accepted {label} and returned a pong")


def check_reordering_and_replay(session: KeyerSession, player_ip: str, http_port: int) -> None:
    low_sequence = session.sequence + 1
    high_sequence = low_sequence + 1
    high_stamp = os.urandom(8)
    low_stamp = os.urandom(8)

    # Deliver a newer envelope first, then an unseen older envelope inside the 64-packet window.
    session.send_sequence(high_sequence, PING, high_stamp)
    session.send_sequence(low_sequence, PING, low_stamp)
    print("Sent type-6 pings in envelope order high, low to exercise bounded reordering")
    wait_for_control_exchange(session, {high_stamp, low_stamp}, False, 3)

    # The exact duplicate must be dropped before a second pong is generated.
    session.send_sequence(low_sequence, PING, low_stamp)
    expect_no_matching_pong(session, low_stamp, 0.6, "a duplicate envelope")

    # Advance the high-water mark by a whole window, then prove an older received sequence is stale.
    jump_sequence = session.sequence + REPLAY_WINDOW
    jump_stamp = os.urandom(8)
    session.send_sequence(jump_sequence, PING, jump_stamp)
    wait_for_control_exchange(session, {jump_stamp}, False, 3)
    stale_stamp = os.urandom(8)
    session.send_sequence(low_sequence, PING, stale_stamp)
    expect_no_matching_pong(session, stale_stamp, 0.6, "an envelope outside the replay window")

    # A replayed type-8 must also be rejected. The status check makes that security property visible.
    session.send_sequence(low_sequence, UNPAIR)
    time.sleep(0.25)
    if not status_is_paired_and_idle(player_ip, http_port):
        raise RuntimeError("a replayed type-8 unpaired the Player")
    print("Verified duplicate, stale, and replayed-unpair envelopes leave the Player safely paired")


def edge_frame(epoch: int, entries: list[tuple[int, int, bool]]) -> bytes:
    if not 1 <= len(entries) <= 4:
        raise ValueError("a complete CW frame needs one to four entries")
    payload = epoch.to_bytes(2, "little") + len(entries).to_bytes(2, "little")
    for edge_sequence, timestamp_ms, key_down in entries:
        payload += (
            edge_sequence.to_bytes(4, "little")
            + timestamp_ms.to_bytes(4, "little")
            + bytes([1 if key_down else 0, 0])
            + b"\0\0"
        )
    return payload


def pump_player_pings(session: KeyerSession, duration: float) -> None:
    deadline = time.monotonic() + duration
    while time.monotonic() < deadline:
        received = receive_or_none(session.sock)
        if not received:
            continue
        handle_player_packet(session, *received)


def exercise_cw(session: KeyerSession, player_ip: str, http_port: int) -> None:
    print("*** WARNING: --exercise-cw now sends CW key-down frames to configured KEY/PTT GPIO. ***")
    print("*** Continue only with the radio disconnected or attached to a controlled test load. ***")

    epoch = int.from_bytes(os.urandom(2), "little") or 1
    timestamp = int(time.monotonic() * 1000) & 0xFFFFFFFF
    down_frame = edge_frame(epoch, [(1, timestamp, True)])
    session.send(EDGE, down_frame)
    wait_until("accepted CW key-down", lambda: status_is_key_down(player_ip, http_port), 3)

    # New authenticated envelopes with the same complete frame keep the 1500 ms lease alive.
    for _ in range(3):
        time.sleep(0.35)
        session.send(EDGE, down_frame)
        pump_player_pings(session, 0.05)
        if not status_is_key_down(player_ip, http_port):
            raise RuntimeError("repeated complete CW frame did not keep KEY asserted")
    print("Verified repeated complete frames keep the asserted-key lease alive")

    # Apply key-up in a newer envelope, then deliver an older down frame within the replay window.
    up_frame = edge_frame(epoch, [(2, (timestamp + 1) & 0xFFFFFFFF, False)])
    low_sequence = session.sequence + 1
    high_sequence = low_sequence + 1
    session.send_sequence(high_sequence, EDGE, up_frame)
    session.send_sequence(low_sequence, EDGE, down_frame)
    wait_until("accepted CW key-up after reordered envelopes", lambda: status_is_key_up(player_ip, http_port), 3)
    print("Verified a reordered older key-down frame cannot reassert KEY after key-up")

    # Assert once more, reply to pings, and deliberately stop complete-frame refreshes.
    lease_down_frame = edge_frame(epoch, [(3, (timestamp + 2) & 0xFFFFFFFF, True)])
    session.send(EDGE, lease_down_frame)
    wait_until("second accepted CW key-down", lambda: status_is_key_down(player_ip, http_port), 3)
    pump_player_pings(session, PLAYER_KEY_LEASE_SECONDS + 0.9)
    wait_until("1500 ms CW lease release", lambda: status_is_safely_unpaired(player_ip, http_port), 3)
    print("Verified the Player releases KEY/PTT after the 1500 ms frame-refresh lease expires")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--bind",
        default="0.0.0.0",
        help="local IPv4 address to bind (default: all local interfaces)",
    )
    parser.add_argument(
        "--esp",
        help="known Player IPv4 address; sends a direct type-10 query before waiting for its type-1 reply",
    )
    parser.add_argument(
        "--player-name",
        help="require this exact Player display name when more than one Player advertises",
    )
    parser.add_argument("--http-port", type=int, default=80, help="Player HTTP port (default: 80)")
    parser.add_argument("--timeout", type=float, default=8.0, help="discovery/handshake timeout in seconds")
    parser.add_argument(
        "--exercise-cw",
        action="store_true",
        help="send controlled CW frames; this can key the radio and is disabled by default",
    )
    args = parser.parse_args()
    if not 1 <= args.http_port <= 65535:
        parser.error("--http-port must be between 1 and 65535")
    if args.timeout <= 0:
        parser.error("--timeout must be positive")

    discovery_listener: socket.socket | None = None
    query_socket: socket.socket | None = None
    session_socket: socket.socket | None = None
    session: KeyerSession | None = None
    session_closed = False
    try:
        keyer_id = os.urandom(6)
        discovery_listener = make_udp_socket(args.bind, DISCOVERY_PORT)
        discovery_listener.settimeout(0.2)
        print(f"Listening for W1VE v2 type-1 Player advertisements on {args.bind}:{DISCOVERY_PORT}")
        if args.esp:
            # Cross-band access points may suppress broadcasts even though same-subnet unicast
            # works. The known Player address makes this first discovery step deterministic.
            discovery_listener.sendto(
                MAGIC + bytes([VERSION, DISCOVERY_QUERY]) + keyer_id,
                (args.esp, DISCOVERY_PORT),
            )
            print(f"Sent direct type-10 discovery query to {args.esp}:{DISCOVERY_PORT}")
        advertisement, source = wait_for_advertisement(
            discovery_listener, args.esp, args.player_name, args.timeout
        )
        player_ip = source[0]
        endpoint = (player_ip, advertisement.port)
        print(
            f"Received Player type-1 advertisement: {advertisement.name or '(unnamed)'} "
            f"at {endpoint[0]}:{endpoint[1]}, id={advertisement.player_id.hex(':')}, "
            f"paired={advertisement.paired}"
        )

        query_socket = make_udp_socket(args.bind)
        query_socket.settimeout(0.2)
        query = MAGIC + bytes([VERSION, DISCOVERY_QUERY]) + keyer_id
        query_socket.sendto(query, (player_ip, DISCOVERY_PORT))
        query_reply = wait_for_query_reply(query_socket, player_ip, advertisement, args.timeout)
        print(
            f"Validated direct type-10 query/type-1 reply from {player_ip}:{query_reply.port} "
            f"for {query_reply.name or '(unnamed)'}"
        )

        session_socket = make_udp_socket(args.bind)
        session_socket.settimeout(0.2)
        initiator_nonce = os.urandom(16)
        session_socket.sendto(pair_request(advertisement.player_id, keyer_id, initiator_nonce), endpoint)
        responder_nonce = receive_pair_accept(
            session_socket,
            endpoint,
            advertisement.player_id,
            keyer_id,
            initiator_nonce,
            args.timeout,
        )
        key = derive_session_key(advertisement.player_id, keyer_id, initiator_nonce, responder_nonce)
        session = KeyerSession(session_socket, endpoint, advertisement.player_id, key, key[:4])
        print(f"Validated Keyer-initiated type-2 / Player type-3 pairing with {endpoint[0]}:{endpoint[1]}")

        ping_stamp = os.urandom(8)
        session.send(PING, ping_stamp)
        wait_for_control_exchange(session, {ping_stamp}, True, max(args.timeout, 3.5))
        print("Validated matching pongs for both Keyer- and Player-originated type-6 pings")

        wait_until(
            "safe paired idle status",
            lambda: status_is_paired_and_idle(player_ip, args.http_port),
            args.timeout,
        )
        check_reordering_and_replay(session, player_ip, args.http_port)

        if args.exercise_cw:
            exercise_cw(session, player_ip, args.http_port)
            session_closed = True
        else:
            session.send(UNPAIR)
            wait_until(
                "safe type-8 unpair status",
                lambda: status_is_safely_unpaired(player_ip, args.http_port),
                args.timeout,
            )
            session_closed = True
            print("PASS: default no-CW W1VE v2 discovery, pairing, liveness, replay, and safe-unpair checks passed")
        if args.exercise_cw:
            print("PASS: W1VE v2 diagnostic and explicit controlled CW checks passed")
        return 0
    finally:
        if session is not None and not session_closed:
            try:
                session.send(UNPAIR)
            except OSError:
                pass
        if discovery_listener is not None:
            discovery_listener.close()
        if query_socket is not None:
            query_socket.close()
        if session_socket is not None:
            session_socket.close()


if __name__ == "__main__":
    raise SystemExit(main())
