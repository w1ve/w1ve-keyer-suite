// The CW edge applier: the one piece of logic that turns a stream of RWK-PADDLE frames into
// KEY-line transitions, shared by the LAN transport (remote.c) and the WAN transport (wan.c).
//
// It owns exactly the safety and timing rules that must never diverge between the two paths:
//   * epoch pinning (an epoch change ends the session);
//   * per-edge sequence and timestamp monotonicity, and the "gap that ends key-down is unsafe" rule;
//   * the sender's clock mapped onto ours so a history frame keeps its element spacing;
//   * dit measurement for the remote-speed readout;
//   * the asserted-KEY lease (a complete frame refreshes it; silence past the lease drops the line).
//
// It performs no I/O and takes no locks. The caller holds its own lock around every call, exactly
// as remote.c already does, and supplies the two keyer hooks so the applier never includes keyer.h
// and stays host-testable. A WAN caller additionally runs each edge through the jitter scheduler
// before handing it here, so this module times on the sender's clock and the jitter buffer decides
// *when* a frame's edges are fed.
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define RWK_EDGE_ENTRY_LEN 12
#define RWK_EDGE_MIN_LEN 16
#define RWK_EDGE_MAX_LEN 52
#define RWK_EDGE_LEASE_MS 1500
#define RWK_EDGE_SPEED_REMEASURE_MS 2000
#define RWK_EDGE_WPM_MIN 5
#define RWK_EDGE_WPM_MAX 50
// Idle time after which the next frame starts a fresh playout burst and re-chooses the anchor.
#define RWK_EDGE_BURST_GAP_MS 400

// Delivers one accepted KEY transition. `sidetone_ms` is the sender-clock-mapped millisecond the
// element was keyed at (for the sidetone / element spacing); `play_ms` is the local monotonic
// instant the transition should actually take effect at — equal to now for the LAN path, and the
// jitter-buffered playout time for the WAN path. Returns false when a local paddle owns the line,
// in which case the applier ends the session. The LAN path applies at once; the WAN path schedules.
typedef bool (*rwk_edge_key_fn)(bool down, uint32_t sidetone_ms, int64_t play_ms, void *ctx);

// Current monotonic milliseconds. Maps to now_ms(). Kept injectable for host tests.
typedef int64_t (*rwk_edge_now_fn)(void *ctx);

typedef enum {
    RWK_EDGE_OK = 0,        // frame processed (possibly a pure lease refresh)
    RWK_EDGE_MALFORMED,     // bad count/length/reserved bits: ignored, session survives
    RWK_EDGE_END_SESSION,   // a rule was violated or the local paddle took the line: caller unpairs
} rwk_edge_status_t;

typedef struct {
    bool have_epoch;
    uint16_t epoch;
    bool have_sequence;
    uint32_t last_sequence;
    bool have_timestamp;
    uint32_t last_timestamp;
    int64_t last_refresh_ms;   // when the last complete frame arrived: the lease clock

    bool key_down;
    bool have_down_ts;
    uint32_t down_ts;
    int64_t dit_ms;
    int64_t dit_at_ms;

    // Playout anchor: local_play = peer_timestamp + anchor_ms. Chosen when a burst starts so the
    // sender's element spacing is reproduced exactly, then held; re-chosen after an idle gap or if
    // an edge would otherwise be scheduled in the past. `delay_ms` is the buffer added at the start
    // of a burst (0 on the LAN, a jitter-sized cushion on the WAN) so late frames still land in
    // order instead of being played the instant they arrive.
    bool have_anchor;
    int64_t anchor_ms;
    int64_t last_arrival_ms;
    int64_t last_play_ms;
    uint32_t delay_ms;

    char end_reason[64];

    rwk_edge_key_fn key_fn;
    rwk_edge_now_fn now_fn;
    void *ctx;
} rwk_edge_t;

// `delay_ms` is the playout buffer: 0 for the LAN (apply on arrival), a jitter cushion for the WAN.
void rwk_edge_reset(rwk_edge_t *e, uint32_t delay_ms, rwk_edge_key_fn key_fn, rwk_edge_now_fn now_fn,
                    void *ctx);

// Feeds one validated RWK-PADDLE frame payload (epoch u16 LE, count u16 LE, count x 12-byte
// entries). `now_ms` is the moment to treat as the frame's arrival for the lease and clock
// mapping; the LAN path passes the real now, the WAN path passes the jittered play time of the
// frame's newest edge so every edge is scheduled off one consistent base.
rwk_edge_status_t rwk_edge_feed(rwk_edge_t *e, const uint8_t *payload, size_t len, int64_t now_ms);

// True once the asserted-KEY lease has expired with the line still down: the caller must release.
bool rwk_edge_lease_expired(const rwk_edge_t *e, int64_t now_ms);

// Measured remote speed in WPM, 0 until the peer has keyed. Held for the whole session.
uint8_t rwk_edge_wpm(const rwk_edge_t *e);

// Why the last feed returned END_SESSION, for the status reason string.
const char *rwk_edge_end_reason(const rwk_edge_t *e);

// True when the line is currently asserted by this applier.
bool rwk_edge_keying(const rwk_edge_t *e);
