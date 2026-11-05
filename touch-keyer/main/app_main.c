// Boot: NVS, display + LVGL, touch, keyer, UI, network status.
//
// The keyer runs on its own task and keeps working whatever happens here; the display is
// a view, not part of the keying path.
#include "board.h"
#include "remote.h"
#include "wan.h"
#include "keyer.h"
#include "macro.h"
#include "net.h"
#include "ui.h"
#include "web.h"
#include "rwk/rwk_selftest.h"

#include "esp_log.h"
#include "esp_system.h"
#include "sdkconfig.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "lvgl.h"
#include "nvs_flash.h"

static const char *TAG = "app";

void app_main(void)
{
    esp_err_t err = nvs_flash_init();
    if (err == ESP_ERR_NVS_NO_FREE_PAGES || err == ESP_ERR_NVS_NEW_VERSION_FOUND) {
        ESP_ERROR_CHECK(nvs_flash_erase());
        err = nvs_flash_init();
    }
    ESP_ERROR_CHECK(err);

    ESP_LOGI(TAG, "W1VE Touch Keyer v%s starting on %s", W1VE_FW_VERSION, CONFIG_IDF_TARGET);

    board_display_init();
    board_touch_init();
    board_led_init();
    keyer_init();
    macro_init();
#if KEYER_BOOT_SELFTEST
    keyer_selftest(50, 40);
#endif
    ui_orient_selftest();
    ui_init();
    net_init();
    remote_init();
    wan_init();
    web_init();
    rwk_selftest_start();

    for (;;) {
        lv_timer_handler();
        vTaskDelay(pdMS_TO_TICKS(10));
    }
}
