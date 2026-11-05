// Display bring-up: SPI bus, JD9853 panel init (ST7789-compatible command set) and the
// LVGL display driver. Deliberately self-contained — no vendor panel component — so the
// init sequence is ours to tune against the real glass.
#include <string.h>

#include "board.h"

#include "driver/gpio.h"
#include "driver/ledc.h"
#include "driver/spi_master.h"
#include "esp_lcd_panel_io.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "lvgl.h"

static const char *TAG = "display";

static esp_lcd_panel_io_handle_t s_io;
static SemaphoreHandle_t s_flush_done;
static lv_disp_draw_buf_t s_draw_buf;
static lv_color_t s_buf1[LCD_H_RES * LCD_BUF_LINES];
static lv_color_t s_buf2[LCD_H_RES * LCD_BUF_LINES];

static bool IRAM_ATTR on_color_done(esp_lcd_panel_io_handle_t io, esp_lcd_panel_io_event_data_t *edata, void *ctx)
{
    (void)io;
    (void)edata;
    (void)ctx;
    BaseType_t woken = pdFALSE;
    xSemaphoreGiveFromISR(s_flush_done, &woken);
    return woken == pdTRUE;
}

static void cmd(uint8_t c, const uint8_t *params, size_t len)
{
    ESP_ERROR_CHECK(esp_lcd_panel_io_tx_param(s_io, c, params, len));
}

static void cmd0(uint8_t c)
{
    cmd(c, NULL, 0);
}

static void backlight_init(void)
{
    ledc_timer_config_t timer = {
        .speed_mode = LEDC_LOW_SPEED_MODE,
        .duty_resolution = LEDC_TIMER_10_BIT,
        .timer_num = LEDC_TIMER_0,
        .freq_hz = 5000,
        .clk_cfg = LEDC_AUTO_CLK,
    };
    ESP_ERROR_CHECK(ledc_timer_config(&timer));

    ledc_channel_config_t ch = {
        .gpio_num = LCD_PIN_BL,
        .speed_mode = LEDC_LOW_SPEED_MODE,
        .channel = LEDC_CHANNEL_0,
        .timer_sel = LEDC_TIMER_0,
        .duty = 0,
        .hpoint = 0,
    };
    ESP_ERROR_CHECK(ledc_channel_config(&ch));
}

void board_display_set_madctl(uint8_t value)
{
    if (s_io != NULL) {
        esp_lcd_panel_io_tx_param(s_io, 0x36, &value, 1);
    }
}

void board_display_set_brightness(uint8_t percent)
{
    if (percent > 100) {
        percent = 100;
    }
    uint32_t duty = (1023u * percent) / 100u;
    ledc_set_duty(LEDC_LOW_SPEED_MODE, LEDC_CHANNEL_0, duty);
    ledc_update_duty(LEDC_LOW_SPEED_MODE, LEDC_CHANNEL_0);
}

// ST7789-family init. The JD9853 is close enough that the standard sequence drives it,
// which is also what Waveshare's Arduino sample relies on (Arduino_ST7789, 172x320).
static void panel_init(void)
{
    cmd0(0x01);                    // SWRESET
    vTaskDelay(pdMS_TO_TICKS(150));

    static const uint8_t colmod[] = {0x55};             // 16 bits per pixel
    cmd(0x3A, colmod, sizeof(colmod));

    static const uint8_t madctl[] = {LCD_MADCTL};
    cmd(0x36, madctl, sizeof(madctl));

    static const uint8_t porctrl[] = {0x0C, 0x0C, 0x00, 0x33, 0x33};
    cmd(0xB2, porctrl, sizeof(porctrl));

    static const uint8_t gctrl[] = {0x35};
    cmd(0xB7, gctrl, sizeof(gctrl));

    static const uint8_t vcoms[] = {0x19};
    cmd(0xBB, vcoms, sizeof(vcoms));

    static const uint8_t lcmctrl[] = {0x2C};
    cmd(0xC0, lcmctrl, sizeof(lcmctrl));

    static const uint8_t vdvvrhen[] = {0x01};
    cmd(0xC2, vdvvrhen, sizeof(vdvvrhen));

    static const uint8_t vrhs[] = {0x12};
    cmd(0xC3, vrhs, sizeof(vrhs));

    static const uint8_t vdvs[] = {0x20};
    cmd(0xC4, vdvs, sizeof(vdvs));

    static const uint8_t frctrl2[] = {0x0F};
    cmd(0xC6, frctrl2, sizeof(frctrl2));

    static const uint8_t pwctrl1[] = {0xA4, 0xA1};
    cmd(0xD0, pwctrl1, sizeof(pwctrl1));

    static const uint8_t pgamma[] = {0xD0, 0x04, 0x0D, 0x11, 0x13, 0x2B, 0x3F, 0x54,
                                     0x4C, 0x18, 0x0D, 0x0B, 0x1F, 0x23};
    cmd(0xE0, pgamma, sizeof(pgamma));

    static const uint8_t ngamma[] = {0xD0, 0x04, 0x0C, 0x11, 0x13, 0x2C, 0x3F, 0x44,
                                     0x51, 0x2F, 0x1F, 0x1F, 0x20, 0x23};
    cmd(0xE1, ngamma, sizeof(ngamma));

    if (LCD_INVERT_COLOR) {
        cmd0(0x21);                // IPS panel: inversion on
    }

    cmd0(0x11);                    // SLPOUT
    vTaskDelay(pdMS_TO_TICKS(120));
    cmd0(0x29);                    // DISPON
    vTaskDelay(pdMS_TO_TICKS(50));
}

// The SPI peripheral wants big-endian pixel bytes; LVGL keeps RGB565 little-endian.
static void swap_pixels(lv_color_t *px, uint32_t count)
{
    uint16_t *p = (uint16_t *)px;
    for (uint32_t i = 0; i < count; i++) {
        p[i] = (uint16_t)((p[i] >> 8) | (p[i] << 8));
    }
}

static void flush_cb(lv_disp_drv_t *drv, const lv_area_t *area, lv_color_t *px)
{
    const int x1 = area->x1 + LCD_X_OFFSET;
    const int x2 = area->x2 + LCD_X_OFFSET;
    const int y1 = area->y1 + LCD_Y_OFFSET;
    const int y2 = area->y2 + LCD_Y_OFFSET;

    const uint8_t caset[4] = {(uint8_t)(x1 >> 8), (uint8_t)(x1 & 0xFF), (uint8_t)(x2 >> 8), (uint8_t)(x2 & 0xFF)};
    const uint8_t raset[4] = {(uint8_t)(y1 >> 8), (uint8_t)(y1 & 0xFF), (uint8_t)(y2 >> 8), (uint8_t)(y2 & 0xFF)};

    esp_lcd_panel_io_tx_param(s_io, 0x2A, caset, sizeof(caset));
    esp_lcd_panel_io_tx_param(s_io, 0x2B, raset, sizeof(raset));

    const uint32_t pixels = (uint32_t)(area->x2 - area->x1 + 1) * (uint32_t)(area->y2 - area->y1 + 1);
    swap_pixels(px, pixels);
    esp_lcd_panel_io_tx_color(s_io, 0x2C, px, pixels * sizeof(uint16_t));

    xSemaphoreTake(s_flush_done, pdMS_TO_TICKS(100));
    lv_disp_flush_ready(drv);
}

static void tick_cb(void *arg)
{
    (void)arg;
    lv_tick_inc(2);
}

void board_display_init(void)
{
    s_flush_done = xSemaphoreCreateBinary();
    configASSERT(s_flush_done);

    backlight_init();

    spi_bus_config_t bus = {
        .sclk_io_num = LCD_PIN_SCK,
        .mosi_io_num = LCD_PIN_MOSI,
        .miso_io_num = -1,
        .quadwp_io_num = -1,
        .quadhd_io_num = -1,
        .max_transfer_sz = LCD_H_RES * LCD_BUF_LINES * sizeof(uint16_t) + 64,
    };
    ESP_ERROR_CHECK(spi_bus_initialize(SPI2_HOST, &bus, SPI_DMA_CH_AUTO));

    esp_lcd_panel_io_spi_config_t io_cfg = {
        .cs_gpio_num = LCD_PIN_CS,
        .dc_gpio_num = LCD_PIN_DC,
        .spi_mode = 0,
        .pclk_hz = 40 * 1000 * 1000,
        .trans_queue_depth = 4,
        .lcd_cmd_bits = 8,
        .lcd_param_bits = 8,
        .on_color_trans_done = on_color_done,
    };
    ESP_ERROR_CHECK(esp_lcd_new_panel_io_spi((esp_lcd_spi_bus_handle_t)SPI2_HOST, &io_cfg, &s_io));

    gpio_config_t rst = {
        .pin_bit_mask = 1ULL << LCD_PIN_RST,
        .mode = GPIO_MODE_OUTPUT,
    };
    ESP_ERROR_CHECK(gpio_config(&rst));
    gpio_set_level(LCD_PIN_RST, 0);
    vTaskDelay(pdMS_TO_TICKS(20));
    gpio_set_level(LCD_PIN_RST, 1);
    vTaskDelay(pdMS_TO_TICKS(50));

    panel_init();

    lv_init();
    lv_disp_draw_buf_init(&s_draw_buf, s_buf1, s_buf2, LCD_H_RES * LCD_BUF_LINES);

    static lv_disp_drv_t disp_drv;
    lv_disp_drv_init(&disp_drv);
    disp_drv.hor_res = LCD_H_RES;
    disp_drv.ver_res = LCD_V_RES;
    disp_drv.flush_cb = flush_cb;
    disp_drv.draw_buf = &s_draw_buf;
    disp_drv.user_data = NULL;
    lv_disp_t *disp = lv_disp_drv_register(&disp_drv);

    // LVGL's stock theme is light. This is an instrument panel, so build the dark variant
    // explicitly rather than relying on the sdkconfig default.
    lv_theme_t *theme = lv_theme_default_init(disp, lv_color_hex(0x2F6FE0), lv_color_hex(0xE03A3A),
                                              true, LV_FONT_DEFAULT);
    lv_disp_set_theme(disp, theme);

    const esp_timer_create_args_t tick_args = {
        .callback = tick_cb,
        .name = "lv_tick",
    };
    esp_timer_handle_t tick_timer;
    ESP_ERROR_CHECK(esp_timer_create(&tick_args, &tick_timer));
    ESP_ERROR_CHECK(esp_timer_start_periodic(tick_timer, 2000));

    board_display_set_brightness(80);
    ESP_LOGI(TAG, "panel up: %dx%d (offset %d,%d)", LCD_H_RES, LCD_V_RES, LCD_X_OFFSET, LCD_Y_OFFSET);
}
