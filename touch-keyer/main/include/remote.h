#pragma once

#include <stdbool.h>
#include <stdint.h>

// W1VE v2 trusted-LAN transport (w1ve-player-esp32/PROTOCOL.md). This station is the Player:
// it advertises on udp/4994, a W1VE Remote Keyer pairs to it on udp/4995, and every packet after
// the handshake is an HMAC-SHA-256 authenticated envelope carrying CW edge frames. No tunnel, no
// persistent pairing secret.
#define REMOTE_DISCOVERY_PORT 4994
#define REMOTE_TRANSPORT_PORT 4995

#define REMOTE_LEASE_MS 1500
#define REMOTE_NAME_MAX 32
#define REMOTE_PEER_LEN 24   // ip:port
#define REMOTE_KEYER_LEN 18  // A1:A2:A3:A4:A5:A6 plus a terminator
#define REMOTE_REASON_LEN 64

typedef struct {
    bool paired;                  // a Keyer holds the session
    bool alive;                   // it has been heard inside the ping window
    bool available;               // the station LAN is up and the sockets are bound
    bool lan_enabled;             // the stored LAN setting, independent of whether it is bound
    bool active;                  // LAN on and a Keyer paired and alive: the remote owns
                                  // the speed, mode and line
    bool keying;                  // the session is asserting KEY right now
    char keyer[REMOTE_KEYER_LEN]; // the paired Keyer's six-byte identity
    char peer[REMOTE_PEER_LEN];   // ip:port the session is pinned to
    uint32_t session_id;
    int rtt_ms;
    uint8_t peer_wpm;             // measured from the edges it is sending, 0 if not yet known
    char reason[REMOTE_REASON_LEN];
} remote_status_t;

typedef struct {
    char name[REMOTE_NAME_MAX + 1];    // display name in the Keyer's Player list
    char player_id[REMOTE_KEYER_LEN];  // station MAC, the Player's protocol identity
    bool advertising;
} remote_identity_t;

// Starts the transport task and restores the stored name and LAN setting.
void remote_init(void);

remote_identity_t remote_get_identity(void);

// Sets the advertised display name (1..32 bytes) and persists it.
bool remote_set_name(const char *name);

// Turns the LAN transport on or off; off drops any session and closes the sockets.
void remote_set_advertising(bool on);

remote_status_t remote_get_status(void);

// Speed implied by the key-down elements the Keyer is sending, 0 until it has keyed something.
uint8_t remote_peer_wpm(void);

// Drops the session and releases the line.
void remote_unpair(void);
