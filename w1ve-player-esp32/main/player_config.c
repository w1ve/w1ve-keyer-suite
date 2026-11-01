#include "player_config.h"

#include <stdio.h>
#include <string.h>

#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "nvs.h"

static const char *TAG = "w1ve_config";
static const char *NVS_NAMESPACE = "w1ve_player";
static const char *NVS_KEY = "config";
static player_config_t s_current;
static SemaphoreHandle_t s_lock;

static esp_err_t ensure_lock(void)
{
    if (!s_lock) {
        s_lock = xSemaphoreCreateMutex();
    }
    return s_lock ? ESP_OK : ESP_ERR_NO_MEM;
}

static void normalize_config_strings(player_config_t *config)
{
    config->wifi_ssid[sizeof(config->wifi_ssid) - 1] = '\0';
    config->wifi_password[sizeof(config->wifi_password) - 1] = '\0';
    config->device_name[sizeof(config->device_name) - 1] = '\0';
}

static void set_current(const player_config_t *config)
{
    xSemaphoreTake(s_lock, portMAX_DELAY);
    memcpy(&s_current, config, sizeof(s_current));
    xSemaphoreGive(s_lock);
}

void player_config_defaults(player_config_t *config)
{
    memset(config, 0, sizeof(*config));
    config->version = PLAYER_CONFIG_VERSION;
    config->configured = false;
    config->log_streaming_enabled = true;
    config->key_active_low = true;
    config->ptt_active_low = true;
    config->rgb_enabled = true;
    config->key_gpio = 4;
    config->ptt_gpio = 5;
    config->rgb_gpio = 48;
    config->ptt_hang_ms = 200;
    snprintf(config->device_name, sizeof(config->device_name), "W1VE ESP32 Player");
}

bool player_config_is_complete(const player_config_t *config)
{
    return config && config->configured && config->wifi_ssid[0] != '\0';
}

esp_err_t player_config_load(player_config_t *config)
{
    if (!config) {
        return ESP_ERR_INVALID_ARG;
    }
    esp_err_t err = ensure_lock();
    if (err != ESP_OK) {
        return err;
    }

    nvs_handle_t handle;
    err = nvs_open(NVS_NAMESPACE, NVS_READWRITE, &handle);
    if (err != ESP_OK) {
        return err;
    }

    size_t size = sizeof(*config);
    err = nvs_get_blob(handle, NVS_KEY, config, &size);
    nvs_close(handle);
    if (err == ESP_ERR_NVS_NOT_FOUND || size != sizeof(*config) || config->version != PLAYER_CONFIG_VERSION) {
        player_config_defaults(config);
        set_current(config);
        ESP_LOGI(TAG, "Using first-boot configuration defaults");
        return ESP_OK;
    }
    if (err != ESP_OK) {
        return err;
    }

    // Existing v1 blobs are retained, but never let an unterminated legacy field reach a string API.
    normalize_config_strings(config);
    // Rename only the previous stock label; preserve any operator-provided Player name.
    if (strcmp(config->device_name, "W1VE ESP Player") == 0) {
        snprintf(config->device_name, sizeof(config->device_name), "W1VE ESP32 Player");
        (void)player_config_save(config);
    }
    set_current(config);
    return ESP_OK;
}

esp_err_t player_config_get_current(player_config_t *config)
{
    if (!config) {
        return ESP_ERR_INVALID_ARG;
    }
    if (!s_lock) {
        return ESP_ERR_INVALID_STATE;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    memcpy(config, &s_current, sizeof(*config));
    xSemaphoreGive(s_lock);
    return ESP_OK;
}

esp_err_t player_config_replace(const player_config_t *config)
{
    if (!config) {
        return ESP_ERR_INVALID_ARG;
    }
    player_config_t normalized = *config;
    normalize_config_strings(&normalized);
    esp_err_t err = player_config_save(&normalized);
    if (err == ESP_OK) {
        if (!s_lock) {
            return ESP_ERR_INVALID_STATE;
        }
        set_current(&normalized);
    }
    return err;
}

esp_err_t player_config_save(const player_config_t *config)
{
    if (!config) {
        return ESP_ERR_INVALID_ARG;
    }
    player_config_t normalized = *config;
    normalize_config_strings(&normalized);
    nvs_handle_t handle;
    esp_err_t err = nvs_open(NVS_NAMESPACE, NVS_READWRITE, &handle);
    if (err != ESP_OK) {
        return err;
    }
    err = nvs_set_blob(handle, NVS_KEY, &normalized, sizeof(normalized));
    if (err == ESP_OK) {
        err = nvs_commit(handle);
    }
    nvs_close(handle);
    return err;
}
