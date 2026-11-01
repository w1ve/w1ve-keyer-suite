#include "esp_check.h"
#include "esp_log.h"
#include "nvs_flash.h"
#include "psa/crypto.h"

#include "player_config.h"
#include "player_gpio.h"
#include "player_log.h"
#include "player_transport.h"
#include "player_web.h"
#include "player_wifi.h"

static const char *TAG = "w1ve_player";

void app_main(void)
{
    esp_err_t err = nvs_flash_init();
    if (err == ESP_ERR_NVS_NO_FREE_PAGES || err == ESP_ERR_NVS_NEW_VERSION_FOUND) {
        ESP_ERROR_CHECK(nvs_flash_erase());
        err = nvs_flash_init();
    }
    ESP_ERROR_CHECK(err);
    ESP_ERROR_CHECK(psa_crypto_init());

    player_log_init();
    player_config_t config;
    ESP_ERROR_CHECK(player_config_load(&config));
    player_log_set_streaming(config.log_streaming_enabled);
    ESP_ERROR_CHECK(player_gpio_apply_config(&config));
    ESP_ERROR_CHECK(player_wifi_start(&config));
    ESP_ERROR_CHECK(player_transport_start(&config));
    ESP_ERROR_CHECK(player_web_start());

    player_log_write("info", "W1VE ESP32 Player started");
    ESP_LOGI(TAG, "First boot setup AP: %s / w1veplayer", player_wifi_setup_ssid());
}
