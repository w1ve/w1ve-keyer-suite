// Platform crypto the WireGuard core needs. Implemented by rwk_crypto_mbedtls.c on the device and
// by an OpenSSL shim in tests/host, so the protocol code above it is identical in both.
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define RWK_KEY_LEN 32
#define RWK_TAG_LEN 16

// Cryptographically secure random bytes. Must not fail silently.
void rwk_rng(uint8_t *out, size_t n);

// Curve25519 public key for a private key (clamping is applied internally).
bool rwk_x25519_pub(uint8_t pub[32], const uint8_t priv[32]);

// Curve25519 shared secret. Returns false for a low-order point (all-zero result).
bool rwk_x25519(uint8_t out[32], const uint8_t priv[32], const uint8_t pub[32]);

// ChaCha20-Poly1305 (IETF, RFC 8439) with WireGuard's nonce: 4 zero bytes then the 64-bit
// little-endian counter. out receives len + 16 bytes (ciphertext then tag).
bool rwk_aead_seal(uint8_t *out, const uint8_t key[32], uint64_t counter, const uint8_t *ad,
                   size_t ad_len, const uint8_t *in, size_t len);

// in holds len bytes including the 16-byte tag; out receives len - 16 bytes.
bool rwk_aead_open(uint8_t *out, const uint8_t key[32], uint64_t counter, const uint8_t *ad,
                   size_t ad_len, const uint8_t *in, size_t len);
