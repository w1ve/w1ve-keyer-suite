// The only inner traffic this station speaks: IPv4/UDP between the two fixed tunnel addresses
// (rwk-core Role::inner_address) on the edge port, plus the 4/12-byte liveness datagrams that
// share that flow (network.rs EDGE_HEARTBEAT_MAGIC / PING / PONG).
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define RWK_INNER_CLIENT_IP 0x0A060001u   // 10.6.0.1
#define RWK_INNER_STATION_IP 0x0A060002u  // 10.6.0.2
#define RWK_EDGE_PORT 5555

#define RWK_IP_HEADER 20
#define RWK_UDP_HEADER 8

// Builds an IPv4/UDP packet, host-order addresses. Returns bytes written or 0 if cap too small.
size_t rwk_inner_build_udp(uint8_t *out, size_t cap, uint32_t src_ip, uint16_t src_port,
                           uint32_t dst_ip, uint16_t dst_port, const uint8_t *payload,
                           size_t len);

// Parses an IPv4/UDP packet. The packet may carry trailing WireGuard padding; the IP total length
// decides where it ends. Returns false for anything that is not a well-formed UDP/IPv4 packet.
bool rwk_inner_parse_udp(const uint8_t *pkt, size_t len, uint32_t *src_ip, uint16_t *src_port,
                         uint32_t *dst_ip, uint16_t *dst_port, const uint8_t **payload,
                         size_t *payload_len);

typedef enum {
    RWK_EDGE_OTHER = 0,
    RWK_EDGE_HEARTBEAT,   // exactly "RWKH"
    RWK_EDGE_PING,        // "RWKP" + 8 byte stamp
    RWK_EDGE_PONG,        // "RWKQ" + same stamp
    RWK_EDGE_FRAME,       // a well-formed RWK-PADDLE frame (identical to the LAN type 4 payload)
} rwk_edge_kind_t;

rwk_edge_kind_t rwk_edge_classify(const uint8_t *payload, size_t len);

size_t rwk_edge_build_heartbeat(uint8_t out[4]);
// Answers a ping: pong echoes the 8 byte stamp.
size_t rwk_edge_build_pong(uint8_t out[12], const uint8_t ping[12]);
size_t rwk_edge_build_ping(uint8_t out[12], uint64_t stamp_ns);
