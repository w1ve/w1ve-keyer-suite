// Board pin map and geometry for the Waveshare ESP32-C6-Touch-LCD-1.47.
//
// Source of the onboard pin assignments: Waveshare's own pin table for this SKU.
// Note that the vendor's Arduino sample still shows ESP32-S3 pin numbers (45/47/38/39),
// which do not exist on the C6 — do not copy those.
#pragma once

#include <stdint.h>

#include "driver/gpio.h"

// Board bring-up (board_display.c / board_touch.c / board_led.c).
void board_display_init(void);
void board_display_set_brightness(uint8_t percent);
void board_display_set_madctl(uint8_t value);
void board_touch_init(void);

// ---------------------------------------------------------------- status LED
// One addressable RGB LED (WS2812 / NeoPixel), GRB order. Waveshare's table for this family
// gives it as "RGB LED -> RGB_Control -> GPIO8" (docs.waveshare.com/ESP32-C6-LCD-1.47); the
// touch variant's pin table omits the LED and calls GPIO8 BOOT, but the part on this hardware is
// the addressable LED on that pin. GPIO8 is a boot-strap pin, read by the ROM at reset before
// this driver runs, so the board's own pull-up still decides the strap.
// Colours are the wanted brightness per channel, 0..255; the part is bright, so keep them low.
#define BOARD_LED_GPIO GPIO_NUM_8

void board_led_init(void);
void board_led_set(uint8_t r, uint8_t g, uint8_t b);

// ---------------------------------------------------------------- display
// Landscape: the panel is a 172x320 part used on its side, 320 wide by 172 high.
#define LCD_H_RES 320
#define LCD_V_RES 172

// The JD9853 has 240 px of line RAM and the visible 172 px window is centred in it.
// With MADCTL's MV bit set the 172 px axis becomes the row axis, so the 34 px of slack
// moves from X to Y.
#define LCD_X_OFFSET 0
#define LCD_Y_OFFSET 34

// Partial LVGL buffer height. 320x32 @16bpp = 20 KB per buffer, two buffers.
#define LCD_BUF_LINES 32

#define LCD_PIN_SCK GPIO_NUM_1
#define LCD_PIN_MOSI GPIO_NUM_2
#define LCD_PIN_CS GPIO_NUM_14
#define LCD_PIN_DC GPIO_NUM_15
#define LCD_PIN_RST GPIO_NUM_22
#define LCD_PIN_BL GPIO_NUM_23

// Confirmed on this panel by the boot orientation cycle, whose RED page (0x28) came up
// upright with the black calibration bars on the top and left edges:
//   bit 2 (0x20) MV  = landscape, the 172 px axis becomes the row axis
//   bit 3 (0x08) BGR = red/blue order; without it a blue slider came up orange
// and no MX/MY mirroring at all: 0x68 (MX) and 0xA8 (MY) both landed upside down or
// mirrored. Inversion stays off, the panel needs none.
#define LCD_INVERT_COLOR 0
#define LCD_MADCTL 0x28

// At boot, cycle the four landscape MADCTL values, three seconds each, with the value and
// a TOP marker on screen. Whichever page reads correctly is the one for this panel; set it
// as LCD_MADCTL and turn this off.
#define UI_ORIENT_SELFTEST 0

// Dry-run element timing measurement at boot (pins untouched, no transmission).
#define KEYER_BOOT_SELFTEST 1

// ---------------------------------------------------------------- touch
#define TOUCH_PIN_SDA GPIO_NUM_18
#define TOUCH_PIN_SCL GPIO_NUM_19
#define TOUCH_PIN_RST GPIO_NUM_20
#define TOUCH_PIN_INT GPIO_NUM_21
#define TOUCH_I2C_ADDR 0x63
#define TOUCH_I2C_HZ 400000

// Raw coordinate space reported by the AXS5106L. It reports in the panel's native portrait
// space, so a landscape UI always swaps the axes. Calibrate on hardware: the firmware logs
// raw and mapped values for the first eight touches after boot.
#define TOUCH_RAW_W 172
#define TOUCH_RAW_H 320
// With MADCTL 0x28 (MV, no MX/MY) the panel's column is the display row and vice versa,
// with no mirroring: display X = raw Y, display Y = raw X.
#define TOUCH_SWAP_XY 1
#define TOUCH_MIRROR_X 0
#define TOUCH_MIRROR_Y 0

// ---------------------------------------------------------------- keyer
// Station-side keyer: paddles in, rig key line out.
//
// Pin choice is deliberate: GPIO5/6 are the QMI8658A interrupt outputs, which the IMU can
// hold low at reset — that would read as a permanently held paddle. They are avoided
// entirely. GPIO3/4 are the TF card MISO/CS; the card slot is unused on this product, so
// those two pins are the cheapest clean signals on the board.
#define KEYER_PIN_DIT GPIO_NUM_7  // free pad, connected to nothing else
#define KEYER_PIN_DAH GPIO_NUM_4  // TF CS (no card)
#define KEYER_PIN_KEY GPIO_NUM_3  // TF MISO (no card)
#define KEYER_PIN_PTT GPIO_NUM_0  // BAT_ADC tap; divider is harmless for an output

// 1 = the GPIO goes high to key/transmit. Set to 0 for an inverting interface.
#define KEYER_KEY_ACTIVE_HIGH 1
#define KEYER_PTT_ACTIVE_HIGH 1

// PTT hangs for this long after the last element, so the rig does not drop between letters.
#define KEYER_PTT_TAIL_MS 500

// Contact debounce. Shorter than any real element (a 40 WPM dit is 30 ms).
#define KEYER_DEBOUNCE_US 4000
