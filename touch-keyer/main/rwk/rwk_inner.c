#include "rwk_inner.h"

#include <string.h>

#define EDGE_ENTRY_LEN 12
#define EDGE_HEADER_LEN 4

static void put16(uint8_t *p, uint16_t v)
{
    p[0] = (uint8_t)(v >> 8);
    p[1] = (uint8_t)v;
}

static void put32(uint8_t *p, uint32_t v)
{
    p[0] = (uint8_t)(v >> 24);
    p[1] = (uint8_t)(v >> 16);
    p[2] = (uint8_t)(v >> 8);
    p[3] = (uint8_t)v;
}

static uint16_t get16(const uint8_t *p)
{
    return (uint16_t)((p[0] << 8) | p[1]);
}

static uint32_t get32(const uint8_t *p)
{
    return ((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) | ((uint32_t)p[2] << 8) | p[3];
}

// One's complement sum over big-endian 16-bit words; odd trailing byte is padded with zero.
static uint32_t sum16(const uint8_t *p, size_t n, uint32_t acc)
{
    while (n > 1) {
        acc += (uint32_t)((p[0] << 8) | p[1]);
        p += 2;
        n -= 2;
    }
    if (n == 1) {
        acc += (uint32_t)(p[0] << 8);
    }
    return acc;
}

static uint16_t fold(uint32_t acc)
{
    while (acc >> 16) {
        acc = (acc & 0xFFFF) + (acc >> 16);
    }
    return (uint16_t)~acc;
}

size_t rwk_inner_build_udp(uint8_t *out, size_t cap, uint32_t src_ip, uint16_t src_port,
                           uint32_t dst_ip, uint16_t dst_port, const uint8_t *payload, size_t len)
{
    const size_t total = RWK_IP_HEADER + RWK_UDP_HEADER + len;
    if (total > cap || total > 0xFFFF) {
        return 0;
    }
    memset(out, 0, RWK_IP_HEADER + RWK_UDP_HEADER);
    out[0] = 0x45;
    put16(out + 2, (uint16_t)total);
    out[6] = 0x40;  // don't fragment
    out[8] = 64;
    out[9] = 17;
    put32(out + 12, src_ip);
    put32(out + 16, dst_ip);
    put16(out + 10, fold(sum16(out, RWK_IP_HEADER, 0)));

    uint8_t *udp = out + RWK_IP_HEADER;
    put16(udp, src_port);
    put16(udp + 2, dst_port);
    put16(udp + 4, (uint16_t)(RWK_UDP_HEADER + len));
    if (len > 0) {
        memcpy(udp + RWK_UDP_HEADER, payload, len);
    }
    // Pseudo header: addresses, zero, protocol, UDP length.
    uint8_t pseudo[12];
    put32(pseudo, src_ip);
    put32(pseudo + 4, dst_ip);
    pseudo[8] = 0;
    pseudo[9] = 17;
    put16(pseudo + 10, (uint16_t)(RWK_UDP_HEADER + len));
    uint32_t acc = sum16(pseudo, sizeof(pseudo), 0);
    acc = sum16(udp, RWK_UDP_HEADER + len, acc);
    uint16_t check = fold(acc);
    if (check == 0) {
        check = 0xFFFF;  // zero means "no checksum" in UDP
    }
    put16(udp + 6, check);
    return total;
}

bool rwk_inner_parse_udp(const uint8_t *pkt, size_t len, uint32_t *src_ip, uint16_t *src_port,
                         uint32_t *dst_ip, uint16_t *dst_port, const uint8_t **payload,
                         size_t *payload_len)
{
    if (len < RWK_IP_HEADER + RWK_UDP_HEADER || (pkt[0] >> 4) != 4) {
        return false;
    }
    const size_t ihl = (size_t)(pkt[0] & 0x0F) * 4;
    const size_t total = get16(pkt + 2);
    if (ihl < RWK_IP_HEADER || total < ihl + RWK_UDP_HEADER || total > len || pkt[9] != 17) {
        return false;
    }
    // Fragments are not something this flow produces; refusing them avoids reassembly entirely.
    if ((get16(pkt + 6) & 0x3FFF) != 0) {
        return false;
    }
    const uint8_t *udp = pkt + ihl;
    const size_t udp_len = get16(udp + 4);
    if (udp_len < RWK_UDP_HEADER || ihl + udp_len > total) {
        return false;
    }
    *src_ip = get32(pkt + 12);
    *dst_ip = get32(pkt + 16);
    *src_port = get16(udp);
    *dst_port = get16(udp + 2);
    *payload = udp + RWK_UDP_HEADER;
    *payload_len = udp_len - RWK_UDP_HEADER;
    return true;
}

rwk_edge_kind_t rwk_edge_classify(const uint8_t *p, size_t len)
{
    if (len == 4 && memcmp(p, "RWKH", 4) == 0) {
        return RWK_EDGE_HEARTBEAT;
    }
    if (len == 12) {
        if (memcmp(p, "RWKP", 4) == 0) {
            return RWK_EDGE_PING;
        }
        if (memcmp(p, "RWKQ", 4) == 0) {
            return RWK_EDGE_PONG;
        }
    }
    if (len >= EDGE_HEADER_LEN + EDGE_ENTRY_LEN) {
        const unsigned count = (unsigned)(p[2] | (p[3] << 8));
        if (count >= 1 && count <= 4 && len == EDGE_HEADER_LEN + count * EDGE_ENTRY_LEN) {
            return RWK_EDGE_FRAME;
        }
    }
    return RWK_EDGE_OTHER;
}

size_t rwk_edge_build_heartbeat(uint8_t out[4])
{
    memcpy(out, "RWKH", 4);
    return 4;
}

size_t rwk_edge_build_pong(uint8_t out[12], const uint8_t ping[12])
{
    memcpy(out, "RWKQ", 4);
    memcpy(out + 4, ping + 4, 8);
    return 12;
}

size_t rwk_edge_build_ping(uint8_t out[12], uint64_t stamp_ns)
{
    memcpy(out, "RWKP", 4);
    for (int i = 0; i < 8; i++) {
        out[4 + i] = (uint8_t)(stamp_ns >> (56 - 8 * i));
    }
    return 12;
}
