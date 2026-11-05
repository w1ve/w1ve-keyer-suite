// Playout timing for CW edges that arrive over the Internet.
//
// On the LAN an edge is applied the moment it arrives, because jitter there is far smaller than a
// dit. Over a WAN it is not: a 20 WPM dit is 60 ms and a path can wander by more than that, so
// applying on arrival turns clean code into uneven code. The desktop Player de-jitters for the same
// reason (replay/jitter.rs).
//
// The rule is the one the LAN path already uses for its sidetone clock, made strict: inside a
// burst every edge is played at (sender timestamp + a fixed offset), so the sender's element
// spacing is reproduced exactly. The offset is chosen when a burst starts, from the jitter measured
// so far, and is held until the line has been idle. A packet that is later than its slot is played
// at once and pushes the rest of the burst back by the same amount, so spacing still survives.
//
// Pure integer arithmetic on caller supplied times; no clock, no allocation, host testable.
#pragma once

#include <stdbool.h>
#include <stdint.h>

#define RWK_JITTER_MIN_MS 40         // never buffer less than this once WAN is in use
#define RWK_JITTER_DEFAULT_MS 80     // before any jitter has been measured
#define RWK_JITTER_MAX_MS 300        // above this a late edge is simply late
#define RWK_JITTER_BURST_GAP_MS 400  // idle time after which a new burst re-chooses the offset

typedef struct {
    bool have_anchor;
    int64_t anchor_ms;        // play time = sender timestamp + anchor
    int64_t base_anchor_ms;   // anchor as chosen at burst start, to cap lateness growth
    int64_t last_edge_arrival_ms;
    int64_t last_play_ms;
    bool have_prev;
    int64_t prev_transit_ms;
    uint32_t jitter_x16;      // RFC 3550 interarrival jitter estimate, scaled by 16
    uint32_t delay_ms;        // buffer chosen for the current burst
    uint32_t late_edges;      // played late, for diagnostics
} rwk_jitter_t;

void rwk_jitter_init(rwk_jitter_t *j);

// Call once per received frame that carried at least one new edge, with the timestamp of its newest
// new edge. Feeds the jitter estimate only; it does not schedule anything.
void rwk_jitter_observe(rwk_jitter_t *j, uint32_t newest_peer_ts, int64_t arrival_ms);

// Local time at which an edge stamped peer_ts should be applied. Call oldest to newest within a
// frame. Never returns a time before arrival_ms and never goes backwards.
int64_t rwk_jitter_play_time(rwk_jitter_t *j, uint32_t peer_ts, int64_t arrival_ms);

// Current estimate in milliseconds, for status display.
uint32_t rwk_jitter_estimate_ms(const rwk_jitter_t *j);
