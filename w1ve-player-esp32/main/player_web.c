#include "player_web.h"

#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "esp_check.h"
#include "esp_http_server.h"
#include "esp_log.h"
#include "esp_wifi.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "player_config.h"
#include "player_gpio.h"
#include "player_json.h"
#include "player_log.h"
#include "player_transport.h"
#include "player_wifi.h"

static const char *TAG = "w1ve_web";
static httpd_handle_t s_server;
static int s_ws_clients[4];
static TaskHandle_t s_status_task;

#define STATUS_JSON_CAPACITY 2048
#define WS_STATUS_EVENT_CAPACITY (STATUS_JSON_CAPACITY + 64)
#define WS_LOG_EVENT_CAPACITY 1024
#define LOG_RECORD_JSON_CAPACITY 1024
#define SCAN_JSON_CAPACITY 4096

extern const uint8_t web_index_html_start[] asm("_binary_index_html_start");
extern const uint8_t web_index_html_end[] asm("_binary_index_html_end");

typedef struct {
    size_t length;
    char payload[];
} websocket_work_t;

static bool append_string_field(player_json_writer_t *writer, const char *text, size_t capacity)
{
    return player_json_append_string_n(writer, (const uint8_t *)text, strnlen(text, capacity));
}

static bool build_status_json(char *output, size_t output_size)
{
    player_transport_status_t transport;
    player_wifi_status_t wifi;
    player_gpio_status_t gpio;
    player_config_t config;
    if (player_transport_get_status(&transport) != ESP_OK ||
        player_config_get_current(&config) != ESP_OK) {
        return false;
    }
    player_wifi_get_status(&wifi);
    player_gpio_get_status(&gpio);

    player_json_writer_t json;
    player_json_writer_init(&json, output, output_size);
#define APPEND_JSON(value) do { if (!(value)) { return false; } } while (0)
    APPEND_JSON(player_json_append_literal(&json, "{\"deviceName\":"));
    APPEND_JSON(append_string_field(&json, config.device_name, sizeof(config.device_name)));
    APPEND_JSON(player_json_append_literal(&json, ",\"setupSsid\":"));
    APPEND_JSON(append_string_field(&json, wifi.setup_ssid, sizeof(wifi.setup_ssid)));
    APPEND_JSON(player_json_append_literal(&json, ",\"wifiSsid\":"));
    APPEND_JSON(append_string_field(&json, config.wifi_ssid, sizeof(config.wifi_ssid)));
    APPEND_JSON(player_json_append_literal(&json, ",\"ip\":"));
    APPEND_JSON(append_string_field(&json, wifi.ip, sizeof(wifi.ip)));
    APPEND_JSON(player_json_append_literal(&json, ",\"networkConnected\":"));
    APPEND_JSON(player_json_append_bool(&json, wifi.connected));
    APPEND_JSON(player_json_append_literal(&json, ",\"transportAvailable\":"));
    APPEND_JSON(player_json_append_bool(&json, transport.transport_available));
    APPEND_JSON(player_json_append_literal(&json, ",\"paired\":"));
    APPEND_JSON(player_json_append_bool(&json, transport.paired));
    APPEND_JSON(player_json_append_literal(&json, ",\"peerAlive\":"));
    APPEND_JSON(player_json_append_bool(&json, transport.peer_alive));
    APPEND_JSON(player_json_append_literal(&json, ",\"peer\":"));
    APPEND_JSON(append_string_field(&json, transport.peer_address, sizeof(transport.peer_address)));
    APPEND_JSON(player_json_append_literal(&json, ",\"disconnectReason\":"));
    APPEND_JSON(append_string_field(&json, transport.disconnect_reason, sizeof(transport.disconnect_reason)));
    APPEND_JSON(player_json_append_literal(&json, ",\"rttMs\":"));
    APPEND_JSON(player_json_append_int(&json, transport.rtt_ms));
    APPEND_JSON(player_json_append_literal(&json, ",\"missedPings\":"));
    APPEND_JSON(player_json_append_uint(&json, transport.missed_pings));
    APPEND_JSON(player_json_append_literal(&json, ",\"keyDown\":"));
    APPEND_JSON(player_json_append_bool(&json, gpio.key_down));
    APPEND_JSON(player_json_append_literal(&json, ",\"pttOn\":"));
    APPEND_JSON(player_json_append_bool(&json, gpio.ptt_on));
    APPEND_JSON(player_json_append_literal(&json, ",\"keyGpio\":"));
    APPEND_JSON(player_json_append_int(&json, config.key_gpio));
    APPEND_JSON(player_json_append_literal(&json, ",\"pttGpio\":"));
    APPEND_JSON(player_json_append_int(&json, config.ptt_gpio));
    APPEND_JSON(player_json_append_literal(&json, ",\"rgbGpio\":"));
    APPEND_JSON(player_json_append_int(&json, config.rgb_gpio));
    APPEND_JSON(player_json_append_literal(&json, ",\"keyActiveLow\":"));
    APPEND_JSON(player_json_append_bool(&json, config.key_active_low));
    APPEND_JSON(player_json_append_literal(&json, ",\"pttActiveLow\":"));
    APPEND_JSON(player_json_append_bool(&json, config.ptt_active_low));
    APPEND_JSON(player_json_append_literal(&json, ",\"rgbEnabled\":"));
    APPEND_JSON(player_json_append_bool(&json, config.rgb_enabled));
    APPEND_JSON(player_json_append_literal(&json, ",\"pttHangMs\":"));
    APPEND_JSON(player_json_append_uint(&json, config.ptt_hang_ms));
    APPEND_JSON(player_json_append_literal(&json, ",\"logsEnabled\":"));
    APPEND_JSON(player_json_append_bool(&json, config.log_streaming_enabled));
    APPEND_JSON(player_json_append_literal(&json, "}"));
#undef APPEND_JSON
    return player_json_writer_ok(&json);
}

static esp_err_t send_json(httpd_req_t *request, const char *json)
{
    esp_err_t err = httpd_resp_set_type(request, "application/json");
    if (err == ESP_OK) {
        err = httpd_resp_set_hdr(request, "Cache-Control", "no-store");
    }
    if (err == ESP_OK) {
        err = httpd_resp_sendstr(request, json);
    }
    return err;
}

static esp_err_t start_json_stream(httpd_req_t *request)
{
    esp_err_t err = httpd_resp_set_type(request, "application/json");
    if (err == ESP_OK) {
        err = httpd_resp_set_hdr(request, "Cache-Control", "no-store");
    }
    return err;
}

static esp_err_t root_handler(httpd_req_t *request)
{
    esp_err_t err = httpd_resp_set_type(request, "text/html; charset=utf-8");
    if (err == ESP_OK) {
        err = httpd_resp_set_hdr(request, "Cache-Control", "no-store");
    }
    if (err == ESP_OK) {
        err = httpd_resp_send(request, (const char *)web_index_html_start,
                              web_index_html_end - web_index_html_start);
    }
    return err;
}

static esp_err_t status_handler(httpd_req_t *request)
{
    char status[STATUS_JSON_CAPACITY];
    if (!build_status_json(status, sizeof(status))) {
        httpd_resp_send_err(request, HTTPD_500_INTERNAL_SERVER_ERROR, "status serialization failed");
        return ESP_FAIL;
    }
    return send_json(request, status);
}

static bool build_log_entry_json(char *output, size_t output_size, const player_log_entry_t *entry)
{
    player_json_writer_t json;
    player_json_writer_init(&json, output, output_size);
    return player_json_append_literal(&json, "{\"level\":") &&
           append_string_field(&json, entry->level, sizeof(entry->level)) &&
           player_json_append_literal(&json, ",\"message\":") &&
           append_string_field(&json, entry->message, sizeof(entry->message)) &&
           player_json_append_literal(&json, "}") && player_json_writer_ok(&json);
}

static esp_err_t logs_handler(httpd_req_t *request)
{
    player_log_entry_t *entries = calloc(PLAYER_LOG_MAX_ENTRIES, sizeof(*entries));
    if (!entries) {
        httpd_resp_send_err(request, HTTPD_500_INTERNAL_SERVER_ERROR, "log snapshot allocation failed");
        return ESP_ERR_NO_MEM;
    }
    const size_t count = player_log_snapshot(entries, PLAYER_LOG_MAX_ENTRIES);
    esp_err_t err = start_json_stream(request);
    if (err == ESP_OK) {
        err = httpd_resp_send_chunk(request, "[", 1);
    }
    for (size_t index = 0; err == ESP_OK && index < count; ++index) {
        char record[LOG_RECORD_JSON_CAPACITY];
        if (!build_log_entry_json(record, sizeof(record), &entries[index])) {
            // The record is bounded by the entry field limits. If this invariant ever changes,
            // omit the record rather than emitting a partial JSON token.
            ESP_LOGE(TAG, "Skipping log record that exceeded JSON capacity");
            continue;
        }
        if (index != 0) {
            err = httpd_resp_send_chunk(request, ",", 1);
        }
        if (err == ESP_OK) {
            err = httpd_resp_send_chunk(request, record, HTTPD_RESP_USE_STRLEN);
        }
    }
    if (err == ESP_OK) {
        err = httpd_resp_send_chunk(request, "]", 1);
    }
    if (err == ESP_OK) {
        err = httpd_resp_send_chunk(request, NULL, 0);
    }
    free(entries);
    return err;
}

static const char *json_value(const char *json, const char *name)
{
    char key[48];
    snprintf(key, sizeof(key), "\"%s\"", name);
    const char *value = strstr(json, key);
    if (!value) {
        return NULL;
    }
    value += strlen(key);
    while (isspace((unsigned char)*value)) {
        ++value;
    }
    if (*value++ != ':') {
        return NULL;
    }
    while (isspace((unsigned char)*value)) {
        ++value;
    }
    return value;
}

static void json_string_if_present(const char *json, const char *name, char *output, size_t output_len)
{
    const char *value = json_value(json, name);
    if (!value || *value != '"') {
        return;
    }
    ++value;
    size_t written = 0;
    while (*value && *value != '"' && written + 1 < output_len) {
        if (*value == '\\' && value[1]) {
            ++value;
        }
        output[written++] = *value++;
    }
    output[written] = '\0';
}

static void json_int_if_present(const char *json, const char *name, int *target)
{
    const char *value = json_value(json, name);
    if (value) {
        char *end;
        const long parsed = strtol(value, &end, 10);
        if (end != value) {
            *target = (int)parsed;
        }
    }
}

static void json_bool_if_present(const char *json, const char *name, bool *target)
{
    const char *value = json_value(json, name);
    if (!value) {
        return;
    }
    if (strncmp(value, "true", 4) == 0) {
        *target = true;
    } else if (strncmp(value, "false", 5) == 0) {
        *target = false;
    }
}

static esp_err_t config_handler(httpd_req_t *request)
{
    if (request->content_len <= 0 || request->content_len > 1024) {
        httpd_resp_send_err(request, HTTPD_400_BAD_REQUEST, "invalid configuration payload");
        return ESP_FAIL;
    }
    char body[1025];
    const size_t expected = (size_t)request->content_len;
    size_t total_received = 0;
    while (total_received < expected) {
        const size_t remaining = expected - total_received;
        const int received = httpd_req_recv(request, body + total_received, remaining);
        if (received <= 0 || (size_t)received > remaining) {
            httpd_resp_send_err(request, HTTPD_400_BAD_REQUEST, "incomplete configuration payload");
            return ESP_FAIL;
        }
        total_received += (size_t)received;
    }
    body[total_received] = '\0';

    player_config_t next;
    if (player_config_get_current(&next) != ESP_OK) {
        httpd_resp_send_err(request, HTTPD_500_INTERNAL_SERVER_ERROR, "configuration unavailable");
        return ESP_FAIL;
    }
    json_string_if_present(body, "wifiSsid", next.wifi_ssid, sizeof(next.wifi_ssid));
    json_string_if_present(body, "wifiPassword", next.wifi_password, sizeof(next.wifi_password));
    json_string_if_present(body, "deviceName", next.device_name, sizeof(next.device_name));
    int key_gpio = next.key_gpio;
    int ptt_gpio = next.ptt_gpio;
    int rgb_gpio = next.rgb_gpio;
    int hang = next.ptt_hang_ms;
    json_int_if_present(body, "keyGpio", &key_gpio);
    json_int_if_present(body, "pttGpio", &ptt_gpio);
    json_int_if_present(body, "rgbGpio", &rgb_gpio);
    json_int_if_present(body, "pttHangMs", &hang);
    next.key_gpio = (int8_t)key_gpio;
    next.ptt_gpio = (int8_t)ptt_gpio;
    next.rgb_gpio = (int8_t)rgb_gpio;
    next.ptt_hang_ms = (uint16_t)(hang < 75 ? 75 : hang > 600 ? 600 : hang);
    json_bool_if_present(body, "keyActiveLow", &next.key_active_low);
    json_bool_if_present(body, "pttActiveLow", &next.ptt_active_low);
    json_bool_if_present(body, "rgbEnabled", &next.rgb_enabled);
    json_bool_if_present(body, "logsEnabled", &next.log_streaming_enabled);
    next.configured = next.wifi_ssid[0] != '\0';

    if (!player_gpio_is_safe_output(next.key_gpio) ||
        (next.ptt_gpio >= 0 && (!player_gpio_is_safe_output(next.ptt_gpio) || next.ptt_gpio == next.key_gpio)) ||
        (next.rgb_enabled && (next.rgb_gpio < 0 || !player_gpio_is_safe_output(next.rgb_gpio) ||
                              next.rgb_gpio == next.key_gpio || next.rgb_gpio == next.ptt_gpio))) {
        httpd_resp_send_err(request, HTTPD_400_BAD_REQUEST, "unsafe or duplicated key/PTT/RGB GPIO");
        return ESP_FAIL;
    }

    // The control command is acknowledged only after its transport-owned session has been torn
    // down and GPIO release has returned. Do not save, reconfigure pins, or return success first.
    esp_err_t err = player_transport_apply_config(&next);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "Could not confirm safe session release: %s", esp_err_to_name(err));
        httpd_resp_send_err(request, HTTPD_500_INTERNAL_SERVER_ERROR, "could not confirm safe output release");
        return err;
    }
    err = player_gpio_apply_config(&next);
    if (err == ESP_OK) {
        err = player_config_replace(&next);
    }
    if (err == ESP_OK) {
        player_log_set_streaming(next.log_streaming_enabled);
        err = player_wifi_apply_config(&next);
    }
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "Could not apply configuration: %s", esp_err_to_name(err));
        httpd_resp_send_err(request, HTTPD_500_INTERNAL_SERVER_ERROR, "configuration was not fully applied");
        return err;
    }
    player_log_write("info", "Configuration saved to NVS");
    player_web_publish_status();
    return status_handler(request);
}

static esp_err_t disconnect_handler(httpd_req_t *request)
{
    const esp_err_t err = player_transport_unpair();
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "Could not confirm local disconnect: %s", esp_err_to_name(err));
        httpd_resp_send_err(request, HTTPD_500_INTERNAL_SERVER_ERROR, "could not confirm safe output release");
        return err;
    }
    return status_handler(request);
}

static esp_err_t scan_handler(httpd_req_t *request)
{
    wifi_scan_config_t scan = { 0 };
    esp_err_t err = esp_wifi_scan_start(&scan, true);
    if (err != ESP_OK) {
        httpd_resp_send_err(request, HTTPD_500_INTERNAL_SERVER_ERROR, "Wi-Fi scan failed");
        return err;
    }
    uint16_t count = 16;
    wifi_ap_record_t records[16];
    err = esp_wifi_scan_get_ap_records(&count, records);
    if (err != ESP_OK) {
        httpd_resp_send_err(request, HTTPD_500_INTERNAL_SERVER_ERROR, "Wi-Fi scan results unavailable");
        return err;
    }

    char *json = malloc(SCAN_JSON_CAPACITY);
    if (!json) {
        httpd_resp_send_err(request, HTTPD_500_INTERNAL_SERVER_ERROR, "Wi-Fi scan serialization allocation failed");
        return ESP_ERR_NO_MEM;
    }
    player_json_writer_t output;
    player_json_writer_init(&output, json, SCAN_JSON_CAPACITY);
    bool complete = player_json_append_literal(&output, "[");
    for (uint16_t index = 0; complete && index < count; ++index) {
        if (index != 0) {
            complete = player_json_append_literal(&output, ",");
        }
        complete = complete && player_json_append_literal(&output, "{\"ssid\":") &&
                   player_json_append_string_n(&output, records[index].ssid,
                                               strnlen((const char *)records[index].ssid,
                                                       sizeof(records[index].ssid))) &&
                   player_json_append_literal(&output, ",\"rssi\":") &&
                   player_json_append_int(&output, records[index].rssi) &&
                   player_json_append_literal(&output, "}");
    }
    complete = complete && player_json_append_literal(&output, "]") && player_json_writer_ok(&output);
    if (!complete) {
        free(json);
        httpd_resp_send_err(request, HTTPD_500_INTERNAL_SERVER_ERROR, "Wi-Fi scan serialization failed");
        return ESP_FAIL;
    }
    err = send_json(request, json);
    free(json);
    return err;
}

static void remember_ws_client(int fd)
{
    for (size_t index = 0; index < sizeof(s_ws_clients) / sizeof(s_ws_clients[0]); ++index) {
        if (s_ws_clients[index] == fd) {
            return;
        }
    }
    for (size_t index = 0; index < sizeof(s_ws_clients) / sizeof(s_ws_clients[0]); ++index) {
        if (s_ws_clients[index] == 0 ||
            httpd_ws_get_fd_info(s_server, s_ws_clients[index]) != HTTPD_WS_CLIENT_WEBSOCKET) {
            s_ws_clients[index] = fd;
            return;
        }
    }
}

static esp_err_t websocket_handler(httpd_req_t *request)
{
    // ESP-IDF v6.x completes the WebSocket handshake without invoking this handler, so a
    // client is registered when its first data frame (the page's hello) arrives instead.
    httpd_ws_frame_t frame = { .type = HTTPD_WS_TYPE_TEXT };
    esp_err_t err = httpd_ws_recv_frame(request, &frame, 0);
    if (err != ESP_OK) {
        return err;
    }
    if (frame.len > 0) {
        // Consume the payload so the TCP stream stays aligned on the next frame header.
        uint8_t payload[64];
        if (frame.len > sizeof(payload)) {
            return ESP_FAIL;
        }
        frame.payload = payload;
        err = httpd_ws_recv_frame(request, &frame, frame.len);
        if (err != ESP_OK) {
            return err;
        }
    }
    remember_ws_client(httpd_req_to_sockfd(request));
    player_web_publish_status();
    return ESP_OK;
}

static void websocket_broadcast_work(void *arg)
{
    websocket_work_t *work = arg;
    if (!work) {
        return;
    }
    if (s_server) {
        httpd_ws_frame_t frame = {
            .final = true,
            .type = HTTPD_WS_TYPE_TEXT,
            .payload = (uint8_t *)work->payload,
            .len = work->length,
        };
        // This function runs in the HTTPD context. It is the sole writer of the client array,
        // so queued producers cannot race a handler adding or removing a descriptor.
        for (size_t index = 0; index < sizeof(s_ws_clients) / sizeof(s_ws_clients[0]); ++index) {
            const int fd = s_ws_clients[index];
            if (!fd) {
                continue;
            }
            if (httpd_ws_get_fd_info(s_server, fd) != HTTPD_WS_CLIENT_WEBSOCKET ||
                httpd_ws_send_frame_async(s_server, fd, &frame) != ESP_OK) {
                s_ws_clients[index] = 0;
            }
        }
    }
    free(work);
}

static void queue_websocket_message(const char *message)
{
    if (!s_server || !message) {
        return;
    }
    const size_t length = strlen(message);
    websocket_work_t *work = malloc(sizeof(*work) + length + 1);
    if (!work) {
        ESP_LOGW(TAG, "Dropping WebSocket event: allocation failed");
        return;
    }
    work->length = length;
    memcpy(work->payload, message, length + 1);
    const esp_err_t err = httpd_queue_work(s_server, websocket_broadcast_work, work);
    if (err != ESP_OK) {
        ESP_LOGW(TAG, "Dropping WebSocket event: %s", esp_err_to_name(err));
        free(work);
    }
}

static void status_publish_task(void *arg)
{
    (void)arg;
    for (;;) {
        (void)ulTaskNotifyTake(pdTRUE, portMAX_DELAY);
        player_web_publish_status();
    }
}

void player_web_request_status_update(void)
{
    if (s_status_task) {
        xTaskNotifyGive(s_status_task);
    }
}

void player_web_publish_status(void)
{
    char status[STATUS_JSON_CAPACITY];
    if (!build_status_json(status, sizeof(status))) {
        ESP_LOGE(TAG, "Could not serialize status WebSocket event");
        return;
    }
    char event[WS_STATUS_EVENT_CAPACITY];
    player_json_writer_t json;
    player_json_writer_init(&json, event, sizeof(event));
    if (!player_json_append_literal(&json, "{\"type\":\"status\",\"status\":") ||
        !player_json_append_literal(&json, status) || !player_json_append_literal(&json, "}") ||
        !player_json_writer_ok(&json)) {
        ESP_LOGE(TAG, "Status WebSocket event exceeded bounded capacity");
        return;
    }
    queue_websocket_message(event);
}

void player_web_publish_log(const char *level, const char *message)
{
    if (!level || !message) {
        return;
    }
    char event[WS_LOG_EVENT_CAPACITY];
    player_json_writer_t json;
    player_json_writer_init(&json, event, sizeof(event));
    if (!player_json_append_literal(&json, "{\"type\":\"log\",\"level\":") ||
        !append_string_field(&json, level, PLAYER_LOG_LEVEL_MAX) ||
        !player_json_append_literal(&json, ",\"message\":") ||
        !append_string_field(&json, message, PLAYER_LOG_MESSAGE_MAX) ||
        !player_json_append_literal(&json, "}") || !player_json_writer_ok(&json)) {
        ESP_LOGE(TAG, "Log WebSocket event exceeded bounded capacity");
        return;
    }
    queue_websocket_message(event);
}

esp_err_t player_web_start(void)
{
    httpd_config_t config = HTTPD_DEFAULT_CONFIG();
    config.max_uri_handlers = 7;
    config.stack_size = 8192;
    ESP_RETURN_ON_ERROR(httpd_start(&s_server, &config), TAG, "start http server");
    const httpd_uri_t endpoints[] = {
        { .uri = "/", .method = HTTP_GET, .handler = root_handler },
        { .uri = "/api/status", .method = HTTP_GET, .handler = status_handler },
        { .uri = "/api/logs", .method = HTTP_GET, .handler = logs_handler },
        { .uri = "/api/wifi-scan", .method = HTTP_GET, .handler = scan_handler },
        { .uri = "/api/config", .method = HTTP_POST, .handler = config_handler },
        { .uri = "/api/player/disconnect", .method = HTTP_POST, .handler = disconnect_handler },
        { .uri = "/ws", .method = HTTP_GET, .handler = websocket_handler, .is_websocket = true },
    };
    for (size_t index = 0; index < sizeof(endpoints) / sizeof(endpoints[0]); ++index) {
        ESP_RETURN_ON_ERROR(httpd_register_uri_handler(s_server, &endpoints[index]), TAG, "register endpoint");
    }
    if (xTaskCreate(status_publish_task, "w1ve_web_status", 6144, NULL, 4, &s_status_task) != pdPASS) {
        return ESP_ERR_NO_MEM;
    }
    player_log_write("info", "W1VE ESP32 Player web server started");
    return ESP_OK;
}
