// WireGuard wire protocol for exactly one peer, as boringtun speaks it in rwk-core
// (Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s, no pre-shared key, no cookies).
//
// This is not a network interface. It turns datagrams into inner packets and back; the caller
// owns sockets, timers and the inner IP handling. Everything is a plain function over a caller
// supplied state, no heap, no platform code, so the same file builds on the host for tests.
//
// Time is passed in (monotonic milliseconds) so behaviour is deterministic under test.
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define RWK_WG_INITIATION_LEN 148
#define RWK_WG_RESPONSE_LEN 92
#define RWK_WG_DATA_OVERHEAD 32   // 16 header + 16 tag
#define RWK_WG_MAX_DATAGRAM 512   // anything longer is not ours: edge traffic is under 128 bytes

// WireGuard timing (whitepaper section 6). Rekey is the initiator's job; we only enforce expiry.
#define RWK_WG_REJECT_AFTER_MS 180000
#define RWK_WG_REKEY_AFTER_MS 120000

typedef enum {
    RWK_WG_NONE = 0,       // consumed, nothing to do (stale, replayed, or wrong peer)
    RWK_WG_REPLY,          // send out[0..out_len) to the peer
    RWK_WG_DATA,           // plain[0..plain_len) is an inner packet
    RWK_WG_KEEPALIVE,      // authenticated empty packet
    RWK_WG_ERR_FORMAT,     // not a WireGuard message we understand
    RWK_WG_ERR_AUTH,       // failed authentication; drop silently
} rwk_wg_result_t;

typedef struct {
    bool valid;
    bool initiator;
    bool confirmed;          // responder: a data packet has arrived under this key
    uint32_t local_index;
    uint32_t remote_index;
    uint8_t send_key[32];
    uint8_t recv_key[32];
    uint64_t send_counter;
    uint64_t recv_max;
    uint64_t window[4];      // 256-packet sliding replay window
    int64_t born_ms;
} rwk_wg_keypair_t;

typedef struct {
    uint8_t sk[32];
    uint8_t pk[32];
    uint8_t peer_pk[32];
    uint8_t ss[32];              // DH(sk, peer_pk), constant for the pairing
    uint8_t mac1_key_self[32];   // HASH("mac1----" | pk)
    uint8_t mac1_key_peer[32];   // HASH("mac1----" | peer_pk)
    uint8_t c0[32];
    uint8_t h_resp[32];          // starting hash when we answer (binds our pk)
    uint8_t h_init[32];          // starting hash when we initiate (binds peer pk)

    bool hs_pending;             // an initiation of ours is awaiting its response
    uint32_t hs_index;
    uint8_t hs_eph_priv[32];
    uint8_t hs_c[32];
    uint8_t hs_h[32];

    bool have_ts;
    uint8_t last_ts[12];         // newest TAI64N accepted from the peer

    rwk_wg_keypair_t cur;
    rwk_wg_keypair_t next;
    rwk_wg_keypair_t prev;
} rwk_wg_t;

// Computes the public key from sk and all per-peer constants. Returns false on a bad key.
bool rwk_wg_init(rwk_wg_t *w, const uint8_t sk[32], const uint8_t peer_pk[32]);

// Wipes every secret in the state.
void rwk_wg_free(rwk_wg_t *w);

// Builds an initiation. unix_sec is wall-clock time and must be believable (peers reject a
// timestamp that goes backwards); pass 0 when the clock is not set and this returns false.
bool rwk_wg_initiate(rwk_wg_t *w, int64_t now_ms, uint64_t unix_sec, uint32_t nanos,
                     uint8_t out[RWK_WG_INITIATION_LEN]);

// Feeds one received datagram. For RWK_WG_REPLY the reply is in out/out_len. For RWK_WG_DATA the
// inner packet is in plain/plain_len. out needs 148 bytes of room, plain needs len bytes.
rwk_wg_result_t rwk_wg_input(rwk_wg_t *w, int64_t now_ms, const uint8_t *dgram, size_t len,
                             uint8_t *out, size_t *out_len, uint8_t *plain, size_t *plain_len);

// True when a transport key exists that we are allowed to send on and has not expired.
bool rwk_wg_can_send(const rwk_wg_t *w, int64_t now_ms);

// Encrypts an inner packet (len may be 0 for a keepalive). out needs len + padding + 32 bytes:
// (len rounded up to 16) + 32. Returns the datagram length, or 0 if no usable key.
size_t rwk_wg_encrypt(rwk_wg_t *w, int64_t now_ms, const uint8_t *plain, size_t len, uint8_t *out,
                      size_t out_cap);

// True once the session has aged past the point where the peer is expected to have re-keyed.
bool rwk_wg_needs_rekey(const rwk_wg_t *w, int64_t now_ms);
