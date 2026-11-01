#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "esp_err.h"
#include "player_config.h"

#define PLAYER_DISCOVERY_PORT 4994
#define PLAYER_TRANSPORT_PORT 4995
#define PLAYER_KEY_LEASE_MS 1500
#define PLAYER_MISSED_PING_LIMIT 3

/** Current W1VE ESP32 Player transport state exposed to the web UI. */
typedef struct {
    bool paired;
    bool peer_alive;
    bool transport_available;
    uint32_t session_id;
    uint32_t last_transport_sequence;
    uint32_t last_edge_sequence;
    int rtt_ms;
    uint8_t missed_pings;
    char peer_address[40];
    char disconnect_reason[96];
} player_transport_status_t;

esp_err_t player_transport_start(const player_config_t *config);
/** Queues a configuration change and waits until the old session has released its outputs. */
esp_err_t player_transport_apply_config(const player_config_t *config);
void player_transport_stop(void);
esp_err_t player_transport_get_status(player_transport_status_t *status);
/** Queues a local unpair and waits until GPIO release has completed. */
esp_err_t player_transport_unpair(void);
/** Wakes the transport owner after an asynchronous station event. */
void player_transport_network_changed(void);
/** Disables current station traffic and waits for transport-side release acknowledgement. */
esp_err_t player_transport_sync_network(void);
