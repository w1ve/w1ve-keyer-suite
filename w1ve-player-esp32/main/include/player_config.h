#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "esp_err.h"

#define PLAYER_CONFIG_VERSION 1
#define PLAYER_WIFI_SSID_MAX 32
#define PLAYER_WIFI_PASSWORD_MAX 64
#define PLAYER_NAME_MAX 32

/**
 * Persistent configuration stored as one raw versioned NVS blob.
 *
 * Keep the reserved layout bytes and all subsequent fields at their deployed v1 offsets. Existing
 * records must continue to load unchanged so saved Wi-Fi configuration is never reset by this
 * firmware update.
 */
typedef struct {
    uint32_t version;
    bool configured;
    bool log_streaming_enabled;
    bool key_active_low;
    bool ptt_active_low;
    bool rgb_enabled;
    int8_t key_gpio;
    int8_t ptt_gpio;
    int8_t rgb_gpio;
    uint16_t ptt_hang_ms;
    uint8_t reserved_after_ptt_hang[3];
    char wifi_ssid[PLAYER_WIFI_SSID_MAX + 1];
    char wifi_password[PLAYER_WIFI_PASSWORD_MAX + 1];
    char device_name[PLAYER_NAME_MAX + 1];
    /** Reserved bytes retained for NVS compatibility; Player LAN pairing stores no secret. */
    uint8_t reserved[33];
} player_config_t;

_Static_assert(offsetof(player_config_t, ptt_hang_ms) == 12, "NVS PTT hang offset changed");
_Static_assert(offsetof(player_config_t, reserved_after_ptt_hang) == 14, "NVS reserved offset changed");
_Static_assert(offsetof(player_config_t, wifi_ssid) == 17, "NVS Wi-Fi SSID offset changed");
_Static_assert(offsetof(player_config_t, wifi_password) == 50, "NVS Wi-Fi password offset changed");
_Static_assert(offsetof(player_config_t, device_name) == 115, "NVS device name offset changed");
_Static_assert(offsetof(player_config_t, reserved) == 148, "NVS trailing reserved offset changed");
_Static_assert(sizeof(player_config_t) == 184, "NVS configuration size changed");

esp_err_t player_config_load(player_config_t *config);
esp_err_t player_config_save(const player_config_t *config);
esp_err_t player_config_get_current(player_config_t *config);
esp_err_t player_config_replace(const player_config_t *config);
void player_config_defaults(player_config_t *config);
bool player_config_is_complete(const player_config_t *config);
