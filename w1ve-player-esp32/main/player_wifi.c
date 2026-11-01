#include "player_wifi.h"

#include <stdio.h>
#include <string.h>

#include "esp_check.h"
#include "esp_event.h"
#include "esp_log.h"
#include "esp_mac.h"
#include "esp_netif.h"
#include "esp_wifi.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "lwip/inet.h"
#include "player_gpio.h"
#include "player_log.h"
#include "player_transport.h"
#include "player_web.h"

static const char *TAG = "w1ve_wifi";
static player_wifi_status_t s_status = {
    .ip = "0.0.0.0",
    .netmask_text = "0.0.0.0",
};
static player_config_t s_config;
static esp_netif_t *s_ap_netif;
static esp_netif_t *s_sta_netif;
static SemaphoreHandle_t s_lock;

static esp_err_t ensure_lock(void)
{
    if (!s_lock) {
        s_lock = xSemaphoreCreateMutex();
    }
    return s_lock ? ESP_OK : ESP_ERR_NO_MEM;
}

static void advance_generation_locked(void)
{
    if (++s_status.generation == 0) {
        ++s_status.generation;
    }
}

static void mark_station_unavailable_locked(void)
{
    s_status.connected = false;
    s_status.transport_available = false;
    s_status.address = 0;
    s_status.netmask = 0;
    strlcpy(s_status.ip, "0.0.0.0", sizeof(s_status.ip));
    strlcpy(s_status.netmask_text, "0.0.0.0", sizeof(s_status.netmask_text));
    advance_generation_locked();
}

static bool networks_overlap(uint32_t first_address, uint32_t first_netmask,
                             uint32_t second_address, uint32_t second_netmask)
{
    if (first_address == 0 || first_netmask == 0 || second_address == 0 || second_netmask == 0) {
        return true;
    }
    const uint32_t common_mask = first_netmask & second_netmask;
    return (first_address & common_mask) == (second_address & common_mask);
}

static bool station_transport_available(const esp_netif_ip_info_t *station_info)
{
    if (!station_info || station_info->ip.addr == 0 || station_info->netmask.addr == 0 || !s_ap_netif) {
        return false;
    }
    esp_netif_ip_info_t ap_info = { 0 };
    if (esp_netif_get_ip_info(s_ap_netif, &ap_info) != ESP_OK) {
        return false;
    }
    // A setup-AP client has an AP-subnet source address.  Refuse raw W1VE when the two networks
    // overlap rather than letting an AP-side source satisfy the station-subnet check.
    return !networks_overlap(station_info->ip.addr, station_info->netmask.addr,
                             ap_info.ip.addr, ap_info.netmask.addr);
}

static esp_err_t connect_station(void)
{
    player_config_t config;
    xSemaphoreTake(s_lock, portMAX_DELAY);
    memcpy(&config, &s_config, sizeof(config));
    xSemaphoreGive(s_lock);
    if (!player_config_is_complete(&config)) {
        return ESP_OK;
    }

    wifi_config_t station = { 0 };
    strlcpy((char *)station.sta.ssid, config.wifi_ssid, sizeof(station.sta.ssid));
    strlcpy((char *)station.sta.password, config.wifi_password, sizeof(station.sta.password));
    station.sta.threshold.authmode = WIFI_AUTH_WPA2_PSK;
    esp_err_t err = esp_wifi_set_config(WIFI_IF_STA, &station);
    if (err == ESP_OK) {
        err = esp_wifi_connect();
    }
    if (err == ESP_OK) {
        // This function is called from Wi-Fi event context. Keep browser/log serialization out
        // of `sys_evt`; the status publisher reports the resulting network state asynchronously.
        ESP_LOGI(TAG, "Joining home Wi-Fi '%s'", config.wifi_ssid);
    } else {
        ESP_LOGW(TAG, "Could not start home Wi-Fi join: %s", esp_err_to_name(err));
    }
    return err;
}

static void wifi_event_handler(void *arg, esp_event_base_t base, int32_t event_id, void *data)
{
    (void)arg;
    if (base == WIFI_EVENT && event_id == WIFI_EVENT_STA_START) {
        (void)connect_station();
        return;
    }
    if (base == WIFI_EVENT && event_id == WIFI_EVENT_STA_DISCONNECTED) {
        // Release hardware before publishing the unavailable station state. A racing pair attempt
        // then sees either a live old LAN or a GPIO layer that refuses its generation.
        const esp_err_t release_err = player_gpio_set_network_connected(false);
        xSemaphoreTake(s_lock, portMAX_DELAY);
        mark_station_unavailable_locked();
        const bool configured = player_config_is_complete(&s_config);
        xSemaphoreGive(s_lock);
        player_transport_network_changed();
        if (release_err != ESP_OK) {
            ESP_LOGE(TAG, "Wi-Fi-loss GPIO release failed: %s", esp_err_to_name(release_err));
        }
        ESP_LOGW(TAG, "Home Wi-Fi disconnected; setup AP remains available");
        player_web_request_status_update();
        if (configured) {
            (void)connect_station();
        }
        return;
    }
    if (base == IP_EVENT && event_id == IP_EVENT_STA_GOT_IP) {
        const ip_event_got_ip_t *event = (const ip_event_got_ip_t *)data;
        if (!event || !s_sta_netif || (event->esp_netif && event->esp_netif != s_sta_netif)) {
            return;
        }
        esp_netif_ip_info_t station_info = event->ip_info;
        if (esp_netif_get_ip_info(s_sta_netif, &station_info) != ESP_OK) {
            return;
        }
        const bool transport_available = station_transport_available(&station_info);
        char ip[PLAYER_WIFI_IP_TEXT_MAX];
        inet_ntoa_r(station_info.ip, ip, sizeof(ip));

        xSemaphoreTake(s_lock, portMAX_DELAY);
        s_status.connected = true;
        s_status.transport_available = transport_available;
        s_status.address = station_info.ip.addr;
        s_status.netmask = station_info.netmask.addr;
        inet_ntoa_r(station_info.ip, s_status.ip, sizeof(s_status.ip));
        inet_ntoa_r(station_info.netmask, s_status.netmask_text, sizeof(s_status.netmask_text));
        advance_generation_locked();
        xSemaphoreGive(s_lock);

        // The transport task must observe the new generation and bind fresh station-only sockets
        // before it can pair. A packet that arrives first is rejected by its generation check.
        const esp_err_t gpio_err = player_gpio_set_network_connected(true);
        player_transport_network_changed();
        if (gpio_err != ESP_OK) {
            ESP_LOGE(TAG, "Wi-Fi-up GPIO state update failed: %s", esp_err_to_name(gpio_err));
        }
        if (transport_available) {
            ESP_LOGI(TAG, "Home Wi-Fi connected: %s", ip);
        } else {
            ESP_LOGW(TAG, "Home Wi-Fi connected at %s, but its subnet overlaps the setup AP; raw W1VE is disabled", ip);
        }
        player_web_request_status_update();
    }
}

esp_err_t player_wifi_start(const player_config_t *config)
{
    if (!config) {
        return ESP_ERR_INVALID_ARG;
    }
    esp_err_t err = ensure_lock();
    if (err != ESP_OK) {
        return err;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    memcpy(&s_config, config, sizeof(s_config));
    s_status.generation = 1;
    xSemaphoreGive(s_lock);

    uint8_t mac[6];
    ESP_RETURN_ON_ERROR(esp_read_mac(mac, ESP_MAC_WIFI_SOFTAP), TAG, "read AP MAC");
    xSemaphoreTake(s_lock, portMAX_DELAY);
    snprintf(s_status.setup_ssid, sizeof(s_status.setup_ssid), "W1VE-Player-%02X%02X", mac[4], mac[5]);
    xSemaphoreGive(s_lock);

    ESP_RETURN_ON_ERROR(esp_netif_init(), TAG, "netif init");
    ESP_RETURN_ON_ERROR(esp_event_loop_create_default(), TAG, "event loop init");
    s_ap_netif = esp_netif_create_default_wifi_ap();
    s_sta_netif = esp_netif_create_default_wifi_sta();
    if (!s_ap_netif || !s_sta_netif) {
        return ESP_ERR_NO_MEM;
    }
    wifi_init_config_t init = WIFI_INIT_CONFIG_DEFAULT();
    ESP_RETURN_ON_ERROR(esp_wifi_init(&init), TAG, "wifi init");
    ESP_RETURN_ON_ERROR(esp_event_handler_instance_register(WIFI_EVENT, ESP_EVENT_ANY_ID,
                                                             wifi_event_handler, NULL, NULL),
                        TAG, "register Wi-Fi events");
    ESP_RETURN_ON_ERROR(esp_event_handler_instance_register(IP_EVENT, IP_EVENT_STA_GOT_IP,
                                                             wifi_event_handler, NULL, NULL),
                        TAG, "register IP events");

    player_wifi_status_t status;
    player_wifi_get_status(&status);
    wifi_config_t access_point = { 0 };
    strlcpy((char *)access_point.ap.ssid, status.setup_ssid, sizeof(access_point.ap.ssid));
    // Setup AP credentials are intentionally local-only and printed on serial boot output.
    strlcpy((char *)access_point.ap.password, "w1veplayer", sizeof(access_point.ap.password));
    access_point.ap.ssid_len = strlen(status.setup_ssid);
    access_point.ap.channel = 1;
    access_point.ap.max_connection = 4;
    access_point.ap.authmode = WIFI_AUTH_WPA2_PSK;
    // Keep WPA2 access compatible with Windows clients that do not negotiate required PMF.
    access_point.ap.pmf_cfg.capable = true;
    access_point.ap.pmf_cfg.required = false;

    ESP_RETURN_ON_ERROR(esp_wifi_set_mode(WIFI_MODE_APSTA), TAG, "set APSTA mode");
    ESP_RETURN_ON_ERROR(esp_wifi_set_config(WIFI_IF_AP, &access_point), TAG, "configure setup AP");
    ESP_RETURN_ON_ERROR(esp_wifi_start(), TAG, "start Wi-Fi");
    player_log_write("info", "Setup Wi-Fi '%s', password w1veplayer, at http://192.168.4.1", status.setup_ssid);
    return ESP_OK;
}

esp_err_t player_wifi_apply_config(const player_config_t *config)
{
    if (!config || !s_lock) {
        return ESP_ERR_INVALID_STATE;
    }

    // Disable the physical output gate first. Then make both the network snapshot and transport
    // state unavailable before asking Wi-Fi to leave or join a station asynchronously.
    const esp_err_t release_err = player_gpio_set_network_connected(false);
    xSemaphoreTake(s_lock, portMAX_DELAY);
    memcpy(&s_config, config, sizeof(s_config));
    mark_station_unavailable_locked();
    xSemaphoreGive(s_lock);
    const esp_err_t transport_err = player_transport_sync_network();

    const esp_err_t disconnect_err = esp_wifi_disconnect();
    const esp_err_t connect_err = connect_station();
    if (release_err != ESP_OK) {
        return release_err;
    }
    if (transport_err != ESP_OK) {
        return transport_err;
    }
    // Disconnect is asynchronous and can report not-connected during first-boot provisioning.
    // A successful connect request is sufficient after the transport gate has been closed.
    if (connect_err != ESP_OK) {
        return connect_err;
    }
    if (disconnect_err != ESP_OK) {
        ESP_LOGD(TAG, "Wi-Fi disconnect returned %s while reconfiguring", esp_err_to_name(disconnect_err));
    }
    return ESP_OK;
}

void player_wifi_get_status(player_wifi_status_t *status)
{
    if (!status) {
        return;
    }
    memset(status, 0, sizeof(*status));
    strlcpy(status->ip, "0.0.0.0", sizeof(status->ip));
    strlcpy(status->netmask_text, "0.0.0.0", sizeof(status->netmask_text));
    if (!s_lock) {
        return;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    memcpy(status, &s_status, sizeof(*status));
    xSemaphoreGive(s_lock);
}

const char *player_wifi_setup_ssid(void)
{
    return s_status.setup_ssid;
}
