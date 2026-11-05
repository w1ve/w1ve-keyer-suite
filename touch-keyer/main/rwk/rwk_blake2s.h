// BLAKE2s (RFC 7693) with the keyed mode and HMAC/HKDF built on it, as WireGuard uses them.
// mbedTLS has no BLAKE2s, so this is a small portable implementation. No heap, no platform code.
#pragma once

#include <stddef.h>
#include <stdint.h>

typedef struct {
    uint32_t h[8];
    uint32_t t[2];
    uint8_t buf[64];
    size_t buflen;
    size_t outlen;
} rwk_blake2s_t;

// outlen 1..32; key may be NULL (keylen 0) or up to 32 bytes.
void rwk_blake2s_init(rwk_blake2s_t *ctx, size_t outlen, const uint8_t *key, size_t keylen);
void rwk_blake2s_update(rwk_blake2s_t *ctx, const uint8_t *in, size_t len);
void rwk_blake2s_final(rwk_blake2s_t *ctx, uint8_t *out);

// One-shot.
void rwk_blake2s(uint8_t *out, size_t outlen, const uint8_t *key, size_t keylen,
                 const uint8_t *in, size_t inlen);

// HMAC-BLAKE2s-256 (64-byte block), key up to 64 bytes.
void rwk_hmac_blake2s(uint8_t out[32], const uint8_t *key, size_t keylen, const uint8_t *in,
                      size_t inlen);

// Overwrites memory in a way the compiler may not elide, for key material.
void rwk_wipe(void *p, size_t n);
