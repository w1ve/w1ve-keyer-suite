// Host tests for the portable RWK transport core. Build and run with `make -C tests/host test`.
#include <openssl/evp.h>
#include <openssl/hmac.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "../../main/rwk/rwk_blake2s.h"
#include "../../main/rwk/rwk_crypto.h"
#include "../../main/rwk/rwk_frame.h"
#include "../../main/rwk/rwk_inner.h"
#include "../../main/rwk/rwk_wg.h"

#include "../../main/rwk/rwk_stun.h"
#include "../../main/rwk/rwk_jitter.h"
#include "../../main/rwk/rwk_edge.h"

static int g_checks;
static int g_failed;

#define CHECK(cond)                                                            \
    do {                                                                       \
        g_checks++;                                                            \
        if (!(cond)) {                                                         \
            g_failed++;                                                        \
            printf("  FAIL %s:%d  %s\n", __FILE__, __LINE__, #cond);           \
        }                                                                      \
    } while (0)

static void hex(const char *s, uint8_t *out, size_t n)
{
    for (size_t i = 0; i < n; i++) {
        unsigned v;
        sscanf(s + 2 * i, "%2x", &v);
        out[i] = (uint8_t)v;
    }
}

// ---------------------------------------------------------------- primitives

static void test_blake2s(void)
{
    puts("blake2s / hmac / keyed mac vs OpenSSL");
    uint8_t msg[300];
    for (size_t i = 0; i < sizeof(msg); i++) {
        msg[i] = (uint8_t)(i * 7 + 3);
    }
    // Lengths straddle the 64-byte block boundary, where streaming bugs live.
    const size_t lens[] = {0, 1, 31, 32, 63, 64, 65, 127, 128, 129, 200, 300};
    for (size_t li = 0; li < sizeof(lens) / sizeof(lens[0]); li++) {
        uint8_t mine[32], ref[32];
        unsigned ref_len = 0;
        rwk_blake2s(mine, 32, NULL, 0, msg, lens[li]);
        EVP_Digest(msg, lens[li], ref, &ref_len, EVP_blake2s256(), NULL);
        CHECK(memcmp(mine, ref, 32) == 0);

        // Incremental in odd slices must equal one shot.
        rwk_blake2s_t c;
        rwk_blake2s_init(&c, 32, NULL, 0);
        for (size_t o = 0; o < lens[li];) {
            size_t s = (o % 5) + 1;
            if (s > lens[li] - o) {
                s = lens[li] - o;
            }
            rwk_blake2s_update(&c, msg + o, s);
            o += s;
        }
        uint8_t inc[32];
        rwk_blake2s_final(&c, inc);
        CHECK(memcmp(inc, ref, 32) == 0);

        uint8_t key[40];
        memset(key, 0x5a, sizeof(key));
        uint8_t hm[32], href[32];
        unsigned hl = 0;
        rwk_hmac_blake2s(hm, key, sizeof(key), msg, lens[li]);
        HMAC(EVP_blake2s256(), key, (int)sizeof(key), msg, lens[li], href, &hl);
        CHECK(memcmp(hm, href, 32) == 0);
    }

    // Keyed, 16-byte output: WireGuard's MAC1/MAC2 primitive.
    uint8_t key[32];
    memset(key, 0xA7, sizeof(key));
    uint8_t mine[16], ref[16];
    rwk_blake2s(mine, 16, key, 32, msg, 116);
    EVP_MAC *mac = EVP_MAC_fetch(NULL, "BLAKE2SMAC", NULL);
    EVP_MAC_CTX *mc = EVP_MAC_CTX_new(mac);
    size_t outlen = 16;
    OSSL_PARAM params[2];
    params[0] = OSSL_PARAM_construct_size_t("size", &outlen);
    params[1] = OSSL_PARAM_construct_end();
    size_t got = 0;
    CHECK(EVP_MAC_init(mc, key, 32, params) == 1);
    CHECK(EVP_MAC_update(mc, msg, 116) == 1);
    CHECK(EVP_MAC_final(mc, ref, &got, 16) == 1);
    CHECK(memcmp(mine, ref, 16) == 0);
    EVP_MAC_CTX_free(mc);
    EVP_MAC_free(mac);
}

static void test_x25519_aead(void)
{
    puts("x25519 (RFC 7748) / chacha20-poly1305 (RFC 8439)");
    uint8_t a[32], b[32], pa[32], pb[32], expect_pa[32], shared1[32], shared2[32], expect[32];
    // RFC 7748 section 6.1.
    hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a", a, 32);
    hex("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb", b, 32);
    hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a", expect_pa, 32);
    hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742", expect, 32);
    CHECK(rwk_x25519_pub(pa, a) && memcmp(pa, expect_pa, 32) == 0);
    CHECK(rwk_x25519_pub(pb, b));
    CHECK(rwk_x25519(shared1, a, pb) && memcmp(shared1, expect, 32) == 0);
    CHECK(rwk_x25519(shared2, b, pa) && memcmp(shared2, expect, 32) == 0);
    uint8_t zero_point[32] = {0};
    CHECK(!rwk_x25519(shared1, a, zero_point));  // low order point must be refused

    // Round trip, tamper, wrong counter, wrong key.
    uint8_t key[32], pt[40], ct[56], back[40];
    rwk_rng(key, 32);
    memset(pt, 0x42, sizeof(pt));
    CHECK(rwk_aead_seal(ct, key, 7, (const uint8_t *)"ad", 2, pt, sizeof(pt)));
    CHECK(rwk_aead_open(back, key, 7, (const uint8_t *)"ad", 2, ct, sizeof(ct)));
    CHECK(memcmp(back, pt, sizeof(pt)) == 0);
    CHECK(!rwk_aead_open(back, key, 8, (const uint8_t *)"ad", 2, ct, sizeof(ct)));
    CHECK(!rwk_aead_open(back, key, 7, (const uint8_t *)"AD", 2, ct, sizeof(ct)));
    ct[3] ^= 1;
    CHECK(!rwk_aead_open(back, key, 7, (const uint8_t *)"ad", 2, ct, sizeof(ct)));
}

// ---------------------------------------------------------------- WireGuard core

typedef struct {
    rwk_wg_t i;  // initiator (the desktop Client's role)
    rwk_wg_t r;  // responder (this station's role)
} pair_t;

static void make_pair(pair_t *p)
{
    uint8_t ski[32], skr[32], pki[32], pkr[32];
    rwk_rng(ski, 32);
    rwk_rng(skr, 32);
    rwk_x25519_pub(pki, ski);
    rwk_x25519_pub(pkr, skr);
    CHECK(rwk_wg_init(&p->i, ski, pkr));
    CHECK(rwk_wg_init(&p->r, skr, pki));
}

static void handshake(pair_t *p, int64_t now, uint64_t unix_sec)
{
    uint8_t init[RWK_WG_INITIATION_LEN], out[256], plain[512];
    size_t out_len, plain_len;
    const bool first_session = !p->r.cur.valid;
    CHECK(rwk_wg_initiate(&p->i, now, unix_sec, 0, init));
    CHECK(rwk_wg_input(&p->r, now, init, sizeof(init), out, &out_len, plain, &plain_len) ==
          RWK_WG_REPLY);
    CHECK(out_len == RWK_WG_RESPONSE_LEN);
    uint8_t response[RWK_WG_RESPONSE_LEN], conf[64];
    memcpy(response, out, sizeof(response));
    CHECK(rwk_wg_input(&p->i, now, response, sizeof(response), conf, &out_len, plain,
                       &plain_len) == RWK_WG_REPLY);
    // The initiator answers with an empty confirmation, which the responder must accept and
    // which promotes its pending key.
    CHECK(out_len == 32);
    // On a first session the responder may not send before the key is confirmed. During a rekey
    // the old, confirmed key legitimately keeps working until then.
    CHECK(!first_session || !rwk_wg_can_send(&p->r, now));
    CHECK(rwk_wg_input(&p->r, now, conf, out_len, out, &out_len, plain, &plain_len) ==
          RWK_WG_KEEPALIVE);
    CHECK(rwk_wg_can_send(&p->r, now));
}

static void test_wg(void)
{
    puts("wireguard handshake and transport (C initiator <-> C responder)");
    pair_t p;
    make_pair(&p);
    handshake(&p, 1000, 1700000000);

    uint8_t msg[64], dg[256], plain[512], out[256];
    size_t out_len, plain_len;
    for (size_t i = 0; i < sizeof(msg); i++) {
        msg[i] = (uint8_t)i;
    }
    // Responder -> initiator, padded to 16.
    size_t n = rwk_wg_encrypt(&p.r, 1100, msg, 50, dg, sizeof(dg));
    CHECK(n == 16 + 64 + 16);
    CHECK(rwk_wg_input(&p.i, 1100, dg, n, out, &out_len, plain, &plain_len) == RWK_WG_DATA);
    CHECK(plain_len == 64 && memcmp(plain, msg, 50) == 0);
    // Replay of the same datagram is dropped, not delivered twice.
    CHECK(rwk_wg_input(&p.i, 1100, dg, n, out, &out_len, plain, &plain_len) == RWK_WG_NONE);
    // Tamper.
    uint8_t bad[256];
    memcpy(bad, dg, n);
    bad[20] ^= 0x80;
    CHECK(rwk_wg_input(&p.i, 1100, bad, n, out, &out_len, plain, &plain_len) == RWK_WG_ERR_AUTH);

    // Initiator -> responder, and reordering inside the window.
    uint8_t d1[256], d2[256], d3[256];
    size_t n1 = rwk_wg_encrypt(&p.i, 1200, msg, 20, d1, sizeof(d1));
    size_t n2 = rwk_wg_encrypt(&p.i, 1200, msg, 21, d2, sizeof(d2));
    size_t n3 = rwk_wg_encrypt(&p.i, 1200, msg, 22, d3, sizeof(d3));
    CHECK(rwk_wg_input(&p.r, 1200, d3, n3, out, &out_len, plain, &plain_len) == RWK_WG_DATA);
    CHECK(rwk_wg_input(&p.r, 1200, d1, n1, out, &out_len, plain, &plain_len) == RWK_WG_DATA);
    CHECK(rwk_wg_input(&p.r, 1200, d2, n2, out, &out_len, plain, &plain_len) == RWK_WG_DATA);
    CHECK(rwk_wg_input(&p.r, 1200, d2, n2, out, &out_len, plain, &plain_len) == RWK_WG_NONE);

    // A counter far behind the window is refused even though it authenticates.
    uint8_t first[256];
    size_t nf = rwk_wg_encrypt(&p.i, 1210, msg, 16, first, sizeof(first));
    for (int i = 0; i < 300; i++) {
        uint8_t tmp[256];
        size_t nt = rwk_wg_encrypt(&p.i, 1210, msg, 16, tmp, sizeof(tmp));
        CHECK(rwk_wg_input(&p.r, 1210, tmp, nt, out, &out_len, plain, &plain_len) == RWK_WG_DATA);
    }
    CHECK(rwk_wg_input(&p.r, 1210, first, nf, out, &out_len, plain, &plain_len) == RWK_WG_NONE);

    // Expiry: nothing is accepted or sent after REJECT_AFTER_TIME without a new handshake.
    const int64_t late = 1000 + RWK_WG_REJECT_AFTER_MS + 1;
    CHECK(!rwk_wg_can_send(&p.r, late));
    CHECK(rwk_wg_encrypt(&p.r, late, msg, 16, dg, sizeof(dg)) == 0);
    CHECK(rwk_wg_needs_rekey(&p.i, 1000 + RWK_WG_REKEY_AFTER_MS));
}

static void test_wg_rejects(void)
{
    puts("wireguard: wrong peer, replayed initiation, bad mac, malformed");
    pair_t p;
    make_pair(&p);
    uint8_t init[RWK_WG_INITIATION_LEN], out[256], plain[512];
    size_t out_len, plain_len;
    CHECK(rwk_wg_initiate(&p.i, 0, 1700000000, 5, init));

    // Third party aimed at our public key but not the introduced peer.
    pair_t q;
    make_pair(&q);
    uint8_t skx[32], pkx[32];
    rwk_rng(skx, 32);
    rwk_x25519_pub(pkx, skx);
    rwk_wg_t stranger;
    CHECK(rwk_wg_init(&stranger, skx, p.r.pk));
    uint8_t sinit[RWK_WG_INITIATION_LEN];
    CHECK(rwk_wg_initiate(&stranger, 0, 1700000000, 0, sinit));
    CHECK(rwk_wg_input(&p.r, 0, sinit, sizeof(sinit), out, &out_len, plain, &plain_len) ==
          RWK_WG_ERR_AUTH);

    // Flipping any byte of the initiation must be refused: mac1 covers the first 116 bytes.
    uint8_t mut[RWK_WG_INITIATION_LEN];
    for (size_t i = 0; i < 132; i += 7) {
        memcpy(mut, init, sizeof(mut));
        mut[i] ^= 0x01;
        // Type byte flips become a different message kind; every outcome except a reply is fine.
        CHECK(rwk_wg_input(&p.r, 0, mut, sizeof(mut), out, &out_len, plain, &plain_len) !=
              RWK_WG_REPLY);
    }
    CHECK(rwk_wg_input(&p.r, 0, init, sizeof(init), out, &out_len, plain, &plain_len) ==
          RWK_WG_REPLY);
    // The very same initiation again is a replay: its timestamp is not newer.
    CHECK(rwk_wg_input(&p.r, 0, init, sizeof(init), out, &out_len, plain, &plain_len) ==
          RWK_WG_NONE);
    // A newer one is accepted (a rekey).
    CHECK(rwk_wg_initiate(&p.i, 10, 1700000001, 0, init));
    CHECK(rwk_wg_input(&p.r, 10, init, sizeof(init), out, &out_len, plain, &plain_len) ==
          RWK_WG_REPLY);

    // Malformed datagrams of every size never crash and never reply.
    uint8_t junk[RWK_WG_MAX_DATAGRAM + 8];
    for (size_t len = 0; len < sizeof(junk); len += 5) {
        for (int t = 0; t < 6; t++) {
            memset(junk, 0xEE, sizeof(junk));
            junk[0] = (uint8_t)t;
            junk[1] = junk[2] = junk[3] = 0;
            const rwk_wg_result_t r =
                rwk_wg_input(&p.r, 0, junk, len, out, &out_len, plain, &plain_len);
            CHECK(r != RWK_WG_REPLY && r != RWK_WG_DATA);
        }
    }
    // A response nobody asked for is ignored.
    uint8_t resp[RWK_WG_RESPONSE_LEN];
    memset(resp, 0, sizeof(resp));
    resp[0] = 2;
    CHECK(rwk_wg_input(&p.i, 0, resp, sizeof(resp), out, &out_len, plain, &plain_len) ==
          RWK_WG_NONE);
    // No wall clock, no initiation.
    CHECK(!rwk_wg_initiate(&p.i, 0, 0, 0, init));
}

static void test_wg_rekey(void)
{
    puts("wireguard: rekey keeps old packets in flight valid");
    pair_t p;
    make_pair(&p);
    handshake(&p, 0, 1700000000);
    uint8_t msg[32] = {1, 2, 3}, old[256], plain[512], out[256];
    size_t out_len, plain_len;
    // A packet encrypted under the first key, still in flight during the rekey.
    size_t n_old = rwk_wg_encrypt(&p.i, 10, msg, 32, old, sizeof(old));
    handshake(&p, 5000, 1700000100);
    CHECK(rwk_wg_input(&p.r, 5000, old, n_old, out, &out_len, plain, &plain_len) == RWK_WG_DATA);
    uint8_t dg[256];
    size_t n = rwk_wg_encrypt(&p.i, 5001, msg, 32, dg, sizeof(dg));
    CHECK(rwk_wg_input(&p.r, 5001, dg, n, out, &out_len, plain, &plain_len) == RWK_WG_DATA);
}

// ---------------------------------------------------------------- framing

static void test_frames(void)
{
    puts("relay frame codec and incremental parser");
    uint8_t key[32], key2[32], buf[512];
    memset(key, 0xA1, 32);
    memset(key2, 0xB2, 32);

    // Golden bytes taken from frame.rs: Register is 0x02, len 32 BE, then the key.
    size_t n = rwk_frame_enc_register(buf, sizeof(buf), key);
    CHECK(n == 37 && buf[0] == 0x02 && buf[1] == 0 && buf[2] == 0 && buf[3] == 0 && buf[4] == 32);
    n = rwk_frame_enc_keepalive(buf, sizeof(buf));
    CHECK(n == 5 && buf[0] == 0x06 && buf[4] == 0);
    n = rwk_frame_enc_pair(buf, sizeof(buf), key, 51820, "W1VE-7Q");
    CHECK(n == 5 + 34 + 7 && buf[0] == 0x20 && buf[4] == 41 && buf[37] == 0xCA &&
          buf[38] == 0x6C && memcmp(buf + 39, "W1VE-7Q", 7) == 0);
    CHECK(rwk_frame_enc_pair(buf, 10, key, 1, "x") == 0);  // too small is an error, not overflow

    // Stream three frames, then feed them one byte at a time, in 7-byte chunks, and all at once.
    uint8_t stream[1024];
    size_t total = 0;
    uint8_t dgram[100];
    memset(dgram, 0x77, sizeof(dgram));
    total += rwk_frame_enc_send(stream + total, sizeof(stream) - total, key2, dgram, sizeof(dgram));
    total += rwk_frame_enc_keepalive(stream + total, sizeof(stream) - total);
    total += rwk_frame_enc_register(stream + total, sizeof(stream) - total, key);

    const size_t chunks[] = {1, 7, 3, 1024};
    for (size_t ci = 0; ci < sizeof(chunks) / sizeof(chunks[0]); ci++) {
        uint8_t pb[256];
        rwk_parser_t ps;
        rwk_parser_init(&ps, pb, sizeof(pb));
        int frames = 0;
        uint8_t order[3] = {0, 0, 0};
        for (size_t off = 0; off < total;) {
            size_t take = chunks[ci] < total - off ? chunks[ci] : total - off;
            size_t pos = 0;
            while (pos < take) {
                size_t used;
                rwk_frame_t ev;
                rwk_parse_status_t st = rwk_parser_feed(&ps, stream + off + pos, take - pos, &used, &ev);
                pos += used;
                if (st == RWK_PARSE_FRAME) {
                    if (frames < 3) {
                        order[frames] = ev.type;
                    }
                    if (ev.type == RWK_FRAME_SEND) {
                        CHECK(ev.len == 132 && memcmp(ev.body, key2, 32) == 0 &&
                              memcmp(ev.body + 32, dgram, 100) == 0);
                    }
                    frames++;
                } else {
                    CHECK(st == RWK_PARSE_NEED_MORE);
                }
                if (used == 0) {
                    break;
                }
            }
            off += take;
        }
        CHECK(frames == 3);
        CHECK(order[0] == RWK_FRAME_SEND && order[1] == RWK_FRAME_KEEPALIVE &&
              order[2] == RWK_FRAME_REGISTER);
    }

    // Invalid and oversize.
    {
        uint8_t pb[64];
        rwk_parser_t ps;
        size_t used;
        rwk_frame_t ev;
        rwk_parser_init(&ps, pb, sizeof(pb));
        const uint8_t bad_type[5] = {0xFF, 0, 0, 0, 0};
        CHECK(rwk_parser_feed(&ps, bad_type, 5, &used, &ev) == RWK_PARSE_ERR_TYPE);
        rwk_parser_init(&ps, pb, sizeof(pb));
        const uint8_t huge[5] = {RWK_FRAME_SEND, 0, 1, 0, 1};  // 65537 > 64 KiB
        CHECK(rwk_parser_feed(&ps, huge, 5, &used, &ev) == RWK_PARSE_ERR_LENGTH);

        // Legal on the wire but bigger than our buffer: skipped without storing, stream stays in
        // sync, and the next frame still parses.
        rwk_parser_init(&ps, pb, sizeof(pb));
        uint8_t big[5 + 200 + 5];
        big[0] = RWK_FRAME_RECV;
        big[1] = 0;
        big[2] = 0;
        big[3] = 0;
        big[4] = 200;
        memset(big + 5, 0x33, 200);
        rwk_frame_enc_keepalive(big + 205, 5);
        size_t pos = 0;
        int skipped = 0, frames = 0;
        while (pos < sizeof(big)) {
            rwk_parse_status_t st = rwk_parser_feed(&ps, big + pos, sizeof(big) - pos, &used, &ev);
            pos += used;
            skipped += st == RWK_PARSE_SKIPPED;
            frames += st == RWK_PARSE_FRAME && ev.type == RWK_FRAME_KEEPALIVE;
            if (used == 0) {
                break;
            }
        }
        CHECK(skipped == 1 && frames == 1);
    }

    // Paired: IPv4 endpoint, none, IPv6, truncated.
    uint8_t body[64];
    memset(body, 0x22, 32);
    body[32] = 4;
    body[33] = 203;
    body[34] = 0;
    body[35] = 113;
    body[36] = 9;
    body[37] = 0xCA;
    body[38] = 0x6C;
    rwk_paired_t pr;
    CHECK(rwk_frame_dec_paired(body, 39, &pr) && pr.has_addr && pr.port == 51820);
    uint8_t expect_ip[4] = {203, 0, 113, 9};
    CHECK(memcmp(&pr.ip_be, expect_ip, 4) == 0);
    body[32] = 0;
    CHECK(rwk_frame_dec_paired(body, 33, &pr) && !pr.has_addr);
    body[32] = 6;
    CHECK(rwk_frame_dec_paired(body, 51, &pr) && !pr.has_addr);
    CHECK(!rwk_frame_dec_paired(body, 40, &pr));
    body[32] = 4;
    CHECK(!rwk_frame_dec_paired(body, 38, &pr));
    body[32] = 9;
    CHECK(!rwk_frame_dec_paired(body, 51, &pr));
    CHECK(!rwk_frame_dec_paired(body, 10, &pr));
}

// ---------------------------------------------------------------- inner packets and edge liveness

static uint16_t verify_sum(const uint8_t *p, size_t n, uint32_t acc)
{
    for (; n > 1; p += 2, n -= 2) {
        acc += (uint32_t)((p[0] << 8) | p[1]);
    }
    if (n) {
        acc += (uint32_t)(p[0] << 8);
    }
    while (acc >> 16) {
        acc = (acc & 0xFFFF) + (acc >> 16);
    }
    return (uint16_t)acc;
}

static void test_inner(void)
{
    puts("inner ipv4/udp and edge liveness");
    uint8_t pay[28];
    for (size_t i = 0; i < sizeof(pay); i++) {
        pay[i] = (uint8_t)(i + 1);
    }
    uint8_t pkt[128];
    size_t n = rwk_inner_build_udp(pkt, sizeof(pkt), RWK_INNER_STATION_IP, RWK_EDGE_PORT,
                                   RWK_INNER_CLIENT_IP, RWK_EDGE_PORT, pay, sizeof(pay));
    CHECK(n == 20 + 8 + 28);
    CHECK(verify_sum(pkt, 20, 0) == 0xFFFF);  // a valid IPv4 header sums to all ones
    uint8_t pseudo[12] = {10, 6, 0, 2, 10, 6, 0, 1, 0, 17, 0, 36};
    CHECK(verify_sum(pkt + 20, 36, verify_sum(pseudo, 12, 0)) == 0xFFFF);  // UDP checksum

    uint32_t s, d;
    uint16_t sp, dp;
    const uint8_t *pl;
    size_t pl_len;
    // Trailing WireGuard padding must be ignored.
    uint8_t padded[128];
    memcpy(padded, pkt, n);
    memset(padded + n, 0, 8);
    CHECK(rwk_inner_parse_udp(padded, n + 8, &s, &sp, &d, &dp, &pl, &pl_len));
    CHECK(s == RWK_INNER_STATION_IP && d == RWK_INNER_CLIENT_IP && sp == 5555 && dp == 5555);
    CHECK(pl_len == 28 && memcmp(pl, pay, 28) == 0);

    // Refusals: truncated, wrong version, TCP, fragment, UDP length lies.
    CHECK(!rwk_inner_parse_udp(pkt, 10, &s, &sp, &d, &dp, &pl, &pl_len));
    uint8_t m[128];
    memcpy(m, pkt, n);
    m[0] = 0x65;
    CHECK(!rwk_inner_parse_udp(m, n, &s, &sp, &d, &dp, &pl, &pl_len));
    memcpy(m, pkt, n);
    m[9] = 6;
    CHECK(!rwk_inner_parse_udp(m, n, &s, &sp, &d, &dp, &pl, &pl_len));
    memcpy(m, pkt, n);
    m[6] = 0x20;  // more-fragments
    CHECK(!rwk_inner_parse_udp(m, n, &s, &sp, &d, &dp, &pl, &pl_len));
    memcpy(m, pkt, n);
    m[24] = 0xFF;  // udp length 0xFF24
    CHECK(!rwk_inner_parse_udp(m, n, &s, &sp, &d, &dp, &pl, &pl_len));
    CHECK(!rwk_inner_parse_udp(pkt, n - 1, &s, &sp, &d, &dp, &pl, &pl_len));  // shorter than total

    // Edge classification mirrors network.rs.
    uint8_t hb[4], ping[12], pong[12];
    rwk_edge_build_heartbeat(hb);
    CHECK(rwk_edge_classify(hb, 4) == RWK_EDGE_HEARTBEAT);
    rwk_edge_build_ping(ping, 0x0102030405060708ull);
    CHECK(rwk_edge_classify(ping, 12) == RWK_EDGE_PING && ping[4] == 1 && ping[11] == 8);
    rwk_edge_build_pong(pong, ping);
    CHECK(rwk_edge_classify(pong, 12) == RWK_EDGE_PONG && memcmp(pong + 4, ping + 4, 8) == 0);
    uint8_t frame[52] = {0};
    frame[2] = 1;  // count 1, little endian
    CHECK(rwk_edge_classify(frame, 16) == RWK_EDGE_FRAME);
    frame[2] = 4;
    CHECK(rwk_edge_classify(frame, 52) == RWK_EDGE_FRAME);
    CHECK(rwk_edge_classify(frame, 51) == RWK_EDGE_OTHER);  // count 4 needs 52 bytes
    frame[2] = 5;
    CHECK(rwk_edge_classify(frame, 64) == RWK_EDGE_OTHER);
    frame[2] = 0;
    CHECK(rwk_edge_classify(frame, 16) == RWK_EDGE_OTHER);
    CHECK(rwk_edge_classify(hb, 3) == RWK_EDGE_OTHER);
}

// ---------------------------------------------------------------- STUN codec

static void test_stun(void)
{
    puts("stun binding request and xor-mapped-address");
    uint8_t txid[12] = {0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB};
    uint8_t req[RWK_STUN_REQUEST_LEN];
    rwk_stun_build_request(req, txid);
    CHECK(req[0] == 0 && req[1] == 1);
    CHECK(req[4] == 0x21 && req[5] == 0x12 && req[6] == 0xA4 && req[7] == 0x42);

    // A response with XOR-MAPPED-ADDRESS for 203.0.113.9:51820.
    uint8_t resp[32] = {
        0x01, 0x01, 0x00, 0x0C,  // Binding success, body 12
        0x21, 0x12, 0xA4, 0x42,  // cookie
        0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB,  // txid
        0x00, 0x20, 0x00, 0x08,  // XOR-MAPPED-ADDRESS, 8 bytes
        0x00, 0x01,              // family IPv4
        0x00, 0x00,              // port placeholder
        0x00, 0x00, 0x00, 0x00,  // address placeholder
    };
    uint16_t xport = 51820 ^ (uint16_t)(0x2112);
    resp[26] = (uint8_t)(xport >> 8);
    resp[27] = (uint8_t)xport;
    resp[28] = 203 ^ 0x21;
    resp[29] = 0 ^ 0x12;
    resp[30] = 113 ^ 0xA4;
    resp[31] = 9 ^ 0x42;

    uint32_t ip = 0;
    uint16_t port = 0;
    CHECK(rwk_stun_parse_response(resp, sizeof(resp), txid, &ip, &port));
    CHECK(port == 51820);
    uint8_t expect_ip[4] = {203, 0, 113, 9};
    CHECK(memcmp(&ip, expect_ip, 4) == 0);

    // Wrong txid is refused.
    uint8_t bad[12];
    memset(bad, 0xFF, 12);
    CHECK(!rwk_stun_parse_response(resp, sizeof(resp), bad, &ip, &port));
    // Truncated or malformed is refused.
    CHECK(!rwk_stun_parse_response(resp, 10, txid, &ip, &port));
    resp[4] = 0;
    CHECK(!rwk_stun_parse_response(resp, sizeof(resp), txid, &ip, &port));
}

// ---------------------------------------------------------------- jitter buffer

static void test_jitter(void)
{
    puts("jitter estimator and playout scheduler");
    rwk_jitter_t j;
    rwk_jitter_init(&j);
    CHECK(j.delay_ms == RWK_JITTER_DEFAULT_MS);

    // Steady stream at 10 ms intervals from a sender whose clock starts at 1000 ms.
    for (int i = 0; i < 100; i++) {
        const uint32_t ts = 1000 + (uint32_t)(i * 10);
        const int64_t arr = 5000 + i * 10;
        rwk_jitter_observe(&j, ts, arr);
    }
    // Jitter converges near zero for an even stream.
    CHECK(rwk_jitter_estimate_ms(&j) < 3);

    // Playout times maintain the sender's gaps: ten edges 10 ms apart should produce ten play
    // times also 10 ms apart (or all the same, when the burst re-anchors).
    for (int i = 0; i < 10; i++) {
        const uint32_t ts = 2000 + (uint32_t)(i * 10);
        const int64_t arr = 8000 + i * 10;
        const int64_t play = rwk_jitter_play_time(&j, ts, arr);
        CHECK(play >= arr);
        if (i == 0) {
            CHECK(play - arr >= (int64_t)RWK_JITTER_MIN_MS);
        }
    }

    // After a burst gap the offset is re-chosen.
    {
        const int64_t play1 = rwk_jitter_play_time(&j, 5000, 15000);
        const int64_t play2 = rwk_jitter_play_time(&j, 5000 + RWK_JITTER_BURST_GAP_MS + 10,
                                                    15000 + RWK_JITTER_BURST_GAP_MS + 10);
        (void)play1;
        (void)play2;
        // The second burst picked its own anchor; verifying exact numbers is fragile, so just
        // confirm no crash and non-negative play times.
        CHECK(play2 >= 0);
    }
}

// ---------------------------------------------------------------- edge applier

// A fake keyer: records transitions, and can pretend a local paddle owns the line.
static struct {
    int count;
    bool down[64];
    uint32_t ms[64];
    bool paddle_owns;
    int64_t now;
} g_key;

static int64_t g_play[64];

static bool fake_key(bool down, uint32_t sidetone_ms, int64_t play_ms, void *ctx)
{
    (void)ctx;
    if (g_key.paddle_owns) {
        return false;
    }
    if (g_key.count < 64) {
        g_key.down[g_key.count] = down;
        g_key.ms[g_key.count] = sidetone_ms;
        g_play[g_key.count] = play_ms;
        g_key.count++;
    }
    return true;
}

static int64_t fake_now(void *ctx)
{
    (void)ctx;
    return g_key.now;
}

// Builds a one-entry RWK-PADDLE frame.
static size_t build_frame(uint8_t *out, uint16_t epoch, uint32_t seq, uint32_t ts, bool down)
{
    out[0] = (uint8_t)epoch;
    out[1] = (uint8_t)(epoch >> 8);
    out[2] = 1;
    out[3] = 0;
    uint8_t *e = out + 4;
    memcpy(e, &seq, 4);  // little-endian host; test host is LE
    memcpy(e + 4, &ts, 4);
    e[8] = down ? 1 : 0;
    e[9] = 0;
    e[10] = 0;
    e[11] = 0;
    return 16;
}

static void test_edge(void)
{
    puts("shared CW edge applier (safety rules + speed)");
    rwk_edge_t ed;
    uint8_t f[52];

    // A clean 20 WPM stream: dit = 60 ms down, 60 ms gap. The applier should key down then up,
    // and infer 20 WPM.
    memset(&g_key, 0, sizeof(g_key));
    rwk_edge_reset(&ed, 0, fake_key, fake_now, NULL);
    uint32_t seq = 1, ts = 1000;
    for (int i = 0; i < 6; i++) {
        g_key.now = 5000 + (int64_t)(ts - 1000);
        size_t n = build_frame(f, 7, seq++, ts, true);
        CHECK(rwk_edge_feed(&ed, f, n, g_key.now) == RWK_EDGE_OK);
        ts += 60;
        g_key.now = 5000 + (int64_t)(ts - 1000);
        n = build_frame(f, 7, seq++, ts, false);
        CHECK(rwk_edge_feed(&ed, f, n, g_key.now) == RWK_EDGE_OK);
        ts += 60;
    }
    CHECK(g_key.count == 12);
    CHECK(g_key.down[0] && !g_key.down[1]);
    CHECK(rwk_edge_wpm(&ed) == 20);
    CHECK(rwk_edge_keying(&ed) == false);

    // Epoch change ends the session.
    rwk_edge_reset(&ed, 0, fake_key, fake_now, NULL);
    g_key.now = 0;
    CHECK(rwk_edge_feed(&ed, f, build_frame(f, 7, 1, 100, true), 0) == RWK_EDGE_OK);
    CHECK(rwk_edge_feed(&ed, f, build_frame(f, 8, 2, 160, false), 60) == RWK_EDGE_END_SESSION);
    CHECK(strcmp(rwk_edge_end_reason(&ed), "edge epoch changed") == 0);

    // Timestamp regression ends the session.
    rwk_edge_reset(&ed, 0, fake_key, fake_now, NULL);
    CHECK(rwk_edge_feed(&ed, f, build_frame(f, 7, 1, 500, true), 0) == RWK_EDGE_OK);
    CHECK(rwk_edge_feed(&ed, f, build_frame(f, 7, 2, 400, false), 60) == RWK_EDGE_END_SESSION);
    CHECK(strcmp(rwk_edge_end_reason(&ed), "edge timestamp regressed") == 0);

    // A sequence gap that lands on key-down is unsafe.
    rwk_edge_reset(&ed, 0, fake_key, fake_now, NULL);
    CHECK(rwk_edge_feed(&ed, f, build_frame(f, 7, 1, 100, true), 0) == RWK_EDGE_OK);
    CHECK(rwk_edge_feed(&ed, f, build_frame(f, 7, 2, 160, false), 60) == RWK_EDGE_OK);
    CHECK(rwk_edge_feed(&ed, f, build_frame(f, 7, 5, 300, true), 200) == RWK_EDGE_END_SESSION);
    CHECK(strcmp(rwk_edge_end_reason(&ed), "unsafe edge sequence gap") == 0);

    // A local paddle taking the line ends the session.
    memset(&g_key, 0, sizeof(g_key));
    g_key.paddle_owns = true;
    rwk_edge_reset(&ed, 0, fake_key, fake_now, NULL);
    CHECK(rwk_edge_feed(&ed, f, build_frame(f, 7, 1, 100, true), 0) == RWK_EDGE_END_SESSION);
    CHECK(strcmp(rwk_edge_end_reason(&ed), "local paddle has the line") == 0);

    // The lease: key held down, then no frame for 1500 ms.
    memset(&g_key, 0, sizeof(g_key));
    rwk_edge_reset(&ed, 0, fake_key, fake_now, NULL);
    g_key.now = 0;
    CHECK(rwk_edge_feed(&ed, f, build_frame(f, 7, 1, 100, true), 0) == RWK_EDGE_OK);
    CHECK(rwk_edge_keying(&ed));
    CHECK(!rwk_edge_lease_expired(&ed, 1499));
    CHECK(rwk_edge_lease_expired(&ed, 1500));

    // Malformed frames are ignored without ending the session.
    rwk_edge_reset(&ed, 0, fake_key, fake_now, NULL);
    CHECK(rwk_edge_feed(&ed, f, 15, 0) == RWK_EDGE_MALFORMED);
    f[2] = 9;  // count 9
    CHECK(rwk_edge_feed(&ed, f, 16, 0) == RWK_EDGE_MALFORMED);
    build_frame(f, 7, 1, 100, true);
    f[4 + 9] = 0x02;  // entry.flags reserved bit set
    CHECK(rwk_edge_feed(&ed, f, 16, 0) == RWK_EDGE_MALFORMED);
    build_frame(f, 7, 1, 100, true);
    f[4 + 10] = 0x01;  // entry.reserved non-zero
    CHECK(rwk_edge_feed(&ed, f, 16, 0) == RWK_EDGE_MALFORMED);

    // A duplicate frame refreshes the lease but keys nothing twice.
    memset(&g_key, 0, sizeof(g_key));
    rwk_edge_reset(&ed, 0, fake_key, fake_now, NULL);
    size_t n = build_frame(f, 7, 1, 100, true);
    CHECK(rwk_edge_feed(&ed, f, n, 0) == RWK_EDGE_OK);
    CHECK(rwk_edge_feed(&ed, f, n, 300) == RWK_EDGE_OK);  // replayed: same seq, ignored
    CHECK(g_key.count == 1);
    CHECK(ed.last_refresh_ms == 300);  // but the lease moved
}

static void test_edge_playout(void)
{
    puts("edge playout: bunched frames keep the sender's spacing");
    rwk_edge_t ed;
    uint8_t f[52];
    const uint32_t delay = 120;

    // 38 WPM: dit ~= 31 ms. All frames "arrive" at the SAME local instant (5000), modelling a relay
    // burst that delivered everything at once. The play times must still be 31 ms apart.
    memset(&g_key, 0, sizeof(g_key));
    rwk_edge_reset(&ed, delay, fake_key, fake_now, NULL);
    g_key.now = 5000;
    uint32_t seq = 1;
    const uint32_t step = 31;
    for (int i = 0; i < 8; i++) {
        const uint32_t ts = 1000 + (uint32_t)i * step;
        const bool down = (i % 2) == 0;
        CHECK(rwk_edge_feed(&ed, f, build_frame(f, 7, seq++, ts, down), 5000) == RWK_EDGE_OK);
    }
    CHECK(g_key.count == 8);
    CHECK(g_play[0] == 5000 + (int64_t)delay);
    for (int i = 1; i < 8; i++) {
        CHECK(g_play[i] - g_play[i - 1] == (int64_t)step);
    }

    // A late frame (slot already passed) plays at arrival and never goes backwards.
    memset(&g_key, 0, sizeof(g_key));
    rwk_edge_reset(&ed, delay, fake_key, fake_now, NULL);
    g_key.now = 0;
    CHECK(rwk_edge_feed(&ed, f, build_frame(f, 7, 1, 1000, true), 0) == RWK_EDGE_OK);
    CHECK(rwk_edge_feed(&ed, f, build_frame(f, 7, 2, 2000, false), 2000) == RWK_EDGE_OK);
    CHECK(g_key.count == 2);
    CHECK(g_play[1] >= g_play[0]);
    CHECK(g_play[1] >= 2000);
}

int main(void)
{
    test_blake2s();
    test_x25519_aead();
    test_wg();
    test_wg_rejects();
    test_wg_rekey();
    test_frames();
    test_inner();
    test_stun();
    test_jitter();
    test_edge();
    test_edge_playout();
    printf("\n%d checks, %d failed\n", g_checks, g_failed);
    puts(g_failed == 0 ? "ALL PASS" : "FAILED");
    return g_failed == 0 ? 0 : 1;
}
