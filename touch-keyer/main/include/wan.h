#pragma once

#include <stdbool.h>
#include <stdint.h>

// WAN transport: reaches a remote W1VE Remote Keyer over the Internet using the RWK relay
// (rendezvous + optional forwarding) and WireGuard, exactly as the desktop Players do. It is an
// optional second transport beside the LAN Player in remote.c; the keyer application and the edge
// applier do not know or care which path a session came in on.
//
// Threading: one task owns all sockets and the WireGuard state. The public getters/setters take a
// short mutex and copy, like remote.c. Nothing here ever blocks the keying path.

#define WAN_RELAY_MAX 64   // "host:port" or "ip:port"
#define WAN_CODE_MAX 32    // pairing code shown by the operator / typed on the desktop
#define WAN_STUN_MAX 64
#define WAN_FP_LEN 16      // "ab12 cd34 ef56" style public-key fingerprint, plus terminator
#define WAN_REASON_LEN 64

typedef enum {
    WAN_DISABLED = 0,
    WAN_WIFI_DOWN,       // no station network yet
    WAN_RESOLVING,       // DNS for the relay
    WAN_RENDEZVOUS,      // TCP to the relay, Pair sent, waiting for Paired
    WAN_PAIRING,         // WireGuard handshake in progress (direct or relay)
    WAN_DIRECT,          // session up over a direct UDP path
    WAN_RELAY,           // session up over the TCP relay
    WAN_RECONNECTING,    // lost; retrying within the budget
    WAN_ERROR,           // gave up; the reason says why
} wan_state_t;

typedef struct {
    bool enabled;
    wan_state_t state;
    bool paired;         // a WireGuard session is established
    bool active;         // enabled + paired + a Keyer is keying/alive: it owns the line
    bool keying;         // the session is asserting KEY right now
    bool direct;         // true on a direct UDP path, false on the relay
    char relay[WAN_RELAY_MAX + 1];
    char code[WAN_CODE_MAX + 1];
    char pubkey_fp[WAN_FP_LEN + 8];  // this device's public key fingerprint (safe to show)
    uint8_t peer_wpm;    // measured remote speed, 0 until the Keyer has keyed
    int rtt_ms;
    uint32_t jitter_ms;  // playout buffer currently in use
    char reason[WAN_REASON_LEN];
} wan_status_t;

// Starts the WAN task and restores stored settings (enable flag, relay, code). Generates a
// WireGuard identity in NVS on first use. Must be called after net_init() and remote_init().
void wan_init(void);

wan_status_t wan_get_status(void);

// Enable/disable WAN. Disabling tears any session down immediately and closes all sockets.
void wan_set_enabled(bool on);

// Relay endpoint ("host:port"); an empty string restores the compiled-in default. Persisted.
void wan_set_relay(const char *relay);

// Pairing code both ends share. Persisted.
void wan_set_code(const char *code);

// Drops any session now.
void wan_disconnect(void);

// True while WAN owns the shared remote line, so remote.c refuses a LAN session (and vice versa).
bool wan_owns_line(void);
