#include "rwk_edge.h"

#include <stdio.h>
#include <string.h>

static uint16_t le16(const uint8_t *p)
{
    return (uint16_t)(p[0] | (p[1] << 8));
}

static uint32_t le32(const uint8_t *p)
{
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24);
}

static void end(rwk_edge_t *e, const char *why)
{
    snprintf(e->end_reason, sizeof(e->end_reason), "%s", why);
}

void rwk_edge_reset(rwk_edge_t *e, uint32_t delay_ms, rwk_edge_key_fn key_fn, rwk_edge_now_fn now_fn,
                    void *ctx)
{
    memset(e, 0, sizeof(*e));
    e->delay_ms = delay_ms;
    e->key_fn = key_fn;
    e->now_fn = now_fn;
    e->ctx = ctx;
}

// One KEY transition, timing the dit for the speed readout exactly as remote.c did. `play_ms` is
// the local instant the edge should take effect at (now for LAN, scheduled for WAN); `peer_ms` is
// the sender's own stamp, used for the sidetone and the dit measurement.
static rwk_edge_status_t apply(rwk_edge_t *e, bool down, uint32_t peer_ms, int64_t play_ms)
{
    if (down == e->key_down) {
        return RWK_EDGE_OK;
    }
    if (!e->key_fn(down, peer_ms, play_ms, e->ctx)) {
        end(e, "local paddle has the line");
        return RWK_EDGE_END_SESSION;
    }
    e->key_down = down;
    if (down) {
        e->down_ts = peer_ms;
        e->have_down_ts = true;
        return RWK_EDGE_OK;
    }
    if (e->have_down_ts && peer_ms > e->down_ts) {
        const int64_t held = (int64_t)(peer_ms - e->down_ts);
        const int64_t now = e->now_fn(e->ctx);
        if (held > 8 && held < 400) {
            const bool fresh = now - e->dit_at_ms > RWK_EDGE_SPEED_REMEASURE_MS;
            if (e->dit_ms == 0 || held < e->dit_ms || (fresh && held > e->dit_ms * 3 / 2)) {
                e->dit_ms = held;
            }
            e->dit_at_ms = now;
        }
    }
    e->have_down_ts = false;
    return RWK_EDGE_OK;
}

rwk_edge_status_t rwk_edge_feed(rwk_edge_t *e, const uint8_t *payload, size_t len, int64_t now_ms)
{
    if (len < RWK_EDGE_MIN_LEN || len > RWK_EDGE_MAX_LEN) {
        return RWK_EDGE_MALFORMED;
    }
    const uint16_t epoch = le16(payload);
    const uint16_t count = le16(payload + 2);
    if (count == 0 || count > 4 || len != 4u + (size_t)count * RWK_EDGE_ENTRY_LEN) {
        return RWK_EDGE_MALFORMED;
    }
    for (unsigned i = 0; i < count; i++) {
        const uint8_t *entry = payload + 4 + i * RWK_EDGE_ENTRY_LEN;
        if (entry[9] != 0 || le16(entry + 10) != 0) {
            return RWK_EDGE_MALFORMED;
        }
    }

    if (!e->have_epoch) {
        e->epoch = epoch;
        e->have_epoch = true;
    } else if (epoch != e->epoch) {
        end(e, "edge epoch changed");
        return RWK_EDGE_END_SESSION;
    }

    // A complete frame refreshes the lease even when every entry is already applied.
    e->last_refresh_ms = now_ms;

    // Playout anchoring. local_play = peer_timestamp + anchor. The anchor is chosen once per burst
    // from the first edge's arrival plus the buffer delay, then held, so every edge in the burst
    // reproduces the sender's spacing regardless of how the packets were bunched on the wire. It is
    // re-chosen after an idle gap (a new burst) or if it would schedule an edge in the past (clock
    // step or a stall), in which case the edge plays now and the rest of the burst shifts with it.
    const uint32_t newest_ts = le32(payload + 4 + 4);
    const bool new_burst = !e->have_anchor || now_ms - e->last_arrival_ms > RWK_EDGE_BURST_GAP_MS;
    if (new_burst) {
        e->anchor_ms = now_ms + (int64_t)e->delay_ms - (int64_t)newest_ts;
        e->have_anchor = true;
        e->last_play_ms = 0;
    }
    e->last_arrival_ms = now_ms;

    // Entries arrive newest first; replay them oldest to newest.
    for (int i = (int)count - 1; i >= 0; i--) {
        const uint8_t *entry = payload + 4 + i * RWK_EDGE_ENTRY_LEN;
        const uint32_t sequence = le32(entry);
        const uint32_t timestamp = le32(entry + 4);
        const bool down = entry[8] != 0;

        if (e->have_sequence && sequence <= e->last_sequence) {
            continue;
        }
        if (e->have_timestamp && timestamp < e->last_timestamp) {
            end(e, "edge timestamp regressed");
            return RWK_EDGE_END_SESSION;
        }
        if (e->have_sequence && sequence > e->last_sequence + 1 && down) {
            end(e, "unsafe edge sequence gap");
            return RWK_EDGE_END_SESSION;
        }
        e->last_sequence = sequence;
        e->last_timestamp = timestamp;
        e->have_sequence = true;
        e->have_timestamp = true;

        int64_t play = (int64_t)timestamp + e->anchor_ms;
        if (play < now_ms) {
            // This edge is already late for its slot: play it now and slide the anchor so the rest
            // of the burst keeps the sender's spacing from here rather than stacking up.
            e->anchor_ms += now_ms - play;
            play = now_ms;
        }
        if (play < e->last_play_ms) {
            play = e->last_play_ms;  // never schedule out of order
        }
        e->last_play_ms = play;

        const rwk_edge_status_t st = apply(e, down, timestamp, play);
        if (st == RWK_EDGE_END_SESSION) {
            return st;
        }
    }
    return RWK_EDGE_OK;
}

bool rwk_edge_lease_expired(const rwk_edge_t *e, int64_t now_ms)
{
    return e->key_down && now_ms - e->last_refresh_ms >= RWK_EDGE_LEASE_MS;
}

uint8_t rwk_edge_wpm(const rwk_edge_t *e)
{
    if (e->dit_ms <= 0) {
        return 0;
    }
    int wpm = (int)(1200 / e->dit_ms);
    if (wpm < RWK_EDGE_WPM_MIN) {
        wpm = RWK_EDGE_WPM_MIN;
    }
    if (wpm > RWK_EDGE_WPM_MAX) {
        wpm = RWK_EDGE_WPM_MAX;
    }
    return (uint8_t)wpm;
}

const char *rwk_edge_end_reason(const rwk_edge_t *e)
{
    return e->end_reason;
}

bool rwk_edge_keying(const rwk_edge_t *e)
{
    return e->key_down;
}
