// RWK relay framing, ported from common/rwk-core/src/engine/relay/frame.rs.
//
//   +--------+------------------+----------------------------+
//   | type:1 | length:4 (BE)    | payload: `length` bytes    |
//   +--------+------------------+----------------------------+
//
// The parser is incremental: TCP is a byte stream, so a frame may arrive in pieces and several may
// arrive together. Bodies are copied into one caller-owned buffer of fixed size; a body that is
// legal on the wire but larger than that buffer is skipped without being stored (the desktop
// allocates up to 64 KiB, we cannot), and a header the desktop would reject closes the stream.
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define RWK_KEY_BYTES 32
#define RWK_FRAME_HEADER 5
#define RWK_FRAME_MAX_PAYLOAD (64 * 1024)

#define RWK_FRAME_REGISTER 0x02
#define RWK_FRAME_SEND 0x04
#define RWK_FRAME_RECV 0x05
#define RWK_FRAME_KEEPALIVE 0x06
#define RWK_FRAME_PAIR 0x20
#define RWK_FRAME_PAIRED 0x21

// Longest pairing code we will send. The desktop does not bound it; a relay accepts any length.
#define RWK_CODE_MAX 64

// ------------------------------------------------------------ encoding (all return bytes written, 0 if cap too small)

size_t rwk_frame_enc_register(uint8_t *out, size_t cap, const uint8_t key[32]);
size_t rwk_frame_enc_keepalive(uint8_t *out, size_t cap);
size_t rwk_frame_enc_pair(uint8_t *out, size_t cap, const uint8_t key[32], uint16_t udp_port,
                          const char *code);
size_t rwk_frame_enc_send(uint8_t *out, size_t cap, const uint8_t dst[32], const uint8_t *dgram,
                          size_t len);

// ------------------------------------------------------------ decoding

typedef struct {
    bool has_addr;       // IPv4 endpoint present (IPv6 and "none" both read as false)
    uint32_t ip_be;      // network byte order, as lwIP wants it
    uint16_t port;       // host order
    uint8_t peer_key[32];
} rwk_paired_t;

// Body of a Paired frame. Returns false for a malformed body.
bool rwk_frame_dec_paired(const uint8_t *body, size_t len, rwk_paired_t *out);

typedef enum {
    RWK_PARSE_NEED_MORE = 0,
    RWK_PARSE_FRAME,       // ev->type / ev->body / ev->len are valid until the next feed
    RWK_PARSE_SKIPPED,     // a legal frame too large for the buffer was discarded
    RWK_PARSE_ERR_TYPE,    // unknown type byte: close the stream
    RWK_PARSE_ERR_LENGTH,  // length over 64 KiB: close the stream
} rwk_parse_status_t;

typedef struct {
    uint8_t type;
    const uint8_t *body;
    size_t len;
} rwk_frame_t;

typedef struct {
    uint8_t header[RWK_FRAME_HEADER];
    uint8_t header_got;
    uint8_t type;
    uint32_t body_len;
    uint32_t body_got;
    bool discarding;
    bool ready;            // a complete frame is sitting in buf awaiting reset on the next feed
    uint8_t *buf;
    size_t cap;
} rwk_parser_t;

void rwk_parser_init(rwk_parser_t *p, uint8_t *buf, size_t cap);

// Consumes bytes from in[0..n) and stops after the first event. *used is how many bytes were
// consumed; call again with the remainder until it returns NEED_MORE with *used == n.
rwk_parse_status_t rwk_parser_feed(rwk_parser_t *p, const uint8_t *in, size_t n, size_t *used,
                                   rwk_frame_t *ev);
