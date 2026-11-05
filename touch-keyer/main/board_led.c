// Onboard WS2812 status LED (NeoPixel) on GPIO8.
//
// One wire, RMT-timed: three GRB bytes at ~1.25 us per bit, which is the WS2812 family's
// protocol. Only the colour changes, so a transmit happens when the state changes, not per
// frame. The pin keeps its idle level between transmissions, which the part latches on; the
// boot strap on GPIO8 is read by the ROM at reset, before this driver runs, so the board's own
// pull-up still decides the strap at boot.
//
// Pin per Waveshare's table for this family: RGB LED / RGB_Control = GPIO8.
#include "board.h"

#include "driver/rmt_tx.h"
#include "esp_log.h"

static const char *TAG = "led";

// 10 MHz: 0.1 us per tick. WS2812B wants 0.4 us high for a one and 0.8 us for a zero with a
// 1.25 us period; 0.3/0.9 and 0.9/0.3 sit inside the tolerance of every part in the family.
#define LED_RESOLUTION_HZ 10000000

static rmt_channel_handle_t s_chan;
static rmt_encoder_handle_t s_encoder;
static uint8_t s_last_r, s_last_g, s_last_b;
static bool s_have_last;

void board_led_init(void)
{
    const rmt_tx_channel_config_t tx = {
        .gpio_num = BOARD_LED_GPIO,
        .clk_src = RMT_CLK_SRC_DEFAULT,
        .resolution_hz = LED_RESOLUTION_HZ,
        .mem_block_symbols = 48,
        .trans_queue_depth = 2,
    };
    if (rmt_new_tx_channel(&tx, &s_chan) != ESP_OK) {
        ESP_LOGE(TAG, "no RMT channel for the status LED, colour indication unavailable");
        s_chan = NULL;
        return;
    }

    const rmt_bytes_encoder_config_t bytes = {
        .bit0 = {.level0 = 1, .duration0 = 3, .level1 = 0, .duration1 = 9},
        .bit1 = {.level0 = 1, .duration0 = 9, .level1 = 0, .duration1 = 3},
        .flags.msb_first = 1,
    };
    if (rmt_new_bytes_encoder(&bytes, &s_encoder) != ESP_OK) {
        ESP_LOGE(TAG, "no bytes encoder for the status LED");
        rmt_del_channel(s_chan);
        s_chan = NULL;
        return;
    }

    if (rmt_enable(s_chan) != ESP_OK) {
        ESP_LOGE(TAG, "cannot enable the status LED channel");
        s_chan = NULL;
        return;
    }
    ESP_LOGI(TAG, "status LED: WS2812 on GPIO%d", BOARD_LED_GPIO);
}

void board_led_set(uint8_t r, uint8_t g, uint8_t b)
{
    if (s_chan == NULL) {
        return;
    }
    if (s_have_last && r == s_last_r && g == s_last_g && b == s_last_b) {
        return;
    }
    s_last_r = r;
    s_last_g = g;
    s_last_b = b;
    s_have_last = true;

    const uint8_t grb[3] = {g, r, b};  // the part takes green first, then red, then blue
    const rmt_transmit_config_t cfg = {.loop_count = 0};
    if (rmt_transmit(s_chan, s_encoder, grb, sizeof(grb), &cfg) == ESP_OK) {
        rmt_tx_wait_all_done(s_chan, 20);
    }
}
