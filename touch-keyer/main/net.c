// Network layer.
//
// The box always comes up as its own access point, so it can be reached and configured
// before it has ever seen a network: SSID "W1VE-Keyer", password "w1vekeyer", address
// 192.168.4.1. When credentials are stored (chosen on the Wi-Fi page, or written into NVS
// by the web UI) it also joins that network, and the screen then shows the station address
// instead. The access point stays up in both cases: it is the way back in if the station
// side breaks.
#include "net.h"

#include <string.h>

#include "esp_event.h"
#include "esp_log.h"
#include "esp_netif.h"
#include "esp_wifi.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "nvs.h"
#include "nvs_flash.h"

static const char *TAG = "net";

#define AP_SSID "W1VE-Keyer"
#define AP_PASS "w1vekeyer"  // WPA2 needs at least 8 characters
#define AP_CHANNEL 6
#define AP_MAX_CLIENTS 4

#define JOIN_TIMEOUT_MS 12000
#define RECONNECT_DELAY_MS 2000

static SemaphoreHandle_t s_lock;
static net_status_t s_status;
static esp_netif_t *s_ap_netif;
static esp_netif_t *s_sta_netif;
static volatile bool s_want_sta;

static void lock(void)
{
    xSemaphoreTake(s_lock, portMAX_DELAY);
}

static void unlock(void)
{
    xSemaphoreGive(s_lock);
}

// Picks the address the screen should show: the station address once joined, otherwise the
// access point's.
static void refresh_address(void)
{
    bool connected;
    lock();
    connected = s_status.connected;
    unlock();

    esp_netif_t *iface = connected ? s_sta_netif : s_ap_netif;
    esp_netif_ip_info_t info = {0};
    char buf[16] = "0.0.0.0";

    if (iface != NULL && esp_netif_get_ip_info(iface, &info) == ESP_OK && info.ip.addr != 0) {
        esp_ip4addr_ntoa(&info.ip, buf, sizeof(buf));
    }

    lock();
    snprintf(s_status.address, sizeof(s_status.address), "%s", buf);
    unlock();
}

static void on_wifi_event(void *arg, esp_event_base_t base, int32_t id, void *data)
{
    (void)arg;
    (void)data;

    if (base != WIFI_EVENT) {
        return;
    }

    switch (id) {
    case WIFI_EVENT_AP_START:
        lock();
        s_status.ap_active = true;
        unlock();
        refresh_address();
        net_status_t st = net_get_status();
        ESP_LOGI(TAG, "access point up: %s at %s", AP_SSID, st.address);
        break;

    case WIFI_EVENT_AP_STOP:
        lock();
        s_status.ap_active = false;
        unlock();
        break;

    case WIFI_EVENT_STA_START:
        if (s_want_sta) {
            esp_wifi_connect();
        }
        break;

    case WIFI_EVENT_STA_DISCONNECTED:
        lock();
        s_status.connected = false;
        s_status.rssi = 0;
        unlock();
        refresh_address();
        if (s_want_sta) {
            // The access point may simply be off; keep trying.
            vTaskDelay(pdMS_TO_TICKS(RECONNECT_DELAY_MS));
            esp_wifi_connect();
        }
        break;

    default:
        break;
    }
}

static void on_ip_event(void *arg, esp_event_base_t base, int32_t id, void *data)
{
    (void)arg;
    (void)base;

    if (id != IP_EVENT_STA_GOT_IP) {
        return;
    }

    const ip_event_got_ip_t *got = (const ip_event_got_ip_t *)data;
    char addr[16] = {0};
    esp_ip4addr_ntoa(&got->ip_info.ip, addr, sizeof(addr));

    wifi_ap_record_t ap = {0};
    const bool have_ap = esp_wifi_sta_get_ap_info(&ap) == ESP_OK;

    lock();
    s_status.connected = true;
    s_status.rssi = have_ap ? ap.rssi : 0;
    if (have_ap) {
        const size_t n = strnlen((const char *)ap.ssid, sizeof(ap.ssid));
        memcpy(s_status.ssid, ap.ssid, n);
        s_status.ssid[n] = '\0';
    }
    snprintf(s_status.address, sizeof(s_status.address), "%s", addr);
    unlock();

    ESP_LOGI(TAG, "joined '%s' at %s (%d dBm)", s_status.ssid, addr, s_status.rssi);
}

static void store_credentials(const char *ssid, const char *pass)
{
    nvs_handle_t nvs;
    if (nvs_open("w1ve", NVS_READWRITE, &nvs) != ESP_OK) {
        return;
    }
    nvs_set_str(nvs, "ssid", ssid);
    nvs_set_str(nvs, "pass", pass);
    nvs_commit(nvs);
    nvs_close(nvs);
}

static bool load_credentials(char *ssid, size_t ssid_len, char *pass, size_t pass_len)
{
    nvs_handle_t nvs;
    if (nvs_open("w1ve", NVS_READONLY, &nvs) != ESP_OK) {
        return false;
    }
    const bool ok = nvs_get_str(nvs, "ssid", ssid, &ssid_len) == ESP_OK &&
                    nvs_get_str(nvs, "pass", pass, &pass_len) == ESP_OK;
    nvs_close(nvs);
    return ok;
}

static void apply_station(const char *ssid, const char *pass)
{
    wifi_config_t sta = {0};
    memcpy(sta.sta.ssid, ssid, strnlen(ssid, sizeof(sta.sta.ssid)));
    memcpy(sta.sta.password, pass, strnlen(pass, sizeof(sta.sta.password)));
    // Scans are done for us, so accept whatever the chosen network uses.
    sta.sta.threshold.authmode = WIFI_AUTH_OPEN;
    sta.sta.pmf_cfg.capable = true;

    esp_wifi_set_config(WIFI_IF_STA, &sta);

    lock();
    snprintf(s_status.ssid, sizeof(s_status.ssid), "%s", ssid);
    unlock();

    s_want_sta = true;
}

void net_init(void)
{
    s_lock = xSemaphoreCreateMutex();
    configASSERT(s_lock);

    ESP_ERROR_CHECK(esp_netif_init());
    ESP_ERROR_CHECK(esp_event_loop_create_default());
    s_ap_netif = esp_netif_create_default_wifi_ap();
    s_sta_netif = esp_netif_create_default_wifi_sta();

    wifi_init_config_t cfg = WIFI_INIT_CONFIG_DEFAULT();
    ESP_ERROR_CHECK(esp_wifi_init(&cfg));
    ESP_ERROR_CHECK(esp_event_handler_register(WIFI_EVENT, ESP_EVENT_ANY_ID, on_wifi_event, NULL));
    ESP_ERROR_CHECK(esp_event_handler_register(IP_EVENT, IP_EVENT_STA_GOT_IP, on_ip_event, NULL));

    wifi_config_t ap = {0};
    snprintf((char *)ap.ap.ssid, sizeof(ap.ap.ssid), "%s", AP_SSID);
    ap.ap.ssid_len = strlen(AP_SSID);
    snprintf((char *)ap.ap.password, sizeof(ap.ap.password), "%s", AP_PASS);
    ap.ap.channel = AP_CHANNEL;
    ap.ap.max_connection = AP_MAX_CLIENTS;
    ap.ap.authmode = WIFI_AUTH_WPA2_PSK;
    ap.ap.pmf_cfg.required = false;

    ESP_ERROR_CHECK(esp_wifi_set_mode(WIFI_MODE_APSTA));
    ESP_ERROR_CHECK(esp_wifi_set_config(WIFI_IF_AP, &ap));

    lock();
    snprintf(s_status.ap_ssid, sizeof(s_status.ap_ssid), "%s", AP_SSID);
    snprintf(s_status.ap_pass, sizeof(s_status.ap_pass), "%s", AP_PASS);
    unlock();

    char ssid[33] = {0};
    char pass[65] = {0};
    if (load_credentials(ssid, sizeof(ssid), pass, sizeof(pass))) {
        apply_station(ssid, pass);
        ESP_LOGI(TAG, "found stored network '%s'", ssid);
    } else {
        ESP_LOGI(TAG, "no stored network, access point only");
    }

    ESP_ERROR_CHECK(esp_wifi_start());
    refresh_address();
}

bool net_scan(net_scan_result_t *out)
{
    if (out == NULL) {
        return false;
    }
    out->count = 0;

    wifi_scan_config_t scan = {0};
    scan.show_hidden = false;
    scan.scan_type = WIFI_SCAN_TYPE_ACTIVE;

    if (esp_wifi_scan_start(&scan, true) != ESP_OK) {
        ESP_LOGW(TAG, "scan failed to start");
        return false;
    }

    uint16_t found = NET_SCAN_MAX;
    wifi_ap_record_t records[NET_SCAN_MAX] = {0};
    if (esp_wifi_scan_get_ap_records(&found, records) != ESP_OK) {
        return false;
    }

    for (uint16_t i = 0; i < found && out->count < NET_SCAN_MAX; i++) {
        const size_t len = strnlen((const char *)records[i].ssid, sizeof(records[i].ssid));
        if (len == 0) {
            continue;  // hidden network
        }
        net_scan_entry_t *entry = &out->entries[out->count++];
        memcpy(entry->ssid, records[i].ssid, len);
        entry->ssid[len] = '\0';
        entry->rssi = records[i].rssi;
        entry->secure = records[i].authmode != WIFI_AUTH_OPEN;
    }

    ESP_LOGI(TAG, "scan found %d network(s)", out->count);
    return true;
}

bool net_join(const char *ssid, const char *pass)
{
    if (ssid == NULL || ssid[0] == '\0') {
        return false;
    }

    apply_station(ssid, pass != NULL ? pass : "");
    esp_wifi_disconnect();
    esp_wifi_connect();

    for (int waited = 0; waited < JOIN_TIMEOUT_MS; waited += 250) {
        bool connected;
        lock();
        connected = s_status.connected;
        unlock();
        if (connected) {
            store_credentials(ssid, pass != NULL ? pass : "");
            refresh_address();
            return true;
        }
        vTaskDelay(pdMS_TO_TICKS(250));
    }

    ESP_LOGW(TAG, "could not join '%s'", ssid);
    return false;
}

void net_forget(void)
{
    nvs_handle_t nvs;
    if (nvs_open("w1ve", NVS_READWRITE, &nvs) == ESP_OK) {
        nvs_erase_key(nvs, "ssid");
        nvs_erase_key(nvs, "pass");
        nvs_commit(nvs);
        nvs_close(nvs);
    }

    s_want_sta = false;
    esp_wifi_disconnect();

    lock();
    s_status.connected = false;
    s_status.rssi = 0;
    s_status.ssid[0] = '\0';
    unlock();
    refresh_address();
}

net_status_t net_get_status(void)
{
    net_status_t out = {0};
    if (s_lock == NULL) {
        return out;
    }
    lock();
    out = s_status;
    unlock();

    if (out.connected) {
        wifi_ap_record_t ap = {0};
        if (esp_wifi_sta_get_ap_info(&ap) == ESP_OK) {
            out.rssi = ap.rssi;
        }
    }
    return out;
}
