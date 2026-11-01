#include "player_log.h"

#include <stdarg.h>
#include <stdio.h>
#include <string.h>

#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "player_web.h"

static player_log_entry_t s_lines[PLAYER_LOG_MAX_ENTRIES];
static size_t s_next;
static size_t s_count;
static bool s_streaming = true;
static SemaphoreHandle_t s_lock;

void player_log_init(void)
{
    s_lock = xSemaphoreCreateMutex();
}

void player_log_set_streaming(bool enabled)
{
    if (!s_lock) {
        s_streaming = enabled;
        return;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    s_streaming = enabled;
    xSemaphoreGive(s_lock);
}

bool player_log_streaming_enabled(void)
{
    if (!s_lock) {
        return s_streaming;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    const bool enabled = s_streaming;
    xSemaphoreGive(s_lock);
    return enabled;
}

void player_log_write(const char *level, const char *format, ...)
{
    char message[PLAYER_LOG_MESSAGE_MAX + 1];
    va_list args;
    va_start(args, format);
    vsnprintf(message, sizeof(message), format, args);
    va_end(args);

    ESP_LOGI("w1ve", "[%s] %s", level, message);
    bool streaming = false;
    if (s_lock) {
        xSemaphoreTake(s_lock, portMAX_DELAY);
        snprintf(s_lines[s_next].level, sizeof(s_lines[s_next].level), "%s", level ? level : "info");
        snprintf(s_lines[s_next].message, sizeof(s_lines[s_next].message), "%s", message);
        s_next = (s_next + 1) % PLAYER_LOG_MAX_ENTRIES;
        if (s_count < PLAYER_LOG_MAX_ENTRIES) {
            ++s_count;
        }
        streaming = s_streaming;
        xSemaphoreGive(s_lock);
    }
    if (streaming) {
        player_web_publish_log(level ? level : "info", message);
    }
}

size_t player_log_snapshot(player_log_entry_t *entries, size_t capacity)
{
    if (!entries || capacity == 0 || !s_lock) {
        return 0;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    const size_t count = s_count < capacity ? s_count : capacity;
    const size_t first = (s_next + PLAYER_LOG_MAX_ENTRIES - s_count) % PLAYER_LOG_MAX_ENTRIES;
    for (size_t index = 0; index < count; ++index) {
        entries[index] = s_lines[(first + index) % PLAYER_LOG_MAX_ENTRIES];
    }
    xSemaphoreGive(s_lock);
    return count;
}
