// rwk_crypto.h on mbedTLS 3.6 (ESP-IDF 5.3). Curve25519 is software (the C6 ECC block is P-192/256
// only); ChaCha20-Poly1305 is the portable mbedTLS code, fast enough for 50 byte packets.
#include <string.h>

#include "esp_random.h"
#include "mbedtls/chachapoly.h"
#include "mbedtls/ecp.h"
#include "rwk_blake2s.h"
#include "rwk_crypto.h"

void rwk_rng(uint8_t *out, size_t n)
{
    // Backed by the hardware TRNG. It is only a true entropy source while the Wi-Fi radio is
    // running, which is always the case before WAN starts; the key is generated after that.
    esp_fill_random(out, n);
}

static int rng_cb(void *ctx, unsigned char *buf, size_t len)
{
    (void)ctx;
    esp_fill_random(buf, len);
    return 0;
}

// out = k * u (u == NULL means the base point). k is clamped per RFC 7748.
static bool x25519_mul(uint8_t out[32], const uint8_t k[32], const uint8_t *u)
{
    uint8_t kc[32];
    memcpy(kc, k, 32);
    kc[0] &= 248;
    kc[31] &= 127;
    kc[31] |= 64;

    mbedtls_ecp_group grp;
    mbedtls_ecp_point p, r;
    mbedtls_mpi d;
    mbedtls_ecp_group_init(&grp);
    mbedtls_ecp_point_init(&p);
    mbedtls_ecp_point_init(&r);
    mbedtls_mpi_init(&d);

    bool ok = mbedtls_ecp_group_load(&grp, MBEDTLS_ECP_DP_CURVE25519) == 0 &&
              mbedtls_mpi_read_binary_le(&d, kc, 32) == 0;
    if (ok && u == NULL) {
        ok = mbedtls_ecp_copy(&p, &grp.G) == 0;
    } else if (ok) {
        uint8_t uc[32];
        memcpy(uc, u, 32);
        uc[31] &= 127;  // RFC 7748: the top bit of u is ignored
        ok = mbedtls_mpi_read_binary_le(&p.MBEDTLS_PRIVATE(X), uc, 32) == 0 &&
             mbedtls_mpi_lset(&p.MBEDTLS_PRIVATE(Z), 1) == 0;
    }
    ok = ok && mbedtls_ecp_mul(&grp, &r, &d, &p, rng_cb, NULL) == 0 &&
         mbedtls_mpi_write_binary_le(&r.MBEDTLS_PRIVATE(X), out, 32) == 0;

    mbedtls_mpi_free(&d);
    mbedtls_ecp_point_free(&p);
    mbedtls_ecp_point_free(&r);
    mbedtls_ecp_group_free(&grp);
    rwk_wipe(kc, sizeof(kc));
    return ok;
}

bool rwk_x25519_pub(uint8_t pub[32], const uint8_t priv[32])
{
    return x25519_mul(pub, priv, NULL);
}

bool rwk_x25519(uint8_t out[32], const uint8_t priv[32], const uint8_t pub[32])
{
    if (!x25519_mul(out, priv, pub)) {
        return false;
    }
    // A low-order peer point yields all zeros; using it would make the shared secret known.
    uint8_t acc = 0;
    for (int i = 0; i < 32; i++) {
        acc |= out[i];
    }
    return acc != 0;
}

static void nonce_for(uint8_t nonce[12], uint64_t counter)
{
    memset(nonce, 0, 4);
    for (int i = 0; i < 8; i++) {
        nonce[4 + i] = (uint8_t)(counter >> (8 * i));
    }
}

bool rwk_aead_seal(uint8_t *out, const uint8_t key[32], uint64_t counter, const uint8_t *ad,
                   size_t ad_len, const uint8_t *in, size_t len)
{
    uint8_t nonce[12];
    nonce_for(nonce, counter);
    mbedtls_chachapoly_context ctx;
    mbedtls_chachapoly_init(&ctx);
    const bool ok = mbedtls_chachapoly_setkey(&ctx, key) == 0 &&
                    mbedtls_chachapoly_encrypt_and_tag(&ctx, len, nonce, ad, ad_len, in, out,
                                                       out + len) == 0;
    mbedtls_chachapoly_free(&ctx);
    return ok;
}

bool rwk_aead_open(uint8_t *out, const uint8_t key[32], uint64_t counter, const uint8_t *ad,
                   size_t ad_len, const uint8_t *in, size_t len)
{
    if (len < RWK_TAG_LEN) {
        return false;
    }
    uint8_t nonce[12];
    nonce_for(nonce, counter);
    mbedtls_chachapoly_context ctx;
    mbedtls_chachapoly_init(&ctx);
    const size_t body = len - RWK_TAG_LEN;
    const bool ok = mbedtls_chachapoly_setkey(&ctx, key) == 0 &&
                    mbedtls_chachapoly_auth_decrypt(&ctx, body, nonce, ad, ad_len, in + body, in,
                                                    out) == 0;
    mbedtls_chachapoly_free(&ctx);
    return ok;
}
