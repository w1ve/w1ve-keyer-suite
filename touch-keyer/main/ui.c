// Panel: name plate on the main page, settings on a second page.
//
// Page 1 is what the operator needs at the rig: who this box is, the address a phone should use,
// whether a Keyer holds the LAN and at what speed, the live KEY/PTT state, five macro keys and a
// Set key. Page 2 carries the two things worth a finger at the rig — speed and the LAN switch —
// and says whether the box is advertising or a Keyer has it.
//
// The macro keys are separate LVGL objects with a 7 px gutter between them, so a press can only
// ever land on one key; a key that is already sending refuses a second press.
#include "ui.h"

#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "board.h"
#include "keyer.h"
#include "macro.h"
#include "net.h"
#include "remote.h"
#include "wan.h"

#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "lvgl.h"

static const char *TAG = "ui";

#define COL_BG 0x000000
#define COL_TEXT 0xE8E8EA
#define COL_DIM 0x8A8A92
#define COL_KEY 0xE03A3A
#define COL_PTT 0x2F6FE0
#define COL_MACRO 0x2F6FE0
#define COL_MACRO_HELD 0x1B4CA6
#define COL_SET 0x2A2A31
#define COL_SET_HELD 0x3A3A44
#define COL_OFF 0x1C1C20
#define COL_EDGE 0x3A3A42
#define COL_OK 0x3ACF6A
#define COL_TITLE 0xE03A3A
// Network state, the same three colours the board's LED shows.
#define COL_NET_AP 0x2F6FE0
#define COL_NET_STA 0x3ACF6A
#define COL_NET_REMOTE 0xE08600

#define DOT_SIZE 14
#define DOT_X (LCD_H_RES - 22)

#define TITLE_Y 2
#define NAME_Y 20
#define SPEED_Y 46

#define MACRO_ROW1_Y 74
#define MACRO_ROW2_Y 124
#define MACRO_H 44
#define MACRO_W 98
#define MACRO_X0 6
#define MACRO_DX 105

// Slots 0..4 are macro keys; the sixth cell of the grid is the Set key that opens page 2.
#define PANEL_MACROS 5

// Page 2 layout.
#define SET_HEAD_Y 2
#define SET_SPEED_LBL_Y 34
#define SET_SPEED_VAL_Y 28
#define SET_SLIDER_Y 56
#define SET_LAN_Y 94       // the transport toggle, nudged down from the slider
#define SET_BACK_Y 138

static lv_obj_t *s_page_main;
static lv_obj_t *s_page_set;

static lv_obj_t *s_dot_key;
static lv_obj_t *s_dot_ptt;
static lv_obj_t *s_dot_key_set;
static lv_obj_t *s_dot_ptt_set;
static lv_obj_t *s_dot_net;
static lv_obj_t *s_lbl_name;
static lv_obj_t *s_lbl_speed;
static lv_obj_t *s_lbl_wpm;
static lv_obj_t *s_lbl_lan_state;
static lv_obj_t *s_btn_lan;
static lv_obj_t *s_btn_lan_lbl;
static lv_obj_t *s_slider;
static lv_obj_t *s_macro_btn[PANEL_MACROS];
static lv_obj_t *s_macro_lbl[PANEL_MACROS];
static char s_macro_shown[PANEL_MACROS][MACRO_LABEL_MAX];
static char s_lan_state_shown[24];

static bool s_last_key;
static bool s_last_ptt;
static bool s_remote_lock;  // a Keyer owns the speed: the slider is greyed and untouchable
static uint8_t s_led_state = 0xFF;  // 0 = access point only, 1 = station Wi-Fi, 2 = Keyer
static uint16_t s_ticks;

static void dot_set(lv_obj_t *dot, bool on, uint32_t color)
{
    lv_obj_set_style_bg_color(dot, lv_color_hex(on ? color : COL_OFF), 0);
}

static lv_obj_t *make_dot(lv_obj_t *parent, int y)
{
    lv_obj_t *dot = lv_obj_create(parent);
    lv_obj_remove_style_all(dot);
    lv_obj_set_size(dot, DOT_SIZE, DOT_SIZE);
    lv_obj_set_pos(dot, DOT_X, y);
    lv_obj_set_style_radius(dot, LV_RADIUS_CIRCLE, 0);
    lv_obj_set_style_bg_opa(dot, LV_OPA_COVER, 0);
    lv_obj_set_style_bg_color(dot, lv_color_hex(COL_OFF), 0);
    lv_obj_set_style_border_width(dot, 1, 0);
    lv_obj_set_style_border_color(dot, lv_color_hex(COL_EDGE), 0);
    lv_obj_clear_flag(dot, LV_OBJ_FLAG_SCROLLABLE);
    return dot;
}

// A touch button with a centred label. Returns the button and hands back the label so callers
// that must rewrite its text (the LAN switch) keep a pointer.
static lv_obj_t *make_button(lv_obj_t *parent, const char *text, int x, int y, int w, int h,
                             uint32_t bg, uint32_t bg_held, lv_obj_t **out_label)
{
    lv_obj_t *btn = lv_obj_create(parent);
    lv_obj_remove_style_all(btn);
    lv_obj_add_flag(btn, LV_OBJ_FLAG_CLICKABLE);
    lv_obj_clear_flag(btn, LV_OBJ_FLAG_SCROLLABLE);
    lv_obj_set_size(btn, w, h);
    lv_obj_set_pos(btn, x, y);
    lv_obj_set_style_radius(btn, 9, 0);
    lv_obj_set_style_bg_opa(btn, LV_OPA_COVER, 0);
    lv_obj_set_style_bg_color(btn, lv_color_hex(bg), 0);
    lv_obj_set_style_bg_color(btn, lv_color_hex(bg_held), LV_STATE_PRESSED);
    lv_obj_set_style_border_width(btn, 0, 0);

    lv_obj_t *label = lv_label_create(btn);
    lv_label_set_text(label, text);
    lv_obj_set_style_text_font(label, &lv_font_montserrat_20, 0);
    lv_obj_set_style_text_color(label, lv_color_hex(COL_TEXT), 0);
    lv_obj_align(label, LV_ALIGN_CENTER, 0, 0);

    if (out_label != NULL) {
        *out_label = label;
    }
    return btn;
}

static lv_obj_t *make_page(void)
{
    lv_obj_t *page = lv_obj_create(NULL);
    lv_obj_set_style_bg_color(page, lv_color_hex(COL_BG), 0);
    lv_obj_set_style_bg_opa(page, LV_OPA_COVER, 0);
    lv_obj_set_style_pad_all(page, 0, 0);
    lv_obj_clear_flag(page, LV_OBJ_FLAG_SCROLLABLE);
    return page;
}

// ------------------------------------------------------------------ events

static void macro_pressed(lv_event_t *e)
{
    const int index = (int)(intptr_t)lv_event_get_user_data(e);
    if (!macro_play(index)) {
        ESP_LOGW(TAG, "macro %d not sent: line busy or slot empty", index);
    }
}

static void set_pressed(lv_event_t *e)
{
    (void)e;
    lv_scr_load(s_page_set);
}

static void back_pressed(lv_event_t *e)
{
    (void)e;
    lv_scr_load(s_page_main);
}

static void slider_changed(lv_event_t *e)
{
    const int value = (int)lv_slider_get_value(lv_event_get_target(e));
    keyer_set_wpm((uint8_t)value);
    lv_label_set_text_fmt(s_lbl_wpm, "%d WPM", value);
}

// The board's WS2812 says where the network stands, readable across the shack: blue with only
// the setup access point up, green once the station Wi-Fi is joined, orange while a Keyer holds
// the session. It follows the same flags as the speed line, so the LED and the glass agree.
static void led_update(bool remote, bool station)
{
    const uint8_t state = remote ? 2 : (station ? 1 : 0);
    if (state == s_led_state) {
        return;
    }
    s_led_state = state;

    uint8_t r = 0;
    uint8_t g = 0;
    uint8_t b = 0;
    uint32_t colour;
    const char *what;
    if (state == 2) {
        r = 64;
        g = 22;
        colour = COL_NET_REMOTE;
        what = "orange, a Keyer holds the session";
    } else if (state == 1) {
        g = 56;
        colour = COL_NET_STA;
        what = "green, joined the station Wi-Fi";
    } else {
        b = 56;
        colour = COL_NET_AP;
        what = "blue, access point only";
    }
    board_led_set(r, g, b);
    dot_set(s_dot_net, true, colour);
    ESP_LOGI(TAG, "status LED: %s", what);
}

// Locked looks greyed: the indicator and knob lose their colour as well as their touch, because
// a slider that only ignores input still looks adjustable.
static void slider_look(bool locked)
{
    lv_obj_set_style_bg_color(s_slider, lv_color_hex(locked ? COL_EDGE : COL_PTT),
                              LV_PART_INDICATOR);
    lv_obj_set_style_bg_color(s_slider, lv_color_hex(locked ? COL_DIM : COL_TEXT), LV_PART_KNOB);
    lv_obj_set_style_text_color(s_lbl_wpm, lv_color_hex(locked ? COL_DIM : COL_TEXT), 0);
}

// The transport toggle cycles Off -> LAN -> WAN -> Off. LAN and WAN are mutually exclusive, so
// turning one on always turns the other off. The WAN relay and pairing code are set on the web
// page; the panel only chooses which transport is live.
static void lan_pressed(lv_event_t *e)
{
    (void)e;
    const bool lan = remote_get_status().lan_enabled;
    const bool wan = wan_get_status().enabled;
    if (!lan && !wan) {
        // Off -> LAN
        wan_set_enabled(false);
        remote_set_advertising(true);
        ESP_LOGI(TAG, "panel: transport LAN");
    } else if (lan && !wan) {
        // LAN -> WAN
        remote_set_advertising(false);
        wan_set_enabled(true);
        ESP_LOGI(TAG, "panel: transport WAN");
    } else {
        // WAN (or both, defensively) -> Off
        wan_set_enabled(false);
        remote_set_advertising(false);
        ESP_LOGI(TAG, "panel: transport off");
    }
}

// ------------------------------------------------------------------ refresh

static void refresh_cb(lv_timer_t *timer)
{
    (void)timer;

    const keyer_status_t st = keyer_get_status();
    if (st.key_down != s_last_key) {
        s_last_key = st.key_down;
        dot_set(s_dot_key, st.key_down, COL_KEY);
        dot_set(s_dot_key_set, st.key_down, COL_KEY);
    }
    if (st.ptt_down != s_last_ptt) {
        s_last_ptt = st.ptt_down;
        dot_set(s_dot_ptt, st.ptt_down, COL_PTT);
        dot_set(s_dot_ptt_set, st.ptt_down, COL_PTT);
    }

    if (++s_ticks < 33) {
        return;  // twice a second is plenty for the network and macro labels
    }
    s_ticks = 0;

    const net_status_t net = net_get_status();
    if (net.connected) {
        lv_label_set_text_fmt(s_lbl_name, "%s  %s", net.ssid, net.address);
    } else {
        lv_label_set_text_fmt(s_lbl_name, "AP %s  %s", net.ap_ssid, net.address);
    }

    // A live Keyer sets the pace: the speed on screen is the speed being copied from, and the
    // station speed is no longer ours to set while it does.
    const remote_status_t rem = remote_get_status();
    const wan_status_t wan = wan_get_status();
    const bool remote = rem.active || wan.active;
    // Whichever transport owns the line sets the pace. A WAN session takes the readout when it is
    // the active one; otherwise the LAN session does.
    const unsigned remote_wpm = (wan.active && wan.peer_wpm > 0)   ? wan.peer_wpm
                                : (rem.active && rem.peer_wpm > 0) ? rem.peer_wpm
                                                                   : 0;
    if (remote) {
        // Never show the station speed under a REMOTE label: until the Keyer has keyed, say so.
        if (remote_wpm > 0) {
            lv_label_set_text_fmt(s_lbl_speed, "REMOTE: %u WPM", remote_wpm);
        } else {
            lv_label_set_text(s_lbl_speed, "REMOTE: -- WPM");
        }
    } else {
        lv_label_set_text_fmt(s_lbl_speed, "LOCAL: %u WPM", (unsigned)st.wpm);
    }
    lv_obj_set_style_text_color(s_lbl_speed, lv_color_hex(remote ? COL_OK : COL_TEXT), 0);

    if (remote != s_remote_lock) {
        s_remote_lock = remote;
        if (remote) {
            lv_obj_add_state(s_slider, LV_STATE_DISABLED);
            lv_obj_clear_flag(s_slider, LV_OBJ_FLAG_CLICKABLE);
        } else {
            lv_obj_clear_state(s_slider, LV_STATE_DISABLED);
            lv_obj_add_flag(s_slider, LV_OBJ_FLAG_CLICKABLE);
        }
        slider_look(remote);
        ESP_LOGI(TAG, "remote %s: speed line %s, speed slider %s", remote ? "up" : "gone",
                 remote ? "REMOTE" : "LOCAL", remote ? "locked" : "live");
    }

    led_update(remote, net.connected);

    for (int i = 0; i < PANEL_MACROS; i++) {
        char label[MACRO_LABEL_MAX];
        if (!macro_get(i, label, sizeof(label), NULL, 0)) {
            continue;
        }
        if (strcmp(label, s_macro_shown[i]) != 0) {
            snprintf(s_macro_shown[i], sizeof(s_macro_shown[i]), "%s", label);
            lv_label_set_text(s_macro_lbl[i], s_macro_shown[i]);
        }
    }

    // Page 2: the slider follows the station speed unless a finger is on it, and the LAN switch
    // follows the stored setting rather than the socket state, so it stays honest with the LAN
    // down.
    if (!lv_obj_has_state(s_slider, LV_STATE_PRESSED)) {
        const int station_wpm = (int)keyer_get_wpm();
        if (lv_slider_get_value(s_slider) != station_wpm) {
            lv_slider_set_value(s_slider, station_wpm, LV_ANIM_OFF);
        }
        lv_label_set_text_fmt(s_lbl_wpm, "%d WPM", station_wpm);
    }

    // The one toggle shows which transport is live (OFF / LAN / WAN) and the state line says what
    // that transport is doing. LAN and WAN are mutually exclusive, so at most one is enabled.
    char state[24];
    uint32_t state_color;
    const char *btn;
    uint32_t btn_color;
    if (wan.enabled) {
        btn = "WAN";
        btn_color = COL_OK;
        if (wan.active) {
            snprintf(state, sizeof(state), "WAN %s", wan.direct ? "direct" : "relay");
            state_color = COL_OK;
        } else if (wan.state == WAN_WIFI_DOWN) {
            snprintf(state, sizeof(state), "No network");
            state_color = COL_DIM;
        } else if (wan.state == WAN_RENDEZVOUS || wan.state == WAN_PAIRING ||
                   wan.state == WAN_RESOLVING) {
            snprintf(state, sizeof(state), "Pairing");
            state_color = COL_PTT;
        } else {
            // Idle but enabled: show this station's key fingerprint, so the operator can read the
            // "cryptokey value" off the glass.
            snprintf(state, sizeof(state), "%s", wan.pubkey_fp);
            state_color = COL_PTT;
        }
    } else if (rem.lan_enabled) {
        btn = "LAN";
        btn_color = COL_PTT;
        if (!rem.available) {
            snprintf(state, sizeof(state), "No network");
            state_color = COL_DIM;
        } else if (rem.paired && rem.alive) {
            snprintf(state, sizeof(state), "Paired");
            state_color = COL_OK;
        } else {
            snprintf(state, sizeof(state), "Advertising");
            state_color = COL_PTT;
        }
    } else {
        btn = "OFF";
        btn_color = COL_SET;
        snprintf(state, sizeof(state), "off");
        state_color = COL_DIM;
    }
    lv_label_set_text(s_btn_lan_lbl, btn);
    lv_obj_set_style_bg_color(s_btn_lan, lv_color_hex(btn_color), 0);
    if (strcmp(state, s_lan_state_shown) != 0) {
        snprintf(s_lan_state_shown, sizeof(s_lan_state_shown), "%s", state);
        lv_label_set_text(s_lbl_lan_state, s_lan_state_shown);
    }
    lv_obj_set_style_text_color(s_lbl_lan_state, lv_color_hex(state_color), 0);
}

// ------------------------------------------------------------------ orientation test

#if UI_ORIENT_SELFTEST

static void pump_for(uint32_t ms)
{
    const uint32_t start = lv_tick_get();
    while (lv_tick_get() - start < ms) {
        lv_timer_handler();
        vTaskDelay(pdMS_TO_TICKS(10));
    }
}

static void orient_page(uint32_t bg, const char *counter, uint32_t fg)
{
    lv_obj_t *scr = lv_scr_act();
    lv_obj_clean(scr);
    lv_obj_set_style_bg_color(scr, lv_color_hex(bg), 0);
    lv_obj_set_style_bg_opa(scr, LV_OPA_COVER, 0);

    lv_obj_t *top = lv_obj_create(scr);
    lv_obj_remove_style_all(top);
    lv_obj_set_size(top, LCD_H_RES, 8);
    lv_obj_set_pos(top, 0, 0);
    lv_obj_set_style_bg_color(top, lv_color_hex(0x000000), 0);
    lv_obj_set_style_bg_opa(top, LV_OPA_COVER, 0);

    lv_obj_t *left = lv_obj_create(scr);
    lv_obj_remove_style_all(left);
    lv_obj_set_size(left, 8, LCD_V_RES);
    lv_obj_set_pos(left, 0, 0);
    lv_obj_set_style_bg_color(left, lv_color_hex(0x000000), 0);
    lv_obj_set_style_bg_opa(left, LV_OPA_COVER, 0);

    lv_obj_t *tag = lv_label_create(scr);
    lv_label_set_text(tag, "TOP");
    lv_obj_set_style_text_font(tag, &lv_font_montserrat_28, 0);
    lv_obj_set_style_text_color(tag, lv_color_hex(fg), 0);
    lv_obj_set_pos(tag, 20, 24);

    lv_obj_t *num = lv_label_create(scr);
    lv_label_set_text(num, counter);
    lv_obj_set_style_text_font(num, &lv_font_montserrat_28, 0);
    lv_obj_set_style_text_color(num, lv_color_hex(fg), 0);
    lv_obj_align(num, LV_ALIGN_BOTTOM_RIGHT, -16, -10);

    lv_obj_invalidate(scr);
}

#endif  // UI_ORIENT_SELFTEST

void ui_orient_selftest(void)
{
#if UI_ORIENT_SELFTEST
    static const uint8_t madctl[] = {0x28, 0x68, 0xA8, 0xE8};
    static const uint32_t bg[] = {0xE00000, 0x00C000, 0x0020E0, 0xF0F0F0};
    static const uint32_t fg[] = {0xFFFFFF, 0x000000, 0xFFFFFF, 0x000000};
    static const char *counter[] = {"1", "2", "3", "4"};

    for (size_t i = 0; i < sizeof(madctl) / sizeof(madctl[0]); i++) {
        ESP_LOGI(TAG, "orient page %u: madctl 0x%02X", (unsigned)(i + 1), madctl[i]);
        board_display_set_madctl(madctl[i]);
        orient_page(bg[i], counter[i], fg[i]);
        pump_for(5000);
    }

    board_display_set_madctl(LCD_MADCTL);
    lv_obj_clean(lv_scr_act());
#endif
}

// ------------------------------------------------------------------ build

static void build_main_page(void)
{
    lv_obj_t *page = s_page_main = make_page();

    lv_obj_t *title = lv_label_create(page);
    lv_label_set_text(title, "W1VE Remote Player/Keyer");
    lv_obj_set_style_text_font(title, &lv_font_montserrat_14, 0);
    lv_obj_set_style_text_color(title, lv_color_hex(COL_TITLE), 0);
    lv_obj_set_width(title, 286);
    lv_label_set_long_mode(title, LV_LABEL_LONG_CLIP);
    lv_obj_set_pos(title, 6, TITLE_Y);

    s_dot_key = make_dot(page, TITLE_Y);
    s_dot_ptt = make_dot(page, NAME_Y);
    // Network state on the glass too: the LED may not be there, or not be in sight.
    s_dot_net = make_dot(page, SPEED_Y);

    // SSID and address, the two things a phone at the rig needs to read off the glass.
    s_lbl_name = lv_label_create(page);
    lv_label_set_text(s_lbl_name, "AP W1VE-Keyer  192.168.4.1");
    lv_obj_set_style_text_font(s_lbl_name, &lv_font_montserrat_20, 0);
    lv_obj_set_style_text_color(s_lbl_name, lv_color_hex(COL_OK), 0);
    lv_obj_set_width(s_lbl_name, 288);
    lv_label_set_long_mode(s_lbl_name, LV_LABEL_LONG_DOT);
    lv_obj_set_pos(s_lbl_name, 6, NAME_Y);

    s_lbl_speed = lv_label_create(page);
    lv_label_set_text_fmt(s_lbl_speed, "LOCAL: %u WPM", (unsigned)keyer_get_wpm());
    lv_obj_set_style_text_font(s_lbl_speed, &lv_font_montserrat_20, 0);
    lv_obj_set_style_text_color(s_lbl_speed, lv_color_hex(COL_TEXT), 0);
    lv_obj_set_width(s_lbl_speed, 282);
    lv_obj_set_pos(s_lbl_speed, 6, SPEED_Y);

    for (int i = 0; i < PANEL_MACROS; i++) {
        const int row = i / 3;
        const int col = i % 3;
        lv_obj_t *btn = make_button(page, "--", MACRO_X0 + col * MACRO_DX,
                                    row == 0 ? MACRO_ROW1_Y : MACRO_ROW2_Y, MACRO_W, MACRO_H,
                                    COL_MACRO, COL_MACRO_HELD, &s_macro_lbl[i]);
        lv_obj_add_event_cb(btn, macro_pressed, LV_EVENT_CLICKED, (void *)(intptr_t)i);
        s_macro_btn[i] = btn;

        char label[MACRO_LABEL_MAX];
        if (macro_get(i, label, sizeof(label), NULL, 0)) {
            snprintf(s_macro_shown[i], sizeof(s_macro_shown[i]), "%s", label);
            lv_label_set_text(s_macro_lbl[i], s_macro_shown[i]);
        }
    }

    lv_obj_t *set = make_button(page, "Set...", MACRO_X0 + 2 * MACRO_DX, MACRO_ROW2_Y, MACRO_W,
                                MACRO_H, COL_SET, COL_SET_HELD, NULL);
    lv_obj_add_event_cb(set, set_pressed, LV_EVENT_CLICKED, NULL);
}

static void build_set_page(void)
{
    lv_obj_t *page = s_page_set = make_page();

    lv_obj_t *head = lv_label_create(page);
    lv_label_set_text(head, "Settings");
    lv_obj_set_style_text_font(head, &lv_font_montserrat_20, 0);
    lv_obj_set_style_text_color(head, lv_color_hex(COL_TEXT), 0);
    lv_obj_set_pos(head, 8, SET_HEAD_Y);

    s_dot_key_set = make_dot(page, SET_HEAD_Y);
    s_dot_ptt_set = make_dot(page, SET_HEAD_Y + 18);

    lv_obj_t *speed_lbl = lv_label_create(page);
    lv_label_set_text(speed_lbl, "SPEED");
    lv_obj_set_style_text_font(speed_lbl, &lv_font_montserrat_14, 0);
    lv_obj_set_style_text_color(speed_lbl, lv_color_hex(COL_DIM), 0);
    lv_obj_set_pos(speed_lbl, 8, SET_SPEED_LBL_Y);

    s_lbl_wpm = lv_label_create(page);
    lv_label_set_text_fmt(s_lbl_wpm, "%u WPM", (unsigned)keyer_get_wpm());
    lv_obj_set_style_text_font(s_lbl_wpm, &lv_font_montserrat_20, 0);
    lv_obj_set_style_text_color(s_lbl_wpm, lv_color_hex(COL_TEXT), 0);
    lv_obj_set_width(s_lbl_wpm, 110);
    lv_obj_set_style_text_align(s_lbl_wpm, LV_TEXT_ALIGN_RIGHT, 0);
    lv_obj_set_pos(s_lbl_wpm, 200, SET_SPEED_VAL_Y);

    s_slider = lv_slider_create(page);
    lv_obj_set_size(s_slider, LCD_H_RES - 16, 22);
    lv_obj_set_pos(s_slider, 8, SET_SLIDER_Y);
    lv_slider_set_range(s_slider, KEYER_WPM_MIN, KEYER_WPM_MAX);
    lv_slider_set_value(s_slider, keyer_get_wpm(), LV_ANIM_OFF);
    lv_obj_set_style_bg_opa(s_slider, LV_OPA_COVER, LV_PART_MAIN);
    lv_obj_set_style_bg_color(s_slider, lv_color_hex(COL_OFF), LV_PART_MAIN);
    lv_obj_set_style_radius(s_slider, 6, LV_PART_MAIN);
    lv_obj_set_style_bg_opa(s_slider, LV_OPA_COVER, LV_PART_INDICATOR);
    lv_obj_set_style_bg_color(s_slider, lv_color_hex(COL_PTT), LV_PART_INDICATOR);
    lv_obj_set_style_radius(s_slider, 6, LV_PART_INDICATOR);
    lv_obj_set_style_bg_opa(s_slider, LV_OPA_COVER, LV_PART_KNOB);
    lv_obj_set_style_bg_color(s_slider, lv_color_hex(COL_TEXT), LV_PART_KNOB);
    lv_obj_set_style_radius(s_slider, LV_RADIUS_CIRCLE, LV_PART_KNOB);
    lv_obj_set_style_pad_all(s_slider, 8, LV_PART_KNOB);  // knob a thumb can find
    lv_obj_add_event_cb(s_slider, slider_changed, LV_EVENT_VALUE_CHANGED, NULL);

    // One transport toggle cycling Off -> LAN -> WAN -> Off. The two cannot run at once, so a
    // single button is honest about the exclusive choice; the state line says what it is doing.
    s_btn_lan = make_button(page, "OFF", 8, SET_LAN_Y, 118, 42, COL_PTT, COL_MACRO_HELD,
                            &s_btn_lan_lbl);
    lv_obj_add_event_cb(s_btn_lan, lan_pressed, LV_EVENT_CLICKED, NULL);

    s_lbl_lan_state = lv_label_create(page);
    lv_label_set_text(s_lbl_lan_state, "off");
    lv_obj_set_style_text_font(s_lbl_lan_state, &lv_font_montserrat_20, 0);
    lv_obj_set_style_text_color(s_lbl_lan_state, lv_color_hex(COL_PTT), 0);
    lv_obj_set_width(s_lbl_lan_state, 176);
    lv_obj_set_pos(s_lbl_lan_state, 136, SET_LAN_Y + 10);

    // Back sits at the right with room, clear of the toggle.
    lv_obj_t *back = make_button(page, "Back", 212, SET_BACK_Y, 100, 34, COL_SET, COL_SET_HELD,
                                 NULL);
    lv_obj_add_event_cb(back, back_pressed, LV_EVENT_CLICKED, NULL);
}

void ui_init(void)
{
    build_main_page();
    build_set_page();
    lv_scr_load(s_page_main);

    lv_timer_create(refresh_cb, 15, NULL);
    ESP_LOGI(TAG, "panel up: page 1 name/speed + %d macros + Set, page 2 speed/LAN, %dx%d",
             PANEL_MACROS, LCD_H_RES, LCD_V_RES);
}
