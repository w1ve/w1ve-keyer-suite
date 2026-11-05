// Host implementation of rwk_crypto.h on OpenSSL, used only by the tests.
#include <openssl/evp.h>
#include <openssl/rand.h>
#include <stdlib.h>
#include <string.h>

#include "../../main/rwk/rwk_crypto.h"

void rwk_rng(uint8_t *out, size_t n)
{
    if (RAND_bytes(out, (int)n) != 1) {
        abort();
    }
}

bool rwk_x25519_pub(uint8_t pub[32], const uint8_t priv[32])
{
    EVP_PKEY *k = EVP_PKEY_new_raw_private_key(EVP_PKEY_X25519, NULL, priv, 32);
    size_t len = 32;
    const bool ok = k != NULL && EVP_PKEY_get_raw_public_key(k, pub, &len) == 1 && len == 32;
    EVP_PKEY_free(k);
    return ok;
}

bool rwk_x25519(uint8_t out[32], const uint8_t priv[32], const uint8_t pub[32])
{
    EVP_PKEY *a = EVP_PKEY_new_raw_private_key(EVP_PKEY_X25519, NULL, priv, 32);
    EVP_PKEY *b = EVP_PKEY_new_raw_public_key(EVP_PKEY_X25519, NULL, pub, 32);
    EVP_PKEY_CTX *ctx = a != NULL ? EVP_PKEY_CTX_new(a, NULL) : NULL;
    size_t len = 32;
    bool ok = ctx != NULL && b != NULL && EVP_PKEY_derive_init(ctx) == 1 &&
              EVP_PKEY_derive_set_peer(ctx, b) == 1 && EVP_PKEY_derive(ctx, out, &len) == 1 &&
              len == 32;
    EVP_PKEY_CTX_free(ctx);
    EVP_PKEY_free(a);
    EVP_PKEY_free(b);
    return ok;
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
    EVP_CIPHER_CTX *c = EVP_CIPHER_CTX_new();
    int n = 0;
    bool ok = c != NULL && EVP_EncryptInit_ex(c, EVP_chacha20_poly1305(), NULL, key, nonce) == 1;
    if (ok && ad_len > 0) {
        ok = EVP_EncryptUpdate(c, NULL, &n, ad, (int)ad_len) == 1;
    }
    if (ok && len > 0) {
        ok = EVP_EncryptUpdate(c, out, &n, in, (int)len) == 1;
    }
    if (ok) {
        ok = EVP_EncryptFinal_ex(c, out + len, &n) == 1 &&
             EVP_CIPHER_CTX_ctrl(c, EVP_CTRL_AEAD_GET_TAG, 16, out + len) == 1;
    }
    EVP_CIPHER_CTX_free(c);
    return ok;
}

bool rwk_aead_open(uint8_t *out, const uint8_t key[32], uint64_t counter, const uint8_t *ad,
                   size_t ad_len, const uint8_t *in, size_t len)
{
    if (len < 16) {
        return false;
    }
    uint8_t nonce[12];
    uint8_t tag[16];
    nonce_for(nonce, counter);
    memcpy(tag, in + len - 16, 16);
    const size_t body = len - 16;
    EVP_CIPHER_CTX *c = EVP_CIPHER_CTX_new();
    int n = 0;
    bool ok = c != NULL && EVP_DecryptInit_ex(c, EVP_chacha20_poly1305(), NULL, key, nonce) == 1 &&
              EVP_CIPHER_CTX_ctrl(c, EVP_CTRL_AEAD_SET_TAG, 16, tag) == 1;
    if (ok && ad_len > 0) {
        ok = EVP_DecryptUpdate(c, NULL, &n, ad, (int)ad_len) == 1;
    }
    if (ok && body > 0) {
        ok = EVP_DecryptUpdate(c, out, &n, in, (int)body) == 1;
    }
    if (ok) {
        ok = EVP_DecryptFinal_ex(c, tag, &n) == 1;
    }
    EVP_CIPHER_CTX_free(c);
    return ok;
}
