#include "rwk_blake2s.h"

#include <string.h>

static const uint32_t IV[8] = {0x6A09E667u, 0xBB67AE85u, 0x3C6EF372u, 0xA54FF53Au,
                               0x510E527Fu, 0x9B05688Cu, 0x1F83D9ABu, 0x5BE0CD19u};

static const uint8_t SIGMA[10][16] = {
    {0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15},
    {14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3},
    {11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4},
    {7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8},
    {9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13},
    {2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9},
    {12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11},
    {13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10},
    {6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5},
    {10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0},
};

static inline uint32_t rotr32(uint32_t x, unsigned n)
{
    return (x >> n) | (x << (32 - n));
}

static inline uint32_t load32(const uint8_t *p)
{
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24);
}

#define G(a, b, c, d, x, y)        \
    do {                           \
        v[a] = v[a] + v[b] + (x);  \
        v[d] = rotr32(v[d] ^ v[a], 16); \
        v[c] = v[c] + v[d];        \
        v[b] = rotr32(v[b] ^ v[c], 12); \
        v[a] = v[a] + v[b] + (y);  \
        v[d] = rotr32(v[d] ^ v[a], 8);  \
        v[c] = v[c] + v[d];        \
        v[b] = rotr32(v[b] ^ v[c], 7);  \
    } while (0)

static void compress(rwk_blake2s_t *ctx, const uint8_t block[64], int last)
{
    uint32_t m[16];
    uint32_t v[16];
    for (int i = 0; i < 16; i++) {
        m[i] = load32(block + 4 * i);
    }
    for (int i = 0; i < 8; i++) {
        v[i] = ctx->h[i];
        v[i + 8] = IV[i];
    }
    v[12] ^= ctx->t[0];
    v[13] ^= ctx->t[1];
    if (last) {
        v[14] = ~v[14];
    }
    for (int r = 0; r < 10; r++) {
        const uint8_t *s = SIGMA[r];
        G(0, 4, 8, 12, m[s[0]], m[s[1]]);
        G(1, 5, 9, 13, m[s[2]], m[s[3]]);
        G(2, 6, 10, 14, m[s[4]], m[s[5]]);
        G(3, 7, 11, 15, m[s[6]], m[s[7]]);
        G(0, 5, 10, 15, m[s[8]], m[s[9]]);
        G(1, 6, 11, 12, m[s[10]], m[s[11]]);
        G(2, 7, 8, 13, m[s[12]], m[s[13]]);
        G(3, 4, 9, 14, m[s[14]], m[s[15]]);
    }
    for (int i = 0; i < 8; i++) {
        ctx->h[i] ^= v[i] ^ v[i + 8];
    }
}

static void add_counter(rwk_blake2s_t *ctx, uint32_t n)
{
    ctx->t[0] += n;
    if (ctx->t[0] < n) {
        ctx->t[1]++;
    }
}

void rwk_blake2s_init(rwk_blake2s_t *ctx, size_t outlen, const uint8_t *key, size_t keylen)
{
    memset(ctx, 0, sizeof(*ctx));
    memcpy(ctx->h, IV, sizeof(IV));
    ctx->h[0] ^= 0x01010000u ^ ((uint32_t)keylen << 8) ^ (uint32_t)outlen;
    ctx->outlen = outlen;
    if (keylen > 0) {
        uint8_t block[64] = {0};
        memcpy(block, key, keylen);
        rwk_blake2s_update(ctx, block, 64);
        rwk_wipe(block, sizeof(block));
    }
}

void rwk_blake2s_update(rwk_blake2s_t *ctx, const uint8_t *in, size_t len)
{
    // The last block must be compressed with the final flag, so a full buffer is only flushed
    // when more input arrives.
    while (len > 0) {
        if (ctx->buflen == 64) {
            add_counter(ctx, 64);
            compress(ctx, ctx->buf, 0);
            ctx->buflen = 0;
        }
        size_t take = 64 - ctx->buflen;
        if (take > len) {
            take = len;
        }
        memcpy(ctx->buf + ctx->buflen, in, take);
        ctx->buflen += take;
        in += take;
        len -= take;
    }
}

void rwk_blake2s_final(rwk_blake2s_t *ctx, uint8_t *out)
{
    add_counter(ctx, (uint32_t)ctx->buflen);
    memset(ctx->buf + ctx->buflen, 0, 64 - ctx->buflen);
    compress(ctx, ctx->buf, 1);
    uint8_t full[32];
    for (int i = 0; i < 8; i++) {
        full[4 * i] = (uint8_t)ctx->h[i];
        full[4 * i + 1] = (uint8_t)(ctx->h[i] >> 8);
        full[4 * i + 2] = (uint8_t)(ctx->h[i] >> 16);
        full[4 * i + 3] = (uint8_t)(ctx->h[i] >> 24);
    }
    memcpy(out, full, ctx->outlen);
    rwk_wipe(full, sizeof(full));
}

void rwk_blake2s(uint8_t *out, size_t outlen, const uint8_t *key, size_t keylen,
                 const uint8_t *in, size_t inlen)
{
    rwk_blake2s_t ctx;
    rwk_blake2s_init(&ctx, outlen, key, keylen);
    rwk_blake2s_update(&ctx, in, inlen);
    rwk_blake2s_final(&ctx, out);
    rwk_wipe(&ctx, sizeof(ctx));
}

void rwk_hmac_blake2s(uint8_t out[32], const uint8_t *key, size_t keylen, const uint8_t *in,
                      size_t inlen)
{
    uint8_t k[64] = {0};
    uint8_t pad[64];
    uint8_t inner[32];
    rwk_blake2s_t ctx;

    if (keylen > 64) {
        rwk_blake2s(k, 32, NULL, 0, key, keylen);
    } else {
        memcpy(k, key, keylen);
    }
    for (int i = 0; i < 64; i++) {
        pad[i] = k[i] ^ 0x36;
    }
    rwk_blake2s_init(&ctx, 32, NULL, 0);
    rwk_blake2s_update(&ctx, pad, 64);
    rwk_blake2s_update(&ctx, in, inlen);
    rwk_blake2s_final(&ctx, inner);
    for (int i = 0; i < 64; i++) {
        pad[i] = k[i] ^ 0x5c;
    }
    rwk_blake2s_init(&ctx, 32, NULL, 0);
    rwk_blake2s_update(&ctx, pad, 64);
    rwk_blake2s_update(&ctx, inner, 32);
    rwk_blake2s_final(&ctx, out);
    rwk_wipe(k, sizeof(k));
    rwk_wipe(pad, sizeof(pad));
    rwk_wipe(inner, sizeof(inner));
    rwk_wipe(&ctx, sizeof(ctx));
}

void rwk_wipe(void *p, size_t n)
{
    volatile uint8_t *v = (volatile uint8_t *)p;
    while (n--) {
        *v++ = 0;
    }
}
