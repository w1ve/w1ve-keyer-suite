#include "rwk_frame.h"

#include <string.h>

static bool known_type(uint8_t t)
{
    return t == RWK_FRAME_REGISTER || t == RWK_FRAME_SEND || t == RWK_FRAME_RECV ||
           t == RWK_FRAME_KEEPALIVE || t == RWK_FRAME_PAIR || t == RWK_FRAME_PAIRED;
}

static void put_header(uint8_t *out, uint8_t type, uint32_t len)
{
    out[0] = type;
    out[1] = (uint8_t)(len >> 24);
    out[2] = (uint8_t)(len >> 16);
    out[3] = (uint8_t)(len >> 8);
    out[4] = (uint8_t)len;
}

size_t rwk_frame_enc_register(uint8_t *out, size_t cap, const uint8_t key[32])
{
    if (cap < RWK_FRAME_HEADER + 32) {
        return 0;
    }
    put_header(out, RWK_FRAME_REGISTER, 32);
    memcpy(out + RWK_FRAME_HEADER, key, 32);
    return RWK_FRAME_HEADER + 32;
}

size_t rwk_frame_enc_keepalive(uint8_t *out, size_t cap)
{
    if (cap < RWK_FRAME_HEADER) {
        return 0;
    }
    put_header(out, RWK_FRAME_KEEPALIVE, 0);
    return RWK_FRAME_HEADER;
}

size_t rwk_frame_enc_pair(uint8_t *out, size_t cap, const uint8_t key[32], uint16_t udp_port,
                          const char *code)
{
    size_t code_len = 0;
    while (code_len <= RWK_CODE_MAX && code[code_len] != '\0') {
        code_len++;
    }
    if (code_len > RWK_CODE_MAX || cap < RWK_FRAME_HEADER + 34 + code_len) {
        return 0;
    }
    put_header(out, RWK_FRAME_PAIR, (uint32_t)(34 + code_len));
    memcpy(out + RWK_FRAME_HEADER, key, 32);
    out[RWK_FRAME_HEADER + 32] = (uint8_t)(udp_port >> 8);
    out[RWK_FRAME_HEADER + 33] = (uint8_t)udp_port;
    memcpy(out + RWK_FRAME_HEADER + 34, code, code_len);
    return RWK_FRAME_HEADER + 34 + code_len;
}

size_t rwk_frame_enc_send(uint8_t *out, size_t cap, const uint8_t dst[32], const uint8_t *dgram,
                          size_t len)
{
    if (len > RWK_FRAME_MAX_PAYLOAD - 32 || cap < RWK_FRAME_HEADER + 32 + len) {
        return 0;
    }
    put_header(out, RWK_FRAME_SEND, (uint32_t)(32 + len));
    memcpy(out + RWK_FRAME_HEADER, dst, 32);
    if (len > 0) {
        memcpy(out + RWK_FRAME_HEADER + 32, dgram, len);
    }
    return RWK_FRAME_HEADER + 32 + len;
}

bool rwk_frame_dec_paired(const uint8_t *body, size_t len, rwk_paired_t *out)
{
    memset(out, 0, sizeof(*out));
    if (len < 33) {
        return false;
    }
    memcpy(out->peer_key, body, 32);
    switch (body[32]) {
    case 0:
        return true;
    case 4:
        if (len < 32 + 1 + 4 + 2) {
            return false;
        }
        memcpy(&out->ip_be, body + 33, 4);
        out->port = (uint16_t)((body[37] << 8) | body[38]);
        out->has_addr = out->port != 0;
        return true;
    case 6:
        // The station is IPv4 only. A valid IPv6 endpoint is simply not usable, which sends us
        // to the relay exactly as "no address" does.
        return len >= 32 + 1 + 16 + 2;
    default:
        return false;
    }
}

void rwk_parser_init(rwk_parser_t *p, uint8_t *buf, size_t cap)
{
    memset(p, 0, sizeof(*p));
    p->buf = buf;
    p->cap = cap;
}

rwk_parse_status_t rwk_parser_feed(rwk_parser_t *p, const uint8_t *in, size_t n, size_t *used,
                                   rwk_frame_t *ev)
{
    size_t pos = 0;
    *used = 0;

    if (p->ready) {
        // The previous frame was handed out; start the next one.
        p->ready = false;
        p->header_got = 0;
        p->body_got = 0;
        p->discarding = false;
    }

    while (pos < n) {
        if (p->header_got < RWK_FRAME_HEADER) {
            p->header[p->header_got++] = in[pos++];
            if (p->header_got < RWK_FRAME_HEADER) {
                continue;
            }
            p->type = p->header[0];
            p->body_len = ((uint32_t)p->header[1] << 24) | ((uint32_t)p->header[2] << 16) |
                          ((uint32_t)p->header[3] << 8) | p->header[4];
            p->body_got = 0;
            if (!known_type(p->type)) {
                *used = pos;
                return RWK_PARSE_ERR_TYPE;
            }
            if (p->body_len > RWK_FRAME_MAX_PAYLOAD) {
                *used = pos;
                return RWK_PARSE_ERR_LENGTH;
            }
            p->discarding = p->body_len > p->cap;
            if (p->body_len == 0) {
                p->ready = true;
                *used = pos;
                ev->type = p->type;
                ev->body = p->buf;
                ev->len = 0;
                return RWK_PARSE_FRAME;
            }
            continue;
        }

        size_t take = p->body_len - p->body_got;
        if (take > n - pos) {
            take = n - pos;
        }
        if (!p->discarding) {
            memcpy(p->buf + p->body_got, in + pos, take);
        }
        p->body_got += (uint32_t)take;
        pos += take;
        if (p->body_got == p->body_len) {
            p->ready = true;
            *used = pos;
            if (p->discarding) {
                return RWK_PARSE_SKIPPED;
            }
            ev->type = p->type;
            ev->body = p->buf;
            ev->len = p->body_len;
            return RWK_PARSE_FRAME;
        }
    }
    *used = pos;
    return RWK_PARSE_NEED_MORE;
}
