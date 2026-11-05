#include "rwk_stun.h"

#include <string.h>

#define COOKIE 0x2112A442u
#define TYPE_BINDING_REQUEST 0x0001
#define TYPE_BINDING_SUCCESS 0x0101
#define ATTR_MAPPED_ADDRESS 0x0001
#define ATTR_XOR_MAPPED_ADDRESS 0x0020

static uint16_t get16(const uint8_t *p)
{
    return (uint16_t)((p[0] << 8) | p[1]);
}

void rwk_stun_build_request(uint8_t out[RWK_STUN_REQUEST_LEN], const uint8_t txid[12])
{
    out[0] = TYPE_BINDING_REQUEST >> 8;
    out[1] = TYPE_BINDING_REQUEST & 0xFF;
    out[2] = 0;
    out[3] = 0;
    out[4] = COOKIE >> 24;
    out[5] = (COOKIE >> 16) & 0xFF;
    out[6] = (COOKIE >> 8) & 0xFF;
    out[7] = COOKIE & 0xFF;
    memcpy(out + 8, txid, 12);
}

bool rwk_stun_parse_response(const uint8_t *msg, size_t len, const uint8_t txid[12],
                             uint32_t *ip_be, uint16_t *port)
{
    if (len < 20 || get16(msg) != TYPE_BINDING_SUCCESS || (msg[0] & 0xC0) != 0) {
        return false;
    }
    const size_t body = get16(msg + 2);
    if (body + 20 > len || (body & 3) != 0) {
        return false;
    }
    if (msg[4] != 0x21 || msg[5] != 0x12 || msg[6] != 0xA4 || msg[7] != 0x42) {
        return false;
    }
    // Only an answer to our own request counts; a stray or spoofed datagram must not set our port.
    if (memcmp(msg + 8, txid, 12) != 0) {
        return false;
    }

    size_t off = 20;
    bool found = false;
    uint32_t ip = 0;
    uint16_t p = 0;
    while (off + 4 <= 20 + body) {
        const uint16_t type = get16(msg + off);
        const size_t alen = get16(msg + off + 2);
        const size_t padded = (alen + 3) & ~(size_t)3;
        if (off + 4 + alen > 20 + body) {
            return false;
        }
        const uint8_t *a = msg + off + 4;
        if ((type == ATTR_XOR_MAPPED_ADDRESS || type == ATTR_MAPPED_ADDRESS) && alen >= 8 &&
            a[1] == 0x01) {
            uint8_t raw[4];
            memcpy(raw, a + 4, 4);
            p = get16(a + 2);
            if (type == ATTR_XOR_MAPPED_ADDRESS) {
                p ^= (uint16_t)(COOKIE >> 16);
                raw[0] ^= 0x21;
                raw[1] ^= 0x12;
                raw[2] ^= 0xA4;
                raw[3] ^= 0x42;
            }
            memcpy(&ip, raw, 4);
            found = true;
            if (type == ATTR_XOR_MAPPED_ADDRESS) {
                break;  // the XOR form wins when both are present
            }
        }
        off += 4 + padded;
    }
    if (!found || p == 0) {
        return false;
    }
    *ip_be = ip;
    *port = p;
    return true;
}
