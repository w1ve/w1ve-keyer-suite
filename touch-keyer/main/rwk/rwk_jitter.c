#include "rwk_jitter.h"

static uint32_t clamp_delay(uint32_t v)
{
    if (v < RWK_JITTER_MIN_MS) {
        return RWK_JITTER_MIN_MS;
    }
    if (v > RWK_JITTER_MAX_MS) {
        return RWK_JITTER_MAX_MS;
    }
    return v;
}

void rwk_jitter_init(rwk_jitter_t *j)
{
    *j = (rwk_jitter_t){0};
    j->delay_ms = RWK_JITTER_DEFAULT_MS;
}

uint32_t rwk_jitter_estimate_ms(const rwk_jitter_t *j)
{
    return j->jitter_x16 / 16;
}

void rwk_jitter_observe(rwk_jitter_t *j, uint32_t newest_peer_ts, int64_t arrival_ms)
{
    // Transit is arrival minus the sender's stamp. The two clocks have unrelated origins, so only
    // the change between consecutive frames means anything.
    const int64_t transit = arrival_ms - (int64_t)newest_peer_ts;
    if (j->have_prev) {
        int64_t d = transit - j->prev_transit_ms;
        if (d < 0) {
            d = -d;
        }
        if (d > 2000) {
            d = 2000;  // a clock step or a long stall is not jitter, do not let it poison the mean
        }
        const int64_t scaled = d * 16;
        j->jitter_x16 = (uint32_t)((int64_t)j->jitter_x16 + (scaled - (int64_t)j->jitter_x16) / 16);
    }
    j->prev_transit_ms = transit;
    j->have_prev = true;
}

int64_t rwk_jitter_play_time(rwk_jitter_t *j, uint32_t peer_ts, int64_t arrival_ms)
{
    const bool new_burst =
        !j->have_anchor || arrival_ms - j->last_edge_arrival_ms > RWK_JITTER_BURST_GAP_MS;
    if (new_burst) {
        // Four times the smoothed jitter plus a floor covers nearly every packet on a normal path
        // without making the first dit feel late.
        if (j->have_prev) {
            j->delay_ms = clamp_delay(30 + 4 * rwk_jitter_estimate_ms(j));
        }
        j->anchor_ms = arrival_ms + (int64_t)j->delay_ms - (int64_t)peer_ts;
        j->base_anchor_ms = j->anchor_ms;
        j->have_anchor = true;
        j->last_play_ms = 0;
    }
    j->last_edge_arrival_ms = arrival_ms;

    int64_t play = (int64_t)peer_ts + j->anchor_ms;
    if (play < arrival_ms) {
        // Late for its slot. Play now, and move the rest of the burst back by the same amount so
        // the gaps after this edge are still the sender's gaps. Growth is capped, after which an
        // edge is just late rather than letting latency run away.
        int64_t shift = arrival_ms - play;
        const int64_t room = (int64_t)RWK_JITTER_MAX_MS - ((j->anchor_ms - j->base_anchor_ms) + (int64_t)j->delay_ms);
        if (shift > room) {
            shift = room > 0 ? room : 0;
        }
        j->anchor_ms += shift;
        j->late_edges++;
        play = arrival_ms;
    }
    if (play < j->last_play_ms) {
        play = j->last_play_ms;
    }
    j->last_play_ms = play;
    return play;
}
