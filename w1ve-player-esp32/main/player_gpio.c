#include "player_gpio.h"

#include <string.h>

#include "driver/gpio.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "led_strip.h"

static const char *TAG = "w1ve_gpio";
static player_config_t s_config;
static bool s_key_down;
static bool s_ptt_on;
static bool s_network_connected;
static bool s_paired;
static uint32_t s_active_session_generation;
static uint32_t s_tail_generation;
static uint32_t s_armed_tail_generation;
static led_strip_handle_t s_strip;
static esp_timer_handle_t s_ptt_timer;
static SemaphoreHandle_t s_lock;

static esp_err_t ensure_lock(void)
{
    if (!s_lock) {
        s_lock = xSemaphoreCreateMutex();
    }
    return s_lock ? ESP_OK : ESP_ERR_NO_MEM;
}

static bool is_configured_output(int gpio)
{
    return gpio >= 0 && player_gpio_is_safe_output(gpio);
}

bool player_gpio_is_safe_output(int gpio)
{
    // Generic ESP32-S3-safe candidates. Excluded: GPIO0 boot strap; GPIO3 strap-risk;
    // GPIO19/20 USB; GPIO26-37 commonly flash/PSRAM; GPIO39-42 JTAG-risk; GPIO43/44 UART0;
    // GPIO45/46 strap/input-risk. Board profiles can narrow this list further.
    return (gpio >= 1 && gpio <= 2) ||
           (gpio >= 4 && gpio <= 18) ||
           gpio == 21 || gpio == 38 || gpio == 47 || gpio == 48;
}

static esp_err_t write_output_locked(int gpio, bool active, bool active_low)
{
    if (!is_configured_output(gpio)) {
        return ESP_OK;
    }
    return gpio_set_level(gpio, (active ^ active_low) ? 1 : 0);
}

static void update_led_locked(void)
{
    if (!s_strip || !s_config.rgb_enabled) {
        return;
    }
    uint8_t red = 0;
    uint8_t green = 0;
    uint8_t blue = 0;
    if (s_key_down) {
        // RGB devices can combine channels to display white while keyed.
        red = 32;
        green = 32;
        blue = 32;
    } else if (s_network_connected && s_paired) {
        green = 32;
    } else if (!s_network_connected) {
        red = 32;
    } else {
        blue = 12; // online but waiting to pair
    }
    const esp_err_t set_err = led_strip_set_pixel(s_strip, 0, red, green, blue);
    const esp_err_t refresh_err = led_strip_refresh(s_strip);
    if (set_err != ESP_OK || refresh_err != ESP_OK) {
        ESP_LOGW(TAG, "RGB LED update failed: %s / %s", esp_err_to_name(set_err),
                 esp_err_to_name(refresh_err));
    }
}

static void advance_tail_generation_locked(void)
{
    if (++s_tail_generation == 0) {
        ++s_tail_generation;
    }
    s_armed_tail_generation = 0;
}

static esp_err_t stop_ptt_timer_locked(void)
{
    if (!s_ptt_timer) {
        return ESP_OK;
    }
    const esp_err_t err = esp_timer_stop(s_ptt_timer);
    return err == ESP_ERR_INVALID_STATE ? ESP_OK : err;
}

static esp_err_t force_release_locked(void)
{
    advance_tail_generation_locked();
    const esp_err_t timer_err = stop_ptt_timer_locked();
    s_key_down = false;
    s_ptt_on = false;
    const esp_err_t key_err = write_output_locked(s_config.key_gpio, false, s_config.key_active_low);
    const esp_err_t ptt_err = write_output_locked(s_config.ptt_gpio, false, s_config.ptt_active_low);
    update_led_locked();
    if (timer_err != ESP_OK) {
        return timer_err;
    }
    if (key_err != ESP_OK) {
        return key_err;
    }
    return ptt_err;
}

static esp_err_t configure_output_locked(int gpio, bool active_low)
{
    if (!is_configured_output(gpio)) {
        return ESP_OK;
    }
    const gpio_config_t io = {
        .pin_bit_mask = 1ULL << gpio,
        .mode = GPIO_MODE_OUTPUT,
        .pull_up_en = GPIO_PULLUP_DISABLE,
        .pull_down_en = GPIO_PULLDOWN_DISABLE,
        .intr_type = GPIO_INTR_DISABLE,
    };
    esp_err_t err = gpio_config(&io);
    if (err == ESP_OK) {
        err = write_output_locked(gpio, false, active_low);
    }
    return err;
}

static void ptt_timer_callback(void *arg)
{
    (void)arg;
    // This timer uses ESP_TIMER_TASK dispatch, so it can serialize with edge application.  A
    // callback that began before esp_timer_stop() cannot drop PTT after a new down edge because
    // that edge advances s_tail_generation while holding the same mutex.
    if (!s_lock) {
        return;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    if (s_armed_tail_generation != 0 && s_armed_tail_generation == s_tail_generation &&
        !s_key_down && s_ptt_on && s_network_connected && s_paired &&
        s_active_session_generation != 0) {
        s_ptt_on = false;
        s_armed_tail_generation = 0;
        const esp_err_t err = write_output_locked(s_config.ptt_gpio, false, s_config.ptt_active_low);
        if (err != ESP_OK) {
            ESP_LOGE(TAG, "PTT tail release failed: %s", esp_err_to_name(err));
        }
        update_led_locked();
    }
    xSemaphoreGive(s_lock);
}

esp_err_t player_gpio_apply_config(const player_config_t *config)
{
    if (!config || !player_gpio_is_safe_output(config->key_gpio) ||
        (config->ptt_gpio >= 0 && !player_gpio_is_safe_output(config->ptt_gpio)) ||
        (config->ptt_gpio >= 0 && config->key_gpio == config->ptt_gpio) ||
        (config->rgb_enabled &&
         (config->rgb_gpio < 0 || !player_gpio_is_safe_output(config->rgb_gpio) ||
          config->rgb_gpio == config->key_gpio || config->rgb_gpio == config->ptt_gpio))) {
        return ESP_ERR_INVALID_ARG;
    }
    esp_err_t err = ensure_lock();
    if (err != ESP_OK) {
        return err;
    }

    xSemaphoreTake(s_lock, portMAX_DELAY);
    s_paired = false;
    s_active_session_generation = 0;
    err = force_release_locked();
    if (err == ESP_OK) {
        memcpy(&s_config, config, sizeof(s_config));
        err = configure_output_locked(s_config.key_gpio, s_config.key_active_low);
    }
    if (err == ESP_OK) {
        err = configure_output_locked(s_config.ptt_gpio, s_config.ptt_active_low);
    }
    if (err == ESP_OK && !s_ptt_timer) {
        const esp_timer_create_args_t timer_args = {
            .callback = ptt_timer_callback,
            .dispatch_method = ESP_TIMER_TASK,
            .name = "w1ve_ptt_tail",
        };
        err = esp_timer_create(&timer_args, &s_ptt_timer);
    }
    if (s_strip) {
        led_strip_del(s_strip);
        s_strip = NULL;
    }
    if (err == ESP_OK && s_config.rgb_enabled && player_gpio_is_safe_output(s_config.rgb_gpio)) {
        const led_strip_config_t strip_cfg = {
            .strip_gpio_num = s_config.rgb_gpio,
            .max_leds = 1,
            .led_model = LED_MODEL_WS2812,
            .color_component_format = LED_STRIP_COLOR_COMPONENT_FMT_GRB,
            .flags = { .invert_out = false },
        };
        const led_strip_rmt_config_t rmt_cfg = {
            .clk_src = RMT_CLK_SRC_DEFAULT,
            .resolution_hz = 10 * 1000 * 1000,
            .mem_block_symbols = 64,
            .flags = { .with_dma = false },
        };
        const esp_err_t strip_err = led_strip_new_rmt_device(&strip_cfg, &rmt_cfg, &s_strip);
        if (strip_err != ESP_OK) {
            ESP_LOGW(TAG, "RGB LED unavailable on GPIO %d: %s", s_config.rgb_gpio,
                     esp_err_to_name(strip_err));
            s_strip = NULL;
        }
    }
    update_led_locked();
    xSemaphoreGive(s_lock);
    return err;
}

esp_err_t player_gpio_force_release(void)
{
    const esp_err_t lock_err = ensure_lock();
    if (lock_err != ESP_OK) {
        return lock_err;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    const esp_err_t err = force_release_locked();
    xSemaphoreGive(s_lock);
    return err;
}

esp_err_t player_gpio_activate_session(uint32_t session_generation)
{
    if (session_generation == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    const esp_err_t lock_err = ensure_lock();
    if (lock_err != ESP_OK) {
        return lock_err;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    esp_err_t err = ESP_ERR_INVALID_STATE;
    if (s_network_connected) {
        err = force_release_locked();
        if (err == ESP_OK) {
            s_paired = true;
            s_active_session_generation = session_generation;
            update_led_locked();
        }
    }
    xSemaphoreGive(s_lock);
    return err;
}

esp_err_t player_gpio_deactivate_session(void)
{
    const esp_err_t lock_err = ensure_lock();
    if (lock_err != ESP_OK) {
        return lock_err;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    s_paired = false;
    s_active_session_generation = 0;
    const esp_err_t err = force_release_locked();
    xSemaphoreGive(s_lock);
    return err;
}

esp_err_t player_gpio_set_network_connected(bool connected)
{
    const esp_err_t lock_err = ensure_lock();
    if (lock_err != ESP_OK) {
        return lock_err;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    s_network_connected = connected;
    esp_err_t err = ESP_OK;
    if (!connected) {
        s_paired = false;
        s_active_session_generation = 0;
        err = force_release_locked();
    } else {
        update_led_locked();
    }
    xSemaphoreGive(s_lock);
    return err;
}

esp_err_t player_gpio_apply_key(uint32_t session_generation, bool key_down)
{
    if (session_generation == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    const esp_err_t lock_err = ensure_lock();
    if (lock_err != ESP_OK) {
        return lock_err;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    if (!s_network_connected || !s_paired || s_active_session_generation != session_generation) {
        xSemaphoreGive(s_lock);
        return ESP_ERR_INVALID_STATE;
    }

    esp_err_t err = ESP_OK;
    if (key_down) {
        advance_tail_generation_locked();
        const esp_err_t timer_err = stop_ptt_timer_locked();
        s_ptt_on = is_configured_output(s_config.ptt_gpio);
        const esp_err_t ptt_err = write_output_locked(s_config.ptt_gpio, s_ptt_on, s_config.ptt_active_low);
        s_key_down = true;
        const esp_err_t key_err = write_output_locked(s_config.key_gpio, true, s_config.key_active_low);
        if (timer_err != ESP_OK) {
            err = timer_err;
        } else if (ptt_err != ESP_OK) {
            err = ptt_err;
        } else {
            err = key_err;
        }
    } else {
        s_key_down = false;
        err = write_output_locked(s_config.key_gpio, false, s_config.key_active_low);
        if (err == ESP_OK && s_ptt_on && s_ptt_timer) {
            s_armed_tail_generation = s_tail_generation;
            err = esp_timer_start_once(s_ptt_timer, (uint64_t)s_config.ptt_hang_ms * 1000ULL);
            if (err != ESP_OK) {
                s_armed_tail_generation = 0;
                s_ptt_on = false;
                const esp_err_t ptt_err = write_output_locked(s_config.ptt_gpio, false, s_config.ptt_active_low);
                if (ptt_err != ESP_OK) {
                    err = ptt_err;
                }
            }
        }
    }
    if (err != ESP_OK) {
        (void)force_release_locked();
    }
    update_led_locked();
    xSemaphoreGive(s_lock);
    return err;
}

void player_gpio_get_status(player_gpio_status_t *status)
{
    if (!status) {
        return;
    }
    memset(status, 0, sizeof(*status));
    if (!s_lock) {
        return;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    status->key_down = s_key_down;
    status->ptt_on = s_ptt_on;
    status->network_connected = s_network_connected;
    status->paired = s_paired;
    status->active_session_generation = s_active_session_generation;
    xSemaphoreGive(s_lock);
}

bool player_gpio_key_down(void)
{
    player_gpio_status_t status;
    player_gpio_get_status(&status);
    return status.key_down;
}

bool player_gpio_ptt_on(void)
{
    player_gpio_status_t status;
    player_gpio_get_status(&status);
    return status.ptt_on;
}
