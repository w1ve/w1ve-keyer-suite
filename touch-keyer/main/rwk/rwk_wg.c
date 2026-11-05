#include "rwk_wg.h"

#include <string.h>

#include "rwk_blake2s.h"
#include "rwk_crypto.h"

static const char CONSTRUCTION[] = "Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s";
static const char IDENTIFIER[] = "WireGuard v1 zx2c4 Jason@zx2c4.com";
static const char LABEL_MAC1[] = "mac1----";

#define MSG_INITIATION 1
#define MSG_RESPONSE 2
#define MSG_DATA 4
#define REJECT_AFTER_MESSAGES (UINT64_MAX - 0x2000u)

// ---------------------------------------------------------------- helpers

static void put32(uint8_t *p, uint32_t v)
{
    p[0] = (uint8_t)v;
    p[1] = (uint8_t)(v >> 8);
    p[2] = (uint8_t)(v >> 16);
    p[3] = (uint8_t)(v >> 24);
}

static uint32_t get32(const uint8_t *p)
{
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24);
}

static void put64(uint8_t *p, uint64_t v)
{
    for (int i = 0; i < 8; i++) {
        p[i] = (uint8_t)(v >> (8 * i));
    }
}

static uint64_t get64(const uint8_t *p)
{
    uint64_t v = 0;
    for (int i = 7; i >= 0; i--) {
        v = (v << 8) | p[i];
    }
    return v;
}

// Constant-time compare, so a wrong MAC or key reveals nothing through timing.
static bool ct_equal(const uint8_t *a, const uint8_t *b, size_t n)
{
    uint8_t diff = 0;
    for (size_t i = 0; i < n; i++) {
        diff |= (uint8_t)(a[i] ^ b[i]);
    }
    return diff == 0;
}

static void hash2(uint8_t out[32], const uint8_t *a, size_t alen, const uint8_t *b, size_t blen)
{
    rwk_blake2s_t ctx;
    rwk_blake2s_init(&ctx, 32, NULL, 0);
    rwk_blake2s_update(&ctx, a, alen);
    if (blen > 0) {
        rwk_blake2s_update(&ctx, b, blen);
    }
    rwk_blake2s_final(&ctx, out);
    rwk_wipe(&ctx, sizeof(ctx));
}

// h = HASH(h | data)
static void mix_hash(uint8_t h[32], const uint8_t *data, size_t len)
{
    hash2(h, h, 32, data, len);
}

// HKDF as WireGuard defines it: PRK = HMAC(c, input); T1 = HMAC(PRK, 0x1); Tn = HMAC(PRK, Tn-1 | n).
static void kdf(const uint8_t c[32], const uint8_t *in, size_t in_len, uint8_t *t1, uint8_t *t2,
                uint8_t *t3)
{
    uint8_t prk[32];
    uint8_t buf[33];
    rwk_hmac_blake2s(prk, c, 32, in, in_len);
    buf[0] = 1;
    rwk_hmac_blake2s(t1, prk, 32, buf, 1);
    if (t2 != NULL) {
        memcpy(buf, t1, 32);
        buf[32] = 2;
        rwk_hmac_blake2s(t2, prk, 32, buf, 33);
    }
    if (t3 != NULL) {
        memcpy(buf, t2, 32);
        buf[32] = 3;
        rwk_hmac_blake2s(t3, prk, 32, buf, 33);
    }
    rwk_wipe(prk, sizeof(prk));
    rwk_wipe(buf, sizeof(buf));
}

static void mix_key(uint8_t c[32], uint8_t k[32], const uint8_t *in, size_t in_len)
{
    uint8_t new_c[32];
    kdf(c, in, in_len, new_c, k, NULL);
    memcpy(c, new_c, 32);
    rwk_wipe(new_c, sizeof(new_c));
}

static void mix_chain(uint8_t c[32], const uint8_t *in, size_t in_len)
{
    uint8_t new_c[32];
    kdf(c, in, in_len, new_c, NULL, NULL);
    memcpy(c, new_c, 32);
    rwk_wipe(new_c, sizeof(new_c));
}

static void mac1(uint8_t out[16], const uint8_t key[32], const uint8_t *msg, size_t len)
{
    rwk_blake2s(out, 16, key, 32, msg, len);
}

static void keypair_clear(rwk_wg_keypair_t *kp)
{
    rwk_wipe(kp, sizeof(*kp));
}

// ---------------------------------------------------------------- replay window

static bool window_accept(rwk_wg_keypair_t *kp, uint64_t counter)
{
    if (counter >= REJECT_AFTER_MESSAGES) {
        return false;
    }
    // Bit i of the window means "recv_max - i was received". Nothing received yet is all zeros
    // with recv_max 0, so a first packet numbered 0 falls through to the bit test below.
    if (counter > kp->recv_max) {
        const uint64_t shift = counter - kp->recv_max;
        if (shift >= 256) {
            memset(kp->window, 0, sizeof(kp->window));
        } else if (shift > 0) {
            const unsigned words = (unsigned)(shift / 64);
            const unsigned bits = (unsigned)(shift % 64);
            for (int i = 3; i >= 0; i--) {
                uint64_t v = 0;
                const int src = i - (int)words;
                if (src >= 0) {
                    v = kp->window[src] << bits;
                    if (bits != 0 && src - 1 >= 0) {
                        v |= kp->window[src - 1] >> (64 - bits);
                    }
                }
                kp->window[i] = v;
            }
        }
        kp->window[0] |= 1u;
        kp->recv_max = counter;
        return true;
    }
    const uint64_t back = kp->recv_max - counter;
    if (back >= 256) {
        return false;
    }
    uint64_t *word = &kp->window[back / 64];
    const uint64_t bit = 1ull << (back % 64);
    if ((*word & bit) != 0) {
        return false;
    }
    *word |= bit;
    return true;
}

// ---------------------------------------------------------------- setup

bool rwk_wg_init(rwk_wg_t *w, const uint8_t sk[32], const uint8_t peer_pk[32])
{
    memset(w, 0, sizeof(*w));
    memcpy(w->sk, sk, 32);
    memcpy(w->peer_pk, peer_pk, 32);
    if (!rwk_x25519_pub(w->pk, w->sk) || !rwk_x25519(w->ss, w->sk, w->peer_pk)) {
        rwk_wg_free(w);
        return false;
    }
    hash2(w->mac1_key_self, (const uint8_t *)LABEL_MAC1, sizeof(LABEL_MAC1) - 1, w->pk, 32);
    hash2(w->mac1_key_peer, (const uint8_t *)LABEL_MAC1, sizeof(LABEL_MAC1) - 1, w->peer_pk, 32);

    rwk_blake2s(w->c0, 32, NULL, 0, (const uint8_t *)CONSTRUCTION, sizeof(CONSTRUCTION) - 1);
    uint8_t h0[32];
    hash2(h0, w->c0, 32, (const uint8_t *)IDENTIFIER, sizeof(IDENTIFIER) - 1);
    hash2(w->h_resp, h0, 32, w->pk, 32);
    hash2(w->h_init, h0, 32, w->peer_pk, 32);
    return true;
}

void rwk_wg_free(rwk_wg_t *w)
{
    rwk_wipe(w, sizeof(*w));
}

// ---------------------------------------------------------------- initiator

bool rwk_wg_initiate(rwk_wg_t *w, int64_t now_ms, uint64_t unix_sec, uint32_t nanos,
                     uint8_t out[RWK_WG_INITIATION_LEN])
{
    (void)now_ms;
    if (unix_sec == 0) {
        return false;
    }
    uint8_t c[32], h[32], k[32], dh[32];
    uint8_t eph_pub[32];
    uint8_t index_bytes[4];

    rwk_rng(w->hs_eph_priv, 32);
    if (!rwk_x25519_pub(eph_pub, w->hs_eph_priv)) {
        return false;
    }
    rwk_rng(index_bytes, 4);
    w->hs_index = get32(index_bytes);

    memset(out, 0, RWK_WG_INITIATION_LEN);
    out[0] = MSG_INITIATION;
    put32(out + 4, w->hs_index);
    memcpy(out + 8, eph_pub, 32);

    memcpy(c, w->c0, 32);
    memcpy(h, w->h_init, 32);
    mix_chain(c, eph_pub, 32);
    mix_hash(h, eph_pub, 32);

    if (!rwk_x25519(dh, w->hs_eph_priv, w->peer_pk)) {
        return false;
    }
    mix_key(c, k, dh, 32);
    if (!rwk_aead_seal(out + 40, k, 0, h, 32, w->pk, 32)) {
        return false;
    }
    mix_hash(h, out + 40, 48);

    mix_key(c, k, w->ss, 32);
    uint8_t ts[12];
    const uint64_t tai = 0x400000000000000aull + unix_sec;
    for (int i = 0; i < 8; i++) {
        ts[i] = (uint8_t)(tai >> (56 - 8 * i));
    }
    ts[8] = (uint8_t)(nanos >> 24);
    ts[9] = (uint8_t)(nanos >> 16);
    ts[10] = (uint8_t)(nanos >> 8);
    ts[11] = (uint8_t)nanos;
    if (!rwk_aead_seal(out + 88, k, 0, h, 32, ts, 12)) {
        return false;
    }
    mix_hash(h, out + 88, 28);

    mac1(out + 116, w->mac1_key_peer, out, 116);
    memcpy(w->hs_c, c, 32);
    memcpy(w->hs_h, h, 32);
    w->hs_pending = true;

    rwk_wipe(c, 32);
    rwk_wipe(h, 32);
    rwk_wipe(k, 32);
    rwk_wipe(dh, 32);
    return true;
}

static void install_keypair(rwk_wg_t *w, rwk_wg_keypair_t *slot, const uint8_t c[32],
                            bool initiator, uint32_t local_index, uint32_t remote_index,
                            int64_t now_ms)
{
    uint8_t t1[32], t2[32];
    kdf(c, NULL, 0, t1, t2, NULL);
    (void)w;
    keypair_clear(slot);
    slot->valid = true;
    slot->initiator = initiator;
    slot->confirmed = initiator;
    slot->local_index = local_index;
    slot->remote_index = remote_index;
    // The first output keys the initiator's sending direction.
    memcpy(slot->send_key, initiator ? t1 : t2, 32);
    memcpy(slot->recv_key, initiator ? t2 : t1, 32);
    slot->born_ms = now_ms;
    rwk_wipe(t1, 32);
    rwk_wipe(t2, 32);
}

static rwk_wg_result_t handle_response(rwk_wg_t *w, int64_t now_ms, const uint8_t *msg,
                                       uint8_t *out, size_t *out_len)
{
    if (!w->hs_pending || get32(msg + 8) != w->hs_index) {
        return RWK_WG_NONE;
    }
    uint8_t expect[16];
    mac1(expect, w->mac1_key_self, msg, 60);
    if (!ct_equal(expect, msg + 60, 16)) {
        return RWK_WG_ERR_AUTH;
    }
    uint8_t c[32], h[32], k[32], tau[32], dh[32];
    memcpy(c, w->hs_c, 32);
    memcpy(h, w->hs_h, 32);
    const uint8_t *eph = msg + 12;
    mix_chain(c, eph, 32);
    mix_hash(h, eph, 32);

    rwk_wg_result_t result = RWK_WG_ERR_AUTH;
    if (!rwk_x25519(dh, w->hs_eph_priv, eph)) {
        goto done;
    }
    mix_chain(c, dh, 32);
    if (!rwk_x25519(dh, w->sk, eph)) {
        goto done;
    }
    mix_chain(c, dh, 32);

    // Q is the all-zero pre-shared key; TAU feeds the hash, K protects the empty payload.
    static const uint8_t zero_psk[32] = {0};
    {
        uint8_t new_c[32];
        kdf(c, zero_psk, 32, new_c, tau, k);
        memcpy(c, new_c, 32);
        rwk_wipe(new_c, 32);
    }
    mix_hash(h, tau, 32);
    uint8_t nothing[1];
    if (!rwk_aead_open(nothing, k, 0, h, 32, msg + 44, 16)) {
        goto done;
    }
    mix_hash(h, msg + 44, 16);

    // A previous current key becomes the receive-only fallback for packets still in flight.
    if (w->cur.valid) {
        w->prev = w->cur;
    }
    install_keypair(w, &w->cur, c, true, w->hs_index, get32(msg + 4), now_ms);
    w->hs_pending = false;
    rwk_wipe(w->hs_eph_priv, 32);
    rwk_wipe(w->hs_c, 32);
    rwk_wipe(w->hs_h, 32);

    // The initiator confirms the session with an empty packet, as boringtun does.
    *out_len = rwk_wg_encrypt(w, now_ms, NULL, 0, out, RWK_WG_INITIATION_LEN);
    result = *out_len > 0 ? RWK_WG_REPLY : RWK_WG_ERR_AUTH;

done:
    rwk_wipe(c, 32);
    rwk_wipe(h, 32);
    rwk_wipe(k, 32);
    rwk_wipe(tau, 32);
    rwk_wipe(dh, 32);
    return result;
}

// ---------------------------------------------------------------- responder

static rwk_wg_result_t handle_initiation(rwk_wg_t *w, int64_t now_ms, const uint8_t *msg,
                                         uint8_t *out, size_t *out_len)
{
    uint8_t expect[16];
    mac1(expect, w->mac1_key_self, msg, 116);
    if (!ct_equal(expect, msg + 116, 16)) {
        return RWK_WG_ERR_AUTH;
    }

    uint8_t c[32], h[32], k[32], dh[32];
    uint8_t peer_static[32];
    uint8_t ts[12];
    uint8_t eph_priv[32], eph_pub[32];
    rwk_wg_result_t result = RWK_WG_ERR_AUTH;
    const uint8_t *ei = msg + 8;

    memcpy(c, w->c0, 32);
    memcpy(h, w->h_resp, 32);
    mix_chain(c, ei, 32);
    mix_hash(h, ei, 32);

    if (!rwk_x25519(dh, w->sk, ei)) {
        goto done;
    }
    mix_key(c, k, dh, 32);
    if (!rwk_aead_open(peer_static, k, 0, h, 32, msg + 40, 48)) {
        goto done;
    }
    // Only the one peer the rendezvous introduced may talk to us.
    if (!ct_equal(peer_static, w->peer_pk, 32)) {
        goto done;
    }
    mix_hash(h, msg + 40, 48);

    mix_key(c, k, w->ss, 32);
    if (!rwk_aead_open(ts, k, 0, h, 32, msg + 88, 28)) {
        goto done;
    }
    // TAI64N is big-endian, so a byte compare orders it. A replayed initiation is not newer.
    if (w->have_ts && memcmp(ts, w->last_ts, 12) <= 0) {
        result = RWK_WG_NONE;
        goto done;
    }
    mix_hash(h, msg + 88, 28);

    // Response.
    rwk_rng(eph_priv, 32);
    if (!rwk_x25519_pub(eph_pub, eph_priv)) {
        goto done;
    }
    uint8_t index_bytes[4];
    rwk_rng(index_bytes, 4);
    const uint32_t local_index = get32(index_bytes);

    memset(out, 0, RWK_WG_RESPONSE_LEN);
    out[0] = MSG_RESPONSE;
    put32(out + 4, local_index);
    put32(out + 8, get32(msg + 4));
    memcpy(out + 12, eph_pub, 32);

    mix_chain(c, eph_pub, 32);
    mix_hash(h, eph_pub, 32);
    if (!rwk_x25519(dh, eph_priv, ei)) {
        goto done;
    }
    mix_chain(c, dh, 32);
    if (!rwk_x25519(dh, eph_priv, w->peer_pk)) {
        goto done;
    }
    mix_chain(c, dh, 32);

    {
        static const uint8_t zero_psk[32] = {0};
        uint8_t new_c[32], tau[32];
        kdf(c, zero_psk, 32, new_c, tau, k);
        memcpy(c, new_c, 32);
        mix_hash(h, tau, 32);
        rwk_wipe(new_c, 32);
        rwk_wipe(tau, 32);
    }
    if (!rwk_aead_seal(out + 44, k, 0, h, 32, NULL, 0)) {
        goto done;
    }
    mix_hash(h, out + 44, 16);
    mac1(out + 60, w->mac1_key_peer, out, 60);

    // Commit only after everything succeeded, so a bad packet leaves the live session alone.
    memcpy(w->last_ts, ts, 12);
    w->have_ts = true;
    install_keypair(w, &w->next, c, false, local_index, get32(msg + 4), now_ms);
    *out_len = RWK_WG_RESPONSE_LEN;
    result = RWK_WG_REPLY;

done:
    rwk_wipe(c, 32);
    rwk_wipe(h, 32);
    rwk_wipe(k, 32);
    rwk_wipe(dh, 32);
    rwk_wipe(eph_priv, 32);
    return result;
}

// ---------------------------------------------------------------- transport

static rwk_wg_result_t handle_data(rwk_wg_t *w, int64_t now_ms, const uint8_t *msg, size_t len,
                                   uint8_t *plain, size_t *plain_len)
{
    if (len < RWK_WG_DATA_OVERHEAD) {
        return RWK_WG_ERR_FORMAT;
    }
    const uint32_t index = get32(msg + 4);
    const uint64_t counter = get64(msg + 8);
    rwk_wg_keypair_t *kp = NULL;
    if (w->cur.valid && w->cur.local_index == index) {
        kp = &w->cur;
    } else if (w->next.valid && w->next.local_index == index) {
        kp = &w->next;
    } else if (w->prev.valid && w->prev.local_index == index) {
        kp = &w->prev;
    }
    if (kp == NULL || now_ms - kp->born_ms > RWK_WG_REJECT_AFTER_MS) {
        return RWK_WG_NONE;
    }
    const size_t body = len - 16;
    if (!rwk_aead_open(plain, kp->recv_key, counter, NULL, 0, msg + 16, body)) {
        return RWK_WG_ERR_AUTH;
    }
    // Replay check only after authentication, so garbage cannot burn window slots.
    if (!window_accept(kp, counter)) {
        return RWK_WG_NONE;
    }
    if (kp == &w->next) {
        // First authenticated packet under the new key confirms it for the responder.
        w->prev = w->cur;
        w->cur = w->next;
        keypair_clear(&w->next);
        w->cur.confirmed = true;
    }
    const size_t plain_total = body - 16;
    if (plain_total == 0) {
        *plain_len = 0;
        return RWK_WG_KEEPALIVE;
    }
    *plain_len = plain_total;
    return RWK_WG_DATA;
}

rwk_wg_result_t rwk_wg_input(rwk_wg_t *w, int64_t now_ms, const uint8_t *dgram, size_t len,
                             uint8_t *out, size_t *out_len, uint8_t *plain, size_t *plain_len)
{
    *out_len = 0;
    *plain_len = 0;
    if (len < 4 || len > RWK_WG_MAX_DATAGRAM || dgram[1] != 0 || dgram[2] != 0 || dgram[3] != 0) {
        return RWK_WG_ERR_FORMAT;
    }
    switch (dgram[0]) {
    case MSG_INITIATION:
        return len == RWK_WG_INITIATION_LEN ? handle_initiation(w, now_ms, dgram, out, out_len)
                                            : RWK_WG_ERR_FORMAT;
    case MSG_RESPONSE:
        return len == RWK_WG_RESPONSE_LEN ? handle_response(w, now_ms, dgram, out, out_len)
                                          : RWK_WG_ERR_FORMAT;
    case MSG_DATA:
        return handle_data(w, now_ms, dgram, len, plain, plain_len);
    case 3:
        // Cookie reply: we never ask for one and never require one, so there is nothing to do.
        return RWK_WG_NONE;
    default:
        return RWK_WG_ERR_FORMAT;
    }
}

bool rwk_wg_can_send(const rwk_wg_t *w, int64_t now_ms)
{
    return w->cur.valid && w->cur.confirmed && now_ms - w->cur.born_ms < RWK_WG_REJECT_AFTER_MS &&
           w->cur.send_counter < REJECT_AFTER_MESSAGES;
}

size_t rwk_wg_encrypt(rwk_wg_t *w, int64_t now_ms, const uint8_t *plain, size_t len, uint8_t *out,
                      size_t out_cap)
{
    if (!rwk_wg_can_send(w, now_ms)) {
        return 0;
    }
    const size_t padded = (len + 15) & ~(size_t)15;
    const size_t total = 16 + padded + 16;
    if (total > out_cap || padded > RWK_WG_MAX_DATAGRAM) {
        return 0;
    }
    uint8_t buf[RWK_WG_MAX_DATAGRAM];
    memset(buf, 0, padded);
    if (len > 0) {
        memcpy(buf, plain, len);
    }
    const uint64_t counter = w->cur.send_counter++;
    out[0] = MSG_DATA;
    out[1] = out[2] = out[3] = 0;
    put32(out + 4, w->cur.remote_index);
    put64(out + 8, counter);
    const bool ok = rwk_aead_seal(out + 16, w->cur.send_key, counter, NULL, 0, buf, padded);
    rwk_wipe(buf, padded);
    return ok ? total : 0;
}

bool rwk_wg_needs_rekey(const rwk_wg_t *w, int64_t now_ms)
{
    return !w->cur.valid || now_ms - w->cur.born_ms >= RWK_WG_REKEY_AFTER_MS;
}
