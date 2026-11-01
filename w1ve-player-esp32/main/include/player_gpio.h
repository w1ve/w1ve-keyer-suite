#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "esp_err.h"
#include "player_config.h"

typedef struct {
    bool key_down;
    bool ptt_on;
    bool network_connected;
    bool paired;
    uint32_t active_session_generation;
} player_gpio_status_t;

bool player_gpio_is_safe_output(int gpio);
esp_err_t player_gpio_apply_config(const player_config_t *config);
esp_err_t player_gpio_force_release(void);
/** Activates a newly paired transport generation only while the station LAN is available. */
esp_err_t player_gpio_activate_session(uint32_t session_generation);
/** Releases outputs and invalidates any generation that could still drive them. */
esp_err_t player_gpio_deactivate_session(void);
/** Wi-Fi loss releases outputs before the transport task tears down session state. */
esp_err_t player_gpio_set_network_connected(bool connected);
/** Applies an accepted edge only when it belongs to the current active session generation. */
esp_err_t player_gpio_apply_key(uint32_t session_generation, bool key_down);
void player_gpio_get_status(player_gpio_status_t *status);
bool player_gpio_key_down(void);
bool player_gpio_ptt_on(void);
