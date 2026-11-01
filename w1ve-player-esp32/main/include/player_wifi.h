#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "esp_err.h"
#include "player_config.h"

#define PLAYER_WIFI_IP_TEXT_MAX 16
#define PLAYER_WIFI_SETUP_SSID_MAX 33

typedef struct {
    bool connected;
    /** True only when raw W1VE traffic is allowed on the current station IPv4 subnet. */
    bool transport_available;
    uint32_t address;
    uint32_t netmask;
    uint32_t generation;
    char ip[PLAYER_WIFI_IP_TEXT_MAX];
    char netmask_text[PLAYER_WIFI_IP_TEXT_MAX];
    char setup_ssid[PLAYER_WIFI_SETUP_SSID_MAX];
} player_wifi_status_t;

esp_err_t player_wifi_start(const player_config_t *config);
esp_err_t player_wifi_apply_config(const player_config_t *config);
void player_wifi_get_status(player_wifi_status_t *status);
const char *player_wifi_setup_ssid(void);
