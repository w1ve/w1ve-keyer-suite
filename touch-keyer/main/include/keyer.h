// Station keyer engine: paddles in, rig KEY/PTT out.
//
// The paddle inputs and the on-screen squares feed the SAME iambic state machine, so a
// squeeze across a physical paddle and a touch square behaves like a normal squeeze.
#pragma once

#include <stdbool.h>
#include <stdint.h>

typedef enum {
    KEYER_MODE_IAMBIC_A = 0,
    KEYER_MODE_IAMBIC_B,
    KEYER_MODE_STRAIGHT,
    KEYER_MODE_COUNT,
} keyer_mode_t;

#define KEYER_WPM_MIN 5
#define KEYER_WPM_MAX 50
#define KEYER_WPM_DEFAULT 20

// Keying weight in percent: the share of each element period spent key down. 50 is even.
#define KEYER_WEIGHT_MIN 25
#define KEYER_WEIGHT_MAX 75
#define KEYER_WEIGHT_DEFAULT 50

typedef struct {
    bool key_down;      // KEY line currently asserted
    bool ptt_down;      // PTT line currently asserted
    uint8_t wpm;
    uint8_t weight;
    keyer_mode_t mode;
} keyer_status_t;

void keyer_init(void);

// Speed. Applies to the next element; an element already in flight completes.
void keyer_set_wpm(uint8_t wpm);
uint8_t keyer_get_wpm(void);

void keyer_set_mode(keyer_mode_t mode);
keyer_mode_t keyer_get_mode(void);
const char *keyer_mode_name(keyer_mode_t mode);

// Key-down share of each element, 25..75 %.
void keyer_set_weight(uint8_t percent);
uint8_t keyer_get_weight(void);

// Dry run of the element machine with the pins untouched: reports the worst boundary
// lateness for the given speed over that many elements. Blocks for a couple of seconds.
void keyer_selftest(uint16_t wpm, uint16_t elements);

// Keys the line for real from the local engine, with no paddle and no Keyer involved: the
// element machine drives GPIO and the sidetone edge stream exactly as a paddle press would.
// This is the only way to prove the audio path without a remote Keyer at the other end.
// Keys the rig (PTT included), so it is a deliberate operator action. Returns false when the
// line is already busy.
bool keyer_keytest(uint16_t elements);

// Sends a short text string as CW at the current speed, on its own task. Refuses when the line
// is busy locally, a remote Keyer owns it, or another text is already going out. When a remote
// session is active (see keyer_set_remote_speed) the text is sent at the remote's speed instead.
bool keyer_send_text(const char *text);

// Tells the keyer the speed a remote Keyer is running at while it owns the line, so a locally
// played macro matches the QSO. 0 clears it (no remote active). Set by the transport that owns
// the line; the paddle engine and the station speed setting are unaffected.
void keyer_set_remote_speed(uint8_t wpm);

// Worst lateness of an element boundary since the last call, in microseconds. This is the
// number that says whether keying is actually clean, rather than how it sounds.
uint32_t keyer_take_max_jitter_us(void);

// Virtual paddles for the touch squares. Identical effect to the GPIO paddle inputs.
void keyer_paddle_dit(bool down);
void keyer_paddle_dah(bool down);

// Snapshot for the UI. Safe to call from the LVGL task.
keyer_status_t keyer_get_status(void);

// Called from the keyer task on every KEY line transition, with a millisecond timestamp.
// Must be quick and must not block: the web sidetone feeds off it.
typedef void (*keyer_edge_cb_t)(bool down, uint32_t ms);
void keyer_set_edge_cb(keyer_edge_cb_t cb);

// Drives the KEY line from a LAN edge frame. Returns false when a local paddle owns the line,
// so a remote edge is dropped rather than fighting the operator standing at the rig.
bool keyer_remote_key(bool down);

// Same, but the transition carries a local millisecond timestamp chosen by the caller, so a
// history frame that holds several edges keeps the sender's element spacing instead of
// collapsing every entry onto the instant the frame arrived. Must not go backwards for a
// given frame; the caller orders them.
bool keyer_remote_edge(bool down, uint32_t ms);
