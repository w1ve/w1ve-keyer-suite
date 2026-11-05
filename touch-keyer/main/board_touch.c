// Minimal AXS5106L touch driver (I2C 0x63) and LVGL pointer device.
//
// Register layout (AXS15231B-compatible):
//   0x01  6-byte frame: gesture, point count, XH, XL, YH, YL
//   0x05  firmware version (2 bytes)
//   0x08  chip id (3 bytes)
// INT is active low; we poll it from the LVGL read callback rather than using an ISR,
// because the keyer owns the interrupt budget on this board.
#include "board.h"

#include "driver/gpio.h"
#include "driver/i2c_master.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "lvgl.h"

static const char *TAG = "touch";

#define REG_DATA 0x01
#define REG_FW_VER 0x05
#define REG_CHIP_ID 0x08

static i2c_master_bus_handle_t s_bus;
static i2c_master_dev_handle_t s_dev;
// A resting finger produces the occasional empty read. Treating one of those as a release is
// what made keying break up, so a release needs several consecutive empty reads.
#define RELEASE_READS 2

static bool s_held;
static uint8_t s_release_reads;
static uint16_t s_last_x, s_last_y;
static uint32_t s_calib_logs;

static bool reg_read(uint8_t reg, uint8_t *data, size_t len)
{
    if (s_dev == NULL) {
        return false;
    }
    if (i2c_master_transmit(s_dev, &reg, 1, 100) != ESP_OK) {
        return false;
    }
    return i2c_master_receive(s_dev, data, len, 100) == ESP_OK;
}

static void touch_reset(void)
{
    gpio_config_t cfg = {
        .pin_bit_mask = 1ULL << TOUCH_PIN_RST,
        .mode = GPIO_MODE_OUTPUT,
    };
    gpio_config(&cfg);

    gpio_set_level(TOUCH_PIN_RST, 1);
    vTaskDelay(pdMS_TO_TICKS(10));
    gpio_set_level(TOUCH_PIN_RST, 0);
    vTaskDelay(pdMS_TO_TICKS(20));
    gpio_set_level(TOUCH_PIN_RST, 1);
    vTaskDelay(pdMS_TO_TICKS(60));
}

static bool read_point(uint16_t *x, uint16_t *y)
{
    uint8_t buf[6] = {0};
    if (!reg_read(REG_DATA, buf, sizeof(buf))) {
        return false;
    }

    const uint8_t points = buf[1];
    if (points == 0 || points > 1) {
        return false;  // single-touch only; anything else here is noise
    }

    uint16_t raw_x = ((uint16_t)(buf[2] & 0x0F) << 8) | buf[3];
    uint16_t raw_y = ((uint16_t)(buf[4] & 0x0F) << 8) | buf[5];

    if ((raw_x == 0xFFF && raw_y == 0xFFF) || raw_x > 1000 || raw_y > 1000) {
        return false;
    }

    // The controller reports in the panel's native portrait space. Pick which raw axis
    // feeds which screen axis, then scale each one onto the axis it drives, then mirror.
    const uint32_t src_w = TOUCH_SWAP_XY ? TOUCH_RAW_H : TOUCH_RAW_W;
    const uint32_t src_h = TOUCH_SWAP_XY ? TOUCH_RAW_W : TOUCH_RAW_H;
    uint32_t sx = TOUCH_SWAP_XY ? raw_y : raw_x;
    uint32_t sy = TOUCH_SWAP_XY ? raw_x : raw_y;

    sx = (sx * (LCD_H_RES - 1)) / (src_w - 1);
    sy = (sy * (LCD_V_RES - 1)) / (src_h - 1);

#if TOUCH_MIRROR_X
    sx = (LCD_H_RES - 1) - sx;
#endif
#if TOUCH_MIRROR_Y
    sy = (LCD_V_RES - 1) - sy;
#endif

    if (sx >= LCD_H_RES) {
        sx = LCD_H_RES - 1;
    }
    if (sy >= LCD_V_RES) {
        sy = LCD_V_RES - 1;
    }

    if (s_calib_logs < 8) {
        s_calib_logs++;
        ESP_LOGI(TAG, "raw (%u,%u) -> (%lu,%lu)", raw_x, raw_y, (unsigned long)sx, (unsigned long)sy);
    }

    *x = (uint16_t)sx;
    *y = (uint16_t)sy;
    return true;
}

static void indev_read_cb(lv_indev_drv_t *drv, lv_indev_data_t *data)
{
    (void)drv;

    // Fast path: with nothing held, INT high means there is nothing to read.
    if (!s_held && gpio_get_level(TOUCH_PIN_INT) != 0) {
        data->state = LV_INDEV_STATE_RELEASED;
        return;
    }

    uint16_t x, y;
    if (read_point(&x, &y)) {
        if (!s_held) {
            ESP_LOGI(TAG, "press (%u,%u)", x, y);
        }
        s_held = true;
        s_release_reads = 0;
        s_last_x = x;
        s_last_y = y;
        data->point.x = x;
        data->point.y = y;
        data->state = LV_INDEV_STATE_PRESSED;
        return;
    }

    // Nothing this time. Keep reporting the press until enough empty reads have piled up,
    // so a resting finger or a momentary I2C miss never looks like a release to LVGL.
    if (s_held && ++s_release_reads < RELEASE_READS) {
        data->point.x = s_last_x;
        data->point.y = s_last_y;
        data->state = LV_INDEV_STATE_PRESSED;
        return;
    }

    if (s_held) {
        ESP_LOGI(TAG, "release");
    }
    s_held = false;
    s_release_reads = 0;
    data->state = LV_INDEV_STATE_RELEASED;
}

void board_touch_init(void)
{
    i2c_master_bus_config_t bus_cfg = {
        .i2c_port = I2C_NUM_0,
        .sda_io_num = TOUCH_PIN_SDA,
        .scl_io_num = TOUCH_PIN_SCL,
        .clk_source = I2C_CLK_SRC_DEFAULT,
        .glitch_ignore_cnt = 7,
        .flags.enable_internal_pullup = true,
    };
    ESP_ERROR_CHECK(i2c_new_master_bus(&bus_cfg, &s_bus));

    i2c_device_config_t dev_cfg = {
        .dev_addr_length = I2C_ADDR_BIT_LEN_7,
        .device_address = TOUCH_I2C_ADDR,
        .scl_speed_hz = TOUCH_I2C_HZ,
    };
    ESP_ERROR_CHECK(i2c_master_bus_add_device(s_bus, &dev_cfg, &s_dev));

    gpio_config_t int_cfg = {
        .pin_bit_mask = 1ULL << TOUCH_PIN_INT,
        .mode = GPIO_MODE_INPUT,
        .pull_up_en = GPIO_PULLUP_ENABLE,
    };
    ESP_ERROR_CHECK(gpio_config(&int_cfg));

    touch_reset();

    uint8_t chip_id[3] = {0};
    if (reg_read(REG_CHIP_ID, chip_id, sizeof(chip_id))) {
        ESP_LOGI(TAG, "chip id %02X %02X %02X", chip_id[0], chip_id[1], chip_id[2]);
    } else {
        ESP_LOGW(TAG, "chip id read failed — touch may be unresponsive");
    }
    uint8_t fw[2] = {0};
    if (reg_read(REG_FW_VER, fw, sizeof(fw))) {
        ESP_LOGI(TAG, "firmware %u", ((unsigned)fw[0] << 8) | fw[1]);
    }

    static lv_indev_drv_t indev_drv;
    lv_indev_drv_init(&indev_drv);
    indev_drv.type = LV_INDEV_TYPE_POINTER;
    indev_drv.read_cb = indev_read_cb;
    lv_indev_drv_register(&indev_drv);

    ESP_LOGI(TAG, "touch ready (raw space %dx%d)", TOUCH_RAW_W, TOUCH_RAW_H);
}
