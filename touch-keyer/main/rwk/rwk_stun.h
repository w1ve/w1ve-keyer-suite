// Minimal RFC 5389 STUN client codec: one Binding Request, one XOR-MAPPED-ADDRESS answer.
//
// Used to learn the external UDP port a NAT assigned to the WireGuard socket, because the relay
// introduces peers as (TCP-observed IP, the port we report), and the port we bind is not
// necessarily the port the NAT maps it to. IPv4 only.
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define RWK_STUN_REQUEST_LEN 20

// Writes a Binding Request using the given 12-byte transaction id.
void rwk_stun_build_request(uint8_t out[RWK_STUN_REQUEST_LEN], const uint8_t txid[12]);

// Parses a Binding success response for txid. On success returns true with the mapped endpoint
// (ip in network byte order, port in host order).
bool rwk_stun_parse_response(const uint8_t *msg, size_t len, const uint8_t txid[12],
                             uint32_t *ip_be, uint16_t *port);
